// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Only a refusal of this device reads `revoked`. A journal or bridge that
//! fails to answer, and a journal that rejects one request for another reason,
//! must never tell the owner the device was unpaired.

mod support;

use solstone_tmux::config::DEFAULT_SOURCE;
use solstone_tmux::health::{DiagnosticCode, HealthState, SyncFacts};
use solstone_tmux::instance_lock::InstanceLock;
use solstone_tmux::paths::ensure_private_directory;
use solstone_tmux::sync::{JournalSession, SyncJournal, SyncOperationError};
use spl_transport::credential::Credential;
use support::TestDirectory;
use support::private_link_peer::PrivateLinkPeer;

const ACCESS_DENIED: u8 = 49;
const CERTIFICATE_UNKNOWN: u8 = 46;
const INTERNAL_ERROR: u8 = 80;

#[test]
fn only_a_revocation_diagnostic_reads_revoked() {
    for (code, expected) in [
        (DiagnosticCode::JournalRevoked, HealthState::Revoked),
        (DiagnosticCode::JournalRejected, HealthState::Offline),
        (DiagnosticCode::JournalUnavailable, HealthState::Offline),
        (DiagnosticCode::JournalTimeout, HealthState::Offline),
        (DiagnosticCode::BridgeUnavailable, HealthState::Offline),
    ] {
        let mut facts = SyncFacts {
            paired: true,
            ..SyncFacts::default()
        };
        facts.failed(code);
        assert_eq!(facts.state(), expected, "{code:?}");
    }
    assert_eq!(DiagnosticCode::JournalRevoked.as_str(), "journal_revoked");
}

#[test]
fn server_errors_without_an_auth_reason_read_offline() {
    runtime().block_on(async {
        for (status, body) in [
            (502, b"journal unreachable".as_slice()),
            (503, b"".as_slice()),
            (
                500,
                br#"{"error":"x","reason_code":"internal_error","detail":"x"}"#.as_slice(),
            ),
        ] {
            let peer = PrivateLinkPeer::start().await;
            let temporary = TestDirectory::new(&format!("failure-health-server-{status}"));
            let mut session = start_session(peer.credential(), &temporary).await;
            peer.enqueue_response(status, body);

            let code = manifest_failure(&mut session).await;
            assert_eq!(code, DiagnosticCode::JournalUnavailable, "HTTP {status}");
            assert_eq!(state_after(code), HealthState::Offline, "HTTP {status}");

            session.shutdown().await.expect("shutdown session");
            peer.shutdown().await;
        }
    });
}

#[test]
fn a_journal_that_is_down_reads_offline() {
    runtime().block_on(async {
        let peer = PrivateLinkPeer::start().await;
        let credential = peer.credential();
        // Nothing accepts the dial, so the bridge answers its own 502.
        peer.shutdown().await;
        let temporary = TestDirectory::new("failure-health-down");
        let mut session = start_session(credential, &temporary).await;

        let code = manifest_failure(&mut session).await;
        assert_eq!(code, DiagnosticCode::JournalUnavailable);
        assert_eq!(state_after(code), HealthState::Offline);

        session.shutdown().await.expect("shutdown session");
    });
}

#[test]
fn a_rejection_for_another_reason_is_not_revoked() {
    runtime().block_on(async {
        for (status, reason_code) in [
            (400, "source_too_long"),
            (500, "ingest_storage_failed"),
            (426, "protocol_version_future"),
        ] {
            let peer = PrivateLinkPeer::start().await;
            let temporary = TestDirectory::new(&format!("failure-health-{reason_code}"));
            let mut session = start_session(peer.credential(), &temporary).await;
            peer.enqueue_response(status, rejection(reason_code));

            let code = manifest_failure(&mut session).await;
            assert_eq!(code, DiagnosticCode::JournalRejected, "{reason_code}");
            assert_eq!(state_after(code), HealthState::Offline, "{reason_code}");

            session.shutdown().await.expect("shutdown session");
            peer.shutdown().await;
        }
    });
}

#[test]
fn an_auth_refusal_reads_revoked() {
    runtime().block_on(async {
        for (status, reason_code) in [
            (403, "pl_revoked"),
            (401, "auth_required"),
            (401, "auth_key_invalid"),
        ] {
            let peer = PrivateLinkPeer::start().await;
            let temporary = TestDirectory::new(&format!("failure-health-{reason_code}"));
            let mut session = start_session(peer.credential(), &temporary).await;
            peer.enqueue_response(status, rejection(reason_code));

            let code = manifest_failure(&mut session).await;
            assert_eq!(code, DiagnosticCode::JournalRevoked, "{reason_code}");
            assert_eq!(state_after(code), HealthState::Revoked, "{reason_code}");

            session.shutdown().await.expect("shutdown session");
            peer.shutdown().await;
        }
    });
}

#[test]
fn only_an_access_denied_handshake_reads_revoked() {
    runtime().block_on(async {
        for (alert, expected_code, expected_state) in [
            (
                ACCESS_DENIED,
                DiagnosticCode::JournalRevoked,
                HealthState::Revoked,
            ),
            (
                CERTIFICATE_UNKNOWN,
                DiagnosticCode::JournalUnavailable,
                HealthState::Offline,
            ),
            (
                INTERNAL_ERROR,
                DiagnosticCode::JournalUnavailable,
                HealthState::Offline,
            ),
        ] {
            let peer = PrivateLinkPeer::start().await;
            peer.refuse_handshakes_with_alert(alert);
            let temporary = TestDirectory::new(&format!("failure-health-alert-{alert}"));
            let mut session = start_session(peer.credential(), &temporary).await;

            let code = manifest_failure(&mut session).await;
            assert_eq!(code, expected_code, "alert {alert}");
            assert_eq!(state_after(code), expected_state, "alert {alert}");

            session.shutdown().await.expect("shutdown session");
            peer.shutdown().await;
        }
    });
}

async fn start_session(credential: Credential, temporary: &TestDirectory) -> JournalSession {
    ensure_private_directory(temporary.path()).expect("private root");
    let lock = InstanceLock::acquire(temporary.path()).expect("acquire lock");
    let refresh = solstone_tmux::journal_version::VersionRefreshState::new(
        temporary.path().to_path_buf(),
        temporary.path().to_path_buf(),
        credential.instance_id.clone(),
        &credential.ca_fp_prefix,
        lock.identity().clone(),
    );
    let session = JournalSession::start(credential, temporary.path().to_path_buf(), refresh)
        .await
        .expect("start journal session");
    // Let the post-connect burst finish its own dials, so the request under test gets a
    // dial budget of its own and sees the bridge's answer rather than its own timeout.
    session
        .wait_for_post_connect_quiescence(std::time::Duration::from_secs(30))
        .await;
    session
}

async fn manifest_failure(session: &mut JournalSession) -> DiagnosticCode {
    match session.manifest(DEFAULT_SOURCE).await {
        Err(
            SyncOperationError::RetainCandidate(code)
            | SyncOperationError::EndSweepDiagnostic(_, code),
        ) => code,
        Err(SyncOperationError::EndSweep(failure)) => panic!("undiagnosed failure {failure:?}"),
        Ok(_) => panic!("failed manifest was accepted"),
    }
}

fn state_after(code: DiagnosticCode) -> HealthState {
    let mut facts = SyncFacts {
        paired: true,
        ..SyncFacts::default()
    };
    facts.failed(code);
    facts.state()
}

fn rejection(reason_code: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "error": "rejected",
        "reason_code": reason_code,
        "detail": "rejected",
    }))
    .expect("rejection body")
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .expect("runtime")
}
