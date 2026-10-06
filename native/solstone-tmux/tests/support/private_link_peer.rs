// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::collections::{HashMap, VecDeque};
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use futures_util::{SinkExt, StreamExt};
use rcgen::{
    BasicConstraints, Certificate, CertificateParams, CertificateSigningRequestParams,
    ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose, PKCS_ECDSA_P256_SHA256,
};
use rustls::client::danger::HandshakeSignatureValid;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, UnixTime};
use rustls::server::WebPkiClientVerifier;
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{
    CertificateError, DigitallySignedStruct, DistinguishedName, OtherError, RootCertStore,
    ServerConfig, SignatureScheme,
};
use serde_json::json;
use spl_core::frame::{
    FLAG_CLOSE, FLAG_DATA, FLAG_OPEN, FLAG_RESET, FLAG_WINDOW, Frame, FrameDecoder,
    RECOMMENDED_CHUNK,
};
use spl_core::mux::INITIAL_WINDOW;
use spl_transport::credential::{Credential, EndpointAddr};
use tokio::io::{AsyncReadExt, AsyncWriteExt, WriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::server::TlsStream;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;

#[derive(Clone)]
pub struct PeerRequest {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    response_status: Option<u16>,
    authenticated_client_sha256: Option<String>,
}

impl PeerRequest {
    pub fn method(&self) -> &str {
        &self.method
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn path_without_query(&self) -> &str {
        self.path
            .split_once('?')
            .map_or(self.path.as_str(), |(path, _)| path)
    }

    pub fn query_param(&self, name: &str) -> Option<&str> {
        self.path.split_once('?')?.1.split('&').find_map(|pair| {
            let (key, value) = pair.split_once('=')?;
            (key == name).then_some(value)
        })
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(candidate, _)| candidate.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    pub fn body(&self) -> &[u8] {
        &self.body
    }

    pub fn response_status(&self) -> Option<u16> {
        self.response_status
    }

    pub fn authenticated_client_sha256(&self) -> Option<&str> {
        self.authenticated_client_sha256.as_deref()
    }
}

#[derive(Clone)]
enum PeerResponse {
    Structured {
        status: u16,
        body: Vec<u8>,
        delay: Option<std::time::Duration>,
    },
    DelayedBody {
        status: u16,
        body: Vec<u8>,
        delay: std::time::Duration,
    },
    Raw(Vec<u8>),
}

struct OutboundResponse {
    bytes: Vec<u8>,
    offset: usize,
    credit: usize,
    deliver_at: Option<tokio::time::Instant>,
}

#[derive(Clone)]
enum Control {
    GrantUploadCredit(u32),
    CloseCarriers,
}

#[derive(Default)]
struct PathHold {
    enabled: AtomicBool,
    arrivals: AtomicUsize,
    releases: AtomicUsize,
    arrived: Notify,
    release: Notify,
}

impl PathHold {
    fn hold(&self) {
        self.arrivals.store(0, Ordering::SeqCst);
        self.releases.store(0, Ordering::SeqCst);
        self.enabled.store(true, Ordering::SeqCst);
    }

    fn release_one(&self) {
        self.releases.fetch_add(1, Ordering::SeqCst);
        self.release.notify_waiters();
    }

    fn release(&self) {
        self.enabled.store(false, Ordering::SeqCst);
        self.release.notify_waiters();
    }

    async fn wait_if_held(&self) {
        if !self.enabled.load(Ordering::SeqCst) {
            return;
        }
        while self.enabled.load(Ordering::SeqCst) {
            let notified = self.release.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if !self.enabled.load(Ordering::SeqCst) {
                return;
            }
            if self
                .releases
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |count| {
                    count.checked_sub(1)
                })
                .is_ok()
            {
                return;
            }
            self.arrivals.fetch_add(1, Ordering::SeqCst);
            self.arrived.notify_waiters();
            notified.await;
        }
    }

    async fn wait_for_arrivals(&self, target: usize, timeout: std::time::Duration) {
        tokio::time::timeout(timeout, async {
            while self.arrivals.load(Ordering::SeqCst) < target {
                let notified = self.arrived.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if self.arrivals.load(Ordering::SeqCst) < target {
                    notified.await;
                }
            }
        })
        .await
        .expect("peer path hold arrival timed out");
    }
}

#[derive(Clone)]
struct PeerState {
    responses: Arc<Mutex<VecDeque<PeerResponse>>>,
    system_status_responses: Arc<Mutex<VecDeque<PeerResponse>>>,
    clients_self_responses: Arc<Mutex<VecDeque<PeerResponse>>>,
    about_responses: Arc<Mutex<VecDeque<PeerResponse>>>,
    relay_access_responses: Arc<Mutex<VecDeque<PeerResponse>>>,
    requests: Arc<Mutex<Vec<PeerRequest>>>,
    request_count: Arc<AtomicUsize>,
    clients_self_request_count: Arc<AtomicUsize>,
    relay_access_request_count: Arc<AtomicUsize>,
    system_status_request_count: Arc<AtomicUsize>,
    request_arrived: Arc<Notify>,
    clients_self_hold: Arc<PathHold>,
    handshake_hold: Arc<PathHold>,
    relay_access_hold: Arc<PathHold>,
    system_status_hold: Arc<PathHold>,
    answer_uploads_with_descriptors: Arc<AtomicBool>,
    withhold_credit: Arc<AtomicBool>,
    upload_stalled: Arc<Notify>,
    current_stream: Arc<AtomicU32>,
    accepted: Arc<AtomicUsize>,
    accepted_arrived: Arc<Notify>,
    active_carrier_handlers: Arc<AtomicUsize>,
    refusal_alert: Arc<AtomicU8>,
    expected_client_sha256: Arc<Mutex<Option<String>>>,
    migration_authority: Arc<MigrationAuthority>,
    migration_exchange: Arc<Mutex<MigrationExchange>>,
    migration_routes_supported: Arc<AtomicBool>,
    migration_decisions_supported: Arc<AtomicBool>,
    migration_protocol_version: Arc<AtomicU32>,
    lose_next_migration_decision_reply: Arc<AtomicBool>,
}

struct MigrationAuthority {
    ca: Certificate,
    ca_key: KeyPair,
    ca_pem: String,
    instance_id: String,
    source_cid: String,
}

#[derive(Default)]
struct MigrationExchange {
    operation_id: Option<String>,
    previous_cid: Option<String>,
    cid: Option<String>,
    rekey_response: Option<Vec<u8>>,
    decision_id: Option<String>,
    decision_response: Option<Vec<u8>>,
    state: Option<String>,
}

pub struct RelayServer {
    origin: String,
    task: JoinHandle<()>,
    refresh_requests: Arc<Mutex<Vec<String>>>,
}

impl RelayServer {
    pub fn origin(&self) -> &str {
        &self.origin
    }

    pub fn refresh_requests(&self) -> Vec<String> {
        lock(&self.refresh_requests).clone()
    }

    pub async fn shutdown(self) {
        self.task.abort();
        let _ = self.task.await;
    }
}

pub struct PrivateLinkPeer {
    credential: Credential,
    state: PeerState,
    controls: tokio::sync::broadcast::Sender<Control>,
    task: JoinHandle<()>,
}

impl PrivateLinkPeer {
    pub async fn start() -> Self {
        super::authority::verify_client_ingest_authority();
        Self::start_after_authority_validation(None).await
    }

    pub async fn start_with_authority_root(
        repository_root: &Path,
        bind_attempts: &AtomicUsize,
    ) -> Result<Self, String> {
        super::authority::verify_client_ingest_authority_at(repository_root)?;
        Ok(Self::start_after_authority_validation(Some(bind_attempts)).await)
    }

    async fn start_after_authority_validation(bind_attempts: Option<&AtomicUsize>) -> Self {
        if let Some(bind_attempts) = bind_attempts {
            bind_attempts.fetch_add(1, Ordering::SeqCst);
        }
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind private-link peer");
        let address = listener.local_addr().expect("read peer address");
        assert!(address.ip().is_loopback(), "peer did not bind loopback");

        let refusal_alert = Arc::new(AtomicU8::new(0));
        let (credential, acceptor, client_sha256, migration_authority) =
            credential_and_acceptor(address.port(), refusal_alert.clone());
        let state = PeerState {
            responses: Arc::new(Mutex::new(VecDeque::new())),
            system_status_responses: Arc::new(Mutex::new(VecDeque::new())),
            clients_self_responses: Arc::new(Mutex::new(VecDeque::new())),
            about_responses: Arc::new(Mutex::new(VecDeque::new())),
            relay_access_responses: Arc::new(Mutex::new(VecDeque::new())),
            requests: Arc::new(Mutex::new(Vec::new())),
            request_count: Arc::new(AtomicUsize::new(0)),
            clients_self_request_count: Arc::new(AtomicUsize::new(0)),
            relay_access_request_count: Arc::new(AtomicUsize::new(0)),
            system_status_request_count: Arc::new(AtomicUsize::new(0)),
            request_arrived: Arc::new(Notify::new()),
            clients_self_hold: Arc::new(PathHold::default()),
            handshake_hold: Arc::new(PathHold::default()),
            relay_access_hold: Arc::new(PathHold::default()),
            system_status_hold: Arc::new(PathHold::default()),
            answer_uploads_with_descriptors: Arc::new(AtomicBool::new(false)),
            withhold_credit: Arc::new(AtomicBool::new(false)),
            upload_stalled: Arc::new(Notify::new()),
            current_stream: Arc::new(AtomicU32::new(0)),
            accepted: Arc::new(AtomicUsize::new(0)),
            accepted_arrived: Arc::new(Notify::new()),
            active_carrier_handlers: Arc::new(AtomicUsize::new(0)),
            refusal_alert,
            expected_client_sha256: Arc::new(Mutex::new(Some(client_sha256))),
            migration_authority: Arc::new(migration_authority),
            migration_exchange: Arc::new(Mutex::new(MigrationExchange::default())),
            migration_routes_supported: Arc::new(AtomicBool::new(true)),
            migration_decisions_supported: Arc::new(AtomicBool::new(true)),
            migration_protocol_version: Arc::new(AtomicU32::new(1)),
            lose_next_migration_decision_reply: Arc::new(AtomicBool::new(false)),
        };
        let (controls, _) = tokio::sync::broadcast::channel(16);
        let task = tokio::spawn(serve(listener, acceptor, state.clone(), controls.clone()));

        Self {
            credential,
            state,
            controls,
            task,
        }
    }

    pub fn expected_client_sha256(&self) -> String {
        lock(&self.state.expected_client_sha256)
            .clone()
            .unwrap_or_default()
    }

    pub fn set_expected_client_sha256(&self, sha: Option<String>) {
        *lock(&self.state.expected_client_sha256) = sha;
    }

    pub fn set_migration_routes_supported(&self, supported: bool) {
        self.state
            .migration_routes_supported
            .store(supported, Ordering::SeqCst);
    }

    pub fn set_migration_decisions_supported(&self, supported: bool) {
        self.state
            .migration_decisions_supported
            .store(supported, Ordering::SeqCst);
    }

    pub fn set_migration_protocol_version(&self, version: u32) {
        self.state
            .migration_protocol_version
            .store(version, Ordering::SeqCst);
    }

    pub fn lose_next_migration_decision_reply(&self) {
        self.state
            .lose_next_migration_decision_reply
            .store(true, Ordering::SeqCst);
    }

    pub fn relay_credential(&self, relay_origin: &str) -> Credential {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let iat = now - 81;
        let exp = now + 19;
        let token = create_relay_jwt(&self.credential.instance_id, iat, exp);
        let mut cred = self.credential.clone();
        cred.endpoints = Vec::new();
        cred.local_endpoints = None;
        cred.relay_origin = Some(relay_origin.to_owned());
        cred.device_token = Some(token);
        cred.device_token_expires_at = Some(exp);
        cred
    }

    pub async fn start_relay_server(&self) -> RelayServer {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind relay server");
        let port = listener.local_addr().expect("relay addr").port();
        let origin = format!("http://127.0.0.1:{port}");
        let peer_port = self
            .credential
            .endpoints
            .first()
            .map(|e| e.port)
            .unwrap_or(0);
        let instance_id = self.credential.instance_id.clone();
        let refresh_requests = Arc::new(Mutex::new(Vec::new()));
        let refresh_requests_for_task = Arc::clone(&refresh_requests);
        let task = tokio::spawn(async move {
            loop {
                let Ok((tcp, _)) = listener.accept().await else {
                    return;
                };
                let instance_id = instance_id.clone();
                let refresh_requests = Arc::clone(&refresh_requests_for_task);
                tokio::spawn(async move {
                    let _ = handle_relay_connection(tcp, peer_port, &instance_id, refresh_requests)
                        .await;
                });
            }
        });
        RelayServer {
            origin,
            task,
            refresh_requests,
        }
    }

    pub fn answer_uploads_with_received_descriptors(&self) {
        self.state
            .answer_uploads_with_descriptors
            .store(true, Ordering::SeqCst);
    }

    pub fn credential(&self) -> Credential {
        self.credential.clone()
    }

    pub fn enqueue_response(&self, status: u16, body: impl Into<Vec<u8>>) {
        lock(&self.state.responses).push_back(PeerResponse::Structured {
            status,
            body: body.into(),
            delay: None,
        });
    }

    pub fn enqueue_delayed_response(
        &self,
        delay: std::time::Duration,
        status: u16,
        body: impl Into<Vec<u8>>,
    ) {
        lock(&self.state.responses).push_back(PeerResponse::Structured {
            status,
            body: body.into(),
            delay: Some(delay),
        });
    }

    pub fn enqueue_system_status_response(&self, status: u16, body: impl Into<Vec<u8>>) {
        lock(&self.state.system_status_responses).push_back(PeerResponse::Structured {
            status,
            body: body.into(),
            delay: None,
        });
    }

    pub fn enqueue_delayed_system_status_response(
        &self,
        delay: std::time::Duration,
        status: u16,
        body: impl Into<Vec<u8>>,
    ) {
        lock(&self.state.system_status_responses).push_back(PeerResponse::Structured {
            status,
            body: body.into(),
            delay: Some(delay),
        });
    }

    pub fn enqueue_about_response(&self, status: u16, body: impl Into<Vec<u8>>) {
        lock(&self.state.about_responses).push_back(PeerResponse::Structured {
            status,
            body: body.into(),
            delay: None,
        });
    }

    pub fn enqueue_about_pending_body(&self, body: impl Into<Vec<u8>>, delay: std::time::Duration) {
        lock(&self.state.about_responses).push_back(PeerResponse::DelayedBody {
            status: 200,
            body: body.into(),
            delay,
        });
    }

    pub fn enqueue_clients_self_response(&self, status: u16, body: impl Into<Vec<u8>>) {
        lock(&self.state.clients_self_responses).push_back(PeerResponse::Structured {
            status,
            body: body.into(),
            delay: None,
        });
    }

    pub fn enqueue_delayed_clients_self_response(
        &self,
        delay: std::time::Duration,
        status: u16,
        body: impl Into<Vec<u8>>,
    ) {
        lock(&self.state.clients_self_responses).push_back(PeerResponse::Structured {
            status,
            body: body.into(),
            delay: Some(delay),
        });
    }

    pub fn enqueue_relay_access_response(&self, status: u16, body: impl Into<Vec<u8>>) {
        lock(&self.state.relay_access_responses).push_back(PeerResponse::Structured {
            status,
            body: body.into(),
            delay: None,
        });
    }

    pub fn enqueue_delayed_relay_access_response(
        &self,
        delay: std::time::Duration,
        status: u16,
        body: impl Into<Vec<u8>>,
    ) {
        lock(&self.state.relay_access_responses).push_back(PeerResponse::Structured {
            status,
            body: body.into(),
            delay: Some(delay),
        });
    }

    pub fn enqueue_raw_response(&self, response: impl Into<Vec<u8>>) {
        lock(&self.state.responses).push_back(PeerResponse::Raw(response.into()));
    }

    pub fn requests(&self) -> Vec<PeerRequest> {
        lock(&self.state.requests).clone()
    }

    pub fn request_count(&self) -> usize {
        self.state.request_count.load(Ordering::SeqCst)
    }

    pub async fn wait_for_request_count(&self, target: usize, timeout: std::time::Duration) {
        tokio::time::timeout(timeout, async {
            while self.request_count() < target {
                let notified = self.state.request_arrived.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if self.request_count() < target {
                    notified.await;
                }
            }
        })
        .await
        .expect("peer request receipt timed out");
    }

    pub fn clients_self_request_count(&self) -> usize {
        self.state.clients_self_request_count.load(Ordering::SeqCst)
    }

    pub async fn wait_for_clients_self_request_count(
        &self,
        target: usize,
        timeout: std::time::Duration,
    ) {
        tokio::time::timeout(timeout, async {
            while self.clients_self_request_count() < target {
                let notified = self.state.request_arrived.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if self.clients_self_request_count() < target {
                    notified.await;
                }
            }
        })
        .await
        .expect("clients/self request receipt timed out");
    }

    pub fn relay_access_request_count(&self) -> usize {
        self.state.relay_access_request_count.load(Ordering::SeqCst)
    }

    pub async fn wait_for_relay_access_request_count(
        &self,
        target: usize,
        timeout: std::time::Duration,
    ) {
        tokio::time::timeout(timeout, async {
            while self.relay_access_request_count() < target {
                let notified = self.state.request_arrived.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if self.relay_access_request_count() < target {
                    notified.await;
                }
            }
        })
        .await
        .expect("relay access request receipt timed out");
    }

    pub fn system_status_request_count(&self) -> usize {
        self.state
            .system_status_request_count
            .load(Ordering::SeqCst)
    }

    pub async fn wait_for_system_status_request_count(
        &self,
        target: usize,
        timeout: std::time::Duration,
    ) {
        tokio::time::timeout(timeout, async {
            while self.system_status_request_count() < target {
                let notified = self.state.request_arrived.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if self.system_status_request_count() < target {
                    notified.await;
                }
            }
        })
        .await
        .expect("system status request receipt timed out");
    }

    pub fn hold_handshake(&self) {
        self.state.handshake_hold.hold();
    }
    pub async fn wait_for_held_handshake(&self, timeout: std::time::Duration) {
        self.state
            .handshake_hold
            .wait_for_arrivals(1, timeout)
            .await;
    }
    pub fn release_handshake(&self) {
        self.state.handshake_hold.release();
    }

    pub fn hold_clients_self(&self) {
        self.state.clients_self_hold.hold();
    }

    pub async fn wait_for_clients_self_hold(&self, timeout: std::time::Duration) {
        self.state
            .clients_self_hold
            .wait_for_arrivals(1, timeout)
            .await;
    }

    pub fn release_one_clients_self(&self) {
        self.state.clients_self_hold.release_one();
    }

    pub async fn wait_for_clients_self_hold_count(
        &self,
        target: usize,
        timeout: std::time::Duration,
    ) {
        self.state
            .clients_self_hold
            .wait_for_arrivals(target, timeout)
            .await;
    }

    pub fn release_clients_self(&self) {
        self.state.clients_self_hold.release();
    }

    pub fn hold_relay_access(&self) {
        self.state.relay_access_hold.hold();
    }

    pub async fn wait_for_relay_access_hold(&self, timeout: std::time::Duration) {
        self.state
            .relay_access_hold
            .wait_for_arrivals(1, timeout)
            .await;
    }

    pub fn release_relay_access(&self) {
        self.state.relay_access_hold.release();
    }

    pub fn hold_system_status(&self) {
        self.state.system_status_hold.hold();
    }

    pub async fn wait_for_system_status_hold(&self, timeout: std::time::Duration) {
        self.state
            .system_status_hold
            .wait_for_arrivals(1, timeout)
            .await;
    }

    pub fn release_system_status(&self) {
        self.state.system_status_hold.release();
    }

    pub fn close_accepted_carriers(&self) {
        let _ = self.controls.send(Control::CloseCarriers);
    }

    pub fn withhold_upload_credit(&self) {
        self.state.withhold_credit.store(true, Ordering::SeqCst);
    }

    pub async fn wait_for_upload_stall(&self) {
        self.state.upload_stalled.notified().await;
    }

    pub fn grant_upload_credit(&self, credit: u32) {
        let _ = self.controls.send(Control::GrantUploadCredit(credit));
    }

    /// Refuse every later client certificate after the TLS 1.3 handshake, the way a journal
    /// does: 49 (access denied), 46 (certificate unknown) or 48 (unknown CA, any other code).
    pub fn refuse_handshakes_with_alert(&self, description: u8) {
        self.state
            .refusal_alert
            .store(description, Ordering::SeqCst);
    }

    pub fn accepted_carriers(&self) -> usize {
        self.state.accepted.load(Ordering::SeqCst)
    }

    pub async fn wait_for_accepted_carrier_count(&self, target: usize) {
        while self.accepted_carriers() < target {
            let notified = self.state.accepted_arrived.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.accepted_carriers() < target {
                notified.await;
            }
        }
    }

    pub fn active_carrier_handlers(&self) -> usize {
        self.state.active_carrier_handlers.load(Ordering::SeqCst)
    }

    pub async fn shutdown(self) {
        self.task.abort();
        let _ = self.task.await;
    }
}

/// A journal-like client verifier: the real chain check, then an optional refusal.
#[derive(Debug)]
struct RefusingVerifier {
    inner: Arc<dyn ClientCertVerifier>,
    refusal: Arc<AtomicU8>,
}

impl ClientCertVerifier for RefusingVerifier {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        self.inner.root_hint_subjects()
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        let verified = self
            .inner
            .verify_client_cert(end_entity, intermediates, now)?;
        let error = match self.refusal.load(Ordering::SeqCst) {
            0 => return Ok(verified),
            49 => CertificateError::ApplicationVerificationFailure,
            46 => CertificateError::Other(OtherError(Arc::new(io::Error::other(
                "authorization unreadable",
            )))),
            48 => CertificateError::UnknownIssuer,
            other => panic!("unsupported refusal alert {other}"),
        };
        Err(rustls::Error::InvalidCertificate(error))
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

fn credential_and_acceptor(
    port: u16,
    refusal: Arc<AtomicU8>,
) -> (Credential, TlsAcceptor, String, MigrationAuthority) {
    let ca_key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("generate peer CA key");
    let mut ca_params =
        CertificateParams::new(Vec::<String>::new()).expect("construct peer CA parameters");
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.key_usages.push(KeyUsagePurpose::DigitalSignature);
    ca_params.key_usages.push(KeyUsagePurpose::KeyCertSign);
    let ca = ca_params.self_signed(&ca_key).expect("sign peer CA");

    let server_key =
        KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("generate peer server key");
    let mut server_params = CertificateParams::new(vec!["spl.local".to_owned()])
        .expect("construct peer server parameters");
    server_params
        .extended_key_usages
        .push(ExtendedKeyUsagePurpose::ServerAuth);
    let server = server_params
        .signed_by(&server_key, &ca, &ca_key)
        .expect("sign peer server certificate");

    let client_key =
        KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("generate peer client key");
    let mut client_params = CertificateParams::new(vec!["observer.test".to_owned()])
        .expect("construct peer client parameters");
    client_params
        .extended_key_usages
        .push(ExtendedKeyUsagePurpose::ClientAuth);
    let client = client_params
        .signed_by(&client_key, &ca, &ca_key)
        .expect("sign peer client certificate");

    let ca_der = CertificateDer::from(ca.der().to_vec());
    let mut roots = RootCertStore::empty();
    roots.add(ca_der.clone()).expect("trust peer CA");
    let verifier = WebPkiClientVerifier::builder(Arc::new(roots))
        .build()
        .expect("build peer client verifier");
    let server_config =
        ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .expect("select peer TLS versions")
            .with_client_cert_verifier(Arc::new(RefusingVerifier {
                inner: verifier,
                refusal,
            }))
            .with_single_cert(
                vec![CertificateDer::from(server.der().to_vec()), ca_der.clone()],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(server_key.serialize_der())),
            )
            .expect("build peer TLS server");
    let pin = spl_core::ca::sha256(ca_der.as_ref())[..16].to_vec();
    let client_sha256 = spl_core::ca::sha256_hex(client.der());
    let source_cid = format!("sha256:{client_sha256}");
    let migration_ca_pem = ca.pem();
    let credential = Credential {
        client_key_pem: client_key.serialize_pem(),
        client_cert_pem: client.pem(),
        ca_chain_pem: vec![migration_ca_pem.clone()],
        ca_fp_prefix: pin,
        instance_id: "test-private-link-instance".to_owned(),
        home_label: "test home".to_owned(),
        endpoints: vec![EndpointAddr {
            host: "127.0.0.1".to_owned(),
            port,
        }],
        home_attestation: None,
        local_endpoints: None,
        relay_origin: None,
        device_token: None,
        device_token_expires_at: None,
    };
    let migration_authority = MigrationAuthority {
        ca,
        ca_key,
        ca_pem: migration_ca_pem,
        instance_id: "test-private-link-instance".to_owned(),
        source_cid,
    };
    (
        credential,
        TlsAcceptor::from(Arc::new(server_config)),
        client_sha256,
        migration_authority,
    )
}

fn migration_route_response(state: &PeerState, request: &PeerRequest) -> PeerResponse {
    if !state.migration_routes_supported.load(Ordering::SeqCst) {
        return migration_reply(404, Vec::new());
    }
    match (request.method(), request.path_without_query()) {
        ("POST", "/app/network/api/clients/self/rekey") => {
            #[derive(serde::Deserialize)]
            #[serde(deny_unknown_fields)]
            struct RekeyRequest {
                protocol_version: u32,
                operation_id: String,
                csr: String,
                device_label: String,
                client_label: String,
                platform: String,
            }

            let Ok(body) = serde_json::from_slice::<RekeyRequest>(request.body()) else {
                return migration_reply(400, Vec::new());
            };
            if body.protocol_version != 1
                || body.operation_id.is_empty()
                || body.csr.is_empty()
                || body.device_label.is_empty()
                || body.client_label.is_empty()
                || !matches!(body.platform.as_str(), "linux" | "macos")
            {
                return migration_reply(400, Vec::new());
            }
            let mut exchange = lock(&state.migration_exchange);
            if let Some(existing) = exchange.rekey_response.as_ref() {
                if exchange.operation_id.as_deref() == Some(body.operation_id.as_str()) {
                    return migration_reply(200, existing.clone());
                }
                if exchange.state.as_deref() != Some("new_device") {
                    return migration_reply(409, Vec::new());
                }
            }
            let previous_cid = if exchange.state.as_deref() == Some("new_device") {
                exchange
                    .cid
                    .clone()
                    .unwrap_or_else(|| state.migration_authority.source_cid.clone())
            } else {
                state.migration_authority.source_cid.clone()
            };
            let Ok(csr) = CertificateSigningRequestParams::from_pem(&body.csr) else {
                return migration_reply(400, Vec::new());
            };
            let Ok(client_cert) = csr.signed_by(
                &state.migration_authority.ca,
                &state.migration_authority.ca_key,
            ) else {
                return migration_reply(400, Vec::new());
            };
            let cid = format!("sha256:{}", spl_core::ca::sha256_hex(client_cert.der()));
            let response = serde_json::json!({
                "protocol_version": state.migration_protocol_version.load(Ordering::SeqCst),
                "operation_id": body.operation_id,
                "state": "pending",
                "previous_cid": previous_cid,
                "cid": cid,
                "pairing": {
                    "client_cert": client_cert.pem(),
                    "ca_chain": [state.migration_authority.ca_pem],
                    "instance_id": state.migration_authority.instance_id,
                    "home_label": "test migration home",
                    "fingerprint": cid,
                    "home_attestation": null
                }
            });
            let bytes = serde_json::to_vec(&response).expect("serialize rekey response");
            exchange.operation_id = Some(body.operation_id);
            exchange.previous_cid = Some(previous_cid);
            exchange.cid = Some(cid);
            exchange.rekey_response = Some(bytes.clone());
            exchange.state = Some("pending".to_owned());
            migration_reply(201, bytes)
        }
        ("GET", "/app/network/api/clients/self/migration") => {
            let exchange = lock(&state.migration_exchange);
            let body = serde_json::json!({
                "protocol_version": state.migration_protocol_version.load(Ordering::SeqCst),
                "rekey_operation_id": exchange.operation_id,
                "previous_cid": exchange.previous_cid,
                "state": exchange.state.as_deref().unwrap_or("none"),
                "replaced_cid": null
            });
            migration_reply(200, serde_json::to_vec(&body).expect("serialize GET state"))
        }
        ("PUT", "/app/network/api/clients/self/migration") => {
            if !state.migration_decisions_supported.load(Ordering::SeqCst) {
                return migration_reply(404, Vec::new());
            }
            #[derive(serde::Deserialize)]
            #[serde(deny_unknown_fields)]
            struct DecisionRequest {
                protocol_version: u32,
                operation_id: String,
                choice: String,
            }

            let Ok(body) = serde_json::from_slice::<DecisionRequest>(request.body()) else {
                return migration_reply(400, Vec::new());
            };
            let mut exchange = lock(&state.migration_exchange);
            if body.protocol_version != 1
                || body.choice != "new_device"
                || exchange.operation_id.is_none()
            {
                return migration_reply(409, Vec::new());
            }
            if exchange.state.as_deref() == Some("new_device") {
                if exchange.decision_id.as_deref() == Some(body.operation_id.as_str()) {
                    return migration_reply(
                        200,
                        exchange.decision_response.clone().unwrap_or_default(),
                    );
                }
                return migration_reply(409, Vec::new());
            }
            let response = serde_json::json!({
                "protocol_version": state.migration_protocol_version.load(Ordering::SeqCst),
                "operation_id": body.operation_id,
                "state": "new_device",
                "previous_cid": exchange.previous_cid,
                "cid": exchange.cid,
                "replaced_cid": null,
                "display_label": "test migration device"
            });
            let bytes = serde_json::to_vec(&response).expect("serialize decision response");
            exchange.decision_id = Some(body.operation_id);
            exchange.decision_response = Some(bytes.clone());
            exchange.state = Some("new_device".to_owned());
            if state
                .lose_next_migration_decision_reply
                .swap(false, Ordering::SeqCst)
            {
                return PeerResponse::Raw(
                    b"HTTP/1.1 200 OK\r\ncontent-length: 100\r\n\r\n{".to_vec(),
                );
            }
            migration_reply(200, bytes)
        }
        _ => migration_reply(404, Vec::new()),
    }
}

fn migration_reply(status: u16, body: Vec<u8>) -> PeerResponse {
    PeerResponse::Structured {
        status,
        body,
        delay: None,
    }
}

struct ActiveHandler {
    counter: Arc<AtomicUsize>,
}

impl ActiveHandler {
    fn enter(state: &PeerState) -> Self {
        state.active_carrier_handlers.fetch_add(1, Ordering::SeqCst);
        Self {
            counter: Arc::clone(&state.active_carrier_handlers),
        }
    }
}

impl Drop for ActiveHandler {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::SeqCst);
    }
}

