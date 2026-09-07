// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use solstone_tmux::clock::TestClock;
use solstone_tmux::instance_lock::InstanceLock;
use solstone_tmux::journal::JournalClient;
use solstone_tmux::journal_version::VersionRefreshState;
use solstone_tmux::paths::ensure_private_directory;
use solstone_tmux::post_connect::compute_pairing_generation;
use solstone_tmux::private_link::{PrivateLinkBridge, load_credential, persist_credential};
use solstone_tmux::relay_access::run_relay_access_job;
use solstone_tmux::sync::{CredentialStore, JournalSession, ReadyPublication};
use spl_transport::journal_bridge::CarrierOpener;
use spl_transport::validate_relay_origin;

mod support;
use support::TestDirectory;
use support::private_link_peer::PrivateLinkPeer;

static FAULT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn set_credential_write_fault(
    config_root: &Path,
    fault: Option<solstone_tmux::storage::AtomicWriteFault>,
) {
    solstone_tmux::storage::set_atomic_write_fault_for_path(
        &config_root.join("credentials.json"),
        fault,
    );
}

fn create_jwt_with_custom_claims(claims: serde_json::Value) -> String {
    let header = "eyJhbGciOiJFUzI1NiIsInR5cCI6IkpXVCJ9"; // {"alg":"ES256","typ":"JWT"}
    let claims_bytes = serde_json::to_vec(&claims).expect("json");
    let mut claims_b64 = String::new();
    // base64url encode without padding
    const B64_CHARS: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut i = 0;
    while i < claims_bytes.len() {
        let b0 = claims_bytes[i] as usize;
        let b1 = if i + 1 < claims_bytes.len() {
            claims_bytes[i + 1] as usize
        } else {
            0
        };
        let b2 = if i + 2 < claims_bytes.len() {
            claims_bytes[i + 2] as usize
        } else {
            0
        };

        claims_b64.push(B64_CHARS[b0 >> 2] as char);
        claims_b64.push(B64_CHARS[((b0 & 0x03) << 4) | (b1 >> 4)] as char);
        if i + 1 < claims_bytes.len() {
            claims_b64.push(B64_CHARS[((b1 & 0x0f) << 2) | (b2 >> 6)] as char);
        }
        if i + 2 < claims_bytes.len() {
            claims_b64.push(B64_CHARS[b2 & 0x3f] as char);
        }
        i += 3;
    }
    let signature = "fake_signature_part";
    format!("{header}.{claims_b64}.{signature}")
}

fn create_jwt(instance_id: &str, exp: i64) -> String {
    create_jwt_with_custom_claims(json!({
        "iss": "solstone",
        "sub": format!("instance:{instance_id}"),
        "aud": "spl-relay",
        "scope": "session.dial",
        "ver": 2,
        "instance_id": instance_id,
        "iat": 1700000000,
        "exp": exp,
        "jti": "jwt-id-12345"
    }))
}

#[test]
fn validate_relay_origin_enforces_url_scheme() {
    assert!(validate_relay_origin("https://relay.example.com").is_ok());
    assert!(validate_relay_origin("wss://relay.example.com").is_err());
    assert!(validate_relay_origin("not-a-url").is_err());
    assert!(validate_relay_origin("ftp://relay.example.com/path").is_err());
}

fn clock_at(unix_seconds: i64) -> TestClock {
    TestClock::new(
        time::OffsetDateTime::from_unix_timestamp(unix_seconds).expect("clock"),
        Duration::ZERO,
        time::UtcOffset::UTC,
    )
}

