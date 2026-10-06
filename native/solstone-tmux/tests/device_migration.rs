// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::Value;
use solstone_tmux::config::{CONFIG_FILENAME, RuntimeConfig};
use solstone_tmux::device_migration::{MigrationPhase, MigrationRecord, migrate_if_needed};
use solstone_tmux::health::HealthWriter;
use solstone_tmux::instance_lock::InstanceLock;
use solstone_tmux::journal_version::hex_encode;
use solstone_tmux::pairing_answer::{read_answer_file, write_answer_file};
use solstone_tmux::paths::{PlatformKind, ensure_private_directory};
use solstone_tmux::post_connect::compute_pairing_generation;
use solstone_tmux::private_link::{
    acquire_private_state_lock, load_credential, persist_credential,
};
use solstone_tmux::sync::{RetentionFence, SyncActivity, SyncTask, SyncWake};
use spl_core::ca::sha256_hex;
use support::private_link_peer::PrivateLinkPeer;

struct TestRoots {
    _root: PathBuf,
    config: PathBuf,
    data: PathBuf,
}

impl TestRoots {
    fn new(label: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = PathBuf::from(format!(
            "/var/tmp/solstone-tmux-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).expect("create migration test root");
        let config = root.join("config");
        let data = root.join("data");
        ensure_private_directory(&config).expect("create private config root");
        ensure_private_directory(&data).expect("create private data root");
        Self {
            _root: root,
            config,
            data,
        }
    }

    fn config(&self) -> &Path {
        &self.config
    }

    fn data(&self) -> &Path {
        &self.data
    }
}

impl Drop for TestRoots {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self._root);
    }
}

fn identity() -> solstone_tmux::instance_lock::RunIdentity {
    solstone_tmux::instance_lock::RunIdentity {
        run_id: "device-migration-test".to_owned(),
        lock_inode: 1,
    }
}

fn now() -> i64 {
    1_800_000_000
}

/// The settings file setup writes: the stream is left to follow the hostname.
const SETUP_CONFIG: &[u8] = br#"{"stream":null,"capture_interval":5,"segment_interval":300,"cache_retention_days":7,"status_indicator":true}"#;

fn initialize_source(roots: &TestRoots, peer_credential: &spl_transport::credential::Credential) {
    fs::write(roots.config().join(CONFIG_FILENAME), SETUP_CONFIG).expect("write setup config");
    persist_credential(roots.config(), peer_credential).expect("persist paired source");
    let held = roots
        .data()
        .join("captures/20261006/oldhost.tmux/held.incomplete");
    fs::create_dir_all(held.parent().expect("held parent")).expect("create old stream");
    fs::write(&held, b"held segment bytes\n").expect("write old segment");
}

fn pending_rekey(source: &spl_transport::credential::Credential) -> MigrationRecord {
    let source_cert = spl_transport::tls::parse_certs(&source.client_cert_pem)
        .expect("parse source certificate")
        .into_iter()
        .next()
        .expect("source certificate exists");
    let operation_id = "11111111-2222-4333-8444-555555555555";
    let mut pending = MigrationRecord::default();
    pending.phase = MigrationPhase::RekeyPending;
    pending.pending_marker_digest = Some("a".repeat(64));
    pending.source_cid = Some(format!("sha256:{}", sha256_hex(source_cert.as_ref())));
    pending.source_generation = Some(hex_encode(&compute_pairing_generation(
        &source.client_cert_pem,
    )));
    pending.source_credential = Some(source.clone());
    pending.rekey_operation_id = Some(operation_id.to_owned());
    pending.candidate_key_pem = Some("candidate-key-pem".to_owned());
    pending.csr_pem = Some("candidate-csr".to_owned());
    pending.rekey_request = Some(
        serde_json::to_vec(&serde_json::json!({
            "protocol_version": 1,
            "operation_id": operation_id,
            "csr": "candidate-csr",
            "device_label": "newhost",
            "client_label": "newhost",
            "platform": "linux"
        }))
        .expect("serialize pending request"),
    );
    pending
}