async fn serve(
    listener: TcpListener,
    acceptor: TlsAcceptor,
    state: PeerState,
    controls: tokio::sync::broadcast::Sender<Control>,
) {
    loop {
        let Ok((tcp, _)) = listener.accept().await else {
            return;
        };
        state.accepted.fetch_add(1, Ordering::SeqCst);
        state.accepted_arrived.notify_waiters();
        state.handshake_hold.wait_if_held().await;
        let Ok(tls) = acceptor.accept(tcp).await else {
            continue;
        };
        let state = state.clone();
        let control_rx = controls.subscribe();
        tokio::spawn(async move {
            let _guard = ActiveHandler::enter(&state);
            let _ = handle_carrier(tls, state, control_rx).await;
        });
    }
}

async fn handle_carrier(
    tls: TlsStream<TcpStream>,
    state: PeerState,
    mut controls: tokio::sync::broadcast::Receiver<Control>,
) -> io::Result<()> {
    let authenticated_client_sha256 = tls
        .get_ref()
        .1
        .peer_certificates()
        .and_then(|certificates| certificates.first())
        .map(|certificate| spl_core::ca::sha256_hex(certificate.as_ref()));
    let (mut reader, mut writer) = tokio::io::split(tls);
    let mut decoder = FrameDecoder::new();
    let mut request_bytes: HashMap<u32, Vec<u8>> = HashMap::new();
    let mut outbound: HashMap<u32, OutboundResponse> = HashMap::new();
    let mut read_buffer = [0u8; 16 * 1024];
    let mut pending_upload_credit = 0u32;

    loop {
        let next_delayed = outbound.values().filter_map(|r| r.deliver_at).min();
        tokio::select! {
            _ = async {
                match next_delayed {
                    Some(instant) => tokio::time::sleep_until(instant).await,
                    None => std::future::pending().await,
                }
            } => {
                let now = tokio::time::Instant::now();
                let ready_stream_ids: Vec<u32> = outbound
                    .iter()
                    .filter_map(|(id, resp)| {
                        if resp.deliver_at.is_some_and(|inst| inst <= now) {
                            Some(*id)
                        } else {
                            None
                        }
                    })
                    .collect();
                for stream_id in ready_stream_ids {
                    if let Some(mut response) = outbound.remove(&stream_id) {
                        response.deliver_at = None;
                        flush_response(&mut writer, stream_id, &mut response).await?;
                        if response.offset != response.bytes.len() {
                            outbound.insert(stream_id, response);
                        }
                    }
                }
            }
            read = reader.read(&mut read_buffer) => {
                let count = read?;
                if count == 0 {
                    return Ok(());
                }
                decoder.feed(&read_buffer[..count]);
                let frames = decoder.drain().map_err(|_| io::Error::other("peer frame decode failed"))?;
                for frame in frames {
                    if let Some(pong) = frame.control_pong() {
                        write_frame(&mut writer, pong).await?;
                        continue;
                    }
                    let stream_id = frame.stream_id;
                    if frame.flags & FLAG_OPEN != 0 {
                        request_bytes.entry(stream_id).or_default();
                        state.current_stream.store(stream_id, Ordering::SeqCst);
                        if pending_upload_credit != 0 {
                            write_frame(&mut writer, Frame::window(stream_id, pending_upload_credit)).await?;
                            pending_upload_credit = 0;
                        }
                    }
                    if frame.flags & FLAG_WINDOW != 0
                        && let (Some(credit), Some(response)) =
                            (frame.window_credit(), outbound.get_mut(&stream_id))
                    {
                        response.credit = response.credit.saturating_add(credit as usize);
                        if response.deliver_at.is_none() {
                            flush_response(&mut writer, stream_id, response).await?;
                            if response.offset == response.bytes.len() {
                                outbound.remove(&stream_id);
                            }
                        }
                    }
                    if frame.flags & FLAG_DATA != 0 {
                        if state.withhold_credit.load(Ordering::SeqCst) {
                            state.upload_stalled.notify_one();
                        }
                        request_bytes
                            .entry(stream_id)
                            .or_default()
                            .extend_from_slice(&frame.payload);
                        state.current_stream.store(stream_id, Ordering::SeqCst);
                        if !state.withhold_credit.load(Ordering::SeqCst)
                            && !frame.payload.is_empty()
                        {
                            let credit = u32::try_from(frame.payload.len())
                                .map_err(|_| io::Error::other("peer upload frame too large"))?;
                            write_frame(&mut writer, Frame::window(stream_id, credit)).await?;
                        }
                    }
                    if frame.flags & FLAG_CLOSE != 0 {
                        let _ = state.current_stream.compare_exchange(
                            stream_id,
                            0,
                            Ordering::SeqCst,
                            Ordering::SeqCst,
                        );
                        let raw = request_bytes.remove(&stream_id).unwrap_or_default();
                        let parsed = parse_request(&raw);
                        let path = parsed.as_ref().map(|req| req.path_without_query().to_string());
                        let is_system_status = path.as_deref() == Some("/api/system/status");
                        let is_clients_self = path.as_deref() == Some("/app/network/api/clients/self");
                        let is_about = path.as_deref() == Some("/api/system/about");
                        let is_relay_access = path.as_deref() == Some("/app/network/api/relay/access");
                        let is_migration_route = path.as_deref().is_some_and(|path| matches!(
                            path,
                            "/app/network/api/clients/self/rekey"
                                | "/app/network/api/clients/self/migration"
                        ));
                        state.request_count.fetch_add(1, Ordering::SeqCst);
                        if is_clients_self {
                            state
                                .clients_self_request_count
                                .fetch_add(1, Ordering::SeqCst);
                        } else if is_relay_access {
                            state
                                .relay_access_request_count
                                .fetch_add(1, Ordering::SeqCst);
                        } else if is_system_status {
                            state
                                .system_status_request_count
                                .fetch_add(1, Ordering::SeqCst);
                        }
                        state.request_arrived.notify_waiters();
                        if is_clients_self {
                            state.clients_self_hold.wait_if_held().await;
                        } else if is_relay_access {
                            state.relay_access_hold.wait_if_held().await;
                        } else if is_system_status {
                            state.system_status_hold.wait_if_held().await;
                        }
                        let is_delete = parsed
                            .as_ref()
                            .map(|req| req.method() == "DELETE")
                            .unwrap_or(false);
                        let is_upload = parsed
                            .as_ref()
                            .map(|req| {
                                req.method() == "POST"
                                    && req.path_without_query() == "/app/devices/ingest"
                            })
                            .unwrap_or(false);
                        let response = if is_migration_route {
                            parsed
                                .as_ref()
                                .map(|request| migration_route_response(&state, request))
                                .unwrap_or_else(|| migration_reply(400, Vec::new()))
                        } else if is_upload {
                            if let Some(queued) = lock(&state.responses).pop_front() {
                                queued
                            } else if state.answer_uploads_with_descriptors.load(Ordering::SeqCst) {
                                if let Some(ref req) = parsed {
                                    let content_type = req.header("content-type").unwrap_or_default();
                                    if let Ok(parts) = parse_multipart_parts(content_type, req.body()) {
                                        let mut segment_key = "143000_1".to_string();
                                        if let Some(env_part) = parts.first()
                                            && let Ok(val) =
                                                serde_json::from_slice::<serde_json::Value>(env_part.body)
                                            && let Some(seg) =
                                                val.get("segment").and_then(|s| s.as_str())
                                        {
                                            segment_key = seg.to_string();
                                        }
                                        let mut descriptors = Vec::new();
                                        for part in &parts[1..] {
                                            let filename = part.filename.clone().unwrap_or_default();
                                            let digest = spl_core::ca::sha256(part.body);
                                            let sha256_hex =
                                                solstone_tmux::journal_version::hex_encode(&digest);
                                            descriptors.push(serde_json::json!({
                                                "submitted": filename,
                                                "written": filename,
                                                "size": part.body.len(),
                                                "sha256": sha256_hex,
                                                "disposition": "written",
                                            }));
                                        }
                                        let body = serde_json::json!({
                                            "status": "ok",
                                            "segment": segment_key,
                                            "file_descriptors": descriptors,
                                        });
                                        PeerResponse::Structured {
                                            status: 200,
                                            body: serde_json::to_vec(&body).unwrap_or_default(),
                                            delay: None,
                                        }
                                    } else {
                                        PeerResponse::Structured {
                                            status: 400,
                                            body: Vec::new(),
                                            delay: None,
                                        }
                                    }
                                } else {
                                    PeerResponse::Structured {
                                        status: 400,
                                        body: Vec::new(),
                                        delay: None,
                                    }
                                }
                            } else {
                                lock(&state.responses)
                                    .pop_front()
                                    .unwrap_or(PeerResponse::Structured {
                                        status: 500,
                                        body: Vec::new(),
                                        delay: None,
                                    })
                            }
                        } else if is_system_status {
                            lock(&state.system_status_responses)
                                .back()
                                .cloned()
                                .unwrap_or(PeerResponse::Structured {
                                    status: 500,
                                    body: Vec::new(),
                                    delay: None,
                                    })
                        } else if is_about {
                            lock(&state.about_responses).pop_front().unwrap_or(PeerResponse::Structured { status: 404, body: Vec::new(), delay: None })
                        } else if is_clients_self {
                            lock(&state.clients_self_responses)
                                .pop_front()
                                .unwrap_or(PeerResponse::Structured {
                                    status: 404,
                                    body: Vec::new(),
                                    delay: None,
                                })
                        } else if is_relay_access {
                            lock(&state.relay_access_responses)
                                .pop_front()
                                .unwrap_or(PeerResponse::Structured {
                                    status: 404,
                                    body: Vec::new(),
                                    delay: None,
                                })
                        } else if is_delete {
                            if let Some(queued) = lock(&state.responses).pop_front() {
                                queued
                            } else {
                                let matches = match (
                                    parsed.as_ref(),
                                    lock(&state.expected_client_sha256).as_deref(),
                                ) {
                                    (Some(req), Some(expected)) => {
                                        req.path_without_query()
                                            == format!("/app/network/api/clients/sha256:{expected}")
                                    }
                                    _ => false,
                                };
                                if matches {
                                    PeerResponse::Structured {
                                        status: 200,
                                        body: Vec::new(),
                                        delay: None,
                                    }
                                } else {
                                    PeerResponse::Structured {
                                        status: 404,
                                        body: Vec::new(),
                                        delay: None,
                                    }
                                }
                            }
                        } else {
                            lock(&state.responses)
                                .pop_front()
                                .unwrap_or(PeerResponse::Structured {
                                    status: 500,
                                    body: Vec::new(),
                                    delay: None,
                                })
                        };
                        let response_status = match &response {
                            PeerResponse::Structured { status, .. } | PeerResponse::DelayedBody { status, .. } => Some(*status),
                            PeerResponse::Raw(_) => None,
                        };
                        if let (Some(mut request), false) = (parsed, is_system_status) {
                            request.response_status = response_status;
                            request.authenticated_client_sha256 = authenticated_client_sha256.clone();
                            lock(&state.requests).push(request);
                        }
                        let pending_body = matches!(&response, PeerResponse::DelayedBody { .. });
                        let deliver_at = match &response {
                            PeerResponse::DelayedBody { delay, .. } => Some(tokio::time::Instant::now() + *delay),
                            PeerResponse::Structured {
                                delay: Some(delay), ..
                            } => Some(tokio::time::Instant::now() + *delay),
                            _ => None,
                        };
                        let mut output = OutboundResponse {
                            bytes: match response {
                                PeerResponse::Structured { status, body, .. } | PeerResponse::DelayedBody { status, body, .. } => {
                                    encode_response(status, body)
                                }
                                PeerResponse::Raw(bytes) => bytes,
                            },
                            offset: 0,
                            credit: INITIAL_WINDOW,
                            deliver_at,
                        };
                        if pending_body {
                            let header_len = output.bytes.windows(4).position(|bytes| bytes == b"\r\n\r\n").unwrap() + 4;
                            write_frame(&mut writer, Frame::new(stream_id, FLAG_DATA, output.bytes[..header_len].to_vec())).await?;
                            output.offset = header_len;
                            output.credit -= header_len;
                        }
                        if deliver_at.is_none() {
                            flush_response(&mut writer, stream_id, &mut output).await?;
                        }
                        if output.offset != output.bytes.len() {
                            outbound.insert(stream_id, output);
                        }
                    }
                    if frame.flags & FLAG_RESET != 0 {
                        request_bytes.remove(&stream_id);
                        outbound.remove(&stream_id);
                        let _ = state.current_stream.compare_exchange(
                            stream_id,
                            0,
                            Ordering::SeqCst,
                            Ordering::SeqCst,
                        );
                    }
                }
            }
            control = controls.recv() => {
                let Ok(control) = control else {
                    return Ok(());
                };
                match control {
                    Control::GrantUploadCredit(credit) => {
                        let stream_id = state.current_stream.load(Ordering::SeqCst);
                        if stream_id == 0 {
                            pending_upload_credit = pending_upload_credit.saturating_add(credit);
                        } else {
                            write_frame(&mut writer, Frame::window(stream_id, credit)).await?;
                        }
                    }
                    Control::CloseCarriers => return Ok(()),
                }
            }
        }
    }
}