#[test]
fn relay_access_post_connect_job_redial_quiesces_then_external_redial_starts_one_burst() {
    runtime().block_on(async {
        let peer = PrivateLinkPeer::start().await;
        let temporary = TestDirectory::new("post-connect-burst-redial");
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
        let reported = json!({
            "name": "burst-host", "platform": "linux", "device_type": "terminal",
            "app_id": "solstone-tmux", "app_version": solstone_tmux::cli::version()
        });
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
                "reported": reported.clone(),
                "owner_label": null, "display_label": null, "updated_at": null,
                "journal": { "name": "test-journal", "version": "2026.8.0" }
            }))
            .expect("json"),
        );
        let token = create_jwt(&credential.instance_id, 1_900_608_000);
        peer.enqueue_relay_access_response(
            200,
            serde_json::to_vec(&json!({
                "status": "ready",
                "protocol_version": 2,
                "relay_origin": "https://relay.solstone.io",
                "instance_id": credential.instance_id,
                "device_token": token,
                "expires_at": "2030-03-24T18:40:00Z"
            }))
            .expect("json"),
        );

        peer.hold_clients_self();
        peer.hold_relay_access();
        let session = JournalSession::start_with(
            credential.clone(),
            config_root,
            refresh,
            Duration::from_secs(5),
            Arc::new(|| Some("burst-host".to_owned())),
            solstone_tmux::paths::PlatformKind::Linux,
            Arc::new(solstone_tmux::clock::SystemClock::new(time::UtcOffset::UTC)),
        )
        .await
        .expect("start session");
        peer.wait_for_clients_self_hold(Duration::from_secs(5))
            .await;
        peer.release_clients_self();
        peer.wait_for_relay_access_hold(Duration::from_secs(5))
            .await;

        // This is a job-induced dial while both lanes are active. It must not
        // start either optional lane again.
        session
            .opener()
            .dial_carrier()
            .await
            .expect("job-induced dial");
        peer.release_relay_access();
        session
            .wait_for_post_connect_quiescence(Duration::from_secs(5))
            .await;
        let initial_count = peer.request_count();
        assert_eq!(peer.clients_self_request_count(), 2);
        assert_eq!(peer.relay_access_request_count(), 1);
        assert_eq!(peer.system_status_request_count(), 0);

        // A carrier close after quiescence is external. The resulting dial
        // starts exactly one new shared burst, with no follow-up requested.
        peer.enqueue_clients_self_response(
            200,
            serde_json::to_vec(&json!({
                "protocol_version": 1,
                "revision": 2,
                "reported": reported,
                "owner_label": null, "display_label": null, "updated_at": null,
                "journal": { "name": "test-journal", "version": "2026.8.0" }
            }))
            .expect("json"),
        );
        peer.enqueue_relay_access_response(
            200,
            serde_json::to_vec(&json!({
                "status": "ready",
                "protocol_version": 2,
                "relay_origin": "https://relay.solstone.io",
                "instance_id": credential.instance_id,
                "device_token": create_jwt(&credential.instance_id, 1_900_608_000),
                "expires_at": "2030-03-24T18:40:00Z"
            }))
            .expect("json"),
        );
        peer.close_accepted_carriers();
        session
            .opener()
            .dial_carrier()
            .await
            .expect("external reconnect dial");
        peer.wait_for_request_count(initial_count + 2, Duration::from_secs(5))
            .await;
        session
            .wait_for_post_connect_quiescence(Duration::from_secs(5))
            .await;
        assert_eq!(peer.request_count(), initial_count + 2);
        assert_eq!(peer.clients_self_request_count(), 3);
        assert_eq!(peer.relay_access_request_count(), 2);

        session.shutdown().await.expect("shutdown session");
        peer.shutdown().await;
    });
}

#[test]
fn relay_access_ready_commits_credentials_and_updates_opener() {
    runtime().block_on(async {
        let peer = PrivateLinkPeer::start().await;
        let temporary = TestDirectory::new("relay-access-ready-commit");
        let config_root = temporary.path().join("config");
        let data_root = temporary.path().join("data");
        ensure_private_directory(&config_root).expect("config root");
        ensure_private_directory(&data_root).expect("data root");

        let lock = InstanceLock::acquire(&data_root).expect("acquire lock");
        let mut initial_cred = peer.credential();
        initial_cred.relay_origin = None;
        initial_cred.device_token = None;
        persist_credential(&config_root, &initial_cred).expect("persist initial cred");

        let refresh = VersionRefreshState::new(
            config_root.clone(),
            data_root.clone(),
            initial_cred.instance_id.clone(),
            &initial_cred.ca_fp_prefix,
            lock.identity().clone(),
        );

        let bridge = PrivateLinkBridge::start(initial_cred.clone(), None, refresh)
            .await
            .expect("start bridge");
        let opener = bridge.opener().clone();
        let client = JournalClient::bootstrap(&bridge)
            .await
            .expect("journal client");

        let pairing_gen = compute_pairing_generation(&initial_cred.client_cert_pem);
        let (store, _hook) =
            CredentialStore::new(config_root.clone(), initial_cred.clone(), pairing_gen);

        let expires_at_str = "2030-03-24T18:40:00Z";
        let exp = 1_900_608_000;
        let jwt = create_jwt(&initial_cred.instance_id, exp);
        let response_json = json!({
            "status": "ready",
            "protocol_version": 2,
            "relay_origin": "https://relay.solstone.io",
            "instance_id": initial_cred.instance_id,
            "device_token": jwt,
            "expires_at": expires_at_str
        });

        peer.enqueue_relay_access_response(200, serde_json::to_vec(&response_json).expect("json"));

        let clock = clock_at(1_700_000_000);
        let result =
            run_relay_access_job(&client, &store, &opener, 1, &clock, Duration::from_secs(5)).await;
        assert!(result.is_ok());

        let loaded = load_credential(&config_root)
            .expect("load credential")
            .expect("credential exists");
        assert_eq!(
            loaded.relay_origin.as_deref(),
            Some("https://relay.solstone.io")
        );
        assert_eq!(loaded.device_token.as_deref(), Some(jwt.as_str()));
        assert_eq!(loaded.device_token_expires_at, Some(exp));

        let live_cred = opener.live_dial_credential();
        assert_eq!(
            live_cred.relay_origin.as_deref(),
            Some("https://relay.solstone.io")
        );
        assert_eq!(live_cred.device_token.as_deref(), Some(jwt.as_str()));

        bridge.shutdown().await;
        peer.shutdown().await;
    });
}

