// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

mod support;

use std::fs;
use std::future::Future;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use solstone_tmux::clock::{Clock, TestClock};
use solstone_tmux::config::{CONFIG_FILENAME, RuntimeConfig};
use solstone_tmux::health::{DiagnosticCode, HEALTH_FILENAME, HealthWriter};
use solstone_tmux::instance_lock::InstanceLock;
use solstone_tmux::journal::INGEST_PATH;
use solstone_tmux::model::CaptureResult;
use solstone_tmux::observer::{
    CaptureProvider, ObserverConfig, ObserverOperationError, SegmentManager, ShutdownEvent,
    run_observer, shutdown_barrier, stream_directory,
};
use solstone_tmux::paths::ensure_private_directory;
use solstone_tmux::private_link::{
    OBSERVER_HEADER_NAME, PROTOCOL_VERSION, PROTOCOL_VERSION_HEADER_NAME, persist_credential,
};
use solstone_tmux::segment::SegmentState;
use solstone_tmux::sync::{JournalSession, SyncActivity, SyncScheduler, SyncTask, SyncWake};
use support::private_link_peer::PrivateLinkPeer;
use support::{TestDirectory, golden_capture};
use time::{Date, Month, PrimitiveDateTime, Time, UtcOffset};
use tokio::sync::{oneshot, watch};

const CANDIDATE_BYTES: &[u8] = b"existing cache remains\n";
const LINKED_DEVICE_DAY: &str = "20260729";
const LINKED_DEVICE_STREAM: &str = "host.tmux";
const LINKED_DEVICE_SEGMENT: &str = "120000_300";
const LINKED_DEVICE_FILE: &str = "tmux_linked_device_screen.jsonl";
const LINKED_DEVICE_BYTES: &[u8] = b"linked-device candidate\n";
// A configured stream that does not derive from the running hostname, so the
// binding check must refuse before any network work happens.
const CUSTOM_STREAM_CONFIG: &[u8] = br#"{"stream":"extro.tmux","capture_interval":7,"segment_interval":600,"cache_retention_days":14,"status_indicator":false}"#;

#[tokio::test]
async fn custom_stream_refuses_before_network_while_capture_continues() {
    let fixture = BindingFixture::new("binding-custom-stream");
    fixture.install_config(CUSTOM_STREAM_CONFIG);
    let peer = PrivateLinkPeer::start().await;

    let evidence = run_binding_failure(
        &fixture,
        &peer,
        "different-host.example",
        DiagnosticCode::ConfiguredStreamMismatch,
    )
    .await;

    assert_eq!(peer.accepted_carriers(), 0);
    assert!(peer.requests().is_empty());
    evidence.assert_capture_continued();
    assert_eq!(
        fs::read(&evidence.candidate).expect("retained candidate"),
        CANDIDATE_BYTES
    );
    assert_eq!(
        evidence.health["last_error_code"],
        "configured_stream_mismatch"
    );
    peer.shutdown().await;
}

#[test]
fn linked_device_sweep_diagnostics_are_actionable() {
    assert_eq!(
        DiagnosticCode::ConfiguredStreamMismatch.as_str(),
        "configured_stream_mismatch"
    );
    assert_eq!(
        DiagnosticCode::ConfiguredStreamMismatch.message(),
        "set stream to the hostname-derived tmux name and restart"
    );
    assert_eq!(DiagnosticCode::JournalRejected.as_str(), "journal_rejected");
    assert_eq!(
        DiagnosticCode::JournalRejected.message(),
        "journal request was rejected"
    );
}

#[test]
fn linked_device_sweep_uses_exactly_the_v3_upload_operation_without_legacy_headers() {
    linked_device_runtime().block_on(async {
        let peer = PrivateLinkPeer::start().await;
        peer.answer_uploads_with_received_descriptors();
        let temporary = TestDirectory::new("linked-device-upload-operation");
        ensure_private_directory(temporary.path()).expect("private root");
        let lock = InstanceLock::acquire(temporary.path()).expect("acquire lock");
        let candidate = create_linked_device_candidate(&temporary);
        let credential = peer.credential();
        let refresh = solstone_tmux::journal_version::VersionRefreshState::new(
            temporary.path().to_path_buf(),
            temporary.path().to_path_buf(),
            credential.instance_id.clone(),
            &credential.ca_fp_prefix,
            lock.identity().clone(),
        );
        let mut session =
            JournalSession::start(credential.clone(), temporary.path().to_path_buf(), refresh)
                .await
                .expect("linked-device session");
        let mut scheduler = linked_device_scheduler(&temporary, &credential);

        let summary = scheduler
            .run_sweep(&mut session, linked_device_no_shutdown())
            .await;

        assert_eq!(summary.attempted, 1);
        assert_eq!(summary.custodied, 1, "{summary:?}");
        let ingest_requests = peer
            .requests()
            .into_iter()
            .filter(|req| !req.path_without_query().starts_with("/app/network/api/"))
            .collect::<Vec<_>>();
        assert_eq!(
            ingest_requests
                .iter()
                .map(|request| (
                    request.method().to_owned(),
                    request.path_without_query().to_owned()
                ))
                .collect::<Vec<_>>(),
            vec![("POST".to_owned(), INGEST_PATH.to_owned())],
            "the real mTLS peer must observe no registration or extra liveness request",
        );
        for request in &ingest_requests {
            assert_eq!(
                request.query_param("source"),
                Some(solstone_tmux::config::DEFAULT_SOURCE)
            );
            assert_eq!(
                request.header(PROTOCOL_VERSION_HEADER_NAME),
                Some(PROTOCOL_VERSION)
            );
            assert_legacy_header_is_absent(request, "authorization");
            assert_legacy_header_is_absent(request, OBSERVER_HEADER_NAME);
        }
        assert!(
            !candidate.exists(),
            "confirmed candidate segment was removed"
        );
        session
            .shutdown()
            .await
            .expect("shutdown linked-device session");
        peer.shutdown().await;
    });
}