#[tokio::test]
async fn authenticated_routes_publish_candidate_and_transfer_confirmation_after_marker_change() {
    let roots = TestRoots::new("migration-success");
    let peer = PrivateLinkPeer::start().await;
    peer.lose_next_migration_decision_reply();
    let source = peer.credential();
    initialize_source(&roots, &source);
    let source_generation = hex_encode(&compute_pairing_generation(&source.client_cert_pem));
    write_answer_file(roots.config(), &source_generation).expect("confirm source credential");
    let source_marker = "a".repeat(64);
    let destination_marker = "b".repeat(64);
    solstone_tmux::device_migration::record_setup_baseline(roots.config(), Some(&source_marker))
        .expect("persist source marker");

    let candidate = migrate_if_needed(
        roots.config(),
        roots.data(),
        PlatformKind::Linux,
        "newhost",
        &destination_marker,
        source.clone(),
        identity(),
        now(),
    )
    .await
    .expect("migration finishes");
    assert_ne!(candidate.client_cert_pem, source.client_cert_pem);
    assert_eq!(candidate.instance_id, source.instance_id);
    assert_eq!(
        read_answer_file(roots.config())
            .expect("read migrated confirmation")
            .expect("confirmation exists")
            .confirmed,
        hex_encode(&compute_pairing_generation(&candidate.client_cert_pem))
    );

    let record = MigrationRecord::load(roots.config())
        .expect("load completed record")
        .expect("record exists");
    assert_eq!(record.phase, MigrationPhase::Adopted);
    assert_eq!(
        record.adopted_marker_digest.as_deref(),
        Some(destination_marker.as_str())
    );
    assert!(record.source_credential.is_none());
    assert!(record.candidate_credential.is_none());
    assert!(record.candidate_key_pem.is_none());
    assert!(record.rekey_request.is_none());
    assert!(record.decision_request.is_none());

    let source_sha = sha256_hex(
        spl_transport::tls::parse_certs(&source.client_cert_pem)
            .expect("parse source cert")
            .first()
            .expect("source cert exists")
            .as_ref(),
    );
    let candidate_sha = sha256_hex(
        spl_transport::tls::parse_certs(&candidate.client_cert_pem)
            .expect("parse candidate cert")
            .first()
            .expect("candidate cert exists")
            .as_ref(),
    );
    let requests = peer.requests();
    let rekey = requests
        .iter()
        .find(|request| request.path_without_query().ends_with("/rekey"))
        .expect("authenticated rekey request");
    assert_eq!(rekey.method(), "POST");
    assert_eq!(
        rekey.authenticated_client_sha256(),
        Some(source_sha.as_str())
    );
    let request = serde_json::from_slice::<Value>(rekey.body()).expect("parse sent rekey request");
    assert_eq!(request["protocol_version"], 1);
    assert_eq!(request["platform"], "linux");
    assert_eq!(request["device_label"], "newhost");
    assert_eq!(request["client_label"], "newhost");
    assert!(request.get("replaces_cid").is_none());
    let get = requests
        .iter()
        .find(|request| {
            request.method() == "GET" && request.path_without_query().ends_with("/migration")
        })
        .expect("candidate-authenticated migration state request");
    assert_eq!(
        requests
            .iter()
            .filter(|request| {
                request.method() == "GET" && request.path_without_query().ends_with("/migration")
            })
            .count(),
        2,
        "the lost PUT reply is reconciled by GET"
    );
    let put = requests
        .iter()
        .find(|request| {
            request.method() == "PUT" && request.path_without_query().ends_with("/migration")
        })
        .expect("candidate-authenticated decision request");
    assert_eq!(
        get.authenticated_client_sha256(),
        Some(candidate_sha.as_str())
    );
    assert_eq!(
        put.authenticated_client_sha256(),
        Some(candidate_sha.as_str())
    );
    let decision = serde_json::from_slice::<Value>(put.body()).expect("parse sent decision");
    assert_eq!(decision["choice"], "new_device");
    assert_eq!(decision["protocol_version"], 1);
    assert!(decision.get("replaces_cid").is_none());
    assert_eq!(
        load_credential(roots.config()).expect("load final credential"),
        Some(candidate.clone())
    );
    let candidate = load_credential(roots.config())
        .expect("load first migration credential")
        .expect("credential exists");
    let second_marker = "e".repeat(64);
    let second_candidate = migrate_if_needed(
        roots.config(),
        roots.data(),
        PlatformKind::Linux,
        "newhost",
        &second_marker,
        candidate.clone(),
        identity(),
        now(),
    )
    .await
    .expect("second separate move finishes");
    assert_ne!(second_candidate.client_cert_pem, candidate.client_cert_pem);
    assert_eq!(
        read_answer_file(roots.config())
            .expect("read second migrated confirmation")
            .expect("confirmation exists")
            .confirmed,
        hex_encode(&compute_pairing_generation(
            &second_candidate.client_cert_pem
        ))
    );
    let held = roots
        .data()
        .join("captures/20261006/oldhost.tmux/held.incomplete");
    assert_eq!(
        fs::read(held).expect("old folder remains"),
        b"held segment bytes\n"
    );
    let second_rekey = peer
        .requests()
        .into_iter()
        .filter(|request| request.path_without_query().ends_with("/rekey"))
        .collect::<Vec<_>>();
    assert_eq!(second_rekey.len(), 2);
    assert_eq!(second_rekey[1].method(), "POST");
    assert_eq!(
        second_rekey[1].authenticated_client_sha256(),
        Some(candidate_sha.as_str())
    );
    peer.shutdown().await;
}