#[test]
fn relay_access_not_configured_clears_relay_credentials() {
    runtime().block_on(async {
        let peer = PrivateLinkPeer::start().await;
        let temporary = TestDirectory::new("relay-access-not-configured-clear");
        let config_root = temporary.path().join("config");
        let data_root = temporary.path().join("data");
        ensure_private_directory(&config_root).expect("config root");
        ensure_private_directory(&data_root).expect("data root");

        let lock = InstanceLock::acquire(&data_root).expect("acquire lock");
        let mut initial_cred = peer.credential();
        initial_cred.relay_origin = Some("https://relay.old.solstone.io".to_owned());
        initial_cred.device_token = Some("old-token".to_owned());
        initial_cred.device_token_expires_at = Some(1700000000);
        persist_credential(&config_root, &initial_cred).expect("persist initial cred");

        let refresh = VersionRefreshState::new(
            config_root.clone(),
            data_root.clone(),
            initial_cred.instance_id.clone(),
            &initial_cred.ca_fp_prefix,
            lock.identity().clone(),
        );

        let bridge = PrivateLinkBridge::start(initial_cred.clone(), None, refresh)
            .await
            .expect("start bridge");
        let opener = bridge.opener().clone();
        let client = JournalClient::bootstrap(&bridge)
            .await
            .expect("journal client");

        let pairing_gen = compute_pairing_generation(&initial_cred.client_cert_pem);
        let (store, _hook) =
            CredentialStore::new(config_root.clone(), initial_cred.clone(), pairing_gen);

        let response_json = json!({
            "status": "not_configured",
            "protocol_version": 2
        });
        peer.enqueue_relay_access_response(200, serde_json::to_vec(&response_json).expect("json"));

        let result = run_relay_access_job(
            &client,
            &store,
            &opener,
            1,
            &clock_at(1_700_000_000),
            Duration::from_secs(5),
        )
        .await;
        assert!(result.is_ok());

        let live_cred = opener.live_dial_credential();
        assert_eq!(live_cred.relay_origin, None);
        assert_eq!(live_cred.device_token, None);
        opener
            .dial_carrier()
            .await
            .expect("LAN dial remains usable");

        bridge.shutdown().await;
        peer.shutdown().await;
    });
}

#[test]
fn relay_only_disable_blocks_the_next_dial_locally() {
    runtime().block_on(async {
        let peer = PrivateLinkPeer::start().await;
        let temporary = TestDirectory::new("relay-only-disable");
        let config_root = temporary.path().join("config");
        let data_root = temporary.path().join("data");
        ensure_private_directory(&config_root).expect("config root");
        ensure_private_directory(&data_root).expect("data root");
        let lock = InstanceLock::acquire(&data_root).expect("acquire lock");

        let mut credential = peer.credential();
        credential.endpoints.clear();
        credential.relay_origin = Some("https://relay.example.invalid".to_owned());
        credential.device_token = Some("relay-only-token".to_owned());
        persist_credential(&config_root, &credential).expect("persist credential");
        let refresh = VersionRefreshState::new(
            config_root.clone(),
            data_root,
            credential.instance_id.clone(),
            &credential.ca_fp_prefix,
            lock.identity().clone(),
        );
        let bridge = PrivateLinkBridge::start(credential.clone(), None, refresh)
            .await
            .expect("start bridge");
        let (store, _) = CredentialStore::new(
            config_root,
            credential.clone(),
            compute_pairing_generation(&credential.client_cert_pem),
        );

        store
            .submit_disable(Arc::clone(bridge.opener()), store.capture_access_attempt(1))
            .await
            .expect("disable relay-only credential");
        assert!(matches!(
            bridge.opener().dial_carrier().await,
            Err(spl_transport::TransportError::NoEndpoint)
        ));
        assert_eq!(peer.accepted_carriers(), 0);

        bridge.shutdown().await;
        peer.shutdown().await;
    });
}

#[test]
fn relay_access_stale_not_configured_cannot_clobber_newer_ready() {
    runtime().block_on(async {
        let peer = PrivateLinkPeer::start().await;
        let temporary = TestDirectory::new("relay-access-stale-disable");
        let config_root = temporary.path().join("config");
        let data_root = temporary.path().join("data");
        ensure_private_directory(&config_root).expect("config root");
        ensure_private_directory(&data_root).expect("data root");
        let lock = InstanceLock::acquire(&data_root).expect("acquire lock");
        let credential = peer.credential();
        persist_credential(&config_root, &credential).expect("persist credential");
        let refresh = VersionRefreshState::new(
            config_root.clone(),
            data_root,
            credential.instance_id.clone(),
            &credential.ca_fp_prefix,
            lock.identity().clone(),
        );
        let bridge = PrivateLinkBridge::start(credential.clone(), None, refresh)
            .await
            .expect("start bridge");
        let client = JournalClient::bootstrap(&bridge)
            .await
            .expect("journal client");
        let (store, _) = CredentialStore::new(
            config_root,
            credential.clone(),
            compute_pairing_generation(&credential.client_cert_pem),
        );

        peer.hold_relay_access();
        peer.enqueue_relay_access_response(
            200,
            serde_json::to_vec(&json!({
                "status": "not_configured",
                "protocol_version": 2
            }))
            .expect("json"),
        );
        let stale = tokio::spawn({
            let store = Arc::clone(&store);
            let opener = Arc::clone(bridge.opener());
            async move {
                run_relay_access_job(
                    &client,
                    &store,
                    &opener,
                    1,
                    &clock_at(1_700_000_000),
                    Duration::from_secs(5),
                )
                .await
            }
        });
        peer.wait_for_relay_access_hold(Duration::from_secs(5))
            .await;

        let exp = 1_900_608_000;
        let token = create_jwt(&credential.instance_id, exp);
        store
            .submit_ready(
                Arc::clone(bridge.opener()),
                store.capture_access_attempt(2),
                "https://relay.new.solstone.io".to_owned(),
                token.clone(),
                exp,
            )
            .await
            .expect("newer ready");
        peer.release_relay_access();
        assert!(stale.await.expect("stale task").is_err());

        let live = bridge.opener().live_dial_credential();
        assert_eq!(
            live.relay_origin.as_deref(),
            Some("https://relay.new.solstone.io")
        );
        assert_eq!(live.device_token.as_deref(), Some(token.as_str()));

        bridge.shutdown().await;
        peer.shutdown().await;
    });
}

