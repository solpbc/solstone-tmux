// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

mod support;

use std::fs;
use std::io::Write;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::process::{Command, Stdio};

use solstone_tmux_observer::cli::{CliCommand, parse_args};
use solstone_tmux_observer::health::DiagnosticCode;
use solstone_tmux_observer::paths::ensure_private_directory;
use solstone_tmux_observer::private_link::{
    CREDENTIALS_FILENAME, OBSERVER_FILENAME, ObserverState, load_credential, load_observer,
    persist_credential, persist_observer,
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
fn observer_state_is_bound_to_credential_instance_without_deleting_stale_state() {
    let temporary = TestDirectory::new("observer-instance-binding");
    ensure_private_directory(temporary.path()).expect("private config root");
    let observer = ObserverState {
        credential_instance_id: "instance-one".to_owned(),
        key: "observer-secret".to_owned(),
        prefix: "observer-prefix".to_owned(),
        name: "observer-name".to_owned(),
        ingest_url: "/app/observer/ingest".to_owned(),
        protocol_version: 2,
    };
    persist_observer(temporary.path(), &observer).expect("persist observer");
    let observer_path = temporary.path().join(OBSERVER_FILENAME);
    let original = fs::read(&observer_path).expect("read observer state");

    let matching = load_observer(temporary.path(), "instance-one")
        .expect("load matching observer")
        .expect("matching observer exists");
    assert!(
        matching.credential_instance_id == "instance-one",
        "observer binding differs"
    );
    assert!(
        load_observer(temporary.path(), "instance-two")
            .expect("load stale observer")
            .is_none()
    );
    assert!(
        fs::read(observer_path).expect("read retained stale observer") == original,
        "stale observer state changed"
    );
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
fn invalid_setup_input_creates_only_config_root_and_redacts_input() {
    let temporary = TestDirectory::new("setup-invalid-input");
    let roots = IsolatedRoots::new(temporary.path());
    let input = "sentinel setup input";
    let mut child = Command::new(env!("CARGO_BIN_EXE_solstone-tmux-observer"))
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
    assert!(!roots.data_root().exists());
    assert!(roots.config_root().is_dir());
    assert!(!roots.config_root().join(CREDENTIALS_FILENAME).exists());
    assert!(!roots.config_root().join(OBSERVER_FILENAME).exists());
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
