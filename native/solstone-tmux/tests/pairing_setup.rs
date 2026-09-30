// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

mod support;

use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::Value;
use solstone_tmux::cli::MarkOption;
use solstone_tmux::config::system_hostname;
use solstone_tmux::pairing_answer::{
    ASK_UNAVAILABLE, BODY_UNAVAILABLE, COULDNT_VERIFY, MARK_UNAVAILABLE, PAIRED, STEP,
};
use solstone_tmux::paths::{PlatformKind, resolve_config_root, resolve_data_root};
use solstone_tmux::private_link::{
    CREDENTIALS_FILENAME, format_spoken_mark, load_credential, pairing_ceremony_identity, setup,
    setup_with_identity,
};
use spl_core::PairRequest;
use support::FakeEnvironment;
use support::TestDirectory;
use support::pairing_peer::{DirectPairingPeer, RelayPairingPeer};

struct TestTerminal {
    input: Cursor<Vec<u8>>,
    output: Arc<std::sync::Mutex<Vec<u8>>>,
}

impl TestTerminal {
    fn new(input: &str) -> Self {
        Self {
            input: Cursor::new(input.as_bytes().to_vec()),
            output: Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    #[allow(dead_code)]
    fn output_text(&self) -> String {
        let bytes = self.output.lock().expect("lock").clone();
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

impl std::io::Read for TestTerminal {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.input.read(buf)
    }
}

impl std::io::Write for TestTerminal {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.output.lock().expect("lock").write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.output.lock().expect("lock").flush()
    }
}

#[test]
fn direct_linux_pairing_sends_identity_on_the_wire() {
    assert_direct_identity(PlatformKind::Linux);
}

#[test]
fn direct_macos_pairing_sends_identity_on_the_wire() {
    assert_direct_identity(PlatformKind::Macos);
}

#[test]
fn relay_linux_pairing_sends_identity_on_the_wire() {
    assert_relay_identity(PlatformKind::Linux);
}

#[test]
fn relay_macos_pairing_sends_identity_on_the_wire() {
    assert_relay_identity(PlatformKind::Macos);
}

#[test]
fn direct_and_relay_identity_fields_are_carrier_equivalent() {
    runtime().block_on(async {
        let linux_direct = capture_direct(PlatformKind::Linux).await;
        let linux_relay = capture_relay(PlatformKind::Linux).await;
        assert_eq!(linux_direct.device_label, linux_relay.device_label);
        assert_eq!(
            linux_direct.additional_fields.get("platform"),
            linux_relay.additional_fields.get("platform")
        );
        assert_eq!(
            linux_direct.additional_fields.get("client_label"),
            linux_relay.additional_fields.get("client_label")
        );

        let macos_direct = capture_direct(PlatformKind::Macos).await;
        let macos_relay = capture_relay(PlatformKind::Macos).await;
        assert_eq!(macos_direct.device_label, macos_relay.device_label);
        assert_eq!(
            macos_direct.additional_fields.get("platform"),
            macos_relay.additional_fields.get("platform")
        );
        assert_eq!(
            macos_direct.additional_fields.get("client_label"),
            macos_relay.additional_fields.get("client_label")
        );
    });
}

#[test]
fn grammar_invalid_hostname_omits_client_label_on_the_wire() {
    runtime().block_on(async {
        let peer = DirectPairingPeer::start().await;
        let temporary = TestDirectory::new("pairing-setup-oversize-hostname");
        let (environment, _, config_root) = platform_roots(temporary.path(), PlatformKind::Linux);
        let oversize: Result<String, &'static str> = Ok("é".repeat(127));
        let outcome = setup_with_identity(
            PlatformKind::Linux,
            &environment,
            Cursor::new(peer.pair_link().to_owned()),
            oversize,
            Some(TestTerminal::new("continue\n")),
            MarkOption::Absent,
        )
        .await;
        assert!(matches!(
            outcome,
            solstone_tmux::pairing_answer::Outcome::Owner { code: 0, .. }
        ));
        let raw = peer.captured_body();
        let json: Value = serde_json::from_slice(&raw).expect("pairing JSON");
        assert_eq!(json["device_label"], "tmux");
        assert_eq!(json["platform"], "linux");
        assert!(json.get("client_label").is_none());
        assert!(json.get("additional_fields").is_none());
        assert!(config_root.join(CREDENTIALS_FILENAME).is_file());
        peer.shutdown().await;
    });
}

#[test]
fn hostname_lookup_failure_omits_client_label_on_the_wire() {
    runtime().block_on(async {
        let peer = DirectPairingPeer::start().await;
        let temporary = TestDirectory::new("pairing-setup-hostname-err");
        let (environment, _, config_root) = platform_roots(temporary.path(), PlatformKind::Macos);
        let outcome = setup_with_identity(
            PlatformKind::Macos,
            &environment,
            Cursor::new(peer.pair_link().to_owned()),
            Err("uname failed"),
            Some(TestTerminal::new("continue\n")),
            MarkOption::Absent,
        )
        .await;
        assert!(matches!(
            outcome,
            solstone_tmux::pairing_answer::Outcome::Owner { code: 0, .. }
        ));
        let json: Value = serde_json::from_slice(&peer.captured_body()).expect("pairing JSON");
        assert_eq!(json["device_label"], "tmux");
        assert_eq!(json["platform"], "macos");
        assert!(json.get("client_label").is_none());
        assert!(config_root.join(CREDENTIALS_FILENAME).is_file());
        peer.shutdown().await;
    });
}

#[test]
fn pairing_rejection_preserves_existing_credential_and_allows_retry() {
    runtime().block_on(async {
        let peer = DirectPairingPeer::start().await;
        let temporary = TestDirectory::new("pairing-setup-reject-retry");
        let (environment, _, config_root) = platform_roots(temporary.path(), PlatformKind::Linux);
        let credential_path = config_root.join(CREDENTIALS_FILENAME);
        fs::create_dir_all(&config_root).expect("config root");
        let sentinel = br#"{"sentinel":true}"#;
        fs::write(&credential_path, sentinel).expect("write sentinel credential");

        peer.enqueue_rejection(
            400,
            br#"{"reason_code":"pairing_request_invalid","reason":"pairing_request_invalid","error":"client_label is invalid","detail":"client_label is invalid"}"#,
        );
        let outcome = setup(
            PlatformKind::Linux,
            &environment,
            Cursor::new(peer.pair_link().to_owned()),
            Some(TestTerminal::new("continue\n")),
            MarkOption::Absent,
        )
        .await;
        assert!(matches!(
            outcome,
            solstone_tmux::pairing_answer::Outcome::Diagnostic(
                solstone_tmux::health::DiagnosticCode::PairingFailed
            )
        ));
        assert_eq!(fs::read(&credential_path).expect("sentinel remains"), sentinel);

        peer.enqueue_success();
        let outcome = setup(
            PlatformKind::Linux,
            &environment,
            Cursor::new(peer.pair_link().to_owned()),
            Some(TestTerminal::new("continue\n")),
            MarkOption::Absent,
        )
        .await;
        assert!(matches!(outcome, solstone_tmux::pairing_answer::Outcome::Owner { code: 0, .. }));
        let persisted = fs::read(&credential_path).expect("retry credential");
        assert_ne!(persisted, sentinel);
        assert_eq!(peer.captured_bodies().len(), 2);
        peer.shutdown().await;
    });
}

fn assert_direct_identity(platform: PlatformKind) {
    runtime().block_on(async {
        let captured = capture_direct(platform).await;
        assert_captured_identity(platform, &captured);
    });
}

fn assert_relay_identity(platform: PlatformKind) {
    runtime().block_on(async {
        let captured = capture_relay(platform).await;
        assert_captured_identity(platform, &captured);
    });
}

async fn capture_direct(platform: PlatformKind) -> PairRequest {
    let peer = DirectPairingPeer::start().await;
    let temporary = TestDirectory::new(&format!("pairing-setup-direct-{platform:?}"));
    let (environment, _, config_root) = platform_roots(temporary.path(), platform);
    let outcome = setup(
        platform,
        &environment,
        Cursor::new(peer.pair_link().to_owned()),
        Some(TestTerminal::new("continue\n")),
        MarkOption::Absent,
    )
    .await;
    assert!(matches!(
        outcome,
        solstone_tmux::pairing_answer::Outcome::Owner { code: 0, .. }
    ));
    assert_captured_json(&peer.captured_body(), platform);
    assert!(config_root.join(CREDENTIALS_FILENAME).is_file());
    let request = peer.captured_request();
    peer.shutdown().await;
    request
}

async fn capture_relay(platform: PlatformKind) -> PairRequest {
    let peer = RelayPairingPeer::start().await;
    let temporary = TestDirectory::new(&format!("pairing-setup-relay-{platform:?}"));
    let (environment, _, config_root) = platform_roots(temporary.path(), platform);
    let outcome = setup(
        platform,
        &environment,
        Cursor::new(peer.pair_link().to_owned()),
        Some(TestTerminal::new("yes\n")),
        MarkOption::Absent,
    )
    .await;
    assert!(matches!(
        outcome,
        solstone_tmux::pairing_answer::Outcome::Owner { code: 0, .. }
    ));
    assert_captured_json(&peer.captured_body(), platform);
    assert!(config_root.join(CREDENTIALS_FILENAME).is_file());
    let request = peer.captured_request();
    peer.shutdown().await;
    request
}

fn assert_captured_identity(platform: PlatformKind, request: &PairRequest) {
    let (device_label, fields) = pairing_ceremony_identity(platform, system_hostname());
    assert_eq!(request.device_label, device_label);
    assert_eq!(request.additional_fields, fields);
    assert!(request.csr.contains("BEGIN CERTIFICATE REQUEST"));
}

fn assert_captured_json(body: &[u8], platform: PlatformKind) {
    let json: Value = serde_json::from_slice(body).expect("pairing JSON");
    let (device_label, fields) = pairing_ceremony_identity(platform, system_hostname());
    assert_eq!(json["device_label"], device_label);
    assert_eq!(json["platform"], platform.pairing_platform());
    assert!(json.get("additional_fields").is_none());
    match fields.get("client_label") {
        Some(Value::String(label)) => assert_eq!(json["client_label"], *label),
        None => assert!(json.get("client_label").is_none()),
        Some(_) => panic!("unexpected client_label JSON type"),
    }
    assert!(
        json["csr"]
            .as_str()
            .is_some_and(|csr| csr.contains("BEGIN CERTIFICATE REQUEST"))
    );
}

#[test]
fn relay_pairing_returns_the_paired_journals_spoken_mark() {
    runtime().block_on(async {
        let peer = RelayPairingPeer::start().await;
        let temporary = TestDirectory::new("pairing-setup-relay-spoken-mark");
        let (environment, _, config_root) = platform_roots(temporary.path(), PlatformKind::Linux);
        let term = TestTerminal::new("yes\n");
        let output_ref = term.output.clone();
        let outcome = setup(
            PlatformKind::Linux,
            &environment,
            Cursor::new(peer.pair_link().to_owned()),
            Some(term),
            MarkOption::Absent,
        )
        .await;
        match outcome {
            solstone_tmux::pairing_answer::Outcome::Owner { code, lines } => {
                assert_eq!(code, 0);
                assert_eq!(lines, vec![PAIRED.to_string()]);
            }
            other => panic!("expected Owner outcome 0, got {other:?}"),
        }
        let persisted = load_credential(&config_root)
            .expect("load credential")
            .expect("credential exists");
        let mark = format_spoken_mark(&persisted.instance_id).expect("spoken mark");
        let output_str = String::from_utf8_lossy(&output_ref.lock().expect("lock")).into_owned();
        assert!(output_str.contains(&mark));
        peer.shutdown().await;
    });
}

#[test]
fn direct_pairing_without_a_well_formed_journal_id_still_succeeds_with_no_spoken_mark() {
    runtime().block_on(async {
        let peer = DirectPairingPeer::start().await;
        let temporary = TestDirectory::new("pairing-setup-direct-no-spoken-mark");
        let (environment, _, config_root) = platform_roots(temporary.path(), PlatformKind::Linux);
        let term = TestTerminal::new("continue\n");
        let output_ref = term.output.clone();
        let outcome = setup(
            PlatformKind::Linux,
            &environment,
            Cursor::new(peer.pair_link().to_owned()),
            Some(term),
            MarkOption::Absent,
        )
        .await;
        match outcome {
            solstone_tmux::pairing_answer::Outcome::Owner { code, lines } => {
                assert_eq!(code, 0);
                assert_eq!(lines, vec![PAIRED.to_string()]);
            }
            other => panic!("expected Owner outcome 0, got {other:?}"),
        }
        let output_str = String::from_utf8_lossy(&output_ref.lock().expect("lock")).into_owned();
        let step_pos = output_str.find(STEP).expect("step");
        let mark_pos = output_str.find(MARK_UNAVAILABLE).expect("mark unavailable");
        let verify_pos = output_str.find(COULDNT_VERIFY).expect("couldn't verify");
        let body_pos = output_str.find(BODY_UNAVAILABLE).expect("body unavailable");
        let ask_pos = output_str.find(ASK_UNAVAILABLE).expect("ask unavailable");
        assert!(step_pos < mark_pos);
        assert!(mark_pos < verify_pos);
        assert!(verify_pos < body_pos);
        assert!(body_pos < ask_pos);
        assert!(config_root.join(CREDENTIALS_FILENAME).is_file());
        peer.shutdown().await;
    });
}

fn platform_roots(base: &Path, platform: PlatformKind) -> (FakeEnvironment, PathBuf, PathBuf) {
    let home = base.join("home");
    let data_home = base.join("data");
    let config_home = base.join("config");
    fs::create_dir_all(&home).expect("create HOME");
    fs::create_dir_all(&data_home).expect("create XDG data");
    fs::create_dir_all(&config_home).expect("create XDG config");
    let environment = FakeEnvironment::from_paths([
        ("HOME", home.into_os_string()),
        ("XDG_DATA_HOME", data_home.into_os_string()),
        ("XDG_CONFIG_HOME", config_home.into_os_string()),
    ]);
    let data_root = resolve_data_root(platform, &environment).expect("data root");
    let config_root = resolve_config_root(platform, &environment).expect("config root");
    (environment, data_root, config_root)
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
}
