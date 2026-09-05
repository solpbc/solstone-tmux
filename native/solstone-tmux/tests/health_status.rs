// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

mod support;

use std::fs;
use std::io::Write;
use std::os::unix::fs::symlink;
use std::process::{Command, Stdio};

use serde_json::Value;
use solstone_tmux::health::{
    DiagnosticCode, HEALTH_FILENAME, HealthState, HealthWriter, StatusHealth, SyncFacts,
    read_status_health,
};
use solstone_tmux::instance_lock::{InstanceLock, LOCK_FILENAME};
use solstone_tmux::journal_version::{JournalVersionStatus, read_journal_version};
use solstone_tmux::paths::ensure_private_directory;
use solstone_tmux::private_link::persist_credential;
use solstone_tmux::sync::{JournalSession, SyncFailureClass, SyncOperationError};
use support::private_link_peer::PrivateLinkPeer;
use support::{IsolatedRoots, TestDirectory};

const NOW: i64 = 1_800_000_000;
const PAIR_LINK_SENTINEL: &str = "SENTINEL_PAIR_LINK";
const RELAY_TOKEN_SENTINEL: &str = "SENTINEL_RELAY_TOKEN";
const RESPONSE_BODY_SENTINEL: &str = "SENTINEL_RESPONSE_BODY";
const CAPTURE_PATH_SENTINEL: &str = "SENTINEL_CAPTURE_PATH";

#[test]
fn configured_without_contact_is_offline_and_empty_contact_is_connected() {
    runtime().block_on(async {
        let temporary = TestDirectory::new("health-contact");
        let data_root = temporary.path().join("data");
        ensure_private_directory(&data_root).expect("data root");
        let lock = InstanceLock::acquire(&data_root).expect("instance lock");
        let writer = HealthWriter::new(data_root.clone(), &lock);
        let mut facts = SyncFacts {
            paired: true,
            ..SyncFacts::default()
        };

        writer.write(&facts, NOW).await.expect("offline snapshot");
        assert_eq!(
            read_status_health(&data_root, NOW),
            StatusHealth::Live(HealthState::Offline)
        );

        facts.successful_contact(NOW);
        assert_eq!(facts.last_successful_contact_unix_seconds, Some(NOW));
        assert_eq!(facts.last_successful_sync_unix_seconds, None);
        writer.write(&facts, NOW).await.expect("connected snapshot");
        assert_eq!(
            read_status_health(&data_root, NOW),
            StatusHealth::Live(HealthState::Connected)
        );

        facts.successful_sync(NOW + 1);
        assert_eq!(facts.last_successful_sync_unix_seconds, Some(NOW + 1));
    });
}

#[test]
fn stale_and_prior_run_snapshots_never_read_connected() {
    runtime().block_on(async {
        let temporary = TestDirectory::new("health-stale");
        let data_root = temporary.path().join("data");
        ensure_private_directory(&data_root).expect("data root");
        let first_lock = InstanceLock::acquire(&data_root).expect("first lock");
        let writer = HealthWriter::new(data_root.clone(), &first_lock);
        let mut facts = SyncFacts {
            paired: true,
            ..SyncFacts::default()
        };
        facts.successful_contact(NOW);
        writer.write(&facts, NOW).await.expect("health snapshot");

        assert_eq!(
            read_status_health(&data_root, NOW + 181),
            StatusHealth::Stale
        );
        drop(writer);
        drop(first_lock);
        assert_eq!(read_status_health(&data_root, NOW), StatusHealth::Stale);
    });
}

#[test]
fn retry_work_with_an_existing_error_is_a_valid_syncing_snapshot() {
    runtime().block_on(async {
        let temporary = TestDirectory::new("health-retry-work");
        let data_root = temporary.path().join("data");
        ensure_private_directory(&data_root).expect("data root");
        let lock = InstanceLock::acquire(&data_root).expect("instance lock");
        let writer = HealthWriter::new(data_root.clone(), &lock);
        let mut facts = SyncFacts {
            paired: true,
            sync_in_progress: true,
            ..SyncFacts::default()
        };
        facts.failed(DiagnosticCode::PrivateStateIo);

        writer.write(&facts, NOW).await.expect("syncing snapshot");

        assert_eq!(
            read_status_health(&data_root, NOW),
            StatusHealth::Live(HealthState::Syncing)
        );
    });
}

