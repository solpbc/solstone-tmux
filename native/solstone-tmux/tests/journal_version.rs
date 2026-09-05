// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

mod support;

use std::fs;
use std::path::Path;
use std::time::Duration;

use solstone_tmux::health::{HealthState, HealthWriter, SyncFacts};
use solstone_tmux::instance_lock::InstanceLock;
use solstone_tmux::journal_version::{
    JOURNAL_VERSION_FILENAME, JournalVersionStatus, read_journal_version,
};
use solstone_tmux::paths::ensure_private_directory;
use solstone_tmux::private_link::persist_credential;
use solstone_tmux::sync::JournalSession;
use spl_transport::journal_bridge::CarrierOpener;
use support::TestDirectory;
use support::private_link_peer::PrivateLinkPeer;

const NOW: i64 = 1_800_000_000;

async fn write_test_health(data_root: &Path, lock: &InstanceLock, state: HealthState, now: i64) {
    let writer = HealthWriter::new(data_root.to_path_buf(), lock);
    let mut facts = SyncFacts {
        paired: true,
        ..SyncFacts::default()
    };
    match state {
        HealthState::Connected => {
            facts.successful_contact(now);
        }
        HealthState::Syncing => {
            facts.successful_contact(now);
            facts.sync_in_progress = true;
        }
        _ => {}
    }
    writer.write(&facts, now).await.expect("write health");
}

#[test]
fn initial_fetch_stores_and_live_read_reports_current() {
    runtime().block_on(async {
        let temporary = TestDirectory::new("jv-initial-fetch");
        let config_root = temporary.path().join("config");
        let data_root = temporary.path().join("data");
        ensure_private_directory(&config_root).expect("config root");
        ensure_private_directory(&data_root).expect("data root");

        let lock = InstanceLock::acquire(&data_root).expect("instance lock");
        let peer = PrivateLinkPeer::start().await;
        persist_credential(&config_root, &peer.credential()).expect("persist credential");
        write_test_health(&data_root, &lock, HealthState::Connected, NOW).await;
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
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        assert_eq!(
            read_journal_version(&config_root, &data_root, NOW),
            JournalVersionStatus::Current("2026.8.0".to_owned())
        );

        let version_path = config_root.join(JOURNAL_VERSION_FILENAME);
        assert!(version_path.exists());
        let content = fs::read_to_string(&version_path).expect("read version file");
        let record: serde_json::Value =
            serde_json::from_str(&content).expect("parse version record");
        assert_eq!(record["version"], "2026.8.0");
        assert_eq!(record["confirmed"], true);
        assert_eq!(record["instance_id"], peer.credential().instance_id);
        assert_eq!(record["run_id"], lock.identity().run_id);
        assert_eq!(record["lock_inode"], lock.identity().lock_inode);

        session.shutdown().await.expect("shutdown session");
        peer.shutdown().await;
    });
}

