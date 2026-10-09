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
use solstone_tmux::relay_access::{run_access_lane, run_dial_address_job, run_relay_access_job};
use solstone_tmux::sync::{CredentialStore, JournalSession, ReadyPublication};
use spl_transport::credential::EndpointAddr;
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
                "owner_label": null, "display_label": "test-device", "updated_at": null,
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
                "owner_label": null, "display_label": "test-device", "updated_at": null,
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
            Arc::new(solstone_tmux::clock::SystemClock::utc()),
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
                "owner_label": null, "display_label": "test-device", "updated_at": null,
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
        peer.wait_for_request_count(initial_count + 3, Duration::from_secs(5))
            .await;
        session
            .wait_for_post_connect_quiescence(Duration::from_secs(5))
            .await;
        // One optional About read joins the existing metadata/relay burst.
        assert_eq!(peer.request_count(), initial_count + 3);
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
        let accepted_before = peer.accepted_carriers();
        opener
            .dial_carrier()
            .await
            .expect("next dial keeps the original LAN adapter");
        assert!(peer.accepted_carriers() > accepted_before);

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
            Arc::new(solstone_tmux::clock::SystemClock::utc()),
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
            Arc::new(solstone_tmux::clock::SystemClock::utc()),
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
            Arc::new(solstone_tmux::clock::SystemClock::utc()),
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
            Arc::new(solstone_tmux::clock::SystemClock::utc()),
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
        assert_eq!(
            store.persistence_issue(),
            Some(solstone_tmux::sync::CredentialPersistenceIssue::Uncertain)
        );

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
        let accepted_before = peer.accepted_carriers();
        opener
            .dial_carrier()
            .await
            .expect("next dial uses the accepted replacement adapter");
        assert!(peer.accepted_carriers() > accepted_before);

        let client = JournalClient::bootstrap(&bridge).await.expect("bootstrap");
        peer.enqueue_relay_access_response(503, Vec::new());
        assert!(
            run_relay_access_job(
                &client,
                &store,
                &opener,
                2,
                &clock_at(1_800_000_000),
                Duration::from_secs(5)
            )
            .await
            .is_err()
        );
        assert_eq!(
            store.persistence_issue(),
            None,
            "a later access trigger retries uncertain publication even when its GET fails"
        );
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
                .is_err()
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
fn relay_access_failed_clear_survives_shutdown_and_retries() {
    let _fault_guard = FAULT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    runtime().block_on(async {
        let peer = PrivateLinkPeer::start().await;
        let temporary = TestDirectory::new("relay-access-shutdown-clear-retry");
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
                .is_err()
        );
        set_credential_write_fault(&config_root, None);

        assert_eq!(
            store.persistence_issue(),
            Some(solstone_tmux::sync::CredentialPersistenceIssue::Failed)
        );
        assert!(opener.live_dial_credential().device_token.is_none());
        assert!(
            load_credential(&config_root)
                .unwrap()
                .unwrap()
                .device_token
                .is_some()
        );
        store.invalidate();
        store
            .persist_pending()
            .await
            .expect("shutdown retries accepted disable");
        assert_eq!(store.persistence_issue(), None);
        assert!(
            load_credential(&config_root)
                .unwrap()
                .unwrap()
                .device_token
                .is_none()
        );
        assert!(store.live_credential().device_token.is_none());

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

#[test]
fn shutdown_retires_an_already_blocked_dial() {
    runtime().block_on(async {
        let peer = PrivateLinkPeer::start().await;
        let temporary = TestDirectory::new("shutdown-blocked-dial");
        let config_root = temporary.path().join("config");
        let data_root = temporary.path().join("data");
        ensure_private_directory(&config_root).unwrap();
        ensure_private_directory(&data_root).unwrap();
        let credential = peer.credential();
        persist_credential(&config_root, &credential).unwrap();
        let lock = InstanceLock::acquire(&data_root).unwrap();
        let refresh = VersionRefreshState::new(
            config_root,
            data_root,
            credential.instance_id.clone(),
            &credential.ca_fp_prefix,
            lock.identity().clone(),
        );
        let bridge = PrivateLinkBridge::start(credential, None, refresh)
            .await
            .unwrap();
        let opener = Arc::clone(bridge.opener());
        peer.hold_handshake();
        let old_opener = Arc::clone(&opener);
        let dial = tokio::spawn(async move { old_opener.dial_carrier().await });
        peer.wait_for_held_handshake(Duration::from_secs(5)).await;
        bridge.shutdown().await;
        peer.release_handshake();
        assert!(
            tokio::time::timeout(Duration::from_secs(5), dial)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
        assert!(opener.dial_carrier().await.is_err());
        peer.shutdown().await;
    });
}

#[test]
fn relay_access_dial_address_mixed_set_queries_loopback_only_skips() {
    runtime().block_on(async {
        let peer = PrivateLinkPeer::start().await;
        let temporary = TestDirectory::new("dial-address-mixed-vs-loopback");
        let config_root = temporary.path().join("config");
        let data_root = temporary.path().join("data");
        ensure_private_directory(&config_root).expect("config root");
        ensure_private_directory(&data_root).expect("data root");
        let lock = InstanceLock::acquire(&data_root).expect("acquire lock");

        let loopback_cred = peer.credential();
        persist_credential(&config_root, &loopback_cred).expect("persist cred");
        let refresh = VersionRefreshState::new(
            config_root.clone(),
            data_root.clone(),
            loopback_cred.instance_id.clone(),
            &loopback_cred.ca_fp_prefix,
            lock.identity().clone(),
        );
        let bridge = PrivateLinkBridge::start(loopback_cred.clone(), None, refresh)
            .await
            .expect("start bridge");
        let client = JournalClient::bootstrap(&bridge)
            .await
            .expect("bootstrap client");
        let pairing_gen = compute_pairing_generation(&loopback_cred.client_cert_pem);
        let (store, _) =
            CredentialStore::new(config_root.clone(), loopback_cred.clone(), pairing_gen);

        let res =
            run_dial_address_job(&client, &store, bridge.opener(), Duration::from_secs(5)).await;
        assert!(res.is_ok());
        assert_eq!(peer.local_endpoints_request_count(), 0);

        let lane_res = run_access_lane(
            &client,
            &store,
            bridge.opener(),
            1,
            &clock_at(1_700_000_000),
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(peer.local_endpoints_request_count(), 0);
        assert!(lane_res.addresses.is_ok());
        bridge.shutdown().await;

        let mut mixed_cred = peer.credential();
        mixed_cred.endpoints.push(EndpointAddr {
            host: "192.0.2.9".to_owned(),
            port: 7657,
        });
        persist_credential(&config_root, &mixed_cred).expect("persist mixed cred");
        let refresh = VersionRefreshState::new(
            config_root.clone(),
            data_root,
            mixed_cred.instance_id.clone(),
            &mixed_cred.ca_fp_prefix,
            lock.identity().clone(),
        );
        let bridge = PrivateLinkBridge::start(mixed_cred.clone(), None, refresh)
            .await
            .expect("start bridge");
        let client = JournalClient::bootstrap(&bridge)
            .await
            .expect("bootstrap client");
        let pairing_gen = compute_pairing_generation(&mixed_cred.client_cert_pem);
        let (store, _) = CredentialStore::new(config_root.clone(), mixed_cred.clone(), pairing_gen);

        peer.enqueue_local_endpoints_response(
            200,
            serde_json::to_vec(&json!({
                "v": 2,
                "endpoints": [{"ip": "192.0.2.9", "port": 7657}]
            }))
            .expect("json"),
        );
        let res =
            run_dial_address_job(&client, &store, bridge.opener(), Duration::from_secs(5)).await;
        assert!(res.is_ok());
        assert_eq!(peer.local_endpoints_request_count(), 1);

        peer.enqueue_relay_access_response(
            200,
            serde_json::to_vec(&json!({
                "status": "not_configured",
                "protocol_version": 2
            }))
            .expect("json"),
        );
        peer.enqueue_local_endpoints_response(
            200,
            serde_json::to_vec(&json!({
                "v": 2,
                "endpoints": [{"ip": "192.0.2.9", "port": 7657}]
            }))
            .expect("json"),
        );
        let lane_res = run_access_lane(
            &client,
            &store,
            bridge.opener(),
            2,
            &clock_at(1_700_000_000),
            Duration::from_secs(5),
        )
        .await;
        assert!(lane_res.addresses.is_ok());
        assert_eq!(peer.local_endpoints_request_count(), 2);

        bridge.shutdown().await;
        peer.shutdown().await;
    });
}

#[test]
fn relay_access_dial_address_empty_set_via_relay_server_stores_v2_endpoints() {
    runtime().block_on(async {
        let peer = PrivateLinkPeer::start().await;
        let relay_server = peer.start_relay_server().await;
        let temporary = TestDirectory::new("dial-address-empty-relay-cred");
        let config_root = temporary.path().join("config");
        let data_root = temporary.path().join("data");
        ensure_private_directory(&config_root).expect("config root");
        ensure_private_directory(&data_root).expect("data root");
        let lock = InstanceLock::acquire(&data_root).expect("acquire lock");

        let mut initial_cred = peer.relay_credential(relay_server.origin());
        initial_cred.local_endpoints = Some(json!({"distinctive_field": "test-empty-set"}));
        persist_credential(&config_root, &initial_cred).expect("persist initial cred");

        let refresh = VersionRefreshState::new(
            config_root.clone(),
            data_root,
            initial_cred.instance_id.clone(),
            &initial_cred.ca_fp_prefix,
            lock.identity().clone(),
        );
        let bridge = PrivateLinkBridge::start(initial_cred.clone(), None, refresh)
            .await
            .expect("start bridge");
        let client = JournalClient::bootstrap(&bridge)
            .await
            .expect("bootstrap client");
        let pairing_gen = compute_pairing_generation(&initial_cred.client_cert_pem);
        let (store, _) =
            CredentialStore::new(config_root.clone(), initial_cred.clone(), pairing_gen);

        peer.enqueue_local_endpoints_response(
            200,
            serde_json::to_vec(&json!({
                "v": 2,
                "endpoints": [{"ip": "192.0.2.9", "port": 7657}]
            }))
            .expect("json"),
        );

        let res =
            run_dial_address_job(&client, &store, bridge.opener(), Duration::from_secs(5)).await;
        assert!(res.is_ok());
        assert_eq!(peer.local_endpoints_request_count(), 1);

        let loaded = load_credential(&config_root)
            .expect("load credential")
            .expect("credential exists");
        let live = bridge.opener().live_dial_credential();
        let expected_endpoints = vec![EndpointAddr {
            host: "192.0.2.9".to_owned(),
            port: 7657,
        }];
        assert_eq!(loaded.endpoints, expected_endpoints);
        assert_eq!(live.endpoints, expected_endpoints);
        assert_eq!(loaded.local_endpoints, initial_cred.local_endpoints);
        assert_eq!(live.local_endpoints, initial_cred.local_endpoints);

        bridge.shutdown().await;
        peer.shutdown().await;
    });
}

#[test]
fn relay_access_dial_address_error_responses_preserve_credentials() {
    runtime().block_on(async {
        let peer = PrivateLinkPeer::start().await;
        let temporary = TestDirectory::new("dial-address-error-responses");
        let config_root = temporary.path().join("config");
        let data_root = temporary.path().join("data");
        ensure_private_directory(&config_root).expect("config root");
        ensure_private_directory(&data_root).expect("data root");
        let lock = InstanceLock::acquire(&data_root).expect("acquire lock");

        let mut initial_cred = peer.credential();
        initial_cred.endpoints.push(EndpointAddr {
            host: "192.0.2.9".to_owned(),
            port: 7657,
        });
        initial_cred.local_endpoints = Some(json!({"distinctive_field": "errors-preserve"}));
        persist_credential(&config_root, &initial_cred).expect("persist initial cred");

        let refresh = VersionRefreshState::new(
            config_root.clone(),
            data_root,
            initial_cred.instance_id.clone(),
            &initial_cred.ca_fp_prefix,
            lock.identity().clone(),
        );
        let bridge = PrivateLinkBridge::start(initial_cred.clone(), None, refresh)
            .await
            .expect("start bridge");
        let client = JournalClient::bootstrap(&bridge)
            .await
            .expect("bootstrap client");
        let pairing_gen = compute_pairing_generation(&initial_cred.client_cert_pem);
        let (store, _) =
            CredentialStore::new(config_root.clone(), initial_cred.clone(), pairing_gen);

        let cred_path = config_root.join("credentials.json");
        let initial_bytes = std::fs::read(&cred_path).expect("read cred bytes");
        let initial_endpoints = initial_cred.endpoints.clone();

        peer.enqueue_raw_local_endpoints_response(
            b"HTTP/1.1 500 Internal Server Error\r\nContent-Type: text/plain\r\nContent-Length: 5\r\n\r\nerror".to_vec(),
        );
        assert!(
            run_dial_address_job(&client, &store, bridge.opener(), Duration::from_secs(5))
                .await
                .is_err()
        );
        assert_eq!(peer.local_endpoints_request_count(), 1);
        assert_eq!(std::fs::read(&cred_path).expect("read bytes"), initial_bytes);
        assert_eq!(
            bridge.opener().live_dial_credential().endpoints,
            initial_endpoints
        );

        peer.enqueue_local_endpoints_response(302, Vec::new());
        assert!(
            run_dial_address_job(&client, &store, bridge.opener(), Duration::from_secs(5))
                .await
                .is_err()
        );
        assert_eq!(peer.local_endpoints_request_count(), 2);
        assert_eq!(std::fs::read(&cred_path).expect("read bytes"), initial_bytes);
        assert_eq!(
            bridge.opener().live_dial_credential().endpoints,
            initial_endpoints
        );

        peer.enqueue_local_endpoints_response(404, Vec::new());
        assert!(
            run_dial_address_job(&client, &store, bridge.opener(), Duration::from_secs(5))
                .await
                .is_err()
        );
        assert_eq!(peer.local_endpoints_request_count(), 3);
        assert_eq!(std::fs::read(&cred_path).expect("read bytes"), initial_bytes);
        assert_eq!(
            bridge.opener().live_dial_credential().endpoints,
            initial_endpoints
        );

        peer.enqueue_local_endpoints_response(200, b"not json at all".to_vec());
        assert!(
            run_dial_address_job(&client, &store, bridge.opener(), Duration::from_secs(5))
                .await
                .is_err()
        );
        assert_eq!(peer.local_endpoints_request_count(), 4);
        assert_eq!(std::fs::read(&cred_path).expect("read bytes"), initial_bytes);
        assert_eq!(
            bridge.opener().live_dial_credential().endpoints,
            initial_endpoints
        );

        peer.enqueue_local_endpoints_response(
            200,
            serde_json::to_vec(&json!({
                "v": 2,
                "endpoints": []
            }))
            .expect("json"),
        );
        assert!(
            run_dial_address_job(&client, &store, bridge.opener(), Duration::from_secs(5))
                .await
                .is_ok()
        );
        assert_eq!(peer.local_endpoints_request_count(), 5);
        assert_eq!(std::fs::read(&cred_path).expect("read bytes"), initial_bytes);
        assert_eq!(
            bridge.opener().live_dial_credential().endpoints,
            initial_endpoints
        );

        bridge.shutdown().await;
        peer.shutdown().await;
    });
}

#[test]
fn relay_access_dial_address_version_gate_preserves_on_v1_replaces_on_v2() {
    runtime().block_on(async {
        let peer = PrivateLinkPeer::start().await;
        let temporary = TestDirectory::new("dial-address-version-gate");
        let config_root = temporary.path().join("config");
        let data_root = temporary.path().join("data");
        ensure_private_directory(&config_root).expect("config root");
        ensure_private_directory(&data_root).expect("data root");
        let lock = InstanceLock::acquire(&data_root).expect("acquire lock");

        let mut initial_cred = peer.credential();
        initial_cred.endpoints.push(EndpointAddr {
            host: "192.0.2.9".to_owned(),
            port: 7657,
        });
        initial_cred.local_endpoints = Some(json!({"distinctive_field": "version-gate"}));
        persist_credential(&config_root, &initial_cred).expect("persist initial cred");

        let refresh = VersionRefreshState::new(
            config_root.clone(),
            data_root,
            initial_cred.instance_id.clone(),
            &initial_cred.ca_fp_prefix,
            lock.identity().clone(),
        );
        let bridge = PrivateLinkBridge::start(initial_cred.clone(), None, refresh)
            .await
            .expect("start bridge");
        let client = JournalClient::bootstrap(&bridge)
            .await
            .expect("bootstrap client");
        let pairing_gen = compute_pairing_generation(&initial_cred.client_cert_pem);
        let (store, _) =
            CredentialStore::new(config_root.clone(), initial_cred.clone(), pairing_gen);

        let cred_path = config_root.join("credentials.json");
        let initial_bytes = std::fs::read(&cred_path).expect("read cred bytes");
        let initial_endpoints = initial_cred.endpoints.clone();

        peer.enqueue_local_endpoints_response(
            200,
            serde_json::to_vec(&json!({
                "v": 1,
                "endpoints": [{"ip": "192.0.2.10", "port": 7657}]
            }))
            .expect("json"),
        );
        assert!(
            run_dial_address_job(&client, &store, bridge.opener(), Duration::from_secs(5))
                .await
                .is_ok()
        );
        assert_eq!(peer.local_endpoints_request_count(), 1);
        assert_eq!(
            std::fs::read(&cred_path).expect("read bytes"),
            initial_bytes
        );
        assert_eq!(
            bridge.opener().live_dial_credential().endpoints,
            initial_endpoints
        );

        peer.enqueue_local_endpoints_response(
            200,
            serde_json::to_vec(&json!({
                "endpoints": [{"ip": "192.0.2.11", "port": 7657}]
            }))
            .expect("json"),
        );
        assert!(
            run_dial_address_job(&client, &store, bridge.opener(), Duration::from_secs(5))
                .await
                .is_ok()
        );
        assert_eq!(peer.local_endpoints_request_count(), 2);
        assert_eq!(
            std::fs::read(&cred_path).expect("read bytes"),
            initial_bytes
        );
        assert_eq!(
            bridge.opener().live_dial_credential().endpoints,
            initial_endpoints
        );

        peer.enqueue_local_endpoints_response(
            200,
            serde_json::to_vec(&json!({
                "v": 2,
                "endpoints": [{"ip": "192.0.2.10", "port": 7657}]
            }))
            .expect("json"),
        );
        assert!(
            run_dial_address_job(&client, &store, bridge.opener(), Duration::from_secs(5))
                .await
                .is_ok()
        );
        assert_eq!(peer.local_endpoints_request_count(), 3);

        let loaded = load_credential(&config_root)
            .expect("load credential")
            .expect("credential exists");
        let live = bridge.opener().live_dial_credential();
        let expected = vec![
            EndpointAddr {
                host: "192.0.2.10".to_owned(),
                port: 7657,
            },
            initial_cred.endpoints[0].clone(),
            initial_cred.endpoints[1].clone(),
        ];
        assert_eq!(loaded.endpoints, expected);
        assert_eq!(live.endpoints, expected);
        assert_eq!(loaded.local_endpoints, initial_cred.local_endpoints);
        assert_eq!(live.local_endpoints, initial_cred.local_endpoints);

        bridge.shutdown().await;
        peer.shutdown().await;
    });
}

#[test]
fn relay_access_access_lane_runs_address_refresh_despite_relay_outcomes() {
    runtime().block_on(async {
        let peer = PrivateLinkPeer::start().await;
        let temporary = TestDirectory::new("access-lane-relay-outcomes");
        let config_root = temporary.path().join("config");
        let data_root = temporary.path().join("data");
        ensure_private_directory(&config_root).expect("config root");
        ensure_private_directory(&data_root).expect("data root");
        let lock = InstanceLock::acquire(&data_root).expect("acquire lock");

        let mut initial_cred = peer.credential();
        initial_cred.endpoints.push(EndpointAddr {
            host: "192.0.2.9".to_owned(),
            port: 7657,
        });
        initial_cred.local_endpoints = Some(json!({"distinctive_field": "relay-outcomes"}));
        persist_credential(&config_root, &initial_cred).expect("persist initial cred");

        let refresh = VersionRefreshState::new(
            config_root.clone(),
            data_root,
            initial_cred.instance_id.clone(),
            &initial_cred.ca_fp_prefix,
            lock.identity().clone(),
        );
        let bridge = PrivateLinkBridge::start(initial_cred.clone(), None, refresh)
            .await
            .expect("start bridge");
        let client = JournalClient::bootstrap(&bridge)
            .await
            .expect("bootstrap client");
        let pairing_gen = compute_pairing_generation(&initial_cred.client_cert_pem);
        let (store, _) =
            CredentialStore::new(config_root.clone(), initial_cred.clone(), pairing_gen);

        let clock = clock_at(1_700_000_000);

        peer.enqueue_relay_access_response(404, Vec::new());
        peer.enqueue_local_endpoints_response(
            200,
            serde_json::to_vec(&json!({
                "v": 2,
                "endpoints": [{"ip": "192.0.2.10", "port": 7657}]
            }))
            .expect("json"),
        );
        let res = run_access_lane(
            &client,
            &store,
            bridge.opener(),
            1,
            &clock,
            Duration::from_secs(5),
        )
        .await;
        assert!(res.relay.is_err());
        assert!(res.addresses.is_ok());
        let loaded = load_credential(&config_root).unwrap().unwrap();
        let live = bridge.opener().live_dial_credential();
        assert_eq!(loaded.endpoints, live.endpoints);
        assert_eq!(loaded.local_endpoints, initial_cred.local_endpoints);

        peer.enqueue_relay_access_response(500, Vec::new());
        peer.enqueue_local_endpoints_response(
            200,
            serde_json::to_vec(&json!({
                "v": 2,
                "endpoints": [{"ip": "192.0.2.11", "port": 7657}]
            }))
            .expect("json"),
        );
        let res = run_access_lane(
            &client,
            &store,
            bridge.opener(),
            2,
            &clock,
            Duration::from_secs(5),
        )
        .await;
        assert!(res.relay.is_err());
        assert!(res.addresses.is_ok());
        let loaded = load_credential(&config_root).unwrap().unwrap();
        let live = bridge.opener().live_dial_credential();
        assert_eq!(loaded.endpoints, live.endpoints);
        assert_eq!(loaded.local_endpoints, initial_cred.local_endpoints);

        peer.enqueue_relay_access_response(
            200,
            serde_json::to_vec(&json!({
                "status": "not_configured",
                "protocol_version": 2
            }))
            .expect("json"),
        );
        peer.enqueue_local_endpoints_response(
            200,
            serde_json::to_vec(&json!({
                "v": 2,
                "endpoints": [{"ip": "192.0.2.9", "port": 7657}]
            }))
            .expect("json"),
        );
        let res = run_access_lane(
            &client,
            &store,
            bridge.opener(),
            3,
            &clock,
            Duration::from_secs(5),
        )
        .await;
        assert!(res.relay.is_ok());
        assert!(res.addresses.is_ok());
        let loaded = load_credential(&config_root).unwrap().unwrap();
        let live = bridge.opener().live_dial_credential();
        assert_eq!(loaded.relay_origin, None);
        assert_eq!(loaded.device_token, None);
        assert_eq!(loaded.endpoints, live.endpoints);
        assert_eq!(loaded.local_endpoints, initial_cred.local_endpoints);

        let exp = 1_900_608_000;
        let token = create_jwt(&initial_cred.instance_id, exp);
        store
            .submit_ready(
                Arc::clone(bridge.opener()),
                store.capture_access_attempt(4),
                "https://relay.solstone.io".to_owned(),
                token.clone(),
                exp,
            )
            .await
            .expect("submit ready");

        peer.enqueue_relay_access_response(
            200,
            serde_json::to_vec(&json!({
                "status": "ready",
                "protocol_version": 2,
                "relay_origin": "https://relay.solstone.io",
                "instance_id": initial_cred.instance_id,
                "device_token": token,
                "expires_at": "2030-03-24T18:40:00Z"
            }))
            .expect("json"),
        );
        peer.enqueue_local_endpoints_response(
            200,
            serde_json::to_vec(&json!({
                "v": 2,
                "endpoints": [{"ip": "192.0.2.10", "port": 7657}]
            }))
            .expect("json"),
        );
        let res = run_access_lane(
            &client,
            &store,
            bridge.opener(),
            5,
            &clock,
            Duration::from_secs(5),
        )
        .await;
        assert!(res.relay.is_ok());
        assert!(res.addresses.is_ok());
        let loaded = load_credential(&config_root).unwrap().unwrap();
        let live = bridge.opener().live_dial_credential();
        assert_eq!(loaded.endpoints, live.endpoints);
        assert_eq!(loaded.local_endpoints, initial_cred.local_endpoints);

        bridge.shutdown().await;
        peer.shutdown().await;
    });
}

#[test]
fn relay_access_dial_address_three_refreshes_on_open_carrier_merge_and_evict() {
    runtime().block_on(async {
        let peer = PrivateLinkPeer::start().await;
        let temporary = TestDirectory::new("dial-address-three-refreshes");
        let config_root = temporary.path().join("config");
        let data_root = temporary.path().join("data");
        ensure_private_directory(&config_root).expect("config root");
        ensure_private_directory(&data_root).expect("data root");
        let lock = InstanceLock::acquire(&data_root).expect("acquire lock");

        let o1 = peer.credential().endpoints[0].clone();
        let mut initial_cred = peer.credential();
        initial_cred.local_endpoints = Some(json!({"distinctive_field": "three-refreshes"}));
        persist_credential(&config_root, &initial_cred).expect("persist initial cred");

        let refresh = VersionRefreshState::new(
            config_root.clone(),
            data_root,
            initial_cred.instance_id.clone(),
            &initial_cred.ca_fp_prefix,
            lock.identity().clone(),
        );
        let bridge = PrivateLinkBridge::start(initial_cred.clone(), None, refresh)
            .await
            .expect("start bridge");
        let client = JournalClient::bootstrap(&bridge)
            .await
            .expect("bootstrap client");
        let pairing_gen = compute_pairing_generation(&initial_cred.client_cert_pem);
        let (store, _) =
            CredentialStore::new(config_root.clone(), initial_cred.clone(), pairing_gen);

        let n1 = EndpointAddr {
            host: "192.0.2.9".to_owned(),
            port: 7657,
        };
        let n2 = EndpointAddr {
            host: "192.0.2.10".to_owned(),
            port: 7657,
        };
        let n3 = EndpointAddr {
            host: "192.0.2.11".to_owned(),
            port: 7657,
        };

        peer.enqueue_local_endpoints_response(
            200,
            serde_json::to_vec(&json!({
                "v": 2,
                "endpoints": [
                    {"ip": &n1.host, "port": n1.port},
                    {"ip": &n2.host, "port": n2.port}
                ]
            }))
            .expect("json"),
        );

        let _ = client
            .get_local_endpoints(Duration::from_secs(5))
            .await
            .expect("open carrier via bridge");

        store
            .submit_dial_endpoints(Arc::clone(bridge.opener()), vec![n1.clone(), n2.clone()])
            .await
            .expect("submit step 1");
        let step1_expected = vec![n1.clone(), n2.clone(), o1.clone()];
        let loaded1 = load_credential(&config_root).unwrap().unwrap();
        let live1 = bridge.opener().live_dial_credential();
        assert_eq!(loaded1.endpoints, step1_expected);
        assert_eq!(live1.endpoints, step1_expected);
        assert_eq!(loaded1.local_endpoints, initial_cred.local_endpoints);
        assert_eq!(peer.local_endpoints_request_count(), 1);

        let cred_path = config_root.join("credentials.json");
        let step1_bytes = std::fs::read(&cred_path).expect("read cred bytes");
        let step1_incarnation = bridge.opener().incarnation();
        peer.enqueue_local_endpoints_response(
            200,
            serde_json::to_vec(&json!({
                "v": 2,
                "endpoints": [
                    {"ip": &n1.host, "port": n1.port},
                    {"ip": &n2.host, "port": n2.port}
                ]
            }))
            .expect("json"),
        );
        let res2 =
            run_dial_address_job(&client, &store, bridge.opener(), Duration::from_secs(5)).await;
        assert!(res2.is_ok());
        assert_eq!(peer.local_endpoints_request_count(), 2);
        assert_eq!(std::fs::read(&cred_path).expect("read bytes"), step1_bytes);
        assert_eq!(bridge.opener().incarnation(), step1_incarnation);
        let loaded2 = load_credential(&config_root).unwrap().unwrap();
        let live2 = bridge.opener().live_dial_credential();
        assert_eq!(loaded2.endpoints, step1_expected);
        assert_eq!(live2.endpoints, step1_expected);

        peer.enqueue_local_endpoints_response(
            200,
            serde_json::to_vec(&json!({
                "v": 2,
                "endpoints": [{"ip": &n3.host, "port": n3.port}]
            }))
            .expect("json"),
        );
        let res3 =
            run_dial_address_job(&client, &store, bridge.opener(), Duration::from_secs(5)).await;
        assert!(res3.is_ok());
        assert_eq!(peer.local_endpoints_request_count(), 3);
        let step3_expected = vec![n3.clone(), n1.clone(), n2.clone()];
        let loaded3 = load_credential(&config_root).unwrap().unwrap();
        let live3 = bridge.opener().live_dial_credential();
        assert_eq!(loaded3.endpoints, step3_expected);
        assert_eq!(live3.endpoints, step3_expected);
        assert_eq!(loaded3.local_endpoints, initial_cred.local_endpoints);

        bridge.shutdown().await;
        peer.shutdown().await;
    });
}

#[test]
fn relay_access_dial_address_unchanged_v2_set_preserves_raw_bytes_and_incarnation() {
    runtime().block_on(async {
        let peer = PrivateLinkPeer::start().await;
        let temporary = TestDirectory::new("dial-address-unchanged-set");
        let config_root = temporary.path().join("config");
        let data_root = temporary.path().join("data");
        ensure_private_directory(&config_root).expect("config root");
        ensure_private_directory(&data_root).expect("data root");
        let lock = InstanceLock::acquire(&data_root).expect("acquire lock");

        let mut initial_cred = peer.credential();
        initial_cred.endpoints.push(EndpointAddr {
            host: "192.0.2.9".to_owned(),
            port: 7657,
        });
        initial_cred.local_endpoints = Some(json!({"distinctive_field": "unchanged-preserves"}));
        persist_credential(&config_root, &initial_cred).expect("persist initial cred");

        let refresh = VersionRefreshState::new(
            config_root.clone(),
            data_root,
            initial_cred.instance_id.clone(),
            &initial_cred.ca_fp_prefix,
            lock.identity().clone(),
        );
        let bridge = PrivateLinkBridge::start(initial_cred.clone(), None, refresh)
            .await
            .expect("start bridge");
        let client = JournalClient::bootstrap(&bridge)
            .await
            .expect("bootstrap client");
        let pairing_gen = compute_pairing_generation(&initial_cred.client_cert_pem);
        let (store, _) =
            CredentialStore::new(config_root.clone(), initial_cred.clone(), pairing_gen);

        let peer_endpoint = initial_cred.endpoints[0].clone();
        peer.enqueue_local_endpoints_response(
            200,
            serde_json::to_vec(&json!({
                "v": 2,
                "endpoints": [
                    {"ip": &peer_endpoint.host, "port": peer_endpoint.port},
                    {"ip": "192.0.2.9", "port": 7657}
                ]
            }))
            .expect("json"),
        );
        let cred_path = config_root.join("credentials.json");
        let before_bytes = std::fs::read(&cred_path).expect("read cred bytes");
        let before_incarnation = bridge.opener().incarnation();

        let res =
            run_dial_address_job(&client, &store, bridge.opener(), Duration::from_secs(5)).await;
        assert!(res.is_ok());
        assert_eq!(peer.local_endpoints_request_count(), 1);
        assert_eq!(
            std::fs::read(&cred_path).expect("read cred bytes"),
            before_bytes
        );
        assert_eq!(bridge.opener().incarnation(), before_incarnation);

        bridge.shutdown().await;
        peer.shutdown().await;
    });
}

#[test]
fn relay_access_access_lane_ready_token_and_v2_endpoints_both_commit() {
    runtime().block_on(async {
        let peer = PrivateLinkPeer::start().await;
        let temporary = TestDirectory::new("access-lane-ready-and-endpoints");
        let config_root = temporary.path().join("config");
        let data_root = temporary.path().join("data");
        ensure_private_directory(&config_root).expect("config root");
        ensure_private_directory(&data_root).expect("data root");
        let lock = InstanceLock::acquire(&data_root).expect("acquire lock");

        let mut initial_cred = peer.credential();
        initial_cred.endpoints.push(EndpointAddr {
            host: "192.0.2.9".to_owned(),
            port: 7657,
        });
        initial_cred.local_endpoints = Some(json!({"distinctive_field": "ready-and-endpoints"}));
        persist_credential(&config_root, &initial_cred).expect("persist initial cred");

        let refresh = VersionRefreshState::new(
            config_root.clone(),
            data_root,
            initial_cred.instance_id.clone(),
            &initial_cred.ca_fp_prefix,
            lock.identity().clone(),
        );
        let bridge = PrivateLinkBridge::start(initial_cred.clone(), None, refresh)
            .await
            .expect("start bridge");
        let client = JournalClient::bootstrap(&bridge)
            .await
            .expect("bootstrap client");
        let pairing_gen = compute_pairing_generation(&initial_cred.client_cert_pem);
        let (store, _) =
            CredentialStore::new(config_root.clone(), initial_cred.clone(), pairing_gen);

        let exp = 1_900_608_000;
        let token = create_jwt(&initial_cred.instance_id, exp);
        peer.enqueue_relay_access_response(
            200,
            serde_json::to_vec(&json!({
                "status": "ready",
                "protocol_version": 2,
                "relay_origin": "https://relay.solstone.io",
                "instance_id": initial_cred.instance_id,
                "device_token": token,
                "expires_at": "2030-03-24T18:40:00Z"
            }))
            .expect("json"),
        );
        peer.enqueue_local_endpoints_response(
            200,
            serde_json::to_vec(&json!({
                "v": 2,
                "endpoints": [{"ip": "192.0.2.10", "port": 7657}]
            }))
            .expect("json"),
        );

        let res = run_access_lane(
            &client,
            &store,
            bridge.opener(),
            1,
            &clock_at(1_700_000_000),
            Duration::from_secs(5),
        )
        .await;
        assert!(res.relay.is_ok());
        assert!(res.addresses.is_ok());

        let loaded = load_credential(&config_root).unwrap().unwrap();
        let live = bridge.opener().live_dial_credential();
        assert_eq!(
            loaded.relay_origin.as_deref(),
            Some("https://relay.solstone.io")
        );
        assert_eq!(loaded.device_token.as_deref(), Some(token.as_str()));
        assert_eq!(
            live.relay_origin.as_deref(),
            Some("https://relay.solstone.io")
        );
        assert_eq!(live.device_token.as_deref(), Some(token.as_str()));
        assert_eq!(loaded.endpoints, live.endpoints);
        assert_eq!(
            loaded.endpoints[0],
            EndpointAddr {
                host: "192.0.2.10".to_owned(),
                port: 7657
            }
        );
        assert_eq!(loaded.local_endpoints, initial_cred.local_endpoints);

        bridge.shutdown().await;
        peer.shutdown().await;
    });
}

#[test]
fn relay_access_access_lane_sequential_holds_both_commit() {
    runtime().block_on(async {
        let peer = PrivateLinkPeer::start().await;
        let temporary = TestDirectory::new("access-lane-sequential-holds");
        let config_root = temporary.path().join("config");
        let data_root = temporary.path().join("data");
        ensure_private_directory(&config_root).expect("config root");
        ensure_private_directory(&data_root).expect("data root");
        let lock = InstanceLock::acquire(&data_root).expect("acquire lock");

        let mut initial_cred = peer.credential();
        initial_cred.endpoints.push(EndpointAddr {
            host: "192.0.2.9".to_owned(),
            port: 7657,
        });
        initial_cred.local_endpoints = Some(json!({"distinctive_field": "sequential-holds"}));
        persist_credential(&config_root, &initial_cred).expect("persist initial cred");

        let refresh = VersionRefreshState::new(
            config_root.clone(),
            data_root,
            initial_cred.instance_id.clone(),
            &initial_cred.ca_fp_prefix,
            lock.identity().clone(),
        );
        let bridge = PrivateLinkBridge::start(initial_cred.clone(), None, refresh)
            .await
            .expect("start bridge");
        let client = JournalClient::bootstrap(&bridge)
            .await
            .expect("bootstrap client");
        let pairing_gen = compute_pairing_generation(&initial_cred.client_cert_pem);
        let (store, _) =
            CredentialStore::new(config_root.clone(), initial_cred.clone(), pairing_gen);

        peer.hold_relay_access();
        peer.hold_local_endpoints();

        let exp = 1_900_608_000;
        let token = create_jwt(&initial_cred.instance_id, exp);
        peer.enqueue_relay_access_response(
            200,
            serde_json::to_vec(&json!({
                "status": "ready",
                "protocol_version": 2,
                "relay_origin": "https://relay.solstone.io",
                "instance_id": initial_cred.instance_id,
                "device_token": token,
                "expires_at": "2030-03-24T18:40:00Z"
            }))
            .expect("json"),
        );
        peer.enqueue_local_endpoints_response(
            200,
            serde_json::to_vec(&json!({
                "v": 2,
                "endpoints": [{"ip": "192.0.2.10", "port": 7657}]
            }))
            .expect("json"),
        );

        let opener = Arc::clone(bridge.opener());
        let clock = clock_at(1_700_000_000);
        let task = tokio::spawn({
            let store = Arc::clone(&store);
            async move {
                run_access_lane(&client, &store, &opener, 1, &clock, Duration::from_secs(5)).await
            }
        });

        peer.wait_for_relay_access_hold(Duration::from_secs(5))
            .await;
        peer.release_relay_access();
        peer.wait_for_local_endpoints_hold(Duration::from_secs(5))
            .await;
        peer.release_local_endpoints();

        let res = task.await.expect("task join");
        assert!(res.relay.is_ok());
        assert!(res.addresses.is_ok());

        let loaded = load_credential(&config_root).unwrap().unwrap();
        let live = bridge.opener().live_dial_credential();
        assert_eq!(
            loaded.relay_origin.as_deref(),
            Some("https://relay.solstone.io")
        );
        assert_eq!(loaded.device_token.as_deref(), Some(token.as_str()));
        assert_eq!(live.device_token.as_deref(), Some(token.as_str()));
        assert_eq!(loaded.endpoints, live.endpoints);
        assert_eq!(
            loaded.endpoints[0],
            EndpointAddr {
                host: "192.0.2.10".to_owned(),
                port: 7657
            }
        );
        assert_eq!(loaded.local_endpoints, initial_cred.local_endpoints);

        bridge.shutdown().await;
        peer.shutdown().await;
    });
}

#[test]
fn relay_access_dial_address_commit_preserves_mutation_generation_for_pre_commit_hook() {
    runtime().block_on(async {
        let peer = PrivateLinkPeer::start().await;
        let temporary = TestDirectory::new("dial-address-pre-commit-hook");
        let config_root = temporary.path().join("config");
        let data_root = temporary.path().join("data");
        ensure_private_directory(&config_root).expect("config root");
        ensure_private_directory(&data_root).expect("data root");
        let lock = InstanceLock::acquire(&data_root).expect("acquire lock");

        let mut initial_cred = peer.credential();
        initial_cred.endpoints.push(EndpointAddr {
            host: "192.0.2.9".to_owned(),
            port: 7657,
        });
        initial_cred.local_endpoints = Some(json!({"distinctive_field": "hook-generation"}));
        persist_credential(&config_root, &initial_cred).expect("persist initial cred");

        let refresh = VersionRefreshState::new(
            config_root.clone(),
            data_root,
            initial_cred.instance_id.clone(),
            &initial_cred.ca_fp_prefix,
            lock.identity().clone(),
        );

        let pairing_gen = compute_pairing_generation(&initial_cred.client_cert_pem);
        let (store, hook) =
            CredentialStore::new(config_root.clone(), initial_cred.clone(), pairing_gen);
        let bridge = PrivateLinkBridge::start(initial_cred.clone(), Some(hook.clone()), refresh)
            .await
            .expect("start bridge");
        let client = JournalClient::bootstrap(&bridge)
            .await
            .expect("bootstrap client");

        peer.enqueue_local_endpoints_response(
            200,
            serde_json::to_vec(&json!({
                "v": 2,
                "endpoints": [{"ip": "192.0.2.10", "port": 7657}]
            }))
            .expect("json"),
        );

        let res =
            run_dial_address_job(&client, &store, bridge.opener(), Duration::from_secs(5)).await;
        assert!(res.is_ok());

        let new_token = "token-from-pre-commit-hook";
        let new_exp = 1_950_000_000;
        hook(new_token, new_exp);
        store.persist_pending().await.expect("persist pending");

        let loaded = load_credential(&config_root).unwrap().unwrap();
        let live = store.live_credential();
        assert_eq!(loaded.device_token.as_deref(), Some(new_token));
        assert_eq!(live.device_token.as_deref(), Some(new_token));
        assert_eq!(
            loaded.endpoints[0],
            EndpointAddr {
                host: "192.0.2.10".to_owned(),
                port: 7657
            }
        );
        assert_eq!(
            live.endpoints[0],
            EndpointAddr {
                host: "192.0.2.10".to_owned(),
                port: 7657
            }
        );
        assert_eq!(
            bridge.opener().live_dial_credential().endpoints[0],
            EndpointAddr {
                host: "192.0.2.10".to_owned(),
                port: 7657
            }
        );
        assert_eq!(loaded.local_endpoints, initial_cred.local_endpoints);

        bridge.shutdown().await;
        peer.shutdown().await;
    });
}

#[test]
fn relay_access_session_timeout_budget_independence_allows_address_refresh() {
    runtime().block_on(async {
        let peer = PrivateLinkPeer::start().await;
        let temporary = TestDirectory::new("session-budget-independence");
        let config_root = temporary.path().join("config");
        let data_root = temporary.path().join("data");
        ensure_private_directory(&config_root).expect("config root");
        ensure_private_directory(&data_root).expect("data root");
        let lock = InstanceLock::acquire(&data_root).expect("acquire lock");

        let mut credential = peer.credential();
        credential.endpoints.push(EndpointAddr {
            host: "192.0.2.9".to_owned(),
            port: 7657,
        });
        credential.local_endpoints = Some(json!({"distinctive_field": "budget-independence"}));
        persist_credential(&config_root, &credential).expect("persist cred");

        let refresh = VersionRefreshState::new(
            config_root.clone(),
            data_root,
            credential.instance_id.clone(),
            &credential.ca_fp_prefix,
            lock.identity().clone(),
        );

        peer.enqueue_delayed_relay_access_response(
            Duration::from_secs(5),
            200,
            serde_json::to_vec(&json!({
                "status": "not_configured",
                "protocol_version": 2
            }))
            .expect("json"),
        );
        peer.enqueue_local_endpoints_response(
            200,
            serde_json::to_vec(&json!({
                "v": 2,
                "endpoints": [{"ip": "192.0.2.10", "port": 7657}]
            }))
            .expect("json"),
        );

        let session = JournalSession::start_with(
            credential.clone(),
            config_root.clone(),
            refresh,
            Duration::from_secs(1),
            Arc::new(|| Some("budget-host".to_owned())),
            solstone_tmux::paths::PlatformKind::Linux,
            Arc::new(solstone_tmux::clock::SystemClock::utc()),
        )
        .await
        .expect("start session");

        session
            .wait_for_post_connect_quiescence(Duration::from_secs(5))
            .await;

        assert_eq!(peer.local_endpoints_request_count(), 1);
        let live = session.opener().live_dial_credential();
        assert_eq!(
            live.endpoints[0],
            EndpointAddr {
                host: "192.0.2.10".to_owned(),
                port: 7657
            }
        );
        let loaded = load_credential(&config_root).unwrap().unwrap();
        assert_eq!(
            loaded.endpoints[0],
            EndpointAddr {
                host: "192.0.2.10".to_owned(),
                port: 7657
            }
        );
        assert_eq!(loaded.local_endpoints, credential.local_endpoints);

        session.shutdown().await.expect("shutdown session");
        peer.shutdown().await;
    });
}

#[test]
fn relay_access_dial_address_fault_before_rename_leaves_memory_and_incarnation_clean() {
    let _fault_guard = FAULT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    runtime().block_on(async {
        let peer = PrivateLinkPeer::start().await;
        let temporary = TestDirectory::new("dial-address-fault-before-rename");
        let config_root = temporary.path().join("config");
        let data_root = temporary.path().join("data");
        ensure_private_directory(&config_root).expect("config root");
        ensure_private_directory(&data_root).expect("data root");
        let lock = InstanceLock::acquire(&data_root).expect("acquire lock");

        let mut initial_cred = peer.credential();
        initial_cred.endpoints.push(EndpointAddr {
            host: "192.0.2.9".to_owned(),
            port: 7657,
        });
        initial_cred.local_endpoints = Some(json!({"distinctive_field": "fault-test"}));
        persist_credential(&config_root, &initial_cred).expect("persist initial cred");

        let refresh = VersionRefreshState::new(
            config_root.clone(),
            data_root,
            initial_cred.instance_id.clone(),
            &initial_cred.ca_fp_prefix,
            lock.identity().clone(),
        );
        let bridge = PrivateLinkBridge::start(initial_cred.clone(), None, refresh)
            .await
            .expect("start bridge");
        let client = JournalClient::bootstrap(&bridge)
            .await
            .expect("bootstrap client");
        let pairing_gen = compute_pairing_generation(&initial_cred.client_cert_pem);
        let (store, _) =
            CredentialStore::new(config_root.clone(), initial_cred.clone(), pairing_gen);

        let cred_path = config_root.join("credentials.json");
        let initial_bytes = std::fs::read(&cred_path).expect("read cred bytes");
        let initial_endpoints = initial_cred.endpoints.clone();
        let initial_incarnation = bridge.opener().incarnation();

        set_credential_write_fault(
            &config_root,
            Some(solstone_tmux::storage::AtomicWriteFault::FailBeforeRename),
        );
        peer.enqueue_local_endpoints_response(
            200,
            serde_json::to_vec(&json!({
                "v": 2,
                "endpoints": [{"ip": "192.0.2.10", "port": 7657}]
            }))
            .expect("json"),
        );

        let res =
            run_dial_address_job(&client, &store, bridge.opener(), Duration::from_secs(5)).await;
        set_credential_write_fault(&config_root, None);

        assert!(res.is_err());
        assert_eq!(
            std::fs::read(&cred_path).expect("read cred bytes"),
            initial_bytes
        );
        assert_eq!(
            bridge.opener().live_dial_credential().endpoints,
            initial_endpoints
        );
        assert_eq!(bridge.opener().incarnation(), initial_incarnation);
        assert!(store.persistence_issue().is_none());

        bridge
            .opener()
            .dial_carrier()
            .await
            .expect("later dial still works");

        peer.enqueue_local_endpoints_response(
            200,
            serde_json::to_vec(&json!({
                "v": 2,
                "endpoints": [{"ip": "192.0.2.10", "port": 7657}]
            }))
            .expect("json"),
        );
        let res2 =
            run_dial_address_job(&client, &store, bridge.opener(), Duration::from_secs(5)).await;
        assert!(res2.is_ok());
        let loaded = load_credential(&config_root).unwrap().unwrap();
        assert_eq!(
            loaded.endpoints[0],
            EndpointAddr {
                host: "192.0.2.10".to_owned(),
                port: 7657
            }
        );
        assert_eq!(
            bridge.opener().live_dial_credential().endpoints,
            loaded.endpoints
        );

        bridge.shutdown().await;
        peer.shutdown().await;
    });
}

#[test]
fn relay_access_dial_address_store_invalidation_and_stale_attempt_isolation() {
    runtime().block_on(async {
        let peer = PrivateLinkPeer::start().await;
        let temporary = TestDirectory::new("dial-address-invalidate-and-stale");
        let config_root = temporary.path().join("config");
        let data_root = temporary.path().join("data");
        ensure_private_directory(&config_root).expect("config root");
        ensure_private_directory(&data_root).expect("data root");
        let lock = InstanceLock::acquire(&data_root).expect("acquire lock");

        let mut initial_cred = peer.credential();
        initial_cred.endpoints.push(EndpointAddr {
            host: "192.0.2.9".to_owned(),
            port: 7657,
        });
        initial_cred.local_endpoints = Some(json!({"distinctive_field": "stale-isolation"}));
        persist_credential(&config_root, &initial_cred).expect("persist initial cred");

        let refresh = VersionRefreshState::new(
            config_root.clone(),
            data_root,
            initial_cred.instance_id.clone(),
            &initial_cred.ca_fp_prefix,
            lock.identity().clone(),
        );
        let bridge = PrivateLinkBridge::start(initial_cred.clone(), None, refresh)
            .await
            .expect("start bridge");
        let client = JournalClient::bootstrap(&bridge)
            .await
            .expect("bootstrap client");
        let pairing_gen = compute_pairing_generation(&initial_cred.client_cert_pem);
        let (store, _) =
            CredentialStore::new(config_root.clone(), initial_cred.clone(), pairing_gen);

        let initial_endpoints = initial_cred.endpoints.clone();
        let initial_incarnation = bridge.opener().incarnation();

        peer.hold_local_endpoints();
        peer.enqueue_local_endpoints_response(
            200,
            serde_json::to_vec(&json!({
                "v": 2,
                "endpoints": [{"ip": "192.0.2.10", "port": 7657}]
            }))
            .expect("json"),
        );

        let opener = Arc::clone(bridge.opener());
        let client_for_task = client.clone();
        let task = tokio::spawn({
            let store = Arc::clone(&store);
            async move {
                run_dial_address_job(&client_for_task, &store, &opener, Duration::from_secs(5))
                    .await
            }
        });

        peer.wait_for_local_endpoints_hold(Duration::from_secs(5))
            .await;
        store.invalidate();
        peer.release_local_endpoints();

        assert!(task.await.expect("task join").is_err());
        assert_eq!(
            bridge.opener().live_dial_credential().endpoints,
            initial_endpoints
        );
        assert_eq!(bridge.opener().incarnation(), initial_incarnation);

        let (fresh_store, _) =
            CredentialStore::new(config_root.clone(), initial_cred.clone(), pairing_gen);
        let _att1 = fresh_store.capture_access_attempt(1);
        let _att2 = fresh_store.capture_access_attempt(2);

        peer.enqueue_local_endpoints_response(
            200,
            serde_json::to_vec(&json!({
                "v": 2,
                "endpoints": [{"ip": "192.0.2.10", "port": 7657}]
            }))
            .expect("json"),
        );

        let res = run_dial_address_job(
            &client,
            &fresh_store,
            bridge.opener(),
            Duration::from_secs(5),
        )
        .await;
        assert!(res.is_ok());
        let loaded = load_credential(&config_root).unwrap().unwrap();
        assert_eq!(
            loaded.endpoints[0],
            EndpointAddr {
                host: "192.0.2.10".to_owned(),
                port: 7657
            }
        );

        bridge.shutdown().await;
        peer.shutdown().await;
    });
}

#[test]
fn relay_access_dial_carrier_routes_to_additional_listener_after_address_refresh() {
    runtime().block_on(async {
        let peer = PrivateLinkPeer::start().await;
        let (second_port, additional_accept_count) = peer.bind_additional_listener().await;

        let temporary = TestDirectory::new("dial-carrier-additional-listener");
        let config_root = temporary.path().join("config");
        let data_root = temporary.path().join("data");
        ensure_private_directory(&config_root).expect("config root");
        ensure_private_directory(&data_root).expect("data root");
        let lock = InstanceLock::acquire(&data_root).expect("acquire lock");

        let mut initial_cred = peer.credential();
        initial_cred.endpoints.push(EndpointAddr {
            host: "192.0.2.9".to_owned(),
            port: 7657,
        });
        initial_cred.local_endpoints = Some(json!({"distinctive_field": "additional-listener"}));
        persist_credential(&config_root, &initial_cred).expect("persist initial cred");

        let refresh = VersionRefreshState::new(
            config_root.clone(),
            data_root,
            initial_cred.instance_id.clone(),
            &initial_cred.ca_fp_prefix,
            lock.identity().clone(),
        );
        let bridge = PrivateLinkBridge::start(initial_cred.clone(), None, refresh)
            .await
            .expect("start bridge");
        let client = JournalClient::bootstrap(&bridge)
            .await
            .expect("bootstrap client");
        let pairing_gen = compute_pairing_generation(&initial_cred.client_cert_pem);
        let (store, _) =
            CredentialStore::new(config_root.clone(), initial_cred.clone(), pairing_gen);

        peer.enqueue_local_endpoints_response(
            200,
            serde_json::to_vec(&json!({
                "v": 2,
                "endpoints": [{"ip": "127.0.0.1", "port": second_port}]
            }))
            .expect("json"),
        );

        let res =
            run_dial_address_job(&client, &store, bridge.opener(), Duration::from_secs(5)).await;
        assert!(res.is_ok());

        let accepted_before = peer.accepted_carriers();
        let additional_before = additional_accept_count.load(std::sync::atomic::Ordering::SeqCst);

        peer.close_accepted_carriers();
        bridge
            .opener()
            .dial_carrier()
            .await
            .expect("dial carrier to second listener");

        assert_eq!(
            additional_accept_count.load(std::sync::atomic::Ordering::SeqCst),
            additional_before + 1
        );
        assert_eq!(peer.accepted_carriers(), accepted_before + 1);

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
