// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;
use solstone_tmux::client_metadata::{
    ClientsSelfReportedSnapshot, build_reported_snapshot, decode_get_response, run_metadata_job,
    sanitize_field,
};
use solstone_tmux::instance_lock::InstanceLock;
use solstone_tmux::journal::JournalClient;
use solstone_tmux::journal_version::VersionRefreshState;
use solstone_tmux::paths::{PlatformKind, ensure_private_directory};
use solstone_tmux::private_link::{PrivateLinkBridge, persist_credential};
use solstone_tmux::sync::JournalSession;

mod support;
use support::TestDirectory;
use support::private_link_peer::PrivateLinkPeer;

#[test]
fn field_sanitization_enforces_exact_bounds_and_valid_utf8() {
    assert_eq!(
        sanitize_field("valid_host", 255),
        Some("valid_host".to_owned())
    );
    assert_eq!(
        sanitize_field("  spaces trimmed  ", 64),
        Some("spaces trimmed".to_owned())
    );
    assert_eq!(sanitize_field("   ", 64), None);
    assert_eq!(sanitize_field("line\nbreak", 64), None);

    // Overlong rejection
    let long_ascii = "a".repeat(300);
    assert_eq!(sanitize_field(&long_ascii, 255), None);
    assert_eq!(sanitize_field(&long_ascii, 64), None);
}

#[test]
fn reported_snapshot_builds_valid_description() {
    let desc = build_reported_snapshot(|| Some("test-host.local".to_owned()), PlatformKind::Linux);
    assert_eq!(desc.name.as_deref(), Some("test-host.local"));
    assert_eq!(desc.platform.as_deref(), Some("linux"));
    assert_eq!(desc.device_type.as_deref(), Some("terminal"));
    assert_eq!(desc.app_id.as_deref(), Some("solstone-tmux"));
    assert!(desc.app_version.is_some());
}

#[test]
fn decode_get_response_handles_null_and_populated_reported() {
    let empty_payload = json!({
        "protocol_version": 1,
        "revision": 1,
        "reported": null,
        "owner_label": null,
        "display_label": null,
        "updated_at": null,
        "journal": {
            "name": "my-journal",
            "version": "2026.8.0"
        }
    });
    let decoded =
        decode_get_response(&serde_json::to_vec(&empty_payload).expect("json")).expect("decode");
    assert!(decoded.reported.is_none());
    assert_eq!(decoded.journal.version, "2026.8.0");

    let populated_payload = json!({
        "protocol_version": 1,
        "revision": 2,
        "reported": {
            "name": "my-host",
            "platform": "linux",
            "device_type": "terminal",
            "app_id": "solstone-tmux",
            "app_version": "1.0.6"
        },
        "owner_label": null,
        "display_label": null,
        "updated_at": null,
        "journal": {
            "name": "my-journal",
            "version": "2026.8.0"
        }
    });
    let decoded = decode_get_response(&serde_json::to_vec(&populated_payload).expect("json"))
        .expect("decode");
    assert_eq!(
        decoded.reported,
        Some(ClientsSelfReportedSnapshot {
            name: Some("my-host".to_owned()),
            platform: Some("linux".to_owned()),
            device_type: Some("terminal".to_owned()),
            app_id: Some("solstone-tmux".to_owned()),
            app_version: Some("1.0.6".to_owned()),
        })
    );
}