#[test]
fn linked_device_sweep_sends_the_configured_source_on_every_v3_operation() {
    linked_device_runtime().block_on(async {
        let peer = PrivateLinkPeer::start().await;
        peer.answer_uploads_with_received_descriptors();
        let temporary = TestDirectory::new("linked-device-configured-source");
        ensure_private_directory(temporary.path()).expect("private root");
        let lock = InstanceLock::acquire(temporary.path()).expect("acquire lock");
        let candidate = create_linked_device_candidate(&temporary);
        let credential = peer.credential();
        let refresh = solstone_tmux::journal_version::VersionRefreshState::new(
            temporary.path().to_path_buf(),
            temporary.path().to_path_buf(),
            credential.instance_id.clone(),
            &credential.ca_fp_prefix,
            lock.identity().clone(),
        );
        let mut session =
            JournalSession::start(credential.clone(), temporary.path().to_path_buf(), refresh)
                .await
                .expect("linked-device session");
        let mut scheduler = linked_device_scheduler_with_source(&temporary, "studio", &credential);

        let summary = scheduler
            .run_sweep(&mut session, linked_device_no_shutdown())
            .await;

        assert_eq!(summary.attempted, 1);
        assert_eq!(summary.custodied, 1, "{summary:?}");
        let requests = peer
            .requests()
            .into_iter()
            .filter(|req| !req.path_without_query().starts_with("/app/network/api/"))
            .collect::<Vec<_>>();
        assert_eq!(
            requests
                .iter()
                .map(|request| (
                    request.method().to_owned(),
                    request.path_without_query().to_owned()
                ))
                .collect::<Vec<_>>(),
            vec![("POST".to_owned(), INGEST_PATH.to_owned())],
            "the real mTLS peer must observe no registration or extra liveness request",
        );
        for request in &requests {
            assert_eq!(request.query_param("source"), Some("studio"));
        }
        assert_eq!(captured_upload_envelope(&requests[0])["source"], "studio");
        assert!(
            !candidate.exists(),
            "confirmed candidate segment was removed"
        );
        session
            .shutdown()
            .await
            .expect("shutdown linked-device session");
        peer.shutdown().await;
    });
}

#[test]
fn linked_device_403_and_426_retain_every_candidate_for_each_operation_class() {
    linked_device_runtime().block_on(async {
        for (status, reason_code) in [
            (403, "linked_device_required"),
            (426, "protocol_version_legacy"),
            (426, "protocol_version_future"),
        ] {
            for operation in ["upload"] {
                let peer = PrivateLinkPeer::start().await;
                let temporary = TestDirectory::new(&format!(
                    "linked-device-{status}-{reason_code}-{operation}"
                ));
                ensure_private_directory(temporary.path()).expect("private root");
                let lock = InstanceLock::acquire(temporary.path()).expect("acquire lock");
                let candidate = create_linked_device_candidate(&temporary);
                let credential = peer.credential();
                let refresh = solstone_tmux::journal_version::VersionRefreshState::new(
                    temporary.path().to_path_buf(),
                    temporary.path().to_path_buf(),
                    credential.instance_id.clone(),
                    &credential.ca_fp_prefix,
                    lock.identity().clone(),
                );
                let mut session = JournalSession::start(
                    credential.clone(),
                    temporary.path().to_path_buf(),
                    refresh,
                )
                .await
                .expect("linked-device session");
                enqueue_v3_rejection(&peer, operation, status, reason_code);
                let mut scheduler = linked_device_scheduler(&temporary, &credential);

                let summary = scheduler
                    .run_sweep(&mut session, linked_device_no_shutdown())
                    .await;

                assert_eq!(summary.attempted, 1, "{operation} {reason_code}");
                assert_eq!(summary.custodied, 0, "{operation} {reason_code}");
                assert_eq!(
                    summary.diagnostic,
                    Some(DiagnosticCode::JournalRejected),
                    "{operation} {reason_code}: {summary:?}",
                );
                let requests = peer
                    .requests()
                    .into_iter()
                    .filter(|req| !req.path_without_query().starts_with("/app/network/api/"))
                    .collect::<Vec<_>>();
                assert_eq!(
                    requests
                        .iter()
                        .map(|request| request.path_without_query().to_owned())
                        .collect::<Vec<_>>(),
                    v3_paths_through(operation),
                    "a refused {operation} must not populate reconciliation state or mark the day fresh",
                );
                for request in &requests {
                    assert_eq!(
                        request.query_param("source"),
                        Some(solstone_tmux::config::DEFAULT_SOURCE)
                    );
                }
                assert!(candidate.parent().expect("segment directory").is_dir());
                assert_eq!(
                    fs::read(&candidate).expect("retained candidate bytes"),
                    LINKED_DEVICE_BYTES,
                    "a refused {operation} must not clean up local data",
                );
                session.shutdown().await.expect("shutdown linked-device session");
                peer.shutdown().await;
            }
        }
    });
}