async fn flush_response(
    writer: &mut WriteHalf<TlsStream<TcpStream>>,
    stream_id: u32,
    response: &mut OutboundResponse,
) -> io::Result<()> {
    while response.offset < response.bytes.len() && response.credit > 0 {
        let remaining = response.bytes.len() - response.offset;
        let count = remaining.min(RECOMMENDED_CHUNK).min(response.credit);
        let end = response.offset + count;
        let is_last = end == response.bytes.len();
        let flags = if is_last {
            FLAG_DATA | FLAG_CLOSE
        } else {
            FLAG_DATA
        };
        write_frame(
            writer,
            Frame::new(
                stream_id,
                flags,
                response.bytes[response.offset..end].to_vec(),
            ),
        )
        .await?;
        response.offset = end;
        response.credit -= count;
    }
    Ok(())
}

async fn write_frame(writer: &mut WriteHalf<TlsStream<TcpStream>>, frame: Frame) -> io::Result<()> {
    let bytes = frame
        .encode()
        .map_err(|_| io::Error::other("peer frame encode failed"))?;
    writer.write_all(&bytes).await?;
    writer.flush().await
}

fn encode_response(status: u16, body: Vec<u8>) -> Vec<u8> {
    let reason = match status {
        200 => "OK",
        201 => "Created",
        301 => "Moved Permanently",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        409 => "Conflict",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Response",
    };
    let head = if status == 301 {
        format!(
            "HTTP/1.1 301 Moved Permanently\r\nlocation: /redirected\r\ncontent-length: {}\r\n\r\n",
            body.len()
        )
    } else {
        format!(
            "HTTP/1.1 {} {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n",
            status,
            reason,
            body.len()
        )
    };
    let mut bytes = head.into_bytes();
    bytes.extend_from_slice(&body);
    bytes
}

