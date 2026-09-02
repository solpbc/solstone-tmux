// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

mod support;

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::time::Duration;

use solstone_tmux::config::{CONFIG_FILENAME, ConfigError, DEFAULT_SOURCE, RuntimeConfig};
use support::TestDirectory;

#[test]
fn missing_config_uses_hostname_defaults() {
    let temporary = TestDirectory::new("config-defaults");
    let config = RuntimeConfig::load(temporary.path(), "Owner Host.example.com")
        .expect("default native config");

    assert_eq!(config.stream.as_str(), "owner-host.tmux");
    assert_eq!(config.capture_interval, Duration::from_secs(5));
    assert_eq!(config.segment_interval, Duration::from_secs(300));
    assert_eq!(config.cache_retention_days, 7);
    assert!(config.status_indicator);
    assert_eq!(config.source, DEFAULT_SOURCE);
}

#[test]
fn numeric_hostname_joins_all_labels() {
    let temporary = TestDirectory::new("config-numeric-host");
    let config =
        RuntimeConfig::load(temporary.path(), "192.168.1.1").expect("numeric hostname config");
    assert_eq!(config.stream.as_str(), "192-168-1-1.tmux");
}

#[test]
fn present_config_defaults_status_indicator_to_true() {
    let temporary = TestDirectory::new("config-status-default");
    fs::write(
        temporary.path().join(CONFIG_FILENAME),
        br#"{"stream":"main","capture_interval":5,"segment_interval":300}"#,
    )
    .expect("write config without indicator field");

    let config =
        RuntimeConfig::load(temporary.path(), "ignored").expect("load defaulted indicator");

    assert!(config.status_indicator);
    assert_eq!(config.source, DEFAULT_SOURCE);
}

#[test]
fn present_config_overrides_defaults_and_is_private() {
    let temporary = TestDirectory::new("config-present");
    let path = temporary.path().join(CONFIG_FILENAME);
    fs::write(
        &path,
        br#"{"stream":"main","capture_interval":2,"segment_interval":30,"cache_retention_days":-1,"status_indicator":false}"#,
    )
    .expect("write config");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("set config mode");

    let config = RuntimeConfig::load(temporary.path(), "ignored").expect("native config");

    assert_eq!(config.stream.as_str(), "main");
    assert_eq!(config.capture_interval, Duration::from_secs(2));
    assert_eq!(config.segment_interval, Duration::from_secs(30));
    assert_eq!(config.cache_retention_days, -1);
    assert!(!config.status_indicator);
    assert_eq!(config.source, DEFAULT_SOURCE);
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

#[test]
fn omitted_source_defaults_to_tmux() {
    let temporary = TestDirectory::new("config-source-omitted");
    fs::write(
        temporary.path().join(CONFIG_FILENAME),
        br#"{"stream":"main","capture_interval":5,"segment_interval":300}"#,
    )
    .expect("write source-free config");

    let config = RuntimeConfig::load(temporary.path(), "ignored").expect("load omitted source");
    assert_eq!(config.source, "tmux");
}

#[test]
fn explicit_source_is_accepted_at_the_grammar_bounds() {
    let temporary = TestDirectory::new("config-source-valid");
    let path = temporary.path().join(CONFIG_FILENAME);
    fs::write(
        &path,
        br#"{"stream":"main","capture_interval":5,"segment_interval":300,"source":"studio-mac"}"#,
    )
    .expect("write distinct source");
    let config = RuntimeConfig::load(temporary.path(), "ignored").expect("load distinct source");
    assert_eq!(config.source, "studio-mac");

    let bound = "a".repeat(64);
    fs::write(
        &path,
        format!(
            "{{\"stream\":\"main\",\"capture_interval\":5,\"segment_interval\":300,\"source\":{bound:?}}}"
        ),
    )
    .expect("write 64-byte source");
    let config = RuntimeConfig::load(temporary.path(), "ignored").expect("load 64-byte source");
    assert_eq!(config.source, bound);
}

#[test]
fn invalid_source_values_are_rejected() {
    let temporary = TestDirectory::new("config-source-invalid");
    let path = temporary.path().join(CONFIG_FILENAME);
    for (label, body) in [
        ("empty", br#"{"source":""}"#.as_slice()),
        ("null", br#"{"source":null}"#.as_slice()),
        ("bool", br#"{"source":true}"#.as_slice()),
        ("number", br#"{"source":1}"#.as_slice()),
        ("array", br#"{"source":[]}"#.as_slice()),
        ("object", br#"{"source":{}}"#.as_slice()),
        ("uppercase", br#"{"source":"Upper"}"#.as_slice()),
        ("dot", br#"{"source":"a.b"}"#.as_slice()),
        ("slash", br#"{"source":"a/b"}"#.as_slice()),
        ("backslash", br#"{"source":"a\\b"}"#.as_slice()),
    ] {
        fs::write(&path, body).unwrap_or_else(|_| panic!("write {label} source"));
        assert!(
            matches!(
                RuntimeConfig::load(temporary.path(), "ignored"),
                Err(ConfigError::InvalidSource)
            ),
            "{label} source must be rejected"
        );
    }

    fs::write(&path, format!("{{\"source\":{:?}}}", "a".repeat(65)))
        .expect("write oversize source");
    let error = RuntimeConfig::load(temporary.path(), "ignored").expect_err("oversize source");
    assert!(matches!(error, ConfigError::InvalidSource));
    assert!(error.to_string().contains("update source in config.json"));
}

#[test]
fn unknown_config_fields_are_still_rejected() {
    let temporary = TestDirectory::new("config-unknown-field");
    fs::write(
        temporary.path().join(CONFIG_FILENAME),
        br#"{"stream":"main","capture_interval":5,"segment_interval":300,"unknown":true}"#,
    )
    .expect("write unknown field");
    assert!(matches!(
        RuntimeConfig::load(temporary.path(), "ignored"),
        Err(ConfigError::InvalidConfig { .. })
    ));
}