#[test]
fn status_rejects_a_symlinked_data_root() {
    runtime().block_on(async {
        let temporary = TestDirectory::new("health-symlink-root");
        let data_root = temporary.path().join("real-data");
        let alias = temporary.path().join("alias-data");
        ensure_private_directory(&data_root).expect("data root");
        let lock = InstanceLock::acquire(&data_root).expect("instance lock");
        let writer = HealthWriter::new(data_root.clone(), &lock);
        let mut facts = SyncFacts {
            paired: true,
            ..SyncFacts::default()
        };
        facts.successful_contact(NOW);
        writer.write(&facts, NOW).await.expect("health snapshot");
        symlink(&data_root, &alias).expect("data root alias");

        assert_eq!(read_status_health(&alias, NOW), StatusHealth::Unknown);
        assert_eq!(
            read_status_health(&data_root, NOW),
            StatusHealth::Live(HealthState::Connected)
        );
    });
}

#[test]
fn production_failure_paths_redact_secrets_and_owner_content() {
    let setup_temporary = TestDirectory::new("health-redaction-setup");
    let setup_roots = IsolatedRoots::new(setup_temporary.path());
    let mut command = Command::new(env!("CARGO_BIN_EXE_solstone-tmux"));
    command
        .arg("setup")
        .env_clear()
        .envs(setup_roots.entries().iter().cloned())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut setup = command.spawn().expect("spawn failing setup");
    setup
        .stdin
        .take()
        .expect("setup stdin")
        .write_all(format!("{PAIR_LINK_SENTINEL}\n").as_bytes())
        .expect("write failing pair link");
    let setup_output = setup.wait_with_output().expect("wait for failing setup");
    assert_eq!(setup_output.status.code(), Some(1));
    let setup_stderr = String::from_utf8(setup_output.stderr).expect("setup stderr");

    runtime().block_on(async {
        let temporary = TestDirectory::new("health-redaction-network");
        let config_root = temporary.path().join("config");
        let data_root = temporary.path().join("data");
        ensure_private_directory(&config_root).expect("config root");
        ensure_private_directory(&data_root).expect("data root");
        let lock = InstanceLock::acquire(&data_root).expect("instance lock");
        let writer = HealthWriter::new(data_root.clone(), &lock);

        let peer = PrivateLinkPeer::start().await;
        let mut credential = peer.credential();
        credential.device_token = Some(RELAY_TOKEN_SENTINEL.to_owned());
        credential.device_token_expires_at = Some(NOW + 300);
        let owner = JournalSession::start(
            credential,
            config_root,
            data_root.clone(),
            lock.identity().clone(),
        )
        .await
        .expect("start redaction session");

        peer.enqueue_response(200, RESPONSE_BODY_SENTINEL.as_bytes());
        let response_error = match owner
            .journal()
            .ingest_segments("20260728", solstone_tmux::config::DEFAULT_SOURCE)
            .await
        {
            Err(error) => error,
            Ok(_) => panic!("malformed response was accepted"),
        };
        let requests = peer.requests();
        let authenticated = requests.last().expect("linked-device request");
        assert!(authenticated.header("x-solstone-observer").is_none());
        assert!(authenticated.header("authorization").is_none());

        let capture_path = temporary.path().join(CAPTURE_PATH_SENTINEL);
        let path_error = match owner
            .journal()
            .ingest_upload(
                "20260728",
                "120000_300",
                vec![capture_path],
                solstone_tmux::config::DEFAULT_SOURCE,
            )
            .await
        {
            Err(error) => error,
            Ok(_) => panic!("invalid local segment was accepted"),
        };

        let mut facts = SyncFacts {
            paired: true,
            ..SyncFacts::default()
        };
        facts.failed(response_error.diagnostic());
        facts.failed(path_error.diagnostic());
        writer.write(&facts, NOW).await.expect("health snapshot");
        let snapshot_bytes = fs::read(data_root.join(HEALTH_FILENAME)).expect("snapshot bytes");
        let snapshot: Value = serde_json::from_slice(&snapshot_bytes).expect("snapshot JSON");
        let keys = snapshot
            .as_object()
            .expect("snapshot object")
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(
            keys.len(),
            [
                "schema_version",
                "run_id",
                "lock_inode",
                "updated_at_unix_seconds",
                "state",
                "paired",
                "sync_in_progress",
                "pending_segments",
                "last_successful_contact_unix_seconds",
                "last_successful_sync_unix_seconds",
                "recent_error_count",
                "last_error_code",
            ]
            .len()
        );
        let sync_failure = SyncOperationError::EndSweepDiagnostic(
            SyncFailureClass::Contract,
            response_error.diagnostic(),
        );
        let process_failure = format!("{response_error}; {path_error}; {sync_failure}");
        for sentinel in [
            PAIR_LINK_SENTINEL,
            "SENTINEL_OBSERVER_KEY",
            "SENTINEL_BEARER_TOKEN",
            RELAY_TOKEN_SENTINEL,
            RESPONSE_BODY_SENTINEL,
            CAPTURE_PATH_SENTINEL,
        ] {
            assert!(
                !setup_stderr.contains(sentinel),
                "setup stderr disclosed a sensitive sentinel"
            );
            assert!(
                !process_failure.contains(sentinel),
                "process failure text disclosed a sensitive sentinel"
            );
            assert!(
                !snapshot_bytes
                    .windows(sentinel.len())
                    .any(|window| window == sentinel.as_bytes()),
                "health snapshot disclosed a sensitive sentinel"
            );
        }
        owner.shutdown().await.expect("shutdown journal session");
        peer.shutdown().await;
    });
}