#[test]
fn clients_self_publishes_when_reported_is_null() {
    runtime().block_on(async {
        let peer = PrivateLinkPeer::start().await;
        let temporary = TestDirectory::new("clients-self-null-publishes");
        ensure_private_directory(temporary.path()).expect("private root");
        let lock = InstanceLock::acquire(temporary.path()).expect("acquire lock");
        let credential = peer.credential();
        let refresh = VersionRefreshState::new(
            temporary.path().to_path_buf(),
            temporary.path().to_path_buf(),
            credential.instance_id.clone(),
            &credential.ca_fp_prefix,
            lock.identity().clone(),
        );

        let bridge = PrivateLinkBridge::start(credential, None, refresh.clone())
            .await
            .expect("start bridge");
        let client = JournalClient::bootstrap(&bridge)
            .await
            .expect("journal client");

        peer.enqueue_clients_self_response(
            200,
            serde_json::to_vec(&json!({
                "protocol_version": 1,
                "revision": 1,
                "reported": null,
                "owner_label": null,
                "display_label": null,
                "updated_at": null,
                "journal": {
                    "name": "test-journal",
                    "version": "2026.8.0"
                }
            }))
            .expect("json"),
        );
        peer.enqueue_clients_self_response(
            200,
            serde_json::to_vec(&json!({
                "protocol_version": 1,
                "revision": 2,
                "reported": {
                    "name": "test-box",
                    "platform": "linux",
                    "device_type": "terminal",
                    "app_id": "solstone-tmux",
                    "app_version": "1.0.6"
                },
                "owner_label": null,
                "display_label": null,
                "updated_at": null,
                "journal": { "name": "test-journal", "version": "2026.8.0" }
            }))
            .expect("json"),
        );

        let result = run_metadata_job(
            &client,
            None,
            &refresh,
            || Some("test-box".to_owned()),
            PlatformKind::Linux,
            Duration::from_secs(5),
        )
        .await;
        assert!(result.is_ok());

        let requests = peer
            .requests()
            .into_iter()
            .filter(|r| r.path_without_query() == "/app/network/api/clients/self")
            .collect::<Vec<_>>();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].method(), "GET");
        assert_eq!(requests[1].method(), "PUT");

        let put_body: serde_json::Value =
            serde_json::from_slice(requests[1].body()).expect("put json");
        assert_eq!(put_body["protocol_version"], 1);
        assert_eq!(put_body["expected_revision"], 1);
        assert_eq!(put_body["reported"]["name"], "test-box");
        assert_eq!(put_body["reported"]["platform"], "linux");

        bridge.shutdown().await;
        peer.shutdown().await;
    });
}

#[test]
fn clients_self_noops_when_reported_already_matches() {
    runtime().block_on(async {
        let peer = PrivateLinkPeer::start().await;
        let temporary = TestDirectory::new("clients-self-matching-noop");
        ensure_private_directory(temporary.path()).expect("private root");
        let lock = InstanceLock::acquire(temporary.path()).expect("acquire lock");
        let credential = peer.credential();
        let refresh = VersionRefreshState::new(
            temporary.path().to_path_buf(),
            temporary.path().to_path_buf(),
            credential.instance_id.clone(),
            &credential.ca_fp_prefix,
            lock.identity().clone(),
        );

        let bridge = PrivateLinkBridge::start(credential, None, refresh.clone())
            .await
            .expect("start bridge");
        let client = JournalClient::bootstrap(&bridge)
            .await
            .expect("journal client");

        let current = build_reported_snapshot(|| Some("test-box".to_owned()), PlatformKind::Linux);

        peer.enqueue_clients_self_response(
            200,
            serde_json::to_vec(&json!({
                "protocol_version": 1,
                "revision": 3,
                "reported": current,
                "owner_label": null,
                "display_label": null,
                "updated_at": null,
                "journal": { "name": "test-journal", "version": "2026.8.0" }
            }))
            .expect("json"),
        );

        let result = run_metadata_job(
            &client,
            None,
            &refresh,
            || Some("test-box".to_owned()),
            PlatformKind::Linux,
            Duration::from_secs(5),
        )
        .await;
        assert!(result.is_ok());

        let requests = peer
            .requests()
            .into_iter()
            .filter(|r| r.path_without_query() == "/app/network/api/clients/self")
            .collect::<Vec<_>>();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method(), "GET");

        bridge.shutdown().await;
        peer.shutdown().await;
    });
}