#[test]
fn relay_access_shared_validation_rejections_preserve_live_access() {
    runtime().block_on(async {
        let peer = PrivateLinkPeer::start().await;
        let temporary = TestDirectory::new("relay-access-shared-validation");
        let config_root = temporary.path().join("config");
        let data_root = temporary.path().join("data");
        ensure_private_directory(&config_root).expect("config root");
        ensure_private_directory(&data_root).expect("data root");
        let lock = InstanceLock::acquire(&data_root).expect("acquire lock");
        let mut credential = peer.credential();
        let original_exp = 1_900_608_000;
        let original_token = create_jwt(&credential.instance_id, original_exp);
        credential.relay_origin = Some("https://relay.old.solstone.io".to_owned());
        credential.device_token = Some(original_token.clone());
        credential.device_token_expires_at = Some(original_exp);
        persist_credential(&config_root, &credential).expect("persist credential");
        let refresh = VersionRefreshState::new(
            config_root.clone(),
            data_root,
            credential.instance_id.clone(),
            &credential.ca_fp_prefix,
            lock.identity().clone(),
        );
        let bridge = PrivateLinkBridge::start(credential.clone(), None, refresh)
            .await
            .expect("start bridge");
        let client = JournalClient::bootstrap(&bridge)
            .await
            .expect("journal client");
        let (store, _) = CredentialStore::new(
            config_root,
            credential.clone(),
            compute_pairing_generation(&credential.client_cert_pem),
        );

        let future_iat_token = create_jwt_with_custom_claims(json!({
            "iss": "solstone",
            "sub": format!("instance:{}", credential.instance_id),
            "aud": "spl-relay",
            "scope": "session.dial",
            "ver": 2,
            "instance_id": credential.instance_id,
            "iat": 1_700_000_061,
            "exp": original_exp,
            "jti": "future-iat"
        }));
        let empty_jti_token = create_jwt_with_custom_claims(json!({
            "iss": "solstone",
            "sub": format!("instance:{}", credential.instance_id),
            "aud": "spl-relay",
            "scope": "session.dial",
            "ver": 2,
            "instance_id": credential.instance_id,
            "iat": 1_700_000_000,
            "exp": original_exp,
            "jti": ""
        }));
        let cases = [
            (
                create_jwt(&credential.instance_id, original_exp),
                "2030-03-24T18:40:00☃",
            ),
            (
                create_jwt(&credential.instance_id, original_exp),
                "2030-03-24T18:40:00.5Z",
            ),
            (empty_jti_token, "2030-03-24T18:40:00Z"),
            (future_iat_token, "2030-03-24T18:40:00Z"),
        ];
        let clock = clock_at(1_700_000_000);
        for (index, (token, expires_at)) in cases.into_iter().enumerate() {
            peer.enqueue_relay_access_response(
                200,
                serde_json::to_vec(&json!({
                    "status": "ready",
                    "protocol_version": 2,
                    "relay_origin": "https://relay.new.solstone.io",
                    "instance_id": credential.instance_id,
                    "device_token": token,
                    "expires_at": expires_at
                }))
                .expect("json"),
            );
            assert!(
                run_relay_access_job(
                    &client,
                    &store,
                    bridge.opener(),
                    u64::try_from(index).expect("attempt index") + 1,
                    &clock,
                    Duration::from_secs(5),
                )
                .await
                .is_err()
            );
            let live = bridge.opener().live_dial_credential();
            assert_eq!(
                live.relay_origin.as_deref(),
                Some("https://relay.old.solstone.io")
            );
            assert_eq!(live.device_token.as_deref(), Some(original_token.as_str()));
        }

        bridge.shutdown().await;
        peer.shutdown().await;
    });
}

