// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::process::Command;

pub const HELP_URL: &str = "https://support.solstone.app";

pub fn report_url(last_state: &str) -> String {
    let os_version = os_version();
    let mut fields = vec![
        ("report", "v1".to_owned()),
        ("app", "solstone for tmux".to_owned()),
    ];
    let version = env!("CARGO_PKG_VERSION");
    if !version.is_empty() {
        fields.push(("version", limited(version, 120)));
    }
    let build = option_env!("SOLSTONE_TMUX_SOURCE_COMMIT").unwrap_or("development");
    if !build.is_empty() {
        fields.push(("build", limited(build, 120)));
    }
    if !std::env::consts::OS.is_empty() {
        fields.push(("os", limited(std::env::consts::OS, 120)));
    }
    if let Some(os_version) = os_version {
        fields.push(("os_version", limited(&os_version, 120)));
    }
    if !last_state.is_empty() {
        fields.push(("state", limited(last_state, 500)));
    }
    let fragment = fields
        .into_iter()
        .map(|(key, value)| format!("{}={}", form_encode(key), form_encode(&value)))
        .collect::<Vec<_>>()
        .join("&");
    format!("{HELP_URL}/#{fragment}")
}

fn os_version() -> Option<String> {
    Command::new("uname")
        .arg("-r")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
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

    #[test]
    fn report_stays_in_the_fragment_and_contains_only_owner_safe_context() {
        let url = report_url("offline");
        assert!(url.starts_with("https://support.solstone.app/#report=v1&app=solstone+for+tmux"));
        assert!(url.contains("&state=offline"));
        assert!(!url.contains('?'));
        assert!(!url.contains("description="));
        assert!(!url.contains("hostname"));
        assert!(!url.contains("journal"));
    }

    #[test]
    fn encoding_matches_url_search_params() {
        assert_eq!(form_encode("a b+c~\n"), "a+b%2Bc%7E%0A");
    }

    #[test]
    fn state_is_bounded_and_empty_state_is_omitted() {
        let long = "é".repeat(501);
        let url = report_url(&long);
        assert_eq!(url.matches("%C3%A9").count(), 500);
        assert!(!report_url("").contains("&state="));
    }
}