#[test]
fn peer_hashes_uploaded_parts_and_acks() {
    linked_device_runtime().block_on(async {
        let peer = PrivateLinkPeer::start().await;
        peer.answer_uploads_with_received_descriptors();
        let temporary = TestDirectory::new("peer-hashes-uploaded-parts");
        ensure_private_directory(temporary.path()).expect("private root");
        let lock = InstanceLock::acquire(temporary.path()).expect("acquire lock");
        let candidate = create_linked_device_candidate(&temporary);
        assert!(candidate.exists());
        let credential = peer.credential();
        let refresh = solstone_tmux::journal_version::VersionRefreshState::new(
            temporary.path().to_path_buf(),
            temporary.path().to_path_buf(),
            credential.instance_id.clone(),
            &credential.ca_fp_prefix,
            lock.identity().clone(),
        );
        let mut session =
            JournalSession::start(credential.clone(), temporary.path().to_path_buf(), refresh)
                .await
                .expect("linked-device session");
        let mut scheduler = linked_device_scheduler(&temporary, &credential);

        let summary = scheduler
            .run_sweep(&mut session, linked_device_no_shutdown())
            .await;

        assert_eq!(summary.attempted, 1);
        assert_eq!(summary.custodied, 1);
        assert!(!candidate.exists(), "confirmed candidate is removed");

        let requests = peer.requests();
        for req in requests {
            assert!(
                !req.path_without_query().contains("manifest"),
                "no manifest request"
            );
        }

        session.shutdown().await.expect("shutdown session");
        peer.shutdown().await;
    });
}

#[test]
fn segment_removed_deletes_segment_and_continues() {
    linked_device_runtime().block_on(async {
        let peer = PrivateLinkPeer::start().await;
        peer.answer_uploads_with_received_descriptors();
        let temporary = TestDirectory::new("segment-removed-continues");
        ensure_private_directory(temporary.path()).expect("private root");
        let lock = InstanceLock::acquire(temporary.path()).expect("acquire lock");

        let cand1 = create_linked_device_candidate(&temporary);
        let cand2 = temporary
            .path()
            .join("captures")
            .join(LINKED_DEVICE_DAY)
            .join(LINKED_DEVICE_STREAM)
            .join("120100_300")
            .join(LINKED_DEVICE_FILE);
        fs::create_dir_all(cand2.parent().unwrap()).unwrap();
        fs::write(&cand2, b"second candidate\n").unwrap();

        let credential = peer.credential();
        let refresh = solstone_tmux::journal_version::VersionRefreshState::new(
            temporary.path().to_path_buf(),
            temporary.path().to_path_buf(),
            credential.instance_id.clone(),
            &credential.ca_fp_prefix,
            lock.identity().clone(),
        );
        let mut session =
            JournalSession::start(credential.clone(), temporary.path().to_path_buf(), refresh)
                .await
                .expect("linked-device session");

        peer.enqueue_response(
            500,
            serde_json::to_vec(&serde_json::json!({
                "status": "failed",
                "error": "Ingest request failed",
                "reason_code": "segment_removed",
                "detail": "the owner removed this segment"
            }))
            .unwrap(),
        );

        let clock = Arc::new(test_clock());
        let scheduler = SyncScheduler::new(
            temporary.path().to_path_buf(),
            solstone_tmux::config::DEFAULT_SOURCE.to_owned(),
            Arc::clone(&clock) as Arc<dyn Clock>,
            SyncWake::default(),
            solstone_tmux::sync::JournalIdentity {
                instance_id: credential.instance_id.clone(),
                ca_fp_prefix_hex: solstone_tmux::journal_version::hex_encode(
                    &credential.ca_fp_prefix,
                ),
                pairing_generation_hex: solstone_tmux::journal_version::hex_encode(
                    &solstone_tmux::post_connect::compute_pairing_generation(
                        &credential.client_cert_pem,
                    ),
                ),
            },
        );

        let (activity, _rx) = watch::channel(SyncActivity::Idle);
        let health = HealthWriter::new(temporary.path().to_path_buf(), &lock);
        let (stop, shutdown) = watch::channel(false);
        let mut scheduler = scheduler.with_observability(activity, health);

        let task = tokio::spawn(async move {
            scheduler.run_with_shutdown(&mut session, shutdown).await;
            (scheduler, session)
        });

        let health_json = wait_for_idle_snapshot(temporary.path()).await;
        assert_eq!(health_json["state"], "connected");
        assert!(health_json["last_error_code"].is_null());
        assert_eq!(health_json["recent_error_count"], 0);
        assert_eq!(health_json["pending_segments"], 0);

        assert!(!cand1.exists(), "removed segment unlinked from disk");
        assert!(!cand2.exists(), "second segment unlinked from disk");
        assert!(!cand1.parent().unwrap().exists());
        assert!(!cand2.parent().unwrap().exists());

        stop.send_replace(true);
        let (_scheduler, mut session) = task.await.expect("join task");

        clock.set_wall(clock.wall_now() + time::Duration::hours(48));
        clock.set_monotonic(clock.monotonic_now() + Duration::from_secs(48 * 3600));

        let before_requests = peer.requests().len();

        let mut fresh_scheduler = SyncScheduler::new(
            temporary.path().to_path_buf(),
            solstone_tmux::config::DEFAULT_SOURCE.to_owned(),
            Arc::clone(&clock) as Arc<dyn Clock>,
            SyncWake::default(),
            solstone_tmux::sync::JournalIdentity {
                instance_id: credential.instance_id.clone(),
                ca_fp_prefix_hex: solstone_tmux::journal_version::hex_encode(
                    &credential.ca_fp_prefix,
                ),
                pairing_generation_hex: solstone_tmux::journal_version::hex_encode(
                    &solstone_tmux::post_connect::compute_pairing_generation(
                        &credential.client_cert_pem,
                    ),
                ),
            },
        );

        let summary = fresh_scheduler
            .run_sweep(&mut session, linked_device_no_shutdown())
            .await;
        assert_eq!(summary.attempted, 0);

        let all_requests = peer.requests();
        let sweep2_requests = &all_requests[before_requests..];
        for req in sweep2_requests {
            if req.method() == "POST" && req.path_without_query() == INGEST_PATH {
                let env = captured_upload_envelope(req);
                let seg = env["segment"].as_str().unwrap_or_default();
                assert_ne!(
                    seg, "120000_300",
                    "removed segment must not be uploaded again"
                );
                assert_ne!(
                    seg, "120100_300",
                    "acked segment must not be uploaded again"
                );
            }
        }

        session.shutdown().await.expect("shutdown session");
        peer.shutdown().await;
    });
}

