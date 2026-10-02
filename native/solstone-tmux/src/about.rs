// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::command::CommandRunner;
use crate::journal_version::{JournalVersionStatus, read_journal_about};

const SEPARATOR: &str = " · ";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct About {
    pub protocol_version: u32,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build: Option<String>,
    pub os: String,
    pub os_version: String,
    pub arch: String,
    pub about: String,
}

pub fn normalize_arch(raw: &str) -> &str {
    match raw {
        "aarch64" | "ARM64" | "arm64-v8a" | "arm64" => "arm64",
        "amd64" | "x64" | "AMD64" | "x86_64" => "x86_64",
        other => other,
    }
}

pub fn render_line(
    name: &str,
    version: &str,
    build: Option<&str>,
    os: &str,
    os_version: &str,
    arch: &str,
) -> String {
    let mut line = format!("{name} {}", version.trim_start_matches('v'));
    if let Some(build) = build.filter(|value| !value.is_empty()) {
        line.push_str(&format!(" ({build})"));
    }
    if !os.is_empty() {
        line.push_str(SEPARATOR);
        line.push_str(os);
        if !os_version.is_empty() {
            line.push(' ');
            line.push_str(os_version);
        }
    }
    if !arch.is_empty() {
        line.push_str(SEPARATOR);
        line.push_str(normalize_arch(arch));
    }
    line
}

impl About {
    pub fn valid(&self) -> bool {
        self.protocol_version == 1
            && !self.version.is_empty()
            && self.build.as_ref().is_none_or(|build| !build.is_empty())
            && [
                &self.version,
                &self.os,
                &self.os_version,
                &self.arch,
                &self.about,
            ]
            .into_iter()
            .chain(self.build.iter())
            .all(|value| !value.chars().any(char::is_control))
            && self.about.len() <= 8192
            && self.about
                == render_line(
                    "journal",
                    &self.version,
                    self.build.as_deref(),
                    &self.os,
                    &self.os_version,
                    &self.arch,
                )
    }
}

pub fn decode_about(bytes: &[u8]) -> Option<About> {
    let value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    if value.get("build").is_some_and(|build| !build.is_string()) {
        return None;
    }
    let about: About = serde_json::from_value(value).ok()?;
    about.valid().then_some(about)
}

#[derive(Clone, Debug)]
pub struct HostFacts {
    pub os: String,
    pub os_version: String,
    pub arch: String,
}

pub fn parse_os_release(text: &str) -> (String, String) {
    let value = |key| {
        text.lines().find_map(|line| {
            let (found, value) = line.split_once('=')?;
            if found != key {
                return None;
            }
            let value = value.trim().trim_matches(['"', '\'']);
            (!value.is_empty()
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte)))
            .then(|| value.to_owned())
        })
    };
    (
        value("ID").unwrap_or_else(|| "linux".into()),
        value("VERSION_ID").unwrap_or_default(),
    )
}

pub fn native_macos_arch(translated: Option<bool>, machine: Option<&str>) -> String {
    match translated {
        Some(true) => "arm64".into(),
        Some(false) => machine.map(normalize_arch).unwrap_or_default().into(),
        None => String::new(),
    }
}