#[tokio::test]
async fn absent_legacy_answer_is_grandfathered_before_migration_snapshot() {
    let roots = TestRoots::new("migration-legacy-answer");
    let peer = PrivateLinkPeer::start().await;
    let source = peer.credential();
    initialize_source(&roots, &source);
    assert!(
        read_answer_file(roots.config())
            .expect("read legacy answer")
            .is_none()
    );

    let candidate = migrate_if_needed(
        roots.config(),
        roots.data(),
        PlatformKind::Linux,
        "newhost",
        &"f".repeat(64),
        source,
        identity(),
        now(),
    )
    .await
    .expect("migration finishes after legacy grandfathering");
    assert_eq!(
        read_answer_file(roots.config())
            .expect("read transferred answer")
            .expect("migrated confirmation exists")
            .confirmed,
        hex_encode(&compute_pairing_generation(&candidate.client_cert_pem))
    );
    peer.shutdown().await;
}

#[tokio::test]
async fn restarted_sync_supersedes_stale_rekey_after_fresh_setup() {
    let roots = TestRoots::new("migration-setup-supersedes-rekey");
    let source_peer = PrivateLinkPeer::start().await;
    let setup_peer = PrivateLinkPeer::start().await;
    let source = source_peer.credential();
    let fresh_setup = setup_peer.credential();
    initialize_source(&roots, &source);
    write_answer_file(roots.config(), "").expect("preserve walk-away pause");

    let stale = pending_rekey(&source);
    stale
        .persist(roots.config())
        .expect("persist stale migration");
    persist_credential(roots.config(), &fresh_setup).expect("persist completed setup");

    let current = migrate_if_needed(
        roots.config(),
        roots.data(),
        PlatformKind::Linux,
        "newhost",
        &"a".repeat(64),
        fresh_setup.clone(),
        identity(),
        now(),
    )
    .await
    .expect("stale transaction is superseded");
    assert_eq!(current, fresh_setup);
    assert!(source_peer.requests().is_empty());
    assert!(setup_peer.requests().is_empty());
    assert_eq!(
        read_answer_file(roots.config())
            .expect("read preserved answer")
            .expect("pause remains present")
            .confirmed,
        ""
    );
    let reset = MigrationRecord::load(roots.config())
        .expect("load superseding baseline")
        .expect("migration state remains");
    assert_eq!(reset.phase, MigrationPhase::Adopted);
    assert_eq!(
        reset.adopted_marker_digest.as_deref(),
        Some("a".repeat(64).as_str())
    );
    assert!(reset.source_credential.is_none());
    assert!(reset.rekey_request.is_none());

    source_peer.shutdown().await;
    setup_peer.shutdown().await;
}

