// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::collections::VecDeque;
use std::io;
use std::sync::{Arc, Mutex};

use futures_util::{SinkExt, StreamExt};
use rcgen::{
    BasicConstraints, CertificateParams, CertificateSigningRequestParams, ExtendedKeyUsagePurpose,
    IsCa, KeyPair, KeyUsagePurpose, PKCS_ECDSA_P256_SHA256,
};
use rustls::ServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use serde_json::json;
use spl_core::frame::{FLAG_CLOSE, FLAG_DATA, FLAG_OPEN, FLAG_WINDOW, Frame, FrameDecoder};
use spl_core::{PairRequest, PairResponse};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{WebSocketStream, accept_async};

const DIRECT_NONCE: [u8; 16] = [0x11; 16];
const RELAY_SECRET: [u8; 8] = [0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef];
const ENROLL_TOKEN: &str = "e30.eyJpYXQiOjEwMCwiZXhwIjo5OTk5OTk5OTk5fQ.sig";

#[derive(Clone)]
enum PairOutcome {
    Success,
    Reject { status: u16, body: Vec<u8> },
}

struct PairingCa {
    cert: rcgen::Certificate,
    key: KeyPair,
}

impl PairingCa {
    fn new() -> Self {
        let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("generate pairing CA key");
        let mut params =
            CertificateParams::new(Vec::<String>::new()).expect("construct pairing CA parameters");
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages.push(KeyUsagePurpose::DigitalSignature);
        params.key_usages.push(KeyUsagePurpose::KeyCertSign);
        params.key_usages.push(KeyUsagePurpose::CrlSign);
        let cert = params.self_signed(&key).expect("sign pairing CA");
        Self { cert, key }
    }

    fn cert_der_pin(&self) -> Vec<u8> {
        spl_core::ca::sha256(self.cert.der())[..16].to_vec()
    }

    fn spki_pin(&self) -> Vec<u8> {
        let spki = spl_core::ca::extract_spki_der(self.cert.der()).expect("pairing CA SPKI");
        spl_core::ca::sha256(&spki)[..16].to_vec()
    }

    fn jid(&self) -> String {
        let spki = spl_core::ca::extract_spki_der(self.cert.der()).expect("pairing CA SPKI");
        spl_core::relay_window::jid_from_spki(&spki).expect("pairing CA JID")
    }
}

fn encode_pair_link(blob: &[u8]) -> String {
    format!(
        "https://go.solstone.app/p#{}",
        spl_core::crockford::encode(blob)
    )
}

fn direct_v05_link(port: u16, ca_fp_prefix: &[u8]) -> String {
    let mut blob = vec![0x05, 0x01, 1];
    blob.extend_from_slice(&port.to_be_bytes());
    blob.extend_from_slice(&[127, 0, 0, 1]);
    blob.extend_from_slice(&DIRECT_NONCE);
    blob.extend_from_slice(ca_fp_prefix);
    encode_pair_link(&blob)
}

fn relay_v06_link(origin: &str, ca_fp_spki: &[u8]) -> String {
    let origin_bytes = origin.as_bytes();
    let mut blob = vec![0x06];
    blob.extend_from_slice(&RELAY_SECRET);
    blob.push(0x01);
    blob.extend_from_slice(ca_fp_spki);
    blob.push(u8::try_from(origin_bytes.len()).expect("relay origin fits selector"));
    blob.extend_from_slice(origin_bytes);
    encode_pair_link(&blob)
}

fn certless_acceptor(ca: &PairingCa) -> TlsAcceptor {
    let server_key =
        KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("generate pairing server key");
    let mut server_params = CertificateParams::new(vec!["spl.local".to_owned()])
        .expect("construct pairing server parameters");
    server_params.is_ca = IsCa::NoCa;
    server_params
        .extended_key_usages
        .push(ExtendedKeyUsagePurpose::ServerAuth);
    let server = server_params
        .signed_by(&server_key, &ca.cert, &ca.key)
        .expect("sign pairing server certificate");
    let server_config =
        ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .expect("select pairing TLS versions")
            .with_no_client_auth()
            .with_single_cert(
                vec![
                    CertificateDer::from(server.der().to_vec()),
                    CertificateDer::from(ca.cert.der().to_vec()),
                ],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(server_key.serialize_der())),
            )
            .expect("build pairing TLS server");
    TlsAcceptor::from(Arc::new(server_config))
}

