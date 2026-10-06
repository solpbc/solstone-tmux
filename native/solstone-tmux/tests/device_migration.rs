// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::Value;
use solstone_tmux::config::RuntimeConfig;
use solstone_tmux::device_migration::{
    MigrationPhase, MigrationRecord, migrate_if_needed, prepare_destination,
};
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

fn initialize_source(roots: &TestRoots, peer_credential: &spl_transport::credential::Credential) {
    let hostname = "newhost";
    let config = RuntimeConfig::load(roots.config(), hostname).expect("load initial config");
    let (_, ready) = prepare_destination(roots.config(), hostname, config);
    assert!(ready);
    persist_credential(roots.config(), peer_credential).expect("persist paired source");
    let held = roots
        .data()
        .join("captures/20261006/oldhost.tmux/held.incomplete");
    fs::create_dir_all(held.parent().expect("held parent")).expect("create old stream");
    fs::write(&held, b"held segment bytes\n").expect("write old segment");
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
    assert!(record.decision_reply.is_none());
    assert!(record.decision_state_reply.is_some());
    let request = serde_json::from_slice::<Value>(
        record
            .rekey_request
            .as_deref()
            .expect("exact saved rekey request"),
    )
    .expect("parse saved request");
    assert_eq!(request["protocol_version"], 1);
    assert_eq!(request["platform"], "linux");
    assert_eq!(request["device_label"], "newhost");
    assert_eq!(request["client_label"], "newhost");
    assert!(request.get("replaces_cid").is_none());
    let decision = serde_json::from_slice::<Value>(
        record
            .decision_request
            .as_deref()
            .expect("exact saved decision request"),
    )
    .expect("parse saved decision");
    assert_eq!(decision["choice"], "new_device");
    assert_eq!(decision["protocol_version"], 1);
    assert!(decision.get("replaces_cid").is_none());

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
    assert_eq!(
        rekey.body(),
        record.rekey_request.as_deref().expect("request bytes")
    );
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
    assert_eq!(
        put.body(),
        record.decision_request.as_deref().expect("decision bytes")
    );
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

    let source_cert = spl_transport::tls::parse_certs(&source.client_cert_pem)
        .expect("parse source certificate")
        .into_iter()
        .next()
        .expect("source certificate exists");
    let source_cid = format!("sha256:{}", sha256_hex(source_cert.as_ref()));
    let source_generation = hex_encode(&compute_pairing_generation(&source.client_cert_pem));
    let operation_id = "11111111-2222-4333-8444-555555555555";
    let mut stale = MigrationRecord::new("newhost.tmux".to_owned());
    stale.phase = MigrationPhase::RekeyPending;
    stale.destination_published = true;
    stale.pending_marker_digest = Some("a".repeat(64));
    stale.source_cid = Some(source_cid);
    stale.source_generation = Some(source_generation);
    stale.source_credential = Some(source);
    stale.rekey_operation_id = Some(operation_id.to_owned());
    stale.candidate_key_pem = Some("candidate-key-pem".to_owned());
    stale.csr_pem = Some("candidate-csr".to_owned());
    stale.rekey_request = Some(
        serde_json::to_vec(&serde_json::json!({
            "protocol_version": 1,
            "operation_id": operation_id,
            "csr": "candidate-csr",
            "device_label": "newhost",
            "client_label": "newhost",
            "platform": "linux"
        }))
        .expect("serialize stale request"),
    );
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
