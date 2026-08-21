// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

mod support;

use std::fs;
use std::io::Write;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use solstone_tmux::cli::{CliCommand, parse_args};
use solstone_tmux::health::DiagnosticCode;
use solstone_tmux::instance_lock::LOCK_FILENAME;
use solstone_tmux::paths::ensure_private_directory;
use solstone_tmux::private_link::{
    CREDENTIALS_FILENAME, OBSERVER_FILENAME, load_credential, persist_credential,
};
use spl_transport::credential::{Credential, EndpointAddr};
use support::{IsolatedRoots, TestDirectory};

#[test]
fn credential_round_trip_is_owner_only() {
    let temporary = TestDirectory::new("credential-round-trip");
    ensure_private_directory(temporary.path()).expect("private config root");
    let credential = credential("instance-one");

    persist_credential(temporary.path(), &credential).expect("persist credential");
    let loaded = load_credential(temporary.path())
        .expect("load credential")
        .expect("credential exists");

    assert!(loaded == credential, "credential round trip differs");
    assert_eq!(
        fs::metadata(temporary.path().join(CREDENTIALS_FILENAME))
            .expect("credential metadata")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[test]
fn private_state_targets_refuse_symlinks_without_changing_referents() {
    let temporary = TestDirectory::new("private-state-symlink");
    ensure_private_directory(temporary.path()).expect("private config root");
    let referent = temporary.path().join("referent.json");
    let target = temporary.path().join(CREDENTIALS_FILENAME);
    let original = b"sentinel-referent";
    fs::write(&referent, original).expect("write referent");
    symlink(&referent, &target).expect("credential symlink");

    assert!(matches!(
        load_credential(temporary.path()),
        Err(DiagnosticCode::PrivateStateInvalid)
    ));
    assert!(matches!(
        persist_credential(temporary.path(), &credential("instance-one")),
        Err(DiagnosticCode::PrivateStateInvalid)
    ));
    assert_eq!(fs::read(referent).expect("read referent"), original);
}

#[test]
fn setup_cli_accepts_no_positional_pair_link() {
    assert_eq!(
        parse_args(["observer".into(), "setup".into()]).expect("parse setup"),
        CliCommand::Setup
    );
    assert!(parse_args(["observer".into(), "setup".into(), "link".into()]).is_err());
}

#[test]
fn invalid_setup_input_creates_no_observer_runtime_state_with_default_roots() {
    let temporary = TestDirectory::new("setup-invalid-input-default-roots");
    let roots = IsolatedRoots::new(temporary.path());
    assert_invalid_setup_input_creates_no_observer_runtime_state(&roots);
}

#[test]
fn invalid_setup_input_creates_no_observer_runtime_state_with_aliased_roots() {
    let temporary = TestDirectory::new("setup-invalid-input-aliased-roots");
    let roots = IsolatedRoots::new_aliased(temporary.path());
    assert_invalid_setup_input_creates_no_observer_runtime_state(&roots);
}

fn assert_invalid_setup_input_creates_no_observer_runtime_state(roots: &IsolatedRoots) {
    let input = "sentinel setup input";
    let mut child = Command::new(env!("CARGO_BIN_EXE_solstone-tmux"))
        .arg("setup")
        .env_clear()
        .envs(roots.entries().iter().cloned())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn setup");
    child
        .stdin
        .take()
        .expect("setup stdin")
        .write_all(input.as_bytes())
        .expect("write setup input");
    let output = child.wait_with_output().expect("setup output");

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("setup input is invalid"));
    assert!(!stderr.contains(input));
    assert!(!roots.data_root().join(LOCK_FILENAME).exists());
    assert!(!roots.data_root().join("captures").exists());
    assert!(roots.config_root().is_dir());
    assert!(!roots.config_root().join(CREDENTIALS_FILENAME).exists());
    assert!(!roots.config_root().join(OBSERVER_FILENAME).exists());
}

#[test]
fn setup_serializes_on_the_config_root_before_reading_input_with_default_roots() {
    let temporary = TestDirectory::new("setup-config-lock-default-roots");
    let roots = IsolatedRoots::new(temporary.path());
    assert_setup_serializes_on_the_config_root_before_reading_input(&roots);
}

#[test]
fn setup_serializes_on_the_config_root_before_reading_input_with_aliased_roots() {
    let temporary = TestDirectory::new("setup-config-lock-aliased-roots");
    let roots = IsolatedRoots::new_aliased(temporary.path());
    assert_setup_serializes_on_the_config_root_before_reading_input(&roots);
}

fn assert_setup_serializes_on_the_config_root_before_reading_input(roots: &IsolatedRoots) {
    let mut first = Command::new(env!("CARGO_BIN_EXE_solstone-tmux"))
        .arg("setup")
        .env_clear()
        .envs(roots.entries().iter().cloned())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn first setup");
    for _ in 0..1_000 {
        if fs::read_dir(roots.config_root())
            .ok()
            .is_some_and(|mut entries| entries.next().is_some())
        {
            break;
        }
        thread::sleep(Duration::from_millis(1));
    }
    assert!(
        fs::read_dir(roots.config_root())
            .expect("read config root")
            .next()
            .is_some(),
        "first setup did not acquire its config lock"
    );

    let second = Command::new(env!("CARGO_BIN_EXE_solstone-tmux"))
        .arg("setup")
        .env_clear()
        .envs(roots.entries().iter().cloned())
        .output()
        .expect("run second setup");
    assert_eq!(second.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&second.stderr).contains("setup is unavailable"),
        "concurrent setup was not refused"
    );

    drop(first.stdin.take());
    let first = first.wait_with_output().expect("finish first setup");
    assert_eq!(first.status.code(), Some(1));
    assert!(!roots.data_root().join(LOCK_FILENAME).exists());
    assert!(!roots.data_root().join("captures").exists());
}

fn credential(instance_id: &str) -> Credential {
    Credential {
        client_key_pem: "private-key".to_owned(),
        client_cert_pem: "client-certificate".to_owned(),
        ca_chain_pem: vec!["ca-certificate".to_owned()],
        ca_fp_prefix: vec![1, 2, 3, 4],
        instance_id: instance_id.to_owned(),
        home_label: "home".to_owned(),
        endpoints: vec![EndpointAddr {
            host: "127.0.0.1".to_owned(),
            port: 7657,
        }],
        home_attestation: None,
        local_endpoints: None,
        relay_origin: None,
        device_token: None,
        device_token_expires_at: None,
    }
}
