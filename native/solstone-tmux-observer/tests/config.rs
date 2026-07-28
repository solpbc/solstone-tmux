// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

mod support;

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::time::Duration;

use solstone_tmux_observer::config::{CONFIG_FILENAME, ConfigError, RuntimeConfig};
use support::TestDirectory;

#[test]
fn missing_config_uses_hostname_defaults() {
    let temporary = TestDirectory::new("config-defaults");
    let config = RuntimeConfig::load(temporary.path(), "Owner Host.example.com")
        .expect("default native config");

    assert_eq!(config.stream.as_str(), "owner-host.tmux");
    assert_eq!(config.capture_interval, Duration::from_secs(5));
    assert_eq!(config.segment_interval, Duration::from_secs(300));
}

#[test]
fn numeric_hostname_joins_all_labels() {
    let temporary = TestDirectory::new("config-numeric-host");
    let config =
        RuntimeConfig::load(temporary.path(), "192.168.1.1").expect("numeric hostname config");
    assert_eq!(config.stream.as_str(), "192-168-1-1.tmux");
}

#[test]
fn present_config_overrides_defaults_and_is_private() {
    let temporary = TestDirectory::new("config-present");
    let path = temporary.path().join(CONFIG_FILENAME);
    fs::write(
        &path,
        br#"{"stream":"main","capture_interval":2,"segment_interval":30}"#,
    )
    .expect("write config");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("set config mode");

    let config = RuntimeConfig::load(temporary.path(), "ignored").expect("native config");

    assert_eq!(config.stream.as_str(), "main");
    assert_eq!(config.capture_interval, Duration::from_secs(2));
    assert_eq!(config.segment_interval, Duration::from_secs(30));
    assert_eq!(
        fs::metadata(path)
            .expect("config metadata")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[test]
fn invalid_stream_and_intervals_are_actionable() {
    let temporary = TestDirectory::new("config-invalid");
    let path = temporary.path().join(CONFIG_FILENAME);
    fs::write(
        &path,
        format!(
            "{{\"stream\":{:?},\"capture_interval\":5,\"segment_interval\":300}}",
            "a".repeat(201)
        ),
    )
    .expect("write overlong stream");
    let error = RuntimeConfig::load(temporary.path(), "ignored").expect_err("overlong stream");
    assert!(error.to_string().contains("update stream in config.json"));
    assert!(error.to_string().contains("201 bytes"));

    fs::write(
        &path,
        br#"{"stream":"main","capture_interval":0,"segment_interval":300}"#,
    )
    .expect("write zero interval");
    assert!(matches!(
        RuntimeConfig::load(temporary.path(), "ignored"),
        Err(ConfigError::InvalidInterval {
            field: "capture_interval"
        })
    ));
}

#[test]
fn config_symlink_is_rejected_without_reading_referent() {
    let temporary = TestDirectory::new("config-symlink");
    let referent = temporary.path().join("referent.json");
    let path = temporary.path().join(CONFIG_FILENAME);
    fs::write(&referent, br#"{"stream":"main"}"#).expect("write referent");
    symlink(&referent, &path).expect("config symlink");

    assert!(matches!(
        RuntimeConfig::load(temporary.path(), "ignored"),
        Err(ConfigError::InvalidTarget(target)) if target == path
    ));
    assert_eq!(
        fs::read(referent).expect("referent unchanged"),
        br#"{"stream":"main"}"#
    );
}