#[test]
fn redial_carrier_fetches_newer_version_and_updates_cache() {
    runtime().block_on(async {
        let temporary = TestDirectory::new("jv-redial");
        let config_root = temporary.path().join("config");
        let data_root = temporary.path().join("data");
        ensure_private_directory(&config_root).expect("config root");
        ensure_private_directory(&data_root).expect("data root");

        let lock = InstanceLock::acquire(&data_root).expect("instance lock");
        let peer = PrivateLinkPeer::start().await;
        persist_credential(&config_root, &peer.credential()).expect("persist credential");
        write_test_health(&data_root, &lock, HealthState::Connected, NOW).await;
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
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        assert_eq!(
            read_journal_version(&config_root, &data_root, NOW),
            JournalVersionStatus::Current("2026.8.0".to_owned())
        );

        peer.enqueue_system_status_response(
            200,
            br#"{"ok":true,"version":{"current":"2026.8.1"}}"#.to_vec(),
        );

        let _ = session.opener().dial_carrier().await.expect("dial carrier");

        for _ in 0..50 {
            if read_journal_version(&config_root, &data_root, NOW)
                == JournalVersionStatus::Current("2026.8.1".to_owned())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        assert_eq!(
            read_journal_version(&config_root, &data_root, NOW),
            JournalVersionStatus::Current("2026.8.1".to_owned())
        );

        session.shutdown().await.expect("shutdown session");
        peer.shutdown().await;
    });
}

#[test]
fn malformed_or_failed_response_preserves_existing_cache() {
    runtime().block_on(async {
        let temporary = TestDirectory::new("jv-malformed-preserves");
        let config_root = temporary.path().join("config");
        let data_root = temporary.path().join("data");
        ensure_private_directory(&config_root).expect("config root");
        ensure_private_directory(&data_root).expect("data root");

        let lock = InstanceLock::acquire(&data_root).expect("instance lock");
        let peer = PrivateLinkPeer::start().await;
        persist_credential(&config_root, &peer.credential()).expect("persist credential");
        write_test_health(&data_root, &lock, HealthState::Connected, NOW).await;
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
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        assert_eq!(
            read_journal_version(&config_root, &data_root, NOW),
            JournalVersionStatus::Current("2026.8.0".to_owned())
        );

        // When a subsequent fetch fails (e.g. 500 error), confirmed flips false while version is preserved
        peer.enqueue_system_status_response(500, br#"{"error":"internal error"}"#.to_vec());
        let _ = session.opener().dial_carrier().await.expect("dial carrier");

        for _ in 0..50 {
            if read_journal_version(&config_root, &data_root, NOW)
                == JournalVersionStatus::LastKnown("2026.8.0".to_owned())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        assert_eq!(
            read_journal_version(&config_root, &data_root, NOW),
            JournalVersionStatus::LastKnown("2026.8.0".to_owned())
        );

        let version_path = config_root.join(JOURNAL_VERSION_FILENAME);
        let content = fs::read_to_string(&version_path).expect("read version file");
        let record: serde_json::Value =
            serde_json::from_str(&content).expect("parse version record");
        assert_eq!(record["version"], "2026.8.0");
        assert_eq!(record["confirmed"], false);

        session.shutdown().await.expect("shutdown session");
        peer.shutdown().await;
    });
}

#[test]
fn dial_failure_invalidates_confirmed_and_reports_last_known() {
    runtime().block_on(async {
        let temporary = TestDirectory::new("jv-dial-failure");
        let config_root = temporary.path().join("config");
        let data_root = temporary.path().join("data");
        ensure_private_directory(&config_root).expect("config root");
        ensure_private_directory(&data_root).expect("data root");

        let lock = InstanceLock::acquire(&data_root).expect("instance lock");
        let peer = PrivateLinkPeer::start().await;
        persist_credential(&config_root, &peer.credential()).expect("persist credential");
        write_test_health(&data_root, &lock, HealthState::Connected, NOW).await;
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
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        assert_eq!(
            read_journal_version(&config_root, &data_root, NOW),
            JournalVersionStatus::Current("2026.8.0".to_owned())
        );

        // Shut down peer so carrier dial fails
        peer.shutdown().await;

        let dial_result = session.opener().dial_carrier().await;
        assert!(dial_result.is_err());

        for _ in 0..50 {
            if read_journal_version(&config_root, &data_root, NOW)
                == JournalVersionStatus::LastKnown("2026.8.0".to_owned())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        assert_eq!(
            read_journal_version(&config_root, &data_root, NOW),
            JournalVersionStatus::LastKnown("2026.8.0".to_owned())
        );

        let version_path = config_root.join(JOURNAL_VERSION_FILENAME);
        let content = fs::read_to_string(&version_path).expect("read version file");
        let record: serde_json::Value =
            serde_json::from_str(&content).expect("parse version record");
        assert_eq!(record["version"], "2026.8.0");
        assert_eq!(record["confirmed"], false);

        session.shutdown().await.expect("shutdown session");
    });
}

#[test]
fn credential_mismatch_invalidates_cached_version() {
    runtime().block_on(async {
        let temporary = TestDirectory::new("jv-cred-mismatch");
        let config_root = temporary.path().join("config");
        let data_root = temporary.path().join("data");
        ensure_private_directory(&config_root).expect("config root");
        ensure_private_directory(&data_root).expect("data root");

        let lock = InstanceLock::acquire(&data_root).expect("instance lock");
        let peer = PrivateLinkPeer::start().await;
        persist_credential(&config_root, &peer.credential()).expect("persist credential");
        write_test_health(&data_root, &lock, HealthState::Connected, NOW).await;
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
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        assert_eq!(
            read_journal_version(&config_root, &data_root, NOW),
            JournalVersionStatus::Current("2026.8.0".to_owned())
        );

        let mut modified_cred = peer.credential();
        modified_cred.instance_id = "different-instance-id".to_owned();

        session.shutdown().await.expect("shutdown session");
        peer.shutdown().await;

        // Modify credential on disk with a different instance_id
        persist_credential(&config_root, &modified_cred).expect("persist modified credential");

        assert_eq!(
            read_journal_version(&config_root, &data_root, NOW),
            JournalVersionStatus::Unknown
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