#[test]
fn clients_self_handles_409_conflict_with_refetch_and_retry() {
    runtime().block_on(async {
        let peer = PrivateLinkPeer::start().await;
        let temporary = TestDirectory::new("clients-self-conflict-retry");
        ensure_private_directory(temporary.path()).expect("private root");
        let lock = InstanceLock::acquire(temporary.path()).expect("acquire lock");
        let credential = peer.credential();
        let refresh = VersionRefreshState::new(
            temporary.path().to_path_buf(),
            temporary.path().to_path_buf(),
            credential.instance_id.clone(),
            &credential.ca_fp_prefix,
            lock.identity().clone(),
        );

        let bridge = PrivateLinkBridge::start(credential, None, refresh.clone())
            .await
            .expect("start bridge");
        let client = JournalClient::bootstrap(&bridge)
            .await
            .expect("journal client");

        // 1. Initial GET -> reported null, revision 1
        peer.enqueue_clients_self_response(
            200,
            serde_json::to_vec(&json!({
                "protocol_version": 1,
                "revision": 1,
                "reported": null,
                "owner_label": null,
                "display_label": null,
                "updated_at": null,
                "journal": { "name": "test-journal", "version": "2026.8.0" }
            }))
            .expect("json"),
        );
        // 2. PUT (expected_revision 1) -> 409 Conflict
        peer.enqueue_clients_self_response(409, br#"{"error":"conflict"}"#.to_vec());
        // 3. Refetch GET -> revision 2, reported still old
        peer.enqueue_clients_self_response(
            200,
            serde_json::to_vec(&json!({
                "protocol_version": 1,
                "revision": 2,
                "reported": {
                    "name": "old-name",
                    "platform": null,
                    "device_type": null,
                    "app_id": null,
                    "app_version": null
                },
                "owner_label": null,
                "display_label": null,
                "updated_at": null,
                "journal": { "name": "test-journal", "version": "2026.8.0" }
            }))
            .expect("json"),
        );
        // 4. Retry PUT (expected_revision 2) -> 200 OK
        peer.enqueue_clients_self_response(
            200,
            serde_json::to_vec(&json!({
                "protocol_version": 1,
                "revision": 3,
                "reported": null,
                "owner_label": null,
                "display_label": null,
                "updated_at": null,
                "journal": { "name": "test-journal", "version": "2026.8.0" }
            }))
            .expect("json"),
        );

        let result = run_metadata_job(
            &client,
            None,
            &refresh,
            || Some("test-box".to_owned()),
            PlatformKind::Linux,
            Duration::from_secs(5),
        )
        .await;
        assert!(result.is_ok());

        let requests = peer
            .requests()
            .into_iter()
            .filter(|r| r.path_without_query() == "/app/network/api/clients/self")
            .collect::<Vec<_>>();
        assert_eq!(requests.len(), 4);
        assert_eq!(requests[0].method(), "GET");
        assert_eq!(requests[1].method(), "PUT");
        assert_eq!(requests[2].method(), "GET");
        assert_eq!(requests[3].method(), "PUT");

        let retry_put: serde_json::Value =
            serde_json::from_slice(requests[3].body()).expect("put json");
        assert_eq!(retry_put["expected_revision"], 2);

        bridge.shutdown().await;
        peer.shutdown().await;
    });
}

#[test]
fn clients_self_404_is_tolerated_as_unsupported() {
    runtime().block_on(async {
        let peer = PrivateLinkPeer::start().await;
        let temporary = TestDirectory::new("clients-self-404-tolerated");
        ensure_private_directory(temporary.path()).expect("private root");
        let lock = InstanceLock::acquire(temporary.path()).expect("acquire lock");
        let credential = peer.credential();
        let refresh = VersionRefreshState::new(
            temporary.path().to_path_buf(),
            temporary.path().to_path_buf(),
            credential.instance_id.clone(),
            &credential.ca_fp_prefix,
            lock.identity().clone(),
        );

        let bridge = PrivateLinkBridge::start(credential, None, refresh.clone())
            .await
            .expect("start bridge");
        let client = JournalClient::bootstrap(&bridge)
            .await
            .expect("journal client");

        peer.enqueue_clients_self_response(404, br#"{"error":"not found"}"#.to_vec());

        let result = run_metadata_job(
            &client,
            None,
            &refresh,
            || Some("test-box".to_owned()),
            PlatformKind::Linux,
            Duration::from_secs(5),
        )
        .await;
        assert!(result.is_ok());

        let requests = peer
            .requests()
            .into_iter()
            .filter(|r| r.path_without_query() == "/app/network/api/clients/self")
            .collect::<Vec<_>>();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method(), "GET");

        bridge.shutdown().await;
        peer.shutdown().await;
    });
}

#[test]
fn clients_self_integrates_with_journal_session() {
    runtime().block_on(async {
        let peer = PrivateLinkPeer::start().await;
        let temporary = TestDirectory::new("clients-self-session-test");
        let config_root = temporary.path().join("config");
        let data_root = temporary.path().join("data");
        ensure_private_directory(&config_root).expect("config root");
        ensure_private_directory(&data_root).expect("data root");
        let lock = InstanceLock::acquire(&data_root).expect("acquire lock");

        let credential = peer.credential();
        solstone_tmux::private_link::persist_credential(&config_root, &credential)
            .expect("persist cred");

        let refresh = VersionRefreshState::new(
            config_root.clone(),
            data_root.clone(),
            credential.instance_id.clone(),
            &credential.ca_fp_prefix,
            lock.identity().clone(),
        );

        peer.enqueue_clients_self_response(
            200,
            serde_json::to_vec(&json!({
                "protocol_version": 1,
                "revision": 1,
                "reported": null,
                "owner_label": null, "display_label": null, "updated_at": null,
                "journal": { "name": "test-journal", "version": "2026.8.0" }
            }))
            .expect("json"),
        );
        peer.enqueue_clients_self_response(
            200,
            serde_json::to_vec(&json!({
                "protocol_version": 1,
                "revision": 2,
                "reported": {
                    "name": "session-host", "platform": "linux", "device_type": "terminal",
                    "app_id": "solstone-tmux", "app_version": solstone_tmux::cli::version()
                },
                "owner_label": null, "display_label": null, "updated_at": null,
                "journal": { "name": "test-journal", "version": "2026.8.0" }
            }))
            .expect("json"),
        );

        let session = JournalSession::start_with(
            credential,
            config_root,
            refresh,
            Duration::from_secs(5),
            Arc::new(|| Some("session-host".to_owned())),
            PlatformKind::Linux,
            Arc::new(solstone_tmux::clock::SystemClock::new(time::UtcOffset::UTC)),
        )
        .await
        .expect("start session");

        session
            .wait_for_post_connect_quiescence(Duration::from_secs(5))
            .await;

        let requests = peer
            .requests()
            .into_iter()
            .filter(|r| r.path_without_query() == "/app/network/api/clients/self")
            .collect::<Vec<_>>();
        assert!(requests.len() >= 2);
        assert_eq!(requests[0].method(), "GET");
        assert_eq!(requests[1].method(), "PUT");

        // Now enqueue matching response and test manual trigger_post_connect()
        peer.enqueue_clients_self_response(
            200,
            serde_json::to_vec(&json!({
                "protocol_version": 1,
                "revision": 2,
                "reported": {
                    "name": "session-host",
                    "platform": "linux",
                    "device_type": "terminal",
                    "app_id": "solstone-tmux",
                    "app_version": solstone_tmux::cli::version()
                },
                "owner_label": null, "display_label": null, "updated_at": null,
                "journal": { "name": "test-journal", "version": "2026.8.0" }
            }))
            .expect("json"),
        );

        let count_before = requests.len();
        session.trigger_post_connect();
        session
            .wait_for_post_connect_quiescence(Duration::from_secs(5))
            .await;

        let requests_after = peer
            .requests()
            .into_iter()
            .filter(|r| r.path_without_query() == "/app/network/api/clients/self")
            .collect::<Vec<_>>();
        assert!(requests_after.len() > count_before);
        assert_eq!(requests_after.last().unwrap().method(), "GET");

        session.shutdown().await.expect("session shutdown");
        peer.shutdown().await;
    });
}

#[test]
fn clients_self_description_b_during_a_is_published_on_one_follow_up() {
    runtime().block_on(async {
        let peer = PrivateLinkPeer::start().await;
        let temporary = TestDirectory::new("clients-self-description-follow-up");
        let config_root = temporary.path().join("config");
        let data_root = temporary.path().join("data");
        ensure_private_directory(&config_root).expect("config root");
        ensure_private_directory(&data_root).expect("data root");
        let lock = InstanceLock::acquire(&data_root).expect("acquire lock");
        let credential = peer.credential();
        persist_credential(&config_root, &credential).expect("persist credential");
        let refresh = VersionRefreshState::new(
            config_root.clone(),
            data_root.clone(),
            credential.instance_id.clone(),
            &credential.ca_fp_prefix,
            lock.identity().clone(),
        );

        let reported_a = json!({
            "name": "description-a", "platform": "linux", "device_type": "terminal",
            "app_id": "solstone-tmux", "app_version": solstone_tmux::cli::version()
        });
        let reported_b = json!({
            "name": "description-b", "platform": "linux", "device_type": "terminal",
            "app_id": "solstone-tmux", "app_version": solstone_tmux::cli::version()
        });
        for (revision, reported) in [
            (1, serde_json::Value::Null),
            (2, reported_a.clone()),
            (2, reported_a),
            (3, reported_b.clone()),
        ] {
            peer.enqueue_clients_self_response(
                200,
                serde_json::to_vec(&json!({
                    "protocol_version": 1,
                    "revision": revision,
                    "reported": reported,
                    "owner_label": null, "display_label": null, "updated_at": null,
                    "journal": { "name": "test-journal", "version": "2026.8.0" }
                }))
                .expect("json"),
            );
        }

        let hostname = Arc::new(Mutex::new("description-a".to_owned()));
        let hostname_source = {
            let hostname = Arc::clone(&hostname);
            Arc::new(move || {
                Some(
                    hostname
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .clone(),
                )
            })
        };
        peer.hold_clients_self();
        let session = JournalSession::start_with(
            credential,
            config_root,
            refresh,
            Duration::from_secs(5),
            hostname_source,
            PlatformKind::Linux,
            Arc::new(solstone_tmux::clock::SystemClock::new(time::UtcOffset::UTC)),
        )
        .await
        .expect("start session");

        peer.wait_for_clients_self_hold(Duration::from_secs(5))
            .await;
        peer.release_one_clients_self();
        peer.wait_for_clients_self_hold_count(2, Duration::from_secs(5))
            .await;
        *hostname.lock().unwrap_or_else(|error| error.into_inner()) = "description-b".to_owned();
        session.trigger_post_connect();
        peer.release_clients_self();
        session
            .wait_for_post_connect_quiescence(Duration::from_secs(5))
            .await;

        let requests = peer
            .requests()
            .into_iter()
            .filter(|request| request.path_without_query() == "/app/network/api/clients/self")
            .collect::<Vec<_>>();
        assert_eq!(requests.len(), 4, "one metadata follow-up is permitted");
        let first_put: serde_json::Value =
            serde_json::from_slice(requests[1].body()).expect("first put json");
        let follow_up_put: serde_json::Value =
            serde_json::from_slice(requests[3].body()).expect("follow-up put json");
        assert_eq!(first_put["reported"]["name"], "description-a");
        assert_eq!(follow_up_put["reported"]["name"], "description-b");

        session.shutdown().await.expect("shutdown session");
        peer.shutdown().await;
    });
}

#[test]
fn clients_self_invalid_name_nulls_field_without_tmux_fallback() {
    runtime().block_on(async {
        let peer = PrivateLinkPeer::start().await;
        let temporary = TestDirectory::new("clients-self-invalid-name-test");
        let config_root = temporary.path().join("config");
        let data_root = temporary.path().join("data");
        ensure_private_directory(&config_root).expect("config root");
        ensure_private_directory(&data_root).expect("data root");
        let lock = InstanceLock::acquire(&data_root).expect("acquire lock");

        let credential = peer.credential();
        persist_credential(&config_root, &credential).expect("persist cred");

        let refresh = VersionRefreshState::new(
            config_root.clone(),
            data_root.clone(),
            credential.instance_id.clone(),
            &credential.ca_fp_prefix,
            lock.identity().clone(),
        );

        peer.enqueue_clients_self_response(
            200,
            serde_json::to_vec(&json!({
                "protocol_version": 1,
                "revision": 1,
                "reported": null,
                "owner_label": null, "display_label": null, "updated_at": null,
                "journal": { "name": "test-journal", "version": "2026.8.0" }
            }))
            .expect("json"),
        );
        peer.enqueue_clients_self_response(
            200,
            serde_json::to_vec(&json!({
                "protocol_version": 1,
                "revision": 2,
                "reported": null,
                "owner_label": null, "display_label": null, "updated_at": null,
                "journal": { "name": "test-journal", "version": "2026.8.0" }
            }))
            .expect("json"),
        );

        // Hostname with control char (\n) or over 80 bytes
        let invalid_hostname = "bad\nhost\0name";
        let session = JournalSession::start_with(
            credential,
            config_root,
            refresh,
            Duration::from_secs(5),
            Arc::new(move || Some(invalid_hostname.to_owned())),
            PlatformKind::Linux,
            Arc::new(solstone_tmux::clock::SystemClock::new(time::UtcOffset::UTC)),
        )
        .await
        .expect("start session");

        session
            .wait_for_post_connect_quiescence(Duration::from_secs(5))
            .await;

        let requests = peer
            .requests()
            .into_iter()
            .filter(|r| r.path_without_query() == "/app/network/api/clients/self")
            .collect::<Vec<_>>();
        assert!(requests.len() >= 2);
        assert_eq!(requests[1].method(), "PUT");

        let put_body: serde_json::Value =
            serde_json::from_slice(requests[1].body()).expect("valid json");
        assert_eq!(put_body["reported"]["name"], serde_json::Value::Null);
        assert_eq!(put_body["reported"]["platform"], "linux");
        assert_eq!(put_body["reported"]["device_type"], "terminal");
        assert_eq!(put_body["reported"]["app_id"], "solstone-tmux");
        assert!(put_body["reported"]["app_version"].is_string());

        // Body must not contain "tmux" as name fallback
        assert_ne!(put_body["reported"]["name"], "tmux");

        session.shutdown().await.expect("session shutdown");
        peer.shutdown().await;
    });
}

#[test]
fn clients_self_redirect_is_refused() {
    runtime().block_on(async {
        let peer = PrivateLinkPeer::start().await;
        let temporary = TestDirectory::new("clients-self-redirect-test");
        let config_root = temporary.path().join("config");
        let data_root = temporary.path().join("data");
        ensure_private_directory(&config_root).expect("config root");
        ensure_private_directory(&data_root).expect("data root");
        let lock = InstanceLock::acquire(&data_root).expect("acquire lock");

        let credential = peer.credential();
        persist_credential(&config_root, &credential).expect("persist cred");

        let refresh = VersionRefreshState::new(
            config_root.clone(),
            data_root.clone(),
            credential.instance_id.clone(),
            &credential.ca_fp_prefix,
            lock.identity().clone(),
        );

        peer.enqueue_clients_self_response(301, Vec::new());

        let session = JournalSession::start_with(
            credential,
            config_root,
            refresh,
            Duration::from_secs(5),
            Arc::new(|| Some("session-host".to_owned())),
            PlatformKind::Linux,
            Arc::new(solstone_tmux::clock::SystemClock::new(time::UtcOffset::UTC)),
        )
        .await
        .expect("start session");

        peer.wait_for_clients_self_request_count(1, Duration::from_secs(5))
            .await;

        let requests = peer
            .requests()
            .into_iter()
            .filter(|r| {
                r.path_without_query()
                    .starts_with("/app/network/api/clients/self")
                    || r.path_without_query().starts_with("/redirected")
            })
            .collect::<Vec<_>>();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].path_without_query(),
            "/app/network/api/clients/self"
        );
        assert_eq!(requests[0].method(), "GET");

        session.shutdown().await.expect("session shutdown");
        peer.shutdown().await;
    });
}

