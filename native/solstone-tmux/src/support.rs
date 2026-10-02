// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use crate::about::AboutBlock;

pub const HELP_URL: &str = "https://support.solstone.app";

pub fn report_url(last_state: &str, block: &AboutBlock) -> String {
    let mut fields = vec![
        ("report", "v1".to_owned()),
        ("app", "solstone for tmux".to_owned()),
    ];
    let version = env!("CARGO_PKG_VERSION");
    if !version.is_empty() {
        fields.push(("version", limited(version, 120)));
    }
    if !block.host.os.is_empty() {
        fields.push(("os", limited(&block.host.os, 120)));
    }
    if !block.host.os_version.is_empty() {
        fields.push(("os_version", limited(&block.host.os_version, 120)));
    }
    if !last_state.is_empty() {
        fields.push(("state", limited(last_state, 500)));
    }
    fields.push(("about", block.text.clone()));
    let fragment = fields
        .into_iter()
        .map(|(key, value)| format!("{}={}", form_encode(key), form_encode(&value)))
        .collect::<Vec<_>>()
        .join("&");
    format!("{HELP_URL}/#{fragment}")
}

fn limited(value: &str, limit: usize) -> String {
    value.chars().take(limit).collect()
}

fn form_encode(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte == b' ' {
            encoded.push('+');
        } else if byte.is_ascii_alphanumeric() || matches!(byte, b'*' | b'-' | b'.' | b'_') {
            encoded.push(char::from(byte));
        } else {
            use std::fmt::Write as _;
            write!(&mut encoded, "%{byte:02X}").expect("writing to a string cannot fail");
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block() -> AboutBlock {
        AboutBlock {
            text: "tmux app 2.0.11 · ubuntu 24.04 · x86_64\njournal 2.0.29 · macos 26.5 · arm64"
                .into(),
            host: crate::about::HostFacts {
                os: "ubuntu".into(),
                os_version: "24.04".into(),
                arch: "x86_64".into(),
            },
        }
    }

    #[test]
    fn report_stays_in_the_fragment_and_contains_only_owner_safe_context() {
        let url = report_url("offline", &block());
        assert!(url.starts_with("https://support.solstone.app/#report=v1&app=solstone+for+tmux"));
        assert!(url.contains("&state=offline"));
        assert!(!url.contains('?'));
        assert!(!url.contains("description="));
        assert!(!url.contains("hostname"));
        assert!(url.contains("%0Ajournal+2.0.29"));
        assert!(url.contains("%C2%B7"));
        assert!(url.contains("os_version=24.04"));
        assert!(!url.contains("build="));
        assert!(!url.contains("%2Fhome%2F"));
        assert!(!url.contains("instance_id"));
    }

    #[test]
    fn encoding_matches_url_search_params() {
        assert_eq!(form_encode("a b+c~\n"), "a+b%2Bc%7E%0A");
    }

    #[test]
    fn state_is_bounded_and_empty_state_is_omitted() {
        let long = "é".repeat(501);
        let url = report_url(&long, &block());
        assert_eq!(url.matches("%C3%A9").count(), 500);
        assert!(!report_url("", &block()).contains("&state="));
    }
}