#[tokio::test]
async fn unsupported_decision_route_retries_the_exact_saved_decision_bytes() {
    let roots = TestRoots::new("migration-decision-retry");
    let peer = PrivateLinkPeer::start().await;
    let source = peer.credential();
    initialize_source(&roots, &source);
    solstone_tmux::device_migration::record_setup_baseline(roots.config(), Some(&"1".repeat(64)))
        .expect("persist source marker");
    peer.set_migration_decisions_supported(false);

    for _ in 0..2 {
        assert!(
            migrate_if_needed(
                roots.config(),
                roots.data(),
                PlatformKind::Linux,
                "newhost",
                &"2".repeat(64),
                source.clone(),
                identity(),
                now(),
            )
            .await
            .is_err()
        );
    }
    let pending = MigrationRecord::load(roots.config())
        .expect("load pending decision")
        .expect("transaction exists");
    assert_eq!(pending.phase, MigrationPhase::DecisionPending);
    let decision_bytes = pending
        .decision_request
        .as_deref()
        .expect("decision bytes persisted");
    let candidate_sha = sha256_hex(
        spl_transport::tls::parse_certs(
            &pending
                .candidate_credential
                .as_ref()
                .expect("candidate persisted")
                .client_cert_pem,
        )
        .expect("parse candidate certificate")
        .first()
        .expect("candidate certificate exists")
        .as_ref(),
    );
    let first_attempts = peer
        .requests()
        .into_iter()
        .filter(|request| {
            request.method() == "PUT" && request.path_without_query().ends_with("/migration")
        })
        .collect::<Vec<_>>();
    assert_eq!(first_attempts.len(), 2);
    assert!(first_attempts.iter().all(|request| {
        request.body() == decision_bytes
            && request.authenticated_client_sha256() == Some(candidate_sha.as_str())
    }));

    peer.set_migration_decisions_supported(true);
    let candidate = migrate_if_needed(
        roots.config(),
        roots.data(),
        PlatformKind::Linux,
        "newhost",
        &"2".repeat(64),
        source,
        identity(),
        now(),
    )
    .await
    .expect("decision succeeds after route is restored");
    assert_ne!(candidate.client_cert_pem, peer.credential().client_cert_pem);
    let attempts = peer
        .requests()
        .into_iter()
        .filter(|request| {
            request.method() == "PUT" && request.path_without_query().ends_with("/migration")
        })
        .collect::<Vec<_>>();
    assert_eq!(attempts.len(), 3);
    assert!(attempts.iter().all(|request| {
        request.body() == decision_bytes
            && request.authenticated_client_sha256() == Some(candidate_sha.as_str())
    }));
    peer.shutdown().await;
}

#[tokio::test]
async fn unsupported_rekey_holds_carried_credential_outbound_while_sync_stays_supervised() {
    let roots = TestRoots::new("migration-outbound-hold");
    let peer = PrivateLinkPeer::start().await;
    let source = peer.credential();
    initialize_source(&roots, &source);
    let source_generation = hex_encode(&compute_pairing_generation(&source.client_cert_pem));
    write_answer_file(roots.config(), &source_generation).expect("confirm source credential");
    solstone_tmux::device_migration::record_setup_baseline(roots.config(), Some(&"3".repeat(64)))
        .expect("persist source marker");
    peer.set_migration_routes_supported(false);

    let config = RuntimeConfig::load(roots.config(), "newhost").expect("load config");
    let lock = InstanceLock::acquire(roots.data()).expect("instance lock");
    let _private_state_lock =
        acquire_private_state_lock(roots.config()).expect("private state lock");
    let (stop, shutdown) = tokio::sync::watch::channel(false);
    let (activity, _activity_receiver) = tokio::sync::watch::channel(SyncActivity::Idle);
    let task = SyncTask {
        config_root: roots.config().to_owned(),
        data_root: roots.data().to_owned(),
        config,
        hostname: "newhost".to_owned(),
        clock: Arc::new(solstone_tmux::clock::SystemClock::utc()),
        wake: SyncWake::default(),
        activity,
        health: HealthWriter::new(roots.data().to_owned(), &lock),
        retention_fence: Arc::new(RetentionFence::new()),
        identity: lock.identity().clone(),
        health_refresh_interval: Duration::from_secs(5),
        answer_lock_timeout: Duration::from_secs(1),
        platform: PlatformKind::Linux,
        marker_digest: Some("4".repeat(64)),
    };
    let task_handle = tokio::spawn(task.run(shutdown));
    peer.wait_for_request_count(1, Duration::from_secs(5)).await;
    assert!(
        tokio::time::timeout(
            Duration::from_millis(500),
            peer.wait_for_accepted_carrier_count(2),
        )
        .await
        .is_err(),
        "a held copied credential must not create an ordinary JournalSession bridge"
    );

    let requests = peer.requests();
    assert!(requests.iter().any(|request| {
        request.method() == "POST" && request.path_without_query().ends_with("/rekey")
    }));
    assert!(requests.iter().all(|request| {
        let path = request.path_without_query();
        path != "/app/devices/ingest"
            && path != "/app/network/api/clients/self"
            && path != "/app/network/api/relay/access"
    }));

    stop.send_replace(true);
    tokio::time::timeout(Duration::from_secs(5), task_handle)
        .await
        .expect("held sync shuts down")
        .expect("join held sync")
        .expect("clean shutdown");
    drop(lock);
    peer.shutdown().await;
}