#[test]
fn journal_write_failed_ends_the_sweep() {
    linked_device_runtime().block_on(async {
        let peer = PrivateLinkPeer::start().await;
        let temporary = TestDirectory::new("journal-write-failed");
        ensure_private_directory(temporary.path()).expect("private root");
        let lock = InstanceLock::acquire(temporary.path()).expect("acquire lock");
        let cand1 = create_linked_device_candidate(&temporary);
        let cand2 = temporary
            .path()
            .join("captures")
            .join(LINKED_DEVICE_DAY)
            .join(LINKED_DEVICE_STREAM)
            .join("120100_300")
            .join(LINKED_DEVICE_FILE);
        fs::create_dir_all(cand2.parent().unwrap()).unwrap();
        fs::write(&cand2, b"second\n").unwrap();

        let credential = peer.credential();
        let refresh = solstone_tmux::journal_version::VersionRefreshState::new(
            temporary.path().to_path_buf(),
            temporary.path().to_path_buf(),
            credential.instance_id.clone(),
            &credential.ca_fp_prefix,
            lock.identity().clone(),
        );
        let mut session =
            JournalSession::start(credential.clone(), temporary.path().to_path_buf(), refresh)
                .await
                .expect("linked-device session");

        peer.enqueue_response(
            500,
            serde_json::to_vec(&serde_json::json!({
                "status": "failed",
                "error": "Ingest request failed",
                "reason_code": "journal_write_failed",
                "detail": "journal write failed"
            }))
            .unwrap(),
        );

        let (activity, _rx) = watch::channel(SyncActivity::Idle);
        let health = HealthWriter::new(temporary.path().to_path_buf(), &lock);
        let (stop, shutdown) = watch::channel(false);
        let mut scheduler =
            linked_device_scheduler(&temporary, &credential).with_observability(activity, health);

        let task = tokio::spawn(async move {
            scheduler.run_with_shutdown(&mut session, shutdown).await;
            session.shutdown().await.expect("shutdown session");
        });

        let health_json = wait_for_idle_snapshot(temporary.path()).await;
        assert_eq!(health_json["state"], "offline");
        assert_eq!(
            fs::read(&cand1).expect("segment answered with a write failure"),
            LINKED_DEVICE_BYTES,
            "a journal write failure keeps the segment"
        );
        assert!(
            cand2.exists(),
            "the ended sweep keeps the segment it did not reach"
        );
        assert_eq!(
            peer.requests()
                .iter()
                .filter(|request| request.path_without_query() == INGEST_PATH)
                .count(),
            1,
            "the write failure ends the sweep before the next upload"
        );

        stop.send_replace(true);
        task.await.expect("join task");
        peer.shutdown().await;
    });
}

#[test]
fn ignored_retention_setting_still_removes_a_confirmed_segment_in_the_same_sweep() {
    linked_device_runtime().block_on(async {
        let fixture = BindingFixture::new("retention-setting-ignored");
        fixture.install_config(
            br#"{"stream":"host.tmux","capture_interval":5,"segment_interval":300,"cache_retention_days":-1,"status_indicator":false}"#,
        );
        let hostname = "host";
        let config =
            RuntimeConfig::load(&fixture.config_root, hostname).expect("load runtime settings");
        assert_eq!(config.cache_retention_days, -1);
        assert_eq!(config.stream.as_str(), LINKED_DEVICE_STREAM);

        let peer = PrivateLinkPeer::start().await;
        peer.answer_uploads_with_received_descriptors();
        persist_credential(&fixture.config_root, &peer.credential())
            .expect("persist paired credential");
        let candidate = fixture
            .data_root
            .join("captures")
            .join(LINKED_DEVICE_DAY)
            .join(LINKED_DEVICE_STREAM)
            .join(LINKED_DEVICE_SEGMENT)
            .join(LINKED_DEVICE_FILE);
        fs::create_dir_all(candidate.parent().expect("candidate parent"))
            .expect("candidate directory");
        fs::write(&candidate, LINKED_DEVICE_BYTES).expect("candidate bytes");

        let clock = Arc::new(test_clock());
        let lock = InstanceLock::acquire(&fixture.data_root).expect("instance lock");
        let (stop, shutdown) = watch::channel(false);
        let (activity, _activity_receiver) = watch::channel(SyncActivity::Idle);
        let sync = tokio::spawn(
            SyncTask {
                config_root: fixture.config_root.clone(),
                data_root: fixture.data_root.clone(),
                config,
                hostname: hostname.to_owned(),
                clock: Arc::clone(&clock) as Arc<dyn Clock>,
                wake: SyncWake::default(),
                activity,
                health: HealthWriter::new(fixture.data_root.clone(), &lock),
                retention_fence: Arc::new(solstone_tmux::sync::RetentionFence::new()),
                identity: lock.identity().clone(),
            }
            .run(shutdown),
        );

        // The first published sync ends the first sweep. With no wake and the
        // periodic interval a minute away, no second sweep runs before these
        // reads.
        let health = wait_for_successful_sync(&fixture.data_root).await;
        let removed_in_first_sweep = !candidate.exists();
        let requests = peer.requests();
        stop.send_replace(true);
        tokio::time::timeout(Duration::from_secs(5), sync)
            .await
            .expect("sync stops after shutdown")
            .expect("join sync")
            .expect("clean shutdown");
        drop(lock);

        assert!(
            removed_in_first_sweep,
            "the confirmed segment is removed in the sweep that uploaded it"
        );
        assert!(!candidate.parent().expect("segment directory").exists());
        assert!(
            !fixture
                .data_root
                .join("sync-ledger")
                .join(LINKED_DEVICE_DAY)
                .join(LINKED_DEVICE_STREAM)
                .join(LINKED_DEVICE_SEGMENT)
                .exists(),
            "the removed segment leaves no ledger entry"
        );
        assert_eq!(health["pending_segments"], 0);
        assert!(health["last_error_code"].is_null());
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.path_without_query() == INGEST_PATH)
                .count(),
            1,
            "the segment uploads once"
        );
        peer.shutdown().await;
    });
}

