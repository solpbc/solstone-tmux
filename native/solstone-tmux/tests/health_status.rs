// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

mod support;

use std::fs;
use std::io::Write;
use std::os::unix::fs::symlink;
use std::process::{Command, Stdio};

use serde_json::Value;
use solstone_tmux::health::read_status_report;
use solstone_tmux::health::{
    CONNECTED_LEASE_SECONDS, DiagnosticCode, HEALTH_FILENAME, HealthState, HealthWriter,
    OFFLINE_AFTER_SECONDS, StatusHealth, SyncFacts, read_status_health,
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
fn a_retry_sweep_after_a_drop_keeps_reading_the_drop() {
    runtime().block_on(async {
        let temporary = TestDirectory::new("health-retry-work");
        let data_root = temporary.path().join("data");
        ensure_private_directory(&data_root).expect("data root");
        let lock = InstanceLock::acquire(&data_root).expect("instance lock");
        let writer = HealthWriter::new(data_root.clone(), &lock);
        let mut facts = SyncFacts {
            paired: true,
            ..SyncFacts::default()
        };
        facts.successful_contact(NOW - 120);
        facts.failed(DiagnosticCode::JournalUnavailable);

        for sending in [false, true, false] {
            facts.sync_in_progress = sending;
            writer.write(&facts, NOW).await.expect("retry snapshot");
            assert_eq!(
                read_status_health(&data_root, NOW),
                StatusHealth::Live(HealthState::Offline),
                "sending: {sending}"
            );
        }
    });
}

fn heard_at(contact: i64) -> SyncFacts {
    let mut facts = SyncFacts {
        paired: true,
        episode_started_unix_seconds: Some(contact - 600),
        ..SyncFacts::default()
    };
    facts.successful_contact(contact);
    facts
}

// I1
#[test]
fn a_failed_request_that_is_not_a_timeout_shows_at_once() {
    let mut facts = heard_at(NOW);
    assert_eq!(facts.state_at(NOW), HealthState::Connected);
    facts.failed(DiagnosticCode::JournalUnavailable);
    assert_eq!(facts.state_at(NOW), HealthState::Connecting);
}

// I2
#[test]
fn connected_never_outlives_the_lease() {
    let facts = heard_at(NOW);
    for age in 0..=2 * CONNECTED_LEASE_SECONDS {
        assert_eq!(
            facts.state_at(NOW + age) == HealthState::Connected,
            age <= CONNECTED_LEASE_SECONDS,
            "age {age}"
        );
    }
}

// I2, for a snapshot read later than it was written.
#[test]
fn a_connected_snapshot_reads_offline_once_its_lease_runs_out() {
    runtime().block_on(async {
        let temporary = TestDirectory::new("health-lease");
        let data_root = temporary.path().join("data");
        ensure_private_directory(&data_root).expect("data root");
        let lock = InstanceLock::acquire(&data_root).expect("instance lock");
        let writer = HealthWriter::new(data_root.clone(), &lock);
        writer.write(&heard_at(NOW), NOW).await.expect("snapshot");
        assert_eq!(
            read_status_health(&data_root, NOW + CONNECTED_LEASE_SECONDS),
            StatusHealth::Live(HealthState::Connected)
        );
        assert_eq!(
            read_status_health(&data_root, NOW + CONNECTED_LEASE_SECONDS + 1),
            StatusHealth::Live(HealthState::Offline)
        );
    });
}

// I4
#[test]
fn one_timed_out_check_inside_the_lease_changes_nothing() {
    let mut facts = heard_at(NOW);
    facts.failed(DiagnosticCode::JournalTimeout);
    for age in 0..=CONNECTED_LEASE_SECONDS {
        assert_eq!(
            facts.state_at(NOW + age),
            HealthState::Connected,
            "age {age}"
        );
    }
    facts.sync_in_progress = true;
    assert_eq!(facts.state_at(NOW + 1), HealthState::Syncing);
}

// I6
#[test]
fn a_fresh_start_never_reads_connected_before_an_answer() {
    let mut facts = SyncFacts {
        paired: true,
        episode_started_unix_seconds: Some(NOW),
        ..SyncFacts::default()
    };
    for age in 0..OFFLINE_AFTER_SECONDS {
        assert_eq!(
            facts.state_at(NOW + age),
            HealthState::Connecting,
            "age {age}"
        );
    }
    assert_eq!(
        facts.state_at(NOW + OFFLINE_AFTER_SECONDS),
        HealthState::Offline
    );
    facts.sync_in_progress = true;
    assert_eq!(
        facts.state_at(NOW + OFFLINE_AFTER_SECONDS),
        HealthState::Offline
    );
    facts.successful_contact(NOW + 40);
    facts.sync_in_progress = false;
    assert_eq!(facts.state_at(NOW + 40), HealthState::Connected);
}

#[test]
fn offline_waits_for_thirty_seconds_unreached_and_a_wake_restarts_it() {
    let mut facts = heard_at(NOW);
    facts.failed(DiagnosticCode::JournalUnavailable);
    assert_eq!(
        facts.state_at(NOW + OFFLINE_AFTER_SECONDS - 1),
        HealthState::Connecting
    );
    assert_eq!(
        facts.state_at(NOW + OFFLINE_AFTER_SECONDS),
        HealthState::Offline
    );
    facts.episode_started_unix_seconds = Some(NOW + 3600);
    assert_eq!(facts.state_at(NOW + 3600), HealthState::Connecting);
}

#[test]
fn a_connecting_snapshot_reads_offline_once_it_has_lasted() {
    runtime().block_on(async {
        let temporary = TestDirectory::new("health-connecting-ages");
        let data_root = temporary.path().join("data");
        ensure_private_directory(&data_root).expect("data root");
        let lock = InstanceLock::acquire(&data_root).expect("instance lock");
        let writer = HealthWriter::new(data_root.clone(), &lock);
        let facts = SyncFacts {
            paired: true,
            episode_started_unix_seconds: Some(NOW),
            ..SyncFacts::default()
        };
        writer.write(&facts, NOW).await.expect("snapshot");
        assert_eq!(
            read_status_health(&data_root, NOW + OFFLINE_AFTER_SECONDS - 1),
            StatusHealth::Live(HealthState::Connecting)
        );
        assert_eq!(
            read_status_health(&data_root, NOW + OFFLINE_AFTER_SECONDS),
            StatusHealth::Live(HealthState::Offline)
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
        .arg("--mark")
        .arg("test words")
        .env_clear()
        .envs(setup_roots.entries().iter().cloned())
        .env("SOLSTONE_TMUX_TERMINAL", "-")
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
    assert!(setup_stderr.contains(solstone_tmux::health::DiagnosticCode::PairingFailed.message()));
    assert!(!setup_stderr.contains(PAIR_LINK_SENTINEL));

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
        let refresh = solstone_tmux::journal_version::VersionRefreshState::new(
            config_root.clone(),
            data_root.clone(),
            credential.instance_id.clone(),
            &credential.ca_fp_prefix,
            lock.identity().clone(),
        );
        let owner = JournalSession::start(credential, config_root, refresh)
            .await
            .expect("start redaction session");

        peer.enqueue_system_status_response(200, RESPONSE_BODY_SENTINEL.as_bytes());
        let response_error = match owner.journal().system_status().await {
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
                None,
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
                "app_version",
                "held_segments",
                "held_newest",
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
                .contains("stop solstone-tmux")
        );
        assert!(
            DiagnosticCode::PairingFailed
                .message()
                .contains("check the pairing link")
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
    let stdout = String::from_utf8(output.stdout).expect("status stdout");
    let lines = stdout.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 8);
    assert_eq!(lines[0], "service: absent");
    assert_eq!(lines[1], "sync-health: unknown");
    assert_eq!(
        lines[2],
        solstone_tmux::pairing_answer::JOURNAL_ROW_NOT_PAIRED
    );
    assert_eq!(lines[3], "journal-version: unknown");
    assert!(lines[4].starts_with("tmux app "));
    assert_eq!(lines[5], "journal unknown");
    assert_eq!(lines[6], "get help: https://support.solstone.app");
    assert!(lines[7].starts_with(
        "report a problem: https://support.solstone.app/#report=v1&app=solstone+for+tmux"
    ));
    assert!(lines[7].contains("&state=unknown&about=tmux+app+"));
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
        let credential = peer.credential();
        let refresh = solstone_tmux::journal_version::VersionRefreshState::new(
            config_root.clone(),
            data_root.clone(),
            credential.instance_id.clone(),
            &credential.ca_fp_prefix,
            lock.identity().clone(),
        );
        let session = JournalSession::start(credential, config_root.clone(), refresh)
            .await
            .expect("start session");

        for _ in 0..50 {
            if read_journal_version(&config_root, &data_root, NOW)
                == JournalVersionStatus::Current("2026.8.0".to_owned())
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        assert_eq!(
            read_journal_version(&config_root, &data_root, NOW),
            JournalVersionStatus::Current("2026.8.0".to_owned())
        );

        session.shutdown().await.expect("shutdown session");
        peer.shutdown().await;

        drop(lock);
        assert_eq!(
            read_journal_version(&config_root, &data_root, NOW),
            JournalVersionStatus::LastKnown("2026.8.0".to_owned())
        );
    });
}

#[test]
fn status_names_the_running_version_and_the_segments_the_journal_turned_away() {
    runtime().block_on(async {
        let temporary = TestDirectory::new("status-running-version");
        let roots = IsolatedRoots::new(temporary.path());
        let data_root = roots.data_root();
        fs::create_dir_all(data_root.parent().expect("data parent")).expect("data parent");
        ensure_private_directory(&data_root).expect("data root");
        let lock = InstanceLock::acquire(&data_root).expect("instance lock");
        let writer = HealthWriter::new(data_root.clone(), &lock);
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        let mut facts = SyncFacts {
            paired: true,
            ..SyncFacts::default()
        };
        facts.successful_contact(now);
        facts.held_segments = 1;
        facts.held_newest = Some("20261107/boron.tmux/112400_300 (400 malformed_evidence_row)".to_owned());
        writer.write(&facts, now).await.expect("write health");
        let installed = env!("CARGO_PKG_VERSION");

        // A refused segment is the journal answering, so the link still reads connected.
        let report = read_status_report(&data_root, now);
        assert_eq!(report.health, StatusHealth::Live(HealthState::Connected));
        let status = run_status(&roots);
        assert!(
            status.contains(&format!("service-version: {installed}\n")),
            "{status}"
        );
        assert!(status.contains("sync-health: connected\n"), "{status}");
        assert!(
            status.contains(
                "turned-away: 1 segment, 20261107/boron.tmux/112400_300 (400 malformed_evidence_row)."
            ),
            "{status}"
        );

        let path = data_root.join(HEALTH_FILENAME);
        let mut snapshot: Value =
            serde_json::from_slice(&fs::read(&path).expect("snapshot")).expect("snapshot JSON");
        snapshot["app_version"] = Value::from("2.0.24");
        fs::write(&path, serde_json::to_vec(&snapshot).unwrap()).expect("older run");
        let status = run_status(&roots);
        assert!(
            status.contains(&format!(
                "service-version: 2.0.24, but {installed} is installed."
            )),
            "{status}"
        );

        // A release from before the running version was reported writes no field at all.
        let object = snapshot.as_object_mut().expect("snapshot object");
        object.remove("app_version");
        object.remove("held_segments");
        object.remove("held_newest");
        fs::write(&path, serde_json::to_vec(&snapshot).unwrap()).expect("pre-version run");
        let status = run_status(&roots);
        assert!(
            status.contains(&format!(
                "service-version: older than {installed}, which is installed."
            )),
            "{status}"
        );
        assert!(status.contains("sync-health: connected\n"), "{status}");
        assert!(!status.contains("turned-away"), "{status}");

        drop(lock);
        let status = run_status(&roots);
        assert!(!status.contains("service-version"), "{status}");
    });
}

fn run_status(roots: &IsolatedRoots) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_solstone-tmux"))
        .arg("status")
        .env_clear()
        .envs(roots.entries().iter().cloned())
        .output()
        .expect("run status");
    String::from_utf8(output.stdout).expect("status stdout")
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .expect("test runtime")
}