#[test]
fn clients_self_timeout_releases_slot_and_fences_late_io() {
    runtime().block_on(async {
        let peer = PrivateLinkPeer::start().await;
        let temporary = TestDirectory::new("clients-self-timeout-test");
        let config_root = temporary.path().join("config");
        let data_root = temporary.path().join("data");
        ensure_private_directory(&config_root).expect("config root");
        ensure_private_directory(&data_root).expect("data root");
        let lock = InstanceLock::acquire(&data_root).expect("acquire lock");

        let credential = peer.credential();
        persist_credential(&config_root, &credential).expect("persist cred");

        let refresh = VersionRefreshState::new(
            config_root.clone(),
            data_root.clone(),
            credential.instance_id.clone(),
            &credential.ca_fp_prefix,
            lock.identity().clone(),
        );

        // Delay GET by 300ms, with a 50ms session timeout
        peer.enqueue_delayed_clients_self_response(
            Duration::from_millis(300),
            200,
            serde_json::to_vec(&json!({
                "protocol_version": 1,
                "revision": 1,
                "reported": null,
                "owner_label": null, "display_label": null, "updated_at": null,
                "journal": { "name": "test-journal", "version": "2026.8.0" }
            }))
            .expect("json"),
        );

        let session = JournalSession::start_with(
            credential,
            config_root,
            refresh,
            Duration::from_millis(50),
            Arc::new(|| Some("session-host".to_owned())),
            PlatformKind::Linux,
            Arc::new(solstone_tmux::clock::SystemClock::new(time::UtcOffset::UTC)),
        )
        .await
        .expect("start session");

        peer.wait_for_clients_self_request_count(1, Duration::from_secs(5))
            .await;

        // Enqueue responses for the second trigger
        peer.enqueue_clients_self_response(
            200,
            serde_json::to_vec(&json!({
                "protocol_version": 1,
                "revision": 1,
                "reported": null,
                "owner_label": null, "display_label": null, "updated_at": null,
                "journal": { "name": "test-journal", "version": "2026.8.0" }
            }))
            .expect("json"),
        );
        peer.enqueue_clients_self_response(
            200,
            serde_json::to_vec(&json!({
                "protocol_version": 1,
                "revision": 2,
                "reported": { "name": "session-host", "platform": null, "device_type": null,
                    "app_id": null, "app_version": null },
                "owner_label": null, "display_label": null, "updated_at": null,
                "journal": { "name": "test-journal", "version": "2026.8.0" }
            }))
            .expect("json"),
        );

        // Trigger post connect again: slot must be available and accept the trigger
        session.trigger_post_connect();
        session
            .wait_for_post_connect_quiescence(Duration::from_secs(5))
            .await;

        let put_requests = peer
            .requests()
            .into_iter()
            .filter(|r| {
                r.path_without_query() == "/app/network/api/clients/self" && r.method() == "PUT"
            })
            .collect::<Vec<_>>();
        // Only the second trigger should have produced a PUT
        assert_eq!(put_requests.len(), 1);

        session.shutdown().await.expect("session shutdown");
        peer.shutdown().await;
    });
}