#[test]
fn content_conflict_follows_the_hourly_bound() {
    linked_device_runtime().block_on(async {
        let peer = PrivateLinkPeer::start().await;
        peer.answer_uploads_with_received_descriptors();
        let temporary = TestDirectory::new("content-conflict-bound");
        ensure_private_directory(temporary.path()).expect("private root");
        let lock = InstanceLock::acquire(temporary.path()).expect("acquire lock");
        let _cand1 = create_linked_device_candidate(&temporary);
        let cand2 = temporary
            .path()
            .join("captures")
            .join(LINKED_DEVICE_DAY)
            .join(LINKED_DEVICE_STREAM)
            .join("120100_300")
            .join(LINKED_DEVICE_FILE);
        fs::create_dir_all(cand2.parent().unwrap()).unwrap();
        fs::write(&cand2, b"second\n").unwrap();

        let credential = peer.credential();
        let refresh = solstone_tmux::journal_version::VersionRefreshState::new(
            temporary.path().to_path_buf(),
            temporary.path().to_path_buf(),
            credential.instance_id.clone(),
            &credential.ca_fp_prefix,
            lock.identity().clone(),
        );
        let mut session =
            JournalSession::start(credential.clone(), temporary.path().to_path_buf(), refresh)
                .await
                .expect("linked-device session");

        peer.enqueue_response(
            409,
            serde_json::to_vec(&serde_json::json!({
                "status": "failed",
                "error": "Ingest request failed",
                "reason_code": "content_conflict",
                "detail": "held bytes conflict"
            }))
            .unwrap(),
        );

        let clock = Arc::new(test_clock());
        let mut scheduler = SyncScheduler::new(
            temporary.path().to_path_buf(),
            solstone_tmux::config::DEFAULT_SOURCE.to_owned(),
            Arc::clone(&clock) as Arc<dyn Clock>,
            SyncWake::default(),
            solstone_tmux::sync::JournalIdentity {
                instance_id: credential.instance_id.clone(),
                ca_fp_prefix_hex: solstone_tmux::journal_version::hex_encode(
                    &credential.ca_fp_prefix,
                ),
                pairing_generation_hex: solstone_tmux::journal_version::hex_encode(
                    &solstone_tmux::post_connect::compute_pairing_generation(
                        &credential.client_cert_pem,
                    ),
                ),
            },
        );

        let summary = scheduler
            .run_sweep(&mut session, linked_device_no_shutdown())
            .await;

        assert_eq!(summary.attempted, 2);
        assert_eq!(summary.custodied, 1);
        assert_eq!(summary.failure, None);

        clock.set_wall(clock.wall_now() + time::Duration::minutes(50));
        clock.set_monotonic(clock.monotonic_now() + Duration::from_secs(50 * 60));
        let requests_before = peer.requests().len();
        let summary_early = scheduler
            .run_sweep(&mut session, linked_device_no_shutdown())
            .await;
        assert_eq!(summary_early.attempted, 0);
        assert_eq!(summary_early.custodied, 0);
        assert_eq!(
            peer.requests().len(),
            requests_before,
            "no upload before 1h"
        );

        clock.set_wall(clock.wall_now() + time::Duration::minutes(20));
        clock.set_monotonic(clock.monotonic_now() + Duration::from_secs(20 * 60));
        let summary_late = scheduler
            .run_sweep(&mut session, linked_device_no_shutdown())
            .await;
        assert_eq!(summary_late.attempted, 1);
        assert_eq!(summary_late.custodied, 1);

        session.shutdown().await.expect("shutdown session");
        peer.shutdown().await;
    });
}