fn parse_request(raw: &[u8]) -> Option<PeerRequest> {
    let split = raw.windows(4).position(|window| window == b"\r\n\r\n")?;
    let head = std::str::from_utf8(&raw[..split]).ok()?;
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next()?.split_whitespace();
    let method = request_line.next()?.to_owned();
    let path = request_line.next()?.to_owned();
    if request_line.next()?.get(..5)? != "HTTP/" || request_line.next().is_some() {
        return None;
    }
    let headers = lines
        .map(|line| {
            let (name, value) = line.split_once(':')?;
            Some((name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        })
        .collect::<Option<Vec<_>>>()?;
    Some(PeerRequest {
        method,
        path,
        headers,
        body: raw[split + 4..].to_vec(),
        response_status: None,
        authenticated_client_sha256: None,
    })
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[derive(Clone, Debug)]
pub struct MultipartPart<'a> {
    pub name: String,
    pub filename: Option<String>,
    pub content_type: String,
    pub body: &'a [u8],
}

pub fn parse_multipart_parts<'a>(
    content_type: &str,
    body: &'a [u8],
) -> Result<Vec<MultipartPart<'a>>, String> {
    let boundary = content_type
        .split(';')
        .map(str::trim)
        .find_map(|parameter| parameter.strip_prefix("boundary="))
        .map(|boundary| boundary.trim_matches('"'))
        .filter(|boundary| !boundary.is_empty())
        .ok_or_else(|| "multipart boundary is missing".to_owned())?;
    let opening = format!("--{boundary}\r\n");
    let separator = format!("\r\n--{boundary}");
    let mut remainder = body
        .strip_prefix(opening.as_bytes())
        .ok_or_else(|| "multipart body has no opening boundary".to_owned())?;
    let mut parts = Vec::new();
    loop {
        let separator_offset = find_bytes(remainder, separator.as_bytes())
            .ok_or_else(|| "multipart part has no closing boundary".to_owned())?;
        parts.push(parse_multipart_part(&remainder[..separator_offset])?);
        remainder = &remainder[separator_offset + separator.len()..];
        if remainder == b"--\r\n" {
            return Ok(parts);
        }
        remainder = remainder
            .strip_prefix(b"\r\n")
            .ok_or_else(|| "multipart boundary is malformed".to_owned())?;
    }
}