#[test]
fn relay_access_fault_before_rename_does_not_mutate_live() {
    let _fault_guard = FAULT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    runtime().block_on(async {
        let peer = PrivateLinkPeer::start().await;
        let temporary = TestDirectory::new("relay-access-fault-before-rename");
        let config_root = temporary.path().join("config");
        let data_root = temporary.path().join("data");
        ensure_private_directory(&config_root).expect("config root");
        ensure_private_directory(&data_root).expect("data root");

        let lock = InstanceLock::acquire(&data_root).expect("acquire lock");
        let mut initial_cred = peer.credential();
        initial_cred.relay_origin = None;
        initial_cred.device_token = None;
        persist_credential(&config_root, &initial_cred).expect("persist initial cred");

        let refresh = VersionRefreshState::new(
            config_root.clone(),
            data_root.clone(),
            initial_cred.instance_id.clone(),
            &initial_cred.ca_fp_prefix,
            lock.identity().clone(),
        );

        let bridge = PrivateLinkBridge::start(initial_cred.clone(), None, refresh)
            .await
            .expect("start bridge");
        let opener = bridge.opener().clone();
        let _client = JournalClient::bootstrap(&bridge)
            .await
            .expect("journal client");

        let pairing_gen = compute_pairing_generation(&initial_cred.client_cert_pem);
        let (store, _hook) =
            CredentialStore::new(config_root.clone(), initial_cred.clone(), pairing_gen);

        let exp = 1_900_608_000;
        let jwt = create_jwt(&initial_cred.instance_id, exp);

        // Inject fault before rename
        set_credential_write_fault(
            &config_root,
            Some(solstone_tmux::storage::AtomicWriteFault::FailBeforeRename),
        );

        let result = store
            .submit_ready(
                Arc::clone(&opener),
                store.capture_access_attempt(1),
                "https://relay.solstone.io".to_owned(),
                jwt.clone(),
                exp,
            )
            .await;
        set_credential_write_fault(&config_root, None);
        assert!(result.is_err());

        // CredentialStore should remain unchanged
        let cred = store.live_credential();
        assert_eq!(cred.relay_origin, None);
        assert_eq!(cred.device_token, None);
        let on_disk = load_credential(&config_root)
            .expect("load credential")
            .expect("credential exists");
        assert_eq!(on_disk.relay_origin, None);
        assert_eq!(on_disk.device_token, None);
        assert_eq!(opener.live_dial_credential().relay_origin, None);
        assert_eq!(opener.live_dial_credential().device_token, None);

        bridge.shutdown().await;
        peer.shutdown().await;
    });
}

#[test]
fn relay_access_integrates_with_journal_session() {
    runtime().block_on(async {
        let peer = PrivateLinkPeer::start().await;
        let temporary = TestDirectory::new("relay-access-session-test");
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

        let expires_at_str = "2030-03-24T18:40:00Z";
        let exp = 1_900_608_000;
        let jwt = create_jwt(&credential.instance_id, exp);
        let response_json = json!({
            "status": "ready",
            "protocol_version": 2,
            "relay_origin": "https://relay.solstone.io",
            "instance_id": credential.instance_id,
            "device_token": jwt,
            "expires_at": expires_at_str
        });
        peer.enqueue_relay_access_response(200, serde_json::to_vec(&response_json).expect("json"));

        let session = solstone_tmux::sync::JournalSession::start_with(
            credential.clone(),
            config_root.clone(),
            refresh,
            Duration::from_secs(5),
            Arc::new(|| Some("session-host".to_owned())),
            solstone_tmux::paths::PlatformKind::Linux,
            Arc::new(solstone_tmux::clock::SystemClock::new(time::UtcOffset::UTC)),
        )
        .await
        .expect("start session");

        session
            .wait_for_post_connect_quiescence(Duration::from_secs(5))
            .await;

        let live_cred = session.opener().live_dial_credential();
        assert_eq!(
            live_cred.relay_origin.as_deref(),
            Some("https://relay.solstone.io")
        );
        assert_eq!(live_cred.device_token.as_deref(), Some(jwt.as_str()));

        // Enqueue not_configured and test manual trigger_post_connect()
        let not_conf_json = json!({
            "status": "not_configured",
            "protocol_version": 2
        });
        peer.enqueue_relay_access_response(200, serde_json::to_vec(&not_conf_json).expect("json"));

        session.trigger_post_connect();
        session
            .wait_for_post_connect_quiescence(Duration::from_secs(5))
            .await;

        let cleared_cred = session.opener().live_dial_credential();
        assert_eq!(cleared_cred.relay_origin, None);
        assert_eq!(cleared_cred.device_token, None);

        session.shutdown().await.expect("session shutdown");
        peer.shutdown().await;
    });
}

#[test]
fn relay_access_redirect_is_refused() {
    runtime().block_on(async {
        let peer = PrivateLinkPeer::start().await;
        let temporary = TestDirectory::new("relay-access-redirect");
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

        // Enqueue 301 redirect
        peer.enqueue_relay_access_response(301, Vec::new());

        let session = solstone_tmux::sync::JournalSession::start_with(
            credential,
            config_root,
            refresh,
            Duration::from_secs(5),
            Arc::new(|| Some("session-host".to_owned())),
            solstone_tmux::paths::PlatformKind::Linux,
            Arc::new(solstone_tmux::clock::SystemClock::new(time::UtcOffset::UTC)),
        )
        .await
        .expect("start session");

        peer.wait_for_relay_access_request_count(1, Duration::from_secs(5))
            .await;

        let requests = peer
            .requests()
            .into_iter()
            .filter(|r| {
                r.path_without_query()
                    .starts_with("/app/network/api/relay/access")
                    || r.path_without_query().starts_with("/redirected")
            })
            .collect::<Vec<_>>();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].path_without_query(),
            "/app/network/api/relay/access"
        );

        session.shutdown().await.expect("session shutdown");
        peer.shutdown().await;
    });
}