fn sign_pair_response(ca: &PairingCa, request: &PairRequest, relay: bool) -> PairResponse {
    let client_cert = CertificateSigningRequestParams::from_pem(&request.csr)
        .expect("parse pairing CSR")
        .signed_by(&ca.cert, &ca.key)
        .expect("sign pairing CSR");
    PairResponse {
        client_cert: client_cert.pem(),
        ca_chain: vec![ca.cert.pem()],
        instance_id: if relay {
            ca.jid()
        } else {
            "test-pairing-instance".to_owned()
        },
        home_label: "Home".to_owned(),
        fingerprint: format!("sha256:{}", spl_core::ca::sha256_hex(client_cert.der())),
        home_attestation: relay.then(|| "attestation".to_owned()),
        local_endpoints: relay.then(|| json!([{"ip":"10.0.0.2","port":7657,"scope":"lan"}])),
    }
}

fn http_pair_response(status: u16, body: &[u8]) -> Vec<u8> {
    let reason = if status == 200 { "OK" } else { "ERR" };
    let mut bytes = format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    bytes.extend_from_slice(body);
    bytes
}

fn pair_http_body(raw: &[u8]) -> Option<&[u8]> {
    raw.windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|split| &raw[split + 4..])
}

struct CaptureState {
    outcomes: Mutex<VecDeque<PairOutcome>>,
    bodies: Mutex<Vec<Vec<u8>>>,
}

impl CaptureState {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            outcomes: Mutex::new(VecDeque::new()),
            bodies: Mutex::new(Vec::new()),
        })
    }

    fn push_outcome(&self, outcome: PairOutcome) {
        lock(&self.outcomes).push_back(outcome);
    }

    fn next_outcome(&self) -> PairOutcome {
        lock(&self.outcomes)
            .pop_front()
            .unwrap_or(PairOutcome::Success)
    }

    fn record_body(&self, body: Vec<u8>) {
        lock(&self.bodies).push(body);
    }

    fn bodies(&self) -> Vec<Vec<u8>> {
        lock(&self.bodies).clone()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

async fn serve_mux_pair<S>(
    mut stream: S,
    ca: &PairingCa,
    state: &CaptureState,
    relay: bool,
) -> io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut decoder = FrameDecoder::new();
    let mut request = Vec::new();
    let mut stream_id = 1;
    let mut buf = [0u8; 16 * 1024];
    loop {
        let count = stream.read(&mut buf).await?;
        if count == 0 {
            return Ok(());
        }
        decoder.feed(&buf[..count]);
        let frames = decoder
            .drain()
            .map_err(|_| io::Error::other("pairing peer frame decode failed"))?;
        for frame in frames {
            if let Some(pong) = frame.control_pong() {
                write_frame(&mut stream, pong).await?;
                continue;
            }
            if frame.flags & (FLAG_OPEN | FLAG_DATA | FLAG_CLOSE | FLAG_WINDOW) != 0 {
                stream_id = frame.stream_id;
            }
            if frame.flags & FLAG_DATA != 0 {
                request.extend_from_slice(&frame.payload);
                if !frame.payload.is_empty() {
                    let credit = u32::try_from(frame.payload.len())
                        .map_err(|_| io::Error::other("pairing peer frame too large"))?;
                    write_frame(&mut stream, Frame::window(stream_id, credit)).await?;
                }
            }
            if frame.flags & FLAG_CLOSE != 0 {
                if let Some(body) = pair_http_body(&request) {
                    state.record_body(body.to_vec());
                    let response = match state.next_outcome() {
                        PairOutcome::Reject { status, body } => http_pair_response(status, &body),
                        PairOutcome::Success => {
                            let pair_request: PairRequest =
                                serde_json::from_slice(body).expect("parse pairing request");
                            let payload =
                                serde_json::to_vec(&sign_pair_response(ca, &pair_request, relay))
                                    .expect("serialize pairing response");
                            http_pair_response(200, &payload)
                        }
                    };
                    write_frame(
                        &mut stream,
                        Frame::new(stream_id, FLAG_DATA | FLAG_CLOSE, response),
                    )
                    .await?;
                    let _ = stream.shutdown().await;
                }
                return Ok(());
            }
        }
    }
}

async fn write_frame<S>(stream: &mut S, frame: Frame) -> io::Result<()>
where
    S: AsyncWrite + Unpin,
{
    let bytes = frame
        .encode()
        .map_err(|_| io::Error::other("pairing peer frame encode failed"))?;
    stream.write_all(&bytes).await?;
    stream.flush().await
}

pub struct DirectPairingPeer {
    pair_link: String,
    state: Arc<CaptureState>,
    task: JoinHandle<()>,
}