#[test]
fn health_write_failures_use_the_health_diagnostic_and_setup_guidance_is_actionable() {
    runtime().block_on(async {
        let temporary = TestDirectory::new("health-write-failure");
        let data_root = temporary.path().join("data");
        ensure_private_directory(&data_root).expect("data root");
        let lock = InstanceLock::acquire(&data_root).expect("instance lock");
        let writer = HealthWriter::new(data_root.clone(), &lock);
        fs::create_dir(data_root.join(HEALTH_FILENAME)).expect("invalid health target");

        assert_eq!(
            writer
                .write(&SyncFacts::default(), NOW)
                .await
                .expect_err("invalid health target was accepted"),
            DiagnosticCode::HealthSnapshotIo
        );
        assert!(
            DiagnosticCode::SetupUnavailable
                .message()
                .contains("stop the observer")
        );
        assert!(
            DiagnosticCode::PairingFailed
                .message()
                .contains("verify the link")
        );
        assert_eq!(
            DiagnosticCode::JournalResponseTooLarge.as_str(),
            "journal_response_too_large"
        );
        assert_eq!(
            DiagnosticCode::SyncTaskTimedOut.as_str(),
            "sync_task_timed_out"
        );
    });
}

#[test]
fn status_is_read_only_when_native_state_is_absent() {
    let temporary = TestDirectory::new("status-read-only");
    let roots = IsolatedRoots::new(temporary.path());
    let output = Command::new(env!("CARGO_BIN_EXE_solstone-tmux"))
        .arg("status")
        .env_clear()
        .envs(roots.entries().iter().cloned())
        .output()
        .expect("run status");

    assert_eq!(output.status.code(), Some(4));
    assert_eq!(
        String::from_utf8(output.stdout).expect("status stdout"),
        "service: absent\nsync-health: unknown\njournal-version: unknown\n"
    );
    assert!(!roots.data_root().exists());
    assert!(!roots.data_root().join(LOCK_FILENAME).exists());
    assert!(!roots.data_root().join(HEALTH_FILENAME).exists());
}

#[test]
fn offline_and_stale_journal_version_snapshot_reads_last_known() {
    runtime().block_on(async {
        let temporary = TestDirectory::new("journal-version-stale");
        let config_root = temporary.path().join("config");
        let data_root = temporary.path().join("data");
        ensure_private_directory(&config_root).expect("config root");
        ensure_private_directory(&data_root).expect("data root");

        let lock = InstanceLock::acquire(&data_root).expect("instance lock");
        let writer = HealthWriter::new(data_root.clone(), &lock);
        let mut facts = SyncFacts {
            paired: true,
            ..SyncFacts::default()
        };
        facts.successful_contact(NOW);
        writer.write(&facts, NOW).await.expect("write health");

        let peer = PrivateLinkPeer::start().await;
        persist_credential(&config_root, &peer.credential()).expect("persist credential");
        peer.enqueue_system_status_response(
            200,
            br#"{"ok":true,"version":{"current":"2026.8.0"}}"#.to_vec(),
        );
        let session = JournalSession::start(
            peer.credential(),
            config_root.clone(),
            data_root.clone(),
            lock.identity().clone(),
        )
        .await
        .expect("start session");

        for _ in 0..50 {
            if read_journal_version(&config_root, &data_root)
                == JournalVersionStatus::Current("2026.8.0".to_owned())
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        assert_eq!(
            read_journal_version(&config_root, &data_root),
            JournalVersionStatus::Current("2026.8.0".to_owned())
        );

        session.shutdown().await.expect("shutdown session");
        peer.shutdown().await;

        drop(lock);
        assert_eq!(
            read_journal_version(&config_root, &data_root),
            JournalVersionStatus::LastKnown("2026.8.0".to_owned())
        );
    });
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .expect("test runtime")
}
