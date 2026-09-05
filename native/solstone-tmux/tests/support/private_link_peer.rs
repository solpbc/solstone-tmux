// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::collections::{HashMap, VecDeque};
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use rcgen::{
    BasicConstraints, CertificateParams, ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose,
    PKCS_ECDSA_P256_SHA256,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::server::WebPkiClientVerifier;
use rustls::{RootCertStore, ServerConfig};
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

enum PeerResponse {
    Structured { status: u16, body: Vec<u8> },
    Raw(Vec<u8>),
}

struct OutboundResponse {
    bytes: Vec<u8>,
    offset: usize,
    credit: usize,
}

#[derive(Clone)]
enum Control {
    GrantUploadCredit(u32),
}

#[derive(Clone)]
struct PeerState {
    responses: Arc<Mutex<VecDeque<PeerResponse>>>,
    system_status_responses: Arc<Mutex<VecDeque<PeerResponse>>>,
    requests: Arc<Mutex<Vec<PeerRequest>>>,
    withhold_credit: Arc<AtomicBool>,
    upload_stalled: Arc<Notify>,
    current_stream: Arc<AtomicU32>,
    accepted: Arc<AtomicUsize>,
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

        let (credential, acceptor) = credential_and_acceptor(address.port());
        let state = PeerState {
            responses: Arc::new(Mutex::new(VecDeque::new())),
            system_status_responses: Arc::new(Mutex::new(VecDeque::new())),
            requests: Arc::new(Mutex::new(Vec::new())),
            withhold_credit: Arc::new(AtomicBool::new(false)),
            upload_stalled: Arc::new(Notify::new()),
            current_stream: Arc::new(AtomicU32::new(0)),
            accepted: Arc::new(AtomicUsize::new(0)),
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

    pub fn credential(&self) -> Credential {
        self.credential.clone()
    }

    pub fn enqueue_response(&self, status: u16, body: impl Into<Vec<u8>>) {
        lock(&self.state.responses).push_back(PeerResponse::Structured {
            status,
            body: body.into(),
        });
    }

    pub fn enqueue_system_status_response(&self, status: u16, body: impl Into<Vec<u8>>) {
        lock(&self.state.system_status_responses).push_back(PeerResponse::Structured {
            status,
            body: body.into(),
        });
    }

    pub fn enqueue_raw_response(&self, response: impl Into<Vec<u8>>) {
        lock(&self.state.responses).push_back(PeerResponse::Raw(response.into()));
    }

    pub fn requests(&self) -> Vec<PeerRequest> {
        lock(&self.state.requests).clone()
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

    pub fn accepted_carriers(&self) -> usize {
        self.state.accepted.load(Ordering::SeqCst)
    }

    pub async fn shutdown(self) {
        self.task.abort();
        let _ = self.task.await;
    }
}

fn credential_and_acceptor(port: u16) -> (Credential, TlsAcceptor) {
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
            .with_client_cert_verifier(verifier)
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
        tokio::select! {
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
                        flush_response(&mut writer, stream_id, response).await?;
                        if response.offset == response.bytes.len() {
                            outbound.remove(&stream_id);
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
                        let is_system_status = parsed
                            .as_ref()
                            .is_some_and(|req| req.path_without_query() == "/api/system/status");
                        if let (Some(request), false) = (parsed, is_system_status) {
                            lock(&state.requests).push(request);
                        }
                        let response = if is_system_status {
                            lock(&state.system_status_responses)
                                .pop_front()
                                .unwrap_or(PeerResponse::Structured {
                                    status: 500,
                                    body: Vec::new(),
                                })
                        } else {
                            lock(&state.responses)
                                .pop_front()
                                .unwrap_or(PeerResponse::Structured {
                                    status: 500,
                                    body: Vec::new(),
                                })
                        };
                        let mut output = OutboundResponse {
                            bytes: match response {
                                PeerResponse::Structured { status, body } => {
                                    encode_response(status, body)
                                }
                                PeerResponse::Raw(bytes) => bytes,
                            },
                            offset: 0,
                            credit: INITIAL_WINDOW,
                        };
                        flush_response(&mut writer, stream_id, &mut output).await?;
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
                let Ok(Control::GrantUploadCredit(credit)) = control else {
                    return Ok(());
                };
                let stream_id = state.current_stream.load(Ordering::SeqCst);
                if stream_id == 0 {
                    pending_upload_credit = pending_upload_credit.saturating_add(credit);
                } else {
                    write_frame(&mut writer, Frame::window(stream_id, credit)).await?;
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
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        409 => "Conflict",
        500 => "Internal Server Error",
        _ => "Response",
    };
    let head = format!(
        "HTTP/1.1 {} {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n",
        status,
        reason,
        body.len()
    );
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