#[test]
fn relay_access_404_and_503_preserve_existing_relay_cache() {
    runtime().block_on(async {
        let peer = PrivateLinkPeer::start().await;
        let temporary = TestDirectory::new("relay-access-preserve-cache");
        let config_root = temporary.path().join("config");
        let data_root = temporary.path().join("data");
        ensure_private_directory(&config_root).expect("config root");
        ensure_private_directory(&data_root).expect("data root");
        let lock = InstanceLock::acquire(&data_root).expect("acquire lock");

        let mut credential = peer.credential();
        let exp = 1900000000i64;
        let jwt = create_jwt(&credential.instance_id, exp);
        credential.relay_origin = Some("https://relay.solstone.io".to_owned());
        credential.device_token = Some(jwt.clone());
        credential.device_token_expires_at = Some(exp);
        persist_credential(&config_root, &credential).expect("persist initial cred");

        let refresh = VersionRefreshState::new(
            config_root.clone(),
            data_root.clone(),
            credential.instance_id.clone(),
            &credential.ca_fp_prefix,
            lock.identity().clone(),
        );

        // Enqueue 404
        peer.enqueue_relay_access_response(404, Vec::new());

        let session = solstone_tmux::sync::JournalSession::start_with(
            credential.clone(),
            config_root.clone(),
            refresh,
            Duration::from_secs(5),
            Arc::new(|| Some("session-host".to_owned())),
            solstone_tmux::paths::PlatformKind::Linux,
            Arc::new(solstone_tmux::clock::SystemClock::new(time::UtcOffset::UTC)),
        )
        .await
        .expect("start session");

        session
            .wait_for_post_connect_quiescence(Duration::from_secs(5))
            .await;

        let live = session.opener().live_dial_credential();
        assert_eq!(
            live.relay_origin.as_deref(),
            Some("https://relay.solstone.io")
        );
        assert_eq!(live.device_token.as_deref(), Some(jwt.as_str()));
        let on_disk = load_credential(&config_root)
            .expect("load disk")
            .expect("cred exists");
        assert_eq!(
            on_disk.relay_origin.as_deref(),
            Some("https://relay.solstone.io")
        );
        assert_eq!(on_disk.device_token.as_deref(), Some(jwt.as_str()));

        // Enqueue 503 and trigger again
        peer.enqueue_relay_access_response(503, Vec::new());
        session.trigger_post_connect();
        session
            .wait_for_post_connect_quiescence(Duration::from_secs(5))
            .await;

        let live_after = session.opener().live_dial_credential();
        assert_eq!(
            live_after.relay_origin.as_deref(),
            Some("https://relay.solstone.io")
        );
        assert_eq!(live_after.device_token.as_deref(), Some(jwt.as_str()));
        let on_disk_after = load_credential(&config_root)
            .expect("load disk")
            .expect("cred exists");
        assert_eq!(
            on_disk_after.relay_origin.as_deref(),
            Some("https://relay.solstone.io")
        );
        assert_eq!(on_disk_after.device_token.as_deref(), Some(jwt.as_str()));

        session.shutdown().await.expect("session shutdown");
        peer.shutdown().await;
    });
}

