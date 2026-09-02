// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use serde_json::{Map, Value};
use solstone_tmux::paths::PlatformKind;
use solstone_tmux::private_link::pairing_ceremony_identity;

#[test]
fn pairing_identity_sends_label_and_platform_for_valid_hostnames() {
    for (platform, token) in [
        (PlatformKind::Linux, "linux"),
        (PlatformKind::Macos, "macos"),
    ] {
        let hostname: Result<String, &'static str> = Ok("studio-mac".to_owned());
        let (device_label, fields) = pairing_ceremony_identity(platform, hostname);
        assert_eq!(device_label, "studio-mac");
        assert_identity_keys(&fields, Some("studio-mac"), token);
    }
}

#[test]
fn pairing_identity_omits_client_label_on_every_fallback() {
    let oversize = "é".repeat(127);
    assert_eq!(oversize.len(), 254);
    for (platform, token) in [
        (PlatformKind::Linux, "linux"),
        (PlatformKind::Macos, "macos"),
    ] {
        for hostname in [Ok(String::new()), Ok(oversize.clone()), Err("uname failed")] {
            let (device_label, fields) = pairing_ceremony_identity(platform, hostname);
            assert_eq!(device_label, "tmux");
            assert_identity_keys(&fields, None, token);
        }
    }
}

#[test]
fn pairing_identity_accepts_the_253_byte_client_label_bound() {
    let accepted = "é".repeat(126) + "a";
    assert_eq!(accepted.len(), 253);
    let hostname: Result<String, &'static str> = Ok(accepted.clone());
    let (device_label, fields) = pairing_ceremony_identity(PlatformKind::Linux, hostname);
    assert_eq!(device_label, accepted);
    assert_identity_keys(&fields, Some(accepted.as_str()), "linux");
}

fn assert_identity_keys(fields: &Map<String, Value>, client_label: Option<&str>, platform: &str) {
    assert_eq!(
        fields.get("platform"),
        Some(&Value::String(platform.to_owned()))
    );
    match client_label {
        Some(label) => {
            assert_eq!(fields.len(), 2);
            assert_eq!(
                fields.get("client_label"),
                Some(&Value::String(label.to_owned()))
            );
        }
        None => {
            assert_eq!(fields.len(), 1);
            assert!(!fields.contains_key("client_label"));
        }
    }
}