#[test]
fn linked_device_required_ends_the_sweep_without_a_per_segment_bound() {
    linked_device_runtime().block_on(async {
        for (status, reason_code) in [
            (403, "linked_device_required"),
            (426, "protocol_version_legacy"),
            (426, "protocol_version_future"),
        ] {
            let peer = PrivateLinkPeer::start().await;
            let temporary = TestDirectory::new(&format!("dev-req-{status}-{reason_code}"));
            ensure_private_directory(temporary.path()).expect("private root");
            let lock = InstanceLock::acquire(temporary.path()).expect("acquire lock");
            let cand1 = create_linked_device_candidate(&temporary);
            let cand2 = temporary
                .path()
                .join("captures")
                .join(LINKED_DEVICE_DAY)
                .join(LINKED_DEVICE_STREAM)
                .join("120100_300")
                .join(LINKED_DEVICE_FILE);
            fs::create_dir_all(cand2.parent().unwrap()).unwrap();
            fs::write(&cand2, b"second\n").unwrap();

            let credential = peer.credential();
            let refresh = solstone_tmux::journal_version::VersionRefreshState::new(
                temporary.path().to_path_buf(),
                temporary.path().to_path_buf(),
                credential.instance_id.clone(),
                &credential.ca_fp_prefix,
                lock.identity().clone(),
            );
            let mut session =
                JournalSession::start(credential.clone(), temporary.path().to_path_buf(), refresh)
                    .await
                    .expect("linked-device session");

            peer.enqueue_response(
                status,
                serde_json::to_vec(&serde_json::json!({
                    "status": "failed",
                    "error": "linked device rejected",
                    "reason_code": reason_code,
                    "detail": "device refusal"
                }))
                .unwrap(),
            );

            let mut scheduler = linked_device_scheduler(&temporary, &credential);
            let summary = scheduler
                .run_sweep(&mut session, linked_device_no_shutdown())
                .await;

            assert_eq!(summary.attempted, 1, "{reason_code}");
            assert_eq!(summary.custodied, 0);
            assert_eq!(
                summary.failure,
                Some(solstone_tmux::sync::SyncFailureClass::Contract)
            );

            let ingest_posts = peer
                .requests()
                .into_iter()
                .filter(|req| req.path_without_query() == INGEST_PATH)
                .count();
            assert_eq!(ingest_posts, 1, "exactly one POST");
            assert!(cand1.exists());
            assert!(cand2.exists());

            let state1 = temporary
                .path()
                .join("sync-ledger")
                .join(LINKED_DEVICE_DAY)
                .join(LINKED_DEVICE_STREAM)
                .join(LINKED_DEVICE_SEGMENT)
                .join("state.json");
            let state2 = temporary
                .path()
                .join("sync-ledger")
                .join(LINKED_DEVICE_DAY)
                .join(LINKED_DEVICE_STREAM)
                .join("120100_300")
                .join("state.json");
            assert!(
                !state1.exists(),
                "no per-segment state.json for device refusal"
            );
            assert!(!state2.exists());

            session.shutdown().await.expect("shutdown session");
            peer.shutdown().await;
        }
    });
}

#[test]
fn non_json_server_error_ends_the_sweep() {
    linked_device_runtime().block_on(async {
        let peer = PrivateLinkPeer::start().await;
        let temporary = TestDirectory::new("non-json-server-error");
        ensure_private_directory(temporary.path()).expect("private root");
        let lock = InstanceLock::acquire(temporary.path()).expect("acquire lock");
        let _cand1 = create_linked_device_candidate(&temporary);
        let cand2 = temporary
            .path()
            .join("captures")
            .join(LINKED_DEVICE_DAY)
            .join(LINKED_DEVICE_STREAM)
            .join("120100_300")
            .join(LINKED_DEVICE_FILE);
        fs::create_dir_all(cand2.parent().unwrap()).unwrap();
        fs::write(&cand2, b"second\n").unwrap();

        let credential = peer.credential();
        let refresh = solstone_tmux::journal_version::VersionRefreshState::new(
            temporary.path().to_path_buf(),
            temporary.path().to_path_buf(),
            credential.instance_id.clone(),
            &credential.ca_fp_prefix,
            lock.identity().clone(),
        );
        let mut session =
            JournalSession::start(credential.clone(), temporary.path().to_path_buf(), refresh)
                .await
                .expect("linked-device session");

        peer.enqueue_response(500, b"not-json".to_vec());

        let (activity, _rx) = watch::channel(SyncActivity::Idle);
        let health = HealthWriter::new(temporary.path().to_path_buf(), &lock);
        let (stop, shutdown) = watch::channel(false);
        let mut scheduler =
            linked_device_scheduler(&temporary, &credential).with_observability(activity, health);

        let task = tokio::spawn(async move {
            scheduler.run_with_shutdown(&mut session, shutdown).await;
            session.shutdown().await.expect("shutdown session");
        });

        let health_json = wait_for_idle_snapshot(temporary.path()).await;
        assert_eq!(health_json["state"], "offline");

        stop.send_replace(true);
        task.await.expect("join task");
        peer.shutdown().await;
    });
}