#[test]
fn relay_access_jwt_extra_claims_and_instance_mismatch_preserve_cache() {
    runtime().block_on(async {
        let peer = PrivateLinkPeer::start().await;
        let temporary = TestDirectory::new("relay-access-jwt-claims");
        let config_root = temporary.path().join("config");
        let data_root = temporary.path().join("data");
        ensure_private_directory(&config_root).expect("config root");
        ensure_private_directory(&data_root).expect("data root");
        let lock = InstanceLock::acquire(&data_root).expect("acquire lock");

        let mut credential = peer.credential();
        let exp = 1900000000i64;
        let original_jwt = create_jwt(&credential.instance_id, exp);
        credential.relay_origin = Some("https://relay.solstone.io".to_owned());
        credential.device_token = Some(original_jwt.clone());
        credential.device_token_expires_at = Some(exp);
        persist_credential(&config_root, &credential).expect("persist initial cred");

        let refresh = VersionRefreshState::new(
            config_root.clone(),
            data_root.clone(),
            credential.instance_id.clone(),
            &credential.ca_fp_prefix,
            lock.identity().clone(),
        );

        // Enqueue JWT with extra claim "device_fp"
        let extra_claims_jwt = create_jwt_with_custom_claims(json!({
            "iss": "solstone",
            "sub": format!("instance:{}", credential.instance_id),
            "aud": "spl-relay",
            "scope": "session.dial",
            "ver": 2,
            "instance_id": credential.instance_id,
            "device_fp": "extra_claim_not_allowed",
            "iat": 1700000000,
            "exp": exp,
            "jti": "jwt-id-12345"
        }));
        let bad_response_1 = json!({
            "status": "ready",
            "protocol_version": 2,
            "relay_origin": "https://new-relay.solstone.io",
            "instance_id": credential.instance_id,
            "device_token": extra_claims_jwt,
            "expires_at": "2030-03-24T18:40:00Z"
        });
        peer.enqueue_relay_access_response(200, serde_json::to_vec(&bad_response_1).expect("json"));

        let session = solstone_tmux::sync::JournalSession::start_with(
            credential.clone(),
            config_root.clone(),
            refresh,
            Duration::from_secs(5),
            Arc::new(|| Some("session-host".to_owned())),
            solstone_tmux::paths::PlatformKind::Linux,
            Arc::new(solstone_tmux::clock::SystemClock::new(time::UtcOffset::UTC)),
        )
        .await
        .expect("start session");

        session
            .wait_for_post_connect_quiescence(Duration::from_secs(5))
            .await;

        let live = session.opener().live_dial_credential();
        assert_eq!(
            live.relay_origin.as_deref(),
            Some("https://relay.solstone.io")
        );
        assert_eq!(live.device_token.as_deref(), Some(original_jwt.as_str()));

        // Enqueue instance_id mismatch
        let mismatch_jwt = create_jwt("mismatched-instance-id", exp);
        let bad_response_2 = json!({
            "status": "ready",
            "protocol_version": 2,
            "relay_origin": "https://new-relay.solstone.io",
            "instance_id": "mismatched-instance-id",
            "device_token": mismatch_jwt,
            "expires_at": "2030-03-24T18:40:00Z"
        });
        peer.enqueue_relay_access_response(200, serde_json::to_vec(&bad_response_2).expect("json"));
        session.trigger_post_connect();
        session
            .wait_for_post_connect_quiescence(Duration::from_secs(5))
            .await;

        let live_after = session.opener().live_dial_credential();
        assert_eq!(
            live_after.relay_origin.as_deref(),
            Some("https://relay.solstone.io")
        );
        assert_eq!(
            live_after.device_token.as_deref(),
            Some(original_jwt.as_str())
        );

        session.shutdown().await.expect("session shutdown");
        peer.shutdown().await;
    });
}

#[test]
fn relay_access_fault_after_rename_is_durability_uncertain() {
    let _fault_guard = FAULT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    runtime().block_on(async {
        let peer = PrivateLinkPeer::start().await;
        let temporary = TestDirectory::new("relay-access-fault-after-rename");
        let config_root = temporary.path().join("config");
        let data_root = temporary.path().join("data");
        ensure_private_directory(&config_root).expect("config root");
        ensure_private_directory(&data_root).expect("data root");

        let initial_cred = peer.credential();
        persist_credential(&config_root, &initial_cred).expect("persist initial cred");
        let lock = InstanceLock::acquire(&data_root).expect("acquire lock");
        let refresh = VersionRefreshState::new(
            config_root.clone(),
            data_root.clone(),
            initial_cred.instance_id.clone(),
            &initial_cred.ca_fp_prefix,
            lock.identity().clone(),
        );
        let bridge = PrivateLinkBridge::start(initial_cred.clone(), None, refresh)
            .await
            .expect("start bridge");
        let opener = bridge.opener().clone();

        let pairing_gen = compute_pairing_generation(&initial_cred.client_cert_pem);
        let (store, _hook) =
            CredentialStore::new(config_root.clone(), initial_cred.clone(), pairing_gen);

        let exp = 1900000000i64;
        let jwt = create_jwt(&initial_cred.instance_id, exp);

        // Inject fault after rename
        set_credential_write_fault(
            &config_root,
            Some(solstone_tmux::storage::AtomicWriteFault::FailAfterRename),
        );

        let result = store
            .submit_ready(
                Arc::clone(&opener),
                store.capture_access_attempt(1),
                "https://relay.solstone.io".to_owned(),
                jwt.clone(),
                exp,
            )
            .await;
        set_credential_write_fault(&config_root, None);
        assert_eq!(result, Ok(ReadyPublication::Uncertain));

        // On disk, the rename succeeded so credentials.json has the new bytes
        let on_disk = load_credential(&config_root)
            .expect("load disk")
            .expect("cred exists");
        assert_eq!(
            on_disk.relay_origin.as_deref(),
            Some("https://relay.solstone.io")
        );
        assert_eq!(on_disk.device_token.as_deref(), Some(jwt.as_str()));

        // Store re-read disk on atomic_write error: if on-disk matches updated, store adopts it;
        // LAN endpoints remain valid
        let live = store.live_credential();
        assert_eq!(live.client_cert_pem, initial_cred.client_cert_pem);
        assert_eq!(live.local_endpoints, initial_cred.local_endpoints);
        assert_eq!(opener.live_dial_credential().device_token, Some(jwt));

        bridge.shutdown().await;
        peer.shutdown().await;
    });
}