impl DirectPairingPeer {
    pub async fn start() -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind direct pairing peer");
        let port = listener.local_addr().expect("direct pairing port").port();
        let ca = Arc::new(PairingCa::new());
        let pair_link = direct_v05_link(port, &ca.cert_der_pin());
        let state = CaptureState::new();
        let acceptor = certless_acceptor(&ca);
        let task_state = Arc::clone(&state);
        let task = tokio::spawn(async move {
            loop {
                let Ok((tcp, _)) = listener.accept().await else {
                    return;
                };
                let Ok(tls) = acceptor.accept(tcp).await else {
                    continue;
                };
                let _ = serve_mux_pair(tls, &ca, &task_state, false).await;
            }
        });
        Self {
            pair_link,
            state,
            task,
        }
    }

    pub fn pair_link(&self) -> &str {
        &self.pair_link
    }

    pub fn enqueue_rejection(&self, status: u16, body: impl Into<Vec<u8>>) {
        self.state.push_outcome(PairOutcome::Reject {
            status,
            body: body.into(),
        });
    }

    pub fn enqueue_success(&self) {
        self.state.push_outcome(PairOutcome::Success);
    }

    pub fn captured_body(&self) -> Vec<u8> {
        self.state
            .bodies()
            .last()
            .cloned()
            .expect("captured pairing body")
    }

    pub fn captured_bodies(&self) -> Vec<Vec<u8>> {
        self.state.bodies()
    }

    pub fn captured_request(&self) -> PairRequest {
        serde_json::from_slice(&self.captured_body()).expect("parse captured PairRequest")
    }

    pub async fn shutdown(self) {
        self.task.abort();
        let _ = self.task.await;
    }
}

pub struct RelayPairingPeer {
    pair_link: String,
    state: Arc<CaptureState>,
    task: JoinHandle<()>,
}

impl RelayPairingPeer {
    pub async fn start() -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind relay pairing peer");
        let origin = format!(
            "http://{}",
            listener.local_addr().expect("relay pairing port")
        );
        let ca = Arc::new(PairingCa::new());
        let pair_link = relay_v06_link(&origin, &ca.spki_pin());
        let state = CaptureState::new();
        let task_state = Arc::clone(&state);
        let task_ca = Arc::clone(&ca);
        let task = tokio::spawn(async move {
            loop {
                let Ok((tcp, _)) = listener.accept().await else {
                    return;
                };
                let state = Arc::clone(&task_state);
                let ca = Arc::clone(&task_ca);
                tokio::spawn(async move {
                    let _ = handle_relay_connection(tcp, ca, state).await;
                });
            }
        });
        Self {
            pair_link,
            state,
            task,
        }
    }

    pub fn pair_link(&self) -> &str {
        &self.pair_link
    }

    pub fn captured_body(&self) -> Vec<u8> {
        self.state
            .bodies()
            .last()
            .cloned()
            .expect("captured pairing body")
    }

    pub fn captured_request(&self) -> PairRequest {
        serde_json::from_slice(&self.captured_body()).expect("parse captured PairRequest")
    }

    pub async fn shutdown(self) {
        self.task.abort();
        let _ = self.task.await;
    }
}

async fn handle_relay_connection(
    tcp: TcpStream,
    ca: Arc<PairingCa>,
    state: Arc<CaptureState>,
) -> io::Result<()> {
    let mut peek = [0u8; 512];
    let n = tcp.peek(&mut peek).await?;
    if String::from_utf8_lossy(&peek[..n]).starts_with("GET ") {
        handle_relay_ws(tcp, ca, state).await
    } else {
        handle_relay_http(tcp).await
    }
}

async fn handle_relay_ws(
    tcp: TcpStream,
    ca: Arc<PairingCa>,
    state: Arc<CaptureState>,
) -> io::Result<()> {
    let ws = accept_async(tcp).await.map_err(io::Error::other)?;
    let (relay_side, home_side) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        let _ = pump_ws(ws, relay_side).await;
    });
    let acceptor = certless_acceptor(&ca);
    let tls = acceptor.accept(home_side).await.map_err(io::Error::other)?;
    serve_mux_pair(tls, &ca, &state, true).await
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

async fn handle_relay_http(mut tcp: TcpStream) -> io::Result<()> {
    let raw = read_http_request(&mut tcp).await?;
    let text = String::from_utf8_lossy(&raw);
    let path = text
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/");
    if path == "/enroll/device" {
        let body = json!({"device_token": ENROLL_TOKEN}).to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        tcp.write_all(response.as_bytes()).await?;
        tcp.flush().await?;
    }
    let _ = tcp.shutdown().await;
    Ok(())
}

async fn read_http_request(stream: &mut TcpStream) -> io::Result<Vec<u8>> {
    let mut raw = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = stream.read(&mut buf).await?;
        if n == 0 {
            return Ok(raw);
        }
        raw.extend_from_slice(&buf[..n]);
        if let Some(split) = raw.windows(4).position(|window| window == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&raw[..split]);
            let len = head
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap_or(0);
            if raw.len() >= split + 4 + len {
                return Ok(raw);
            }
        }
    }
}