async fn run_binding_failure(
    fixture: &BindingFixture,
    peer: &PrivateLinkPeer,
    hostname: &str,
    diagnostic: DiagnosticCode,
) -> FailureEvidence {
    let config =
        RuntimeConfig::load(&fixture.config_root, hostname).expect("load runtime settings");
    persist_credential(&fixture.config_root, &peer.credential())
        .expect("persist paired credential");
    let candidate = fixture.create_candidate(config.stream.as_str());
    let clock = Arc::new(test_clock());
    let lock = InstanceLock::acquire(&fixture.data_root).expect("instance lock");
    let stream_dir = stream_directory(
        &fixture.data_root,
        &config.stream,
        clock.wall_now(),
        clock.local_offset(),
    )
    .expect("active stream directory");
    let segment = SegmentState::create(
        &stream_dir,
        clock.wall_now(),
        Duration::ZERO,
        clock.local_offset(),
    )
    .expect("active segment");
    let polls = Arc::new(AtomicUsize::new(0));
    let (observer_stop, observer_stopped) = oneshot::channel();
    let (observer_barrier, supervisor_barrier) = shutdown_barrier();
    drop(supervisor_barrier);
    let observer = tokio::spawn(run_observer(
        Arc::new(CountingCapture(Arc::clone(&polls))),
        Box::new(SegmentManager::new(
            segment,
            fixture.data_root.clone(),
            config.stream.clone(),
            clock.local_offset(),
            SyncWake::default(),
        )),
        Arc::clone(&clock) as Arc<dyn Clock>,
        Box::pin(async move {
            let _ = observer_stopped.await;
            ShutdownEvent::Injected
        }),
        observer_barrier,
        ObserverConfig {
            capture_interval: Duration::from_millis(10),
            segment_interval: Duration::from_secs(300),
        },
    ));

    let (sync_stop, sync_shutdown) = tokio::sync::watch::channel(false);
    let (activity, _activity_receiver) = tokio::sync::watch::channel(SyncActivity::Idle);
    let sync = tokio::spawn(
        SyncTask {
            config_root: fixture.config_root.clone(),
            data_root: fixture.data_root.clone(),
            config,
            hostname: hostname.to_owned(),
            clock: Arc::clone(&clock) as Arc<dyn Clock>,
            wake: SyncWake::default(),
            activity,
            health: HealthWriter::new(fixture.data_root.clone(), &lock),
            retention_fence: Arc::new(solstone_tmux::sync::RetentionFence::new()),
            identity: lock.identity().clone(),
        }
        .run(sync_shutdown),
    );

    let health = wait_for_diagnostic(&fixture.data_root, diagnostic).await;
    wait_until("multiple capture polls", || {
        polls.load(Ordering::SeqCst) >= 2
    })
    .await;
    let _ = observer_stop.send(());
    sync_stop.send_replace(true);
    let observer_exit = observer.await.expect("join observer");
    assert_eq!(observer_exit.exit_code, 0);
    sync.await
        .expect("join sync task")
        .expect("sync task shutdown");
    drop(lock);

    FailureEvidence {
        candidate,
        polls: polls.load(Ordering::SeqCst),
        local_files: jsonl_files(&fixture.data_root.join("captures")),
        health,
    }
}

#[test]
fn unavailable_bridge_at_start_stays_supervised_with_bounded_retry() {
    linked_device_runtime().block_on(async {
        let fixture = BindingFixture::new("linked-device-startup-retry");
        let hostname = "offline-host";
        fs::write(
            fixture.config_root.join(CONFIG_FILENAME),
            br#"{"stream":"offline-host.tmux","capture_interval":5,"segment_interval":300,"status_indicator":false}"#,
        )
        .expect("write native config");
        let config =
            RuntimeConfig::load(&fixture.config_root, hostname).expect("load runtime settings");
        let peer = PrivateLinkPeer::start().await;
        let mut credential = peer.credential();
        credential.client_key_pem.clear();
        persist_credential(&fixture.config_root, &credential).expect("persist paired credential");
        peer.shutdown().await;

        let clock = Arc::new(test_clock());
        let lock = InstanceLock::acquire(&fixture.data_root).expect("instance lock");
        let (stop, shutdown) = watch::channel(false);
        let (activity, _activity_receiver) = watch::channel(SyncActivity::Idle);
        let sync = tokio::spawn(
            SyncTask {
                config_root: fixture.config_root.clone(),
                data_root: fixture.data_root.clone(),
                config,
                hostname: hostname.to_owned(),
                clock: Arc::clone(&clock) as Arc<dyn Clock>,
                wake: SyncWake::default(),
                activity,
                health: HealthWriter::new(fixture.data_root.clone(), &lock),
                retention_fence: Arc::new(solstone_tmux::sync::RetentionFence::new()),
                identity: lock.identity().clone(),
            }
            .run(shutdown),
        );

        let health = wait_for_diagnostic(&fixture.data_root, DiagnosticCode::BridgeUnavailable).await;
        assert_eq!(health["state"], "offline");
        assert_eq!(health["paired"], true);
        assert_eq!(health["recent_error_count"], 1);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            !sync.is_finished(),
            "a transient bootstrap outage must not terminate the supervisor"
        );
        let latest: Value = serde_json::from_slice(
            &fs::read(fixture.data_root.join(HEALTH_FILENAME)).expect("read retry health"),
        )
        .expect("parse retry health");
        assert_eq!(latest["recent_error_count"], 1, "retry must honor backoff");

        stop.send_replace(true);
        tokio::time::timeout(Duration::from_secs(5), sync)
            .await
            .expect("sync stops after shutdown")
            .expect("join sync")
            .expect("clean shutdown");
    });
}

struct FailureEvidence {
    candidate: PathBuf,
    polls: usize,
    local_files: Vec<PathBuf>,
    health: Value,
}

impl FailureEvidence {
    fn assert_capture_continued(&self) {
        assert!(self.polls >= 2, "capture polls: {}", self.polls);
        assert!(
            self.local_files.len() >= 2,
            "durable files after capture: {:?}",
            self.local_files
        );
    }
}

struct BindingFixture {
    _temporary: TestDirectory,
    data_root: PathBuf,
    config_root: PathBuf,
}

impl BindingFixture {
    fn new(label: &str) -> Self {
        let temporary = TestDirectory::new(label);
        let data_root = temporary.path().join("data");
        let config_root = temporary.path().join("config");
        ensure_private_directory(&data_root).expect("data root");
        ensure_private_directory(&config_root).expect("config root");
        Self {
            _temporary: temporary,
            data_root,
            config_root,
        }
    }

    fn install_config(&self, bytes: &[u8]) {
        let path = self.config_root.join(CONFIG_FILENAME);
        fs::write(&path, bytes).expect("native settings");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("settings mode");
    }

    fn create_candidate(&self, stream: &str) -> PathBuf {
        let path = self
            .data_root
            .join("captures/20260729")
            .join(stream)
            .join("110000_300")
            .join("tmux_existing_screen.jsonl");
        fs::create_dir_all(path.parent().expect("candidate parent")).expect("candidate directory");
        fs::write(&path, CANDIDATE_BYTES).expect("candidate bytes");
        path
    }
}