pub fn parse_multipart_part(bytes: &[u8]) -> Result<MultipartPart<'_>, String> {
    let headers_end = find_bytes(bytes, b"\r\n\r\n")
        .ok_or_else(|| "multipart part has no header separator".to_owned())?;
    let headers = std::str::from_utf8(&bytes[..headers_end])
        .map_err(|_| "multipart headers are not UTF-8".to_owned())?;
    let disposition = headers
        .split("\r\n")
        .find_map(|header| header.strip_prefix("Content-Disposition: "))
        .ok_or_else(|| "multipart part has no content disposition".to_owned())?;
    let content_type = headers
        .split("\r\n")
        .find_map(|header| header.strip_prefix("Content-Type: "))
        .ok_or_else(|| "multipart part has no content type".to_owned())?
        .to_owned();
    if !disposition.starts_with("form-data") {
        return Err("multipart disposition is not form-data".to_owned());
    }
    let name = multipart_disposition_parameter(disposition, "name")
        .ok_or_else(|| "multipart part has no name".to_owned())?;
    Ok(MultipartPart {
        name,
        filename: multipart_disposition_parameter(disposition, "filename"),
        content_type,
        body: &bytes[headers_end + b"\r\n\r\n".len()..],
    })
}

pub fn multipart_disposition_parameter(disposition: &str, parameter: &str) -> Option<String> {
    disposition.split(';').skip(1).find_map(|attribute| {
        let (name, value) = attribute.trim().split_once('=')?;
        (name == parameter).then(|| value.trim_matches('"').to_owned())
    })
}

