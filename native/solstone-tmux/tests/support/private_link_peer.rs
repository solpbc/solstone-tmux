// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::collections::{HashMap, VecDeque};
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use rcgen::{
    BasicConstraints, CertificateParams, ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose,
    PKCS_ECDSA_P256_SHA256,
};
use rustls::client::danger::HandshakeSignatureValid;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, UnixTime};
use rustls::server::WebPkiClientVerifier;
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{
    CertificateError, DigitallySignedStruct, DistinguishedName, OtherError, RootCertStore,
    ServerConfig, SignatureScheme,
};
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

#[derive(Clone)]
pub struct PeerRequest {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
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
}

#[derive(Clone)]
enum PeerResponse {
    Structured {
        status: u16,
        body: Vec<u8>,
        delay: Option<std::time::Duration>,
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
    refusal_alert: Arc<AtomicU8>,
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
        let (credential, acceptor) = credential_and_acceptor(address.port(), refusal_alert.clone());
        let state = PeerState {
            responses: Arc::new(Mutex::new(VecDeque::new())),
            system_status_responses: Arc::new(Mutex::new(VecDeque::new())),
            clients_self_responses: Arc::new(Mutex::new(VecDeque::new())),
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
            refusal_alert,
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

fn credential_and_acceptor(port: u16, refusal: Arc<AtomicU8>) -> (Credential, TlsAcceptor) {
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
    let credential = Credential {
        client_key_pem: client_key.serialize_pem(),
        client_cert_pem: client.pem(),
        ca_chain_pem: vec![ca.pem()],
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
    (credential, TlsAcceptor::from(Arc::new(server_config)))
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
        state.handshake_hold.wait_if_held().await;
        let Ok(tls) = acceptor.accept(tcp).await else {
            continue;
        };
        let state = state.clone();
        let control_rx = controls.subscribe();
        tokio::spawn(async move {
            let _ = handle_carrier(tls, state, control_rx).await;
        });
    }
}

async fn handle_carrier(
    tls: TlsStream<TcpStream>,
    state: PeerState,
    mut controls: tokio::sync::broadcast::Receiver<Control>,
) -> io::Result<()> {
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
                        let is_relay_access = path.as_deref() == Some("/app/network/api/relay/access");
                        if let (Some(request), false) = (&parsed, is_system_status) {
                            lock(&state.requests).push(request.clone());
                        }
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
                        let is_upload = parsed
                            .as_ref()
                            .map(|req| {
                                req.method() == "POST"
                                    && req.path_without_query() == "/app/devices/ingest"
                            })
                            .unwrap_or(false);
                        let response = if is_upload {
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
                        } else {
                            lock(&state.responses)
                                .pop_front()
                                .unwrap_or(PeerResponse::Structured {
                                    status: 500,
                                    body: Vec::new(),
                                    delay: None,
                                })
                        };
                        let deliver_at = match &response {
                            PeerResponse::Structured {
                                delay: Some(delay), ..
                            } => Some(tokio::time::Instant::now() + *delay),
                            _ => None,
                        };
                        let mut output = OutboundResponse {
                            bytes: match response {
                                PeerResponse::Structured { status, body, .. } => {
                                    encode_response(status, body)
                                }
                                PeerResponse::Raw(bytes) => bytes,
                            },
                            offset: 0,
                            credit: INITIAL_WINDOW,
                            deliver_at,
                        };
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
