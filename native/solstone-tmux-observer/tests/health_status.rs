// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

mod support;

use std::fs;
use std::process::Command;

use serde_json::Value;
use solstone_tmux_observer::health::{
    DiagnosticCode, HEALTH_FILENAME, HealthState, HealthWriter, StatusHealth, SyncFacts,
    read_status_health,
};
use solstone_tmux_observer::instance_lock::{InstanceLock, LOCK_FILENAME};
use solstone_tmux_observer::paths::ensure_private_directory;
use solstone_tmux_observer::sync::StatusBeacon;
use support::{IsolatedRoots, TestDirectory};

const NOW: i64 = 1_800_000_000;

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
        writer.write(&facts, NOW).await.expect("connected snapshot");
        assert_eq!(
            read_status_health(&data_root, NOW),
            StatusHealth::Live(HealthState::Connected)
        );
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
fn snapshot_and_closed_errors_have_no_secret_or_owner_content_fields() {
    runtime().block_on(async {
        let temporary = TestDirectory::new("health-redaction");
        let data_root = temporary.path().join("data");
        ensure_private_directory(&data_root).expect("data root");
        let lock = InstanceLock::acquire(&data_root).expect("instance lock");
        let writer = HealthWriter::new(data_root.clone(), &lock);
        let sentinels = [
            "SENTINEL_PAIR_LINK",
            "SENTINEL_OBSERVER_KEY",
            "SENTINEL_RELAY_TOKEN",
            "SENTINEL_RESPONSE_BODY",
            "SENTINEL_CAPTURE_PATH",
            "SENTINEL_TERMINAL_CONTENT",
        ];
        let failure_codes = [
            DiagnosticCode::JournalUnavailable,
            DiagnosticCode::BridgeUnavailable,
            DiagnosticCode::JournalRejected,
            DiagnosticCode::JournalTimeout,
            DiagnosticCode::JournalContractInvalid,
            DiagnosticCode::SyncTaskExited,
            DiagnosticCode::SyncTaskPanicked,
            DiagnosticCode::SyncTaskCancelled,
            DiagnosticCode::IndicatorUpdateFailed,
        ];
        let mut rendered_errors = String::new();
        for code in failure_codes {
            let mut facts = SyncFacts {
                paired: true,
                ..SyncFacts::default()
            };
            facts.failed(code);
            writer.write(&facts, NOW).await.expect("health snapshot");
            rendered_errors.push_str(code.message());
        }
        let snapshot = fs::read(data_root.join(HEALTH_FILENAME)).expect("snapshot bytes");
        let snapshot: Value = serde_json::from_slice(&snapshot).expect("snapshot JSON");
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
        let snapshot_text = snapshot.to_string();
        for sentinel in sentinels {
            assert!(!snapshot_text.contains(sentinel));
            assert!(!rendered_errors.contains(sentinel));
        }
    });
}

#[test]
fn status_is_read_only_when_native_state_is_absent() {
    let temporary = TestDirectory::new("status-read-only");
    let roots = IsolatedRoots::new(temporary.path());
    let output = Command::new(env!("CARGO_BIN_EXE_solstone-tmux-observer"))
        .arg("status")
        .env_clear()
        .envs(roots.entries().iter().cloned())
        .output()
        .expect("run status");

    assert_eq!(output.status.code(), Some(4));
    assert_eq!(
        String::from_utf8(output.stdout).expect("status stdout"),
        "service: absent\nsync-health: unknown\n"
    );
    assert!(!roots.data_root().exists());
    assert!(!roots.data_root().join(LOCK_FILENAME).exists());
    assert!(!roots.data_root().join(HEALTH_FILENAME).exists());
}

#[test]
fn status_beacon_has_only_the_eight_closed_diagnostic_fields() {
    let beacon = StatusBeacon {
        name: "observer-name".to_owned(),
        uptime: 60,
        last_successful_sync: Some(NOW),
        pending_queue_depth: 2,
        recent_error_count: 1,
        last_error_reason: Some(DiagnosticCode::JournalTimeout),
    };
    let fields = beacon.fields();
    assert_eq!(
        fields.keys().map(String::as_str).collect::<Vec<_>>(),
        [
            "last_error_reason",
            "last_successful_sync",
            "name",
            "pending_queue_depth",
            "recent_error_count",
            "stream_type",
            "uptime",
            "version",
        ]
    );
    let encoded = serde_json::to_string(&fields).expect("encode beacon");
    for sentinel in [
        "SENTINEL_PAIR_LINK",
        "SENTINEL_OBSERVER_KEY",
        "SENTINEL_RELAY_TOKEN",
        "SENTINEL_RESPONSE_BODY",
        "SENTINEL_CAPTURE_PATH",
        "SENTINEL_TERMINAL_CONTENT",
    ] {
        assert!(!encoded.contains(sentinel));
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("test runtime")
}