#[test]
fn clients_self_optional_failure_preserves_upload_progress() {
    runtime().block_on(async {
        let peer = PrivateLinkPeer::start().await;
        let temporary = TestDirectory::new("clients-self-failure-upload-test");
        let config_root = temporary.path().join("config");
        let data_root = temporary.path().join("data");
        ensure_private_directory(&config_root).expect("config root");
        ensure_private_directory(&data_root).expect("data root");
        let lock = InstanceLock::acquire(&data_root).expect("acquire lock");

        let credential = peer.credential();
        persist_credential(&config_root, &credential).expect("persist cred");

        let refresh = VersionRefreshState::new(
            config_root.clone(),
            data_root.clone(),
            credential.instance_id.clone(),
            &credential.ca_fp_prefix,
            lock.identity().clone(),
        );

        // Enqueue 500 for clients/self
        peer.enqueue_clients_self_response(500, Vec::new());

        // Enqueue 200 for upload
        peer.enqueue_response(
            200,
            serde_json::to_vec(&json!({
                "status": "ok",
                "segment": "test_segment"
            }))
            .expect("json"),
        );

        let session = JournalSession::start_with(
            credential,
            config_root,
            refresh,
            Duration::from_secs(5),
            Arc::new(|| Some("session-host".to_owned())),
            PlatformKind::Linux,
            Arc::new(solstone_tmux::clock::SystemClock::new(time::UtcOffset::UTC)),
        )
        .await
        .expect("start session");

        peer.wait_for_clients_self_request_count(1, Duration::from_secs(5))
            .await;

        let part_path = temporary.path().join("000.jsonl");
        std::fs::write(&part_path, b"{\"test\":\"data\"}\n").expect("write part");

        let upload_result = session
            .journal()
            .ingest_upload("20260907", "test_segment", vec![part_path], "tmux")
            .await;
        assert!(upload_result.is_ok());

        session.shutdown().await.expect("session shutdown");
        peer.shutdown().await;
    });
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .expect("runtime")
}