pub async fn host_facts(_runner: &dyn CommandRunner) -> HostFacts {
    #[cfg(target_os = "linux")]
    {
        // Only public OS-release facts enter this blocking read. Uname describes
        // the native machine; the compiled process slice is never a fallback.
        tokio::task::spawn_blocking(|| {
            let uname = rustix::system::uname();
            let (os, os_version) = match std::fs::read_to_string("/etc/os-release")
                .or_else(|_| std::fs::read_to_string("/usr/lib/os-release"))
            {
                Ok(text) => parse_os_release(&text),
                Err(_) => (
                    "linux".into(),
                    uname.release().to_string_lossy().into_owned(),
                ),
            };
            HostFacts {
                os,
                os_version,
                arch: normalize_arch(&uname.machine().to_string_lossy()).into(),
            }
        })
        .await
        .unwrap_or_else(|_| HostFacts {
            os: "linux".into(),
            os_version: String::new(),
            arch: String::new(),
        })
    }
    #[cfg(target_os = "macos")]
    {
        use crate::command::{CommandInvocation, CommandOperation};
        use std::time::Duration;
        async fn observe(
            runner: &dyn CommandRunner,
            key: &str,
        ) -> Option<crate::command::CommandOutput> {
            runner
                .run(CommandInvocation {
                    operation: CommandOperation::HostFact,
                    executable: "/usr/sbin/sysctl".into(),
                    args: vec!["-n".into(), key.into()],
                    timeout: Duration::from_secs(1),
                })
                .await
                .ok()
        }
        fn text(output: Option<crate::command::CommandOutput>) -> Option<String> {
            let output = output.filter(|output| output.status == 0)?;
            let value = String::from_utf8(output.stdout).ok()?.trim().to_owned();
            (!value.is_empty() && !value.chars().any(char::is_control)).then_some(value)
        }
        let translated = match observe(_runner, "sysctl.proc_translated").await {
            Some(output) if output.status == 0 => match output.stdout.as_slice() {
                b"1\n" => Some(true),
                b"0\n" => Some(false),
                _ => None,
            },
            Some(output) if String::from_utf8_lossy(&output.stderr).contains("unknown oid") => {
                Some(false)
            }
            _ => None,
        };
        let machine = text(observe(_runner, "hw.machine").await);
        let os_version = text(observe(_runner, "kern.osproductversion").await)
            .filter(|value| {
                value
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || byte == b'.')
            })
            .unwrap_or_default();
        HostFacts {
            os: "macos".into(),
            os_version,
            arch: native_macos_arch(translated, machine.as_deref()),
        }
    }
}

#[derive(Clone, Debug)]
pub struct AboutBlock {
    pub text: String,
    pub host: HostFacts,
}

impl AboutBlock {
    pub async fn snapshot(
        config_root: Option<&Path>,
        data_root: Option<&Path>,
        now: i64,
        runner: &dyn CommandRunner,
    ) -> Self {
        let host = host_facts(runner).await;
        let own = render_line(
            "tmux app",
            env!("CARGO_PKG_VERSION"),
            None,
            &host.os,
            &host.os_version,
            &host.arch,
        );
        let journal = match (config_root, data_root) {
            (Some(config), Some(data)) => {
                let (status, facts, seen_at) = read_journal_about(config, data, now);
                journal_line(&status, facts.as_ref(), seen_at, now)
            }
            _ => "journal unknown".into(),
        };
        Self {
            text: format!("{own}\n{journal}"),
            host,
        }
    }
}

pub fn journal_line(
    status: &JournalVersionStatus,
    facts: Option<&About>,
    seen_at: Option<u64>,
    now: i64,
) -> String {
    let (version, current) = match status {
        JournalVersionStatus::Unknown => return "journal unknown".into(),
        JournalVersionStatus::Current(version) => (version, true),
        JournalVersionStatus::LastKnown(version) => (version, false),
    };
    let facts = facts.filter(|facts| {
        facts.valid() && facts.version.trim_start_matches('v') == version.trim_start_matches('v')
    });
    let mut line = facts
        .map(|facts| facts.about.clone())
        .unwrap_or_else(|| render_line("journal", version, None, "", "", ""));
    if !current && let Some(seen_at) = seen_at {
        let seconds = u64::try_from(now)
            .unwrap_or_default()
            .saturating_sub(seen_at);
        let age = match seconds {
            0..60 => "just now".into(),
            60..3600 => relative(seconds / 60, "minute"),
            3600..86400 => relative(seconds / 3600, "hour"),
            _ => relative(seconds / 86400, "day"),
        };
        line.push_str(&format!("{SEPARATOR}last seen {age}"));
    }
    line
}

fn relative(value: u64, unit: &str) -> String {
    format!("{value} {unit}{} ago", if value == 1 { "" } else { "s" })
}