struct CountingCapture(Arc<AtomicUsize>);

impl CaptureProvider for CountingCapture {
    fn poll<'a>(
        &'a self,
        _wall_unix_seconds: i64,
        _capture_interval: Duration,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<CaptureResult>, ObserverOperationError>> + Send + 'a>>
    {
        Box::pin(async move {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(vec![golden_capture("main")])
        })
    }
}

fn create_linked_device_candidate(temporary: &TestDirectory) -> PathBuf {
    let path = temporary
        .path()
        .join("captures")
        .join(LINKED_DEVICE_DAY)
        .join(LINKED_DEVICE_STREAM)
        .join(LINKED_DEVICE_SEGMENT)
        .join(LINKED_DEVICE_FILE);
    fs::create_dir_all(path.parent().expect("candidate parent")).expect("candidate directory");
    fs::write(&path, LINKED_DEVICE_BYTES).expect("candidate bytes");
    path
}

fn linked_device_scheduler(
    temporary: &TestDirectory,
    credential: &spl_transport::credential::Credential,
) -> SyncScheduler {
    linked_device_scheduler_with_source(
        temporary,
        solstone_tmux::config::DEFAULT_SOURCE,
        credential,
    )
}

fn linked_device_scheduler_with_source(
    temporary: &TestDirectory,
    source: &str,
    credential: &spl_transport::credential::Credential,
) -> SyncScheduler {
    let identity = solstone_tmux::sync::JournalIdentity {
        instance_id: credential.instance_id.clone(),
        ca_fp_prefix_hex: solstone_tmux::journal_version::hex_encode(&credential.ca_fp_prefix),
        pairing_generation_hex: solstone_tmux::journal_version::hex_encode(
            &solstone_tmux::post_connect::compute_pairing_generation(&credential.client_cert_pem),
        ),
    };
    SyncScheduler::new(
        temporary.path().to_path_buf(),
        source.to_owned(),
        Arc::new(test_clock()),
        SyncWake::default(),
        identity,
    )
}

fn captured_upload_envelope(request: &support::private_link_peer::PeerRequest) -> Value {
    let body = request.body();
    let headers_end = body
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("upload envelope header separator");
    let rest = &body[headers_end + 4..];
    let json_end = rest
        .windows(4)
        .position(|window| window == b"\r\n--")
        .expect("upload envelope closing boundary");
    serde_json::from_slice(&rest[..json_end]).expect("upload envelope JSON")
}

fn linked_device_no_shutdown() -> watch::Receiver<bool> {
    let (sender, receiver) = watch::channel(false);
    std::mem::forget(sender);
    receiver
}

fn enqueue_v3_rejection(peer: &PrivateLinkPeer, operation: &str, status: u16, reason_code: &str) {
    match operation {
        "upload" => peer.enqueue_response(status, rejection_response(reason_code)),
        _ => panic!("unknown v3 operation: {operation}"),
    }
}

fn rejection_response(reason_code: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "error": "linked device rejected",
        "reason_code": reason_code,
        "detail": "linked-device identity or protocol version is not accepted"
    }))
    .expect("rejection response")
}

fn v3_paths_through(operation: &str) -> Vec<String> {
    match operation {
        "upload" => vec![INGEST_PATH.to_owned()],
        _ => panic!("unknown v3 operation: {operation}"),
    }
}

fn assert_legacy_header_is_absent(request: &support::private_link_peer::PeerRequest, name: &str) {
    assert!(request.header(name).is_none(), "unexpected {name} header");
    assert!(
        request.header(&name.to_ascii_uppercase()).is_none(),
        "unexpected {name} header in another casing",
    );
}

fn linked_device_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .expect("runtime")
}

fn test_clock() -> TestClock {
    let date = Date::from_calendar_date(2026, Month::July, 29).expect("test date");
    let time = Time::from_hms(12, 0, 0).expect("test time");
    TestClock::new(
        PrimitiveDateTime::new(date, time).assume_utc(),
        Duration::ZERO,
        UtcOffset::UTC,
    )
}

async fn wait_for_diagnostic(data_root: &Path, diagnostic: DiagnosticCode) -> Value {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(bytes) = fs::read(data_root.join(HEALTH_FILENAME))
                && let Ok(snapshot) = serde_json::from_slice::<Value>(&bytes)
                && snapshot["last_error_code"] == diagnostic.as_str()
            {
                return snapshot;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("diagnostic health timeout")
}

async fn wait_for_idle_snapshot(data_root: &Path) -> Value {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(bytes) = fs::read(data_root.join(HEALTH_FILENAME))
                && let Ok(snapshot) = serde_json::from_slice::<Value>(&bytes)
                && snapshot["sync_in_progress"] == false
            {
                return snapshot;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("idle health timeout")
}

async fn wait_for_successful_sync(data_root: &Path) -> Value {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(bytes) = fs::read(data_root.join(HEALTH_FILENAME))
                && let Ok(snapshot) = serde_json::from_slice::<Value>(&bytes)
                && snapshot["sync_in_progress"] == false
                && snapshot["last_successful_sync_unix_seconds"].is_number()
            {
                return snapshot;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("successful sync health timeout")
}

async fn wait_until(context: &str, predicate: impl Fn() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !predicate() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{context} timed out"));
}

fn jsonl_files(root: &Path) -> Vec<PathBuf> {
    fn visit(path: &Path, files: &mut Vec<PathBuf>) {
        let Ok(entries) = fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                visit(&path, files);
            } else if path.extension().and_then(|value| value.to_str()) == Some("jsonl") {
                files.push(path);
            }
        }
    }

    let mut files = Vec::new();
    visit(root, &mut files);
    files.sort();
    files
}