#[tokio::test]
async fn unsupported_route_and_version_retries_replay_exact_saved_rekey_bytes() {
    for (label, unsupported_version) in [("missing-route", false), ("unsupported-version", true)] {
        let roots = TestRoots::new(label);
        let peer = PrivateLinkPeer::start().await;
        let source = peer.credential();
        initialize_source(&roots, &source);
        let source_marker = "c".repeat(64);
        let destination_marker = "d".repeat(64);
        solstone_tmux::device_migration::record_setup_baseline(
            roots.config(),
            Some(&source_marker),
        )
        .expect("persist baseline");
        if unsupported_version {
            peer.set_migration_protocol_version(2);
        } else {
            peer.set_migration_routes_supported(false);
        }

        let first = migrate_if_needed(
            roots.config(),
            roots.data(),
            PlatformKind::Linux,
            "newhost",
            &destination_marker,
            source.clone(),
            identity(),
            now(),
        )
        .await;
        assert!(first.is_err(), "{label}");
        let saved_first = MigrationRecord::load(roots.config())
            .expect("load first retry state")
            .expect("record exists")
            .rekey_request
            .expect("request persisted before send");
        let original_candidate_key = MigrationRecord::load(roots.config())
            .expect("load candidate key")
            .expect("record exists")
            .candidate_key_pem
            .expect("candidate key persisted");
        let second = migrate_if_needed(
            roots.config(),
            roots.data(),
            PlatformKind::Linux,
            "newhost",
            &destination_marker,
            source.clone(),
            identity(),
            now(),
        )
        .await;
        assert!(second.is_err(), "{label}");
        let saved_second = MigrationRecord::load(roots.config())
            .expect("load second retry state")
            .expect("record exists")
            .rekey_request
            .expect("request retained");
        assert_eq!(saved_first, saved_second, "{label}");
        let route_requests = peer
            .requests()
            .into_iter()
            .filter(|request| request.path_without_query().ends_with("/rekey"))
            .collect::<Vec<_>>();
        assert_eq!(route_requests.len(), 2, "{label}");
        assert_eq!(
            route_requests[0].body(),
            route_requests[1].body(),
            "{label}"
        );
        assert_eq!(route_requests[0].body(), saved_first.as_slice(), "{label}");
        if label == "missing-route" {
            let copied = TestRoots::new("copied-pending");
            for filename in ["config.json", "credentials.json", "device-migration.json"] {
                fs::copy(
                    roots.config().join(filename),
                    copied.config().join(filename),
                )
                .expect("copy pending private state");
            }
            let held = copied
                .data()
                .join("captures/20261006/sourcehost.tmux/held.incomplete");
            fs::create_dir_all(held.parent().expect("held parent"))
                .expect("create copied machine held folder");
            fs::write(&held, b"copied observation stays held\n")
                .expect("write copied machine held segment");
            assert!(
                migrate_if_needed(
                    copied.config(),
                    copied.data(),
                    PlatformKind::Linux,
                    "newhost",
                    &"e".repeat(64),
                    source.clone(),
                    identity(),
                    now(),
                )
                .await
                .is_err()
            );
            let regenerated = MigrationRecord::load(copied.config())
                .expect("load copied transaction")
                .expect("copied transaction exists");
            assert_eq!(
                regenerated.pending_marker_digest.as_deref(),
                Some("e".repeat(64).as_str())
            );
            assert_ne!(
                regenerated.candidate_key_pem.as_deref(),
                Some(original_candidate_key.as_str())
            );
            assert_ne!(regenerated.rekey_request, Some(saved_first.clone()));
            assert_eq!(
                fs::read(held).expect("copied held segment remains"),
                b"copied observation stays held\n"
            );
        }
        peer.shutdown().await;
    }
}