#[test]
fn relay_access_durable_clear_retry_does_not_clobber_newer_ready() {
    let _fault_guard = FAULT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    runtime().block_on(async {
        let peer = PrivateLinkPeer::start().await;
        let temporary = TestDirectory::new("relay-access-durable-clear-retry");
        let config_root = temporary.path().join("config");
        let data_root = temporary.path().join("data");
        ensure_private_directory(&config_root).expect("config root");
        ensure_private_directory(&data_root).expect("data root");

        let mut initial_cred = peer.credential();
        let exp = 1900000000i64;
        let jwt = create_jwt(&initial_cred.instance_id, exp);
        initial_cred.relay_origin = Some("https://relay.solstone.io".to_owned());
        initial_cred.device_token = Some(jwt.clone());
        persist_credential(&config_root, &initial_cred).expect("persist initial cred");
        let lock = InstanceLock::acquire(&data_root).expect("acquire lock");
        let refresh = VersionRefreshState::new(
            config_root.clone(),
            data_root.clone(),
            initial_cred.instance_id.clone(),
            &initial_cred.ca_fp_prefix,
            lock.identity().clone(),
        );
        let bridge = PrivateLinkBridge::start(initial_cred.clone(), None, refresh)
            .await
            .expect("start bridge");
        let opener = bridge.opener().clone();

        let pairing_gen = compute_pairing_generation(&initial_cred.client_cert_pem);
        let (store, _hook) =
            CredentialStore::new(config_root.clone(), initial_cred.clone(), pairing_gen);

        // Fault before rename makes durable clear fail
        set_credential_write_fault(
            &config_root,
            Some(solstone_tmux::storage::AtomicWriteFault::FailBeforeRename),
        );
        assert!(
            store
                .submit_disable(Arc::clone(&opener), store.capture_access_attempt(1))
                .await
                .is_ok()
        );
        set_credential_write_fault(&config_root, None);

        // Commit newer ready access
        let newer_exp = 1950000000i64;
        let newer_jwt = create_jwt(&initial_cred.instance_id, newer_exp);
        assert!(
            store
                .submit_ready(
                    Arc::clone(&opener),
                    store.capture_access_attempt(1),
                    "https://newer-relay.solstone.io".to_owned(),
                    newer_jwt.clone(),
                    newer_exp,
                )
                .await
                .is_ok()
        );

        // Retry durable clear if pending - must NOT clobber the newer ready credential
        store.retry_durable_clear_if_pending().await;

        let live = store.live_credential();
        assert_eq!(
            live.relay_origin.as_deref(),
            Some("https://newer-relay.solstone.io")
        );
        assert_eq!(live.device_token.as_deref(), Some(newer_jwt.as_str()));

        let on_disk = load_credential(&config_root)
            .expect("load disk")
            .expect("cred exists");
        assert_eq!(
            on_disk.relay_origin.as_deref(),
            Some("https://newer-relay.solstone.io")
        );
        assert_eq!(on_disk.device_token.as_deref(), Some(newer_jwt.as_str()));

        bridge.shutdown().await;
        peer.shutdown().await;
    });
}

#[test]
fn relay_access_stale_hook_cannot_undo_disable() {
    let _fault_guard = FAULT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    runtime().block_on(async {
        let peer = PrivateLinkPeer::start().await;
        let temporary = TestDirectory::new("relay-access-stale-hook");
        let config_root = temporary.path().join("config");
        let data_root = temporary.path().join("data");
        ensure_private_directory(&config_root).expect("config root");
        ensure_private_directory(&data_root).expect("data root");

        let mut initial_cred = peer.credential();
        let exp = 1900000000i64;
        let jwt = create_jwt(&initial_cred.instance_id, exp);
        initial_cred.relay_origin = Some("https://relay.solstone.io".to_owned());
        initial_cred.device_token = Some(jwt.clone());
        persist_credential(&config_root, &initial_cred).expect("persist initial cred");

        let lock = InstanceLock::acquire(&data_root).expect("acquire lock");
        let refresh = VersionRefreshState::new(
            config_root.clone(),
            data_root.clone(),
            initial_cred.instance_id.clone(),
            &initial_cred.ca_fp_prefix,
            lock.identity().clone(),
        );
        let bridge = PrivateLinkBridge::start(initial_cred.clone(), None, refresh)
            .await
            .expect("start bridge");
        let pairing_gen = compute_pairing_generation(&initial_cred.client_cert_pem);
        let (store, old_hook) =
            CredentialStore::new(config_root.clone(), initial_cred.clone(), pairing_gen);

        store
            .submit_disable(Arc::clone(bridge.opener()), store.capture_access_attempt(1))
            .await
            .expect("durable clear");

        // Stale hook attempts to save a new token
        old_hook("stale-token", exp);

        // Persist pending / shutdown
        store.persist_pending().await.expect("persist pending");

        let live = store.live_credential();
        assert_eq!(live.relay_origin, None);
        assert_eq!(live.device_token, None);

        let on_disk = load_credential(&config_root)
            .expect("load disk")
            .expect("cred exists");
        assert_eq!(on_disk.relay_origin, None);
        assert_eq!(on_disk.device_token, None);

        bridge.shutdown().await;
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