pub fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    (!needle.is_empty())
        .then(|| {
            haystack
                .windows(needle.len())
                .position(|window| window == needle)
        })
        .flatten()
}

fn create_relay_jwt(instance_id: &str, iat: i64, exp: i64) -> String {
    let header = "eyJhbGciOiJFUzI1NiIsInR5cCI6IkpXVCJ9"; // {"alg":"ES256","typ":"JWT"}
    let claims = json!({
        "iss": "solstone",
        "sub": format!("instance:{instance_id}"),
        "aud": "spl-relay",
        "scope": "session.dial",
        "ver": 2,
        "instance_id": instance_id,
        "iat": iat,
        "exp": exp,
        "jti": "jwt-id-12345"
    });
    let claims_bytes = serde_json::to_vec(&claims).expect("json");
    let mut claims_b64 = String::new();
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
    format!("{header}.{claims_b64}.fake_sig")
}

async fn handle_relay_connection(
    mut tcp: TcpStream,
    peer_port: u16,
    instance_id: &str,
    refresh_requests: Arc<Mutex<Vec<String>>>,
) -> io::Result<()> {
    let mut peek = [0u8; 512];
    let n = tcp.peek(&mut peek).await?;
    let peek_str = String::from_utf8_lossy(&peek[..n]);
    if peek_str.starts_with("GET ") {
        let ws = tokio_tungstenite::accept_async(tcp)
            .await
            .map_err(io::Error::other)?;
        let mut peer_tcp = TcpStream::connect(("127.0.0.1", peer_port)).await?;
        let (relay_side, mut home_side) = tokio::io::duplex(64 * 1024);
        tokio::spawn(async move {
            let _ = pump_ws(ws, relay_side).await;
        });
        // A client disconnect has to half-close the journal socket. Leaving
        // that socket open until the journal writes keeps the carrier handler
        // running when the DELETE is never answered.
        let _ = tokio::io::copy_bidirectional(&mut home_side, &mut peer_tcp).await;
        Ok(())
    } else {
        let mut buf = [0u8; 4096];
        let n = tcp.read(&mut buf).await?;
        let req_str = String::from_utf8_lossy(&buf[..n]);
        let line = req_str.lines().next().unwrap_or("");
        let path = line.split_whitespace().nth(1).unwrap_or("/");
        if path.starts_with("/token/refresh") {
            lock(&refresh_requests).push(req_str.to_string());
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64;
            let fresh_exp = now + 3600;
            let fresh_token = create_relay_jwt(instance_id, now, fresh_exp);
            let expires_at_rfc3339 = time::OffsetDateTime::from_unix_timestamp(fresh_exp)
                .unwrap()
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap();
            let body = json!({
                "protocol_version": 2,
                "device_token": fresh_token,
                "expires_at": expires_at_rfc3339,
            })
            .to_string();
            let resp = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            tcp.write_all(resp.as_bytes()).await?;
        } else {
            let resp = "HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";
            tcp.write_all(resp.as_bytes()).await?;
        }
        Ok(())
    }
}