#[tokio::test]
async fn interrupted_publication_recovers_through_sync_startup_on_a_renamed_copy() {
    use solstone_tmux::clock::{Clock, TestClock, Zone, ZoneSource};
    use solstone_tmux::observer::SegmentManager;
    use solstone_tmux::private_link::CREDENTIALS_FILENAME;
    use solstone_tmux::storage::{AtomicWriteFault, set_atomic_write_fault_for_path};

    struct UtcZone;
    impl ZoneSource for UtcZone {
        fn read(&mut self) -> Result<Zone, String> {
            Ok(Zone::utc())
        }
    }
    const HELD_BYTES: &[u8] = b"held under the source hostname\n";

    let roots = TestRoots::new("migration-publication-recovery");
    let peer = PrivateLinkPeer::start().await;
    peer.answer_uploads_with_received_descriptors();
    let source = peer.credential();
    initialize_source(&roots, &source);
    write_answer_file(
        roots.config(),
        &hex_encode(&compute_pairing_generation(&source.client_cert_pem)),
    )
    .expect("confirm source credential");
    solstone_tmux::device_migration::record_setup_baseline(roots.config(), Some(&"a".repeat(64)))
        .expect("persist source marker");
    let held_segment = roots
        .data()
        .join("captures/20261006/oldhost.tmux/110000_300");
    fs::create_dir_all(&held_segment).expect("create held segment");
    fs::write(held_segment.join("tmux_held_screen.jsonl"), HELD_BYTES).expect("write held segment");

    // The process stops after the publication checkpoint, before the fresh
    // credential reaches disk.
    let credentials = roots.config().join(CREDENTIALS_FILENAME);
    set_atomic_write_fault_for_path(&credentials, Some(AtomicWriteFault::FailBeforeRename));
    let interrupted = migrate_if_needed(
        roots.config(),
        roots.data(),
        PlatformKind::Linux,
        "newhost",
        &"b".repeat(64),
        source.clone(),
        identity(),
        now(),
    )
    .await;
    set_atomic_write_fault_for_path(&credentials, None);
    assert!(interrupted.is_err());
    let checkpoint = MigrationRecord::load(roots.config())
        .expect("load checkpoint")
        .expect("checkpoint exists");
    assert_eq!(checkpoint.phase, MigrationPhase::Publishing);
    let candidate = checkpoint
        .candidate_credential
        .expect("candidate saved at checkpoint");
    assert_eq!(
        load_credential(roots.config()).expect("load source"),
        Some(source.clone())
    );
    let decisions_before_restart = peer.requests().len();

    // The copied default settings follow the new hostname for capture and sync.
    let config = RuntimeConfig::load(roots.config(), "newhost").expect("load copied settings");
    assert_eq!(config.stream.as_str(), "newhost.tmux");
    let clock = Arc::new(TestClock::new(
        time::OffsetDateTime::from_unix_timestamp(1_791_288_000).expect("fixed wall time"),
        Duration::ZERO,
        time::UtcOffset::UTC,
    ));
    let manager = SegmentManager::start(
        roots.data().to_owned(),
        config.stream.clone(),
        clock.as_ref(),
        config.segment_interval,
        SyncWake::default(),
        Box::new(UtcZone),
        Arc::new(solstone_tmux::tmux::StderrWarnings),
    )
    .expect("capture opens under the new hostname");
    assert!(roots.data().join("captures/20261006/newhost.tmux").is_dir());
    drop(manager);

    let lock = InstanceLock::acquire(roots.data()).expect("instance lock");
    let _private_state_lock =
        acquire_private_state_lock(roots.config()).expect("private state lock");
    let (stop, shutdown) = tokio::sync::watch::channel(false);
    let (activity, _activity_receiver) = tokio::sync::watch::channel(SyncActivity::Idle);
    let task = tokio::spawn(
        SyncTask {
            config_root: roots.config().to_owned(),
            data_root: roots.data().to_owned(),
            config,
            hostname: "newhost".to_owned(),
            clock: clock as Arc<dyn Clock>,
            wake: SyncWake::default(),
            activity,
            health: HealthWriter::new(roots.data().to_owned(), &lock),
            retention_fence: Arc::new(RetentionFence::new()),
            identity: lock.identity().clone(),
            health_refresh_interval: Duration::from_millis(50),
            answer_lock_timeout: Duration::from_secs(1),
            platform: PlatformKind::Linux,
            marker_digest: Some("b".repeat(64)),
        }
        .run(shutdown),
    );
    let ingest = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(request) = peer
                .requests()
                .into_iter()
                .find(|request| request.path_without_query() == "/app/devices/ingest")
            {
                return request;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the held segment uploads after recovery");
    stop.send_replace(true);
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("sync stops")
        .expect("join sync")
        .expect("clean shutdown");

    let candidate_sha = sha256_hex(
        spl_transport::tls::parse_certs(&candidate.client_cert_pem)
            .expect("parse candidate cert")
            .first()
            .expect("candidate cert exists")
            .as_ref(),
    );
    assert_eq!(
        ingest.authenticated_client_sha256(),
        Some(candidate_sha.as_str())
    );
    for literal in [
        HELD_BYTES,
        b"110000_300".as_slice(),
        b"tmux_held_screen.jsonl".as_slice(),
    ] {
        assert!(support::private_link_peer::find_bytes(ingest.body(), literal).is_some());
    }
    assert_eq!(
        load_credential(roots.config()).expect("load recovered credential"),
        Some(candidate.clone())
    );
    assert_eq!(
        read_answer_file(roots.config())
            .expect("read recovered answer")
            .expect("answer exists")
            .confirmed,
        hex_encode(&compute_pairing_generation(&candidate.client_cert_pem))
    );
    let recovered = MigrationRecord::load(roots.config())
        .expect("load recovered record")
        .expect("record exists");
    assert_eq!(recovered.phase, MigrationPhase::Adopted);
    assert_eq!(
        recovered.adopted_marker_digest.as_deref(),
        Some("b".repeat(64).as_str())
    );
    assert!(
        peer.requests()[decisions_before_restart..]
            .iter()
            .all(|request| !request
                .path_without_query()
                .starts_with("/app/network/api/clients/self/")),
        "recovery replays no migration operation"
    );
    drop(lock);
    peer.shutdown().await;
}

#[tokio::test]
async fn rejection_while_a_migration_is_pending_retires_nothing() {
    use solstone_tmux::cli::MarkOption;
    use solstone_tmux::health::DiagnosticCode;
    use solstone_tmux::pairing_answer::{Outcome, TerminalSeat};
    use solstone_tmux::private_link::confirm;

    let temporary = support::TestDirectory::new("migration-pending-rejection");
    let roots = support::IsolatedRoots::new(temporary.path());
    let environment = support::FakeEnvironment::from_paths(roots.entries().iter().cloned());
    let config_root = roots.config_root();
    ensure_private_directory(&config_root).expect("config root");
    let peer = PrivateLinkPeer::start().await;
    let mut source = peer.credential();
    let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).expect("key");
    let certificate = rcgen::CertificateParams::new(Vec::<String>::new())
        .expect("params")
        .self_signed(&key)
        .expect("certificate");
    source.instance_id = spl_core::relay_window::jid_from_spki(
        &spl_core::ca::extract_spki_der(certificate.der()).expect("spki"),
    )
    .expect("spoken instance id");
    persist_credential(&config_root, &source).expect("persist carried credential");
    write_answer_file(&config_root, "").expect("answer awaits confirmation");
    pending_rekey(&source)
        .persist(&config_root)
        .expect("migration owns the carried credential");

    let outcome = confirm(
        solstone_tmux::service::current_platform(),
        &environment,
        TerminalSeat::Scripted(None::<std::io::Cursor<Vec<u8>>>),
        MarkOption::Value("wrong mark".to_owned()),
    )
    .await;

    assert_eq!(outcome, Outcome::Diagnostic(DiagnosticCode::PrivateStateIo));
    assert!(peer.requests().is_empty(), "no DELETE of the source device");
    assert_eq!(peer.accepted_carriers(), 0);
    assert_eq!(
        load_credential(&config_root).expect("load carried credential"),
        Some(source)
    );
    assert_eq!(
        MigrationRecord::load(&config_root)
            .expect("load migration")
            .expect("migration remains")
            .phase,
        MigrationPhase::RekeyPending
    );
    peer.shutdown().await;
}