async fn pump_ws(
    ws: WebSocketStream<TcpStream>,
    relay_side: tokio::io::DuplexStream,
) -> io::Result<()> {
    let (mut ws_sink, mut ws_stream) = ws.split();
    let (mut relay_read, mut relay_write) = tokio::io::split(relay_side);

    let to_inner = async move {
        while let Some(message) = ws_stream.next().await {
            match message.map_err(io::Error::other)? {
                Message::Binary(bytes) => {
                    relay_write.write_all(&bytes).await?;
                    relay_write.flush().await?;
                }
                Message::Close(_) => {
                    let _ = relay_write.shutdown().await;
                    return Ok(());
                }
                Message::Ping(_) | Message::Pong(_) => {}
                Message::Text(_) | Message::Frame(_) => {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "bad ws message"));
                }
            }
        }
        Ok(())
    };

    let to_ws = async move {
        let mut buf = [0u8; 4096];
        loop {
            let n = relay_read.read(&mut buf).await?;
            if n == 0 {
                let _ = ws_sink.close().await;
                return Ok(());
            }
            ws_sink
                .send(Message::Binary(buf[..n].to_vec().into()))
                .await
                .map_err(io::Error::other)?;
        }
    };

    tokio::select! {
        result = to_inner => result,
        result = to_ws => result,
    }
}
