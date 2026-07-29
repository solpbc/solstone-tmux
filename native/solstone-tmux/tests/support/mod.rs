// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

#![allow(dead_code)]

pub mod private_link_peer;

use std::collections::{HashMap, VecDeque};
use std::ffi::OsString;
use std::fs;
use std::future::Future;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use serde::Deserialize;
use serde_json::Value;
use solstone_tmux::command::{CommandError, CommandInvocation, CommandOutput, CommandRunner};
use solstone_tmux::model::{CaptureResult, PaneInfo, WindowInfo};
use solstone_tmux::paths::{Environment, resolve_config_root, resolve_data_root};
use solstone_tmux::service::{current_platform, launchd::LABEL};
use solstone_tmux::tmux::WarningSink;

#[derive(Clone, Debug)]
pub enum FixtureOutcome {
    Output(CommandOutput),
    Timeout,
    SpawnFailure,
}

#[derive(Clone, Debug)]
pub struct ExpectedInvocation {
    pub invocation: CommandInvocation,
    pub outcome: FixtureOutcome,
}

#[derive(Clone, Debug, Default)]
pub struct FixtureRunner {
    expected: Arc<Mutex<VecDeque<ExpectedInvocation>>>,
    calls: Arc<Mutex<Vec<CommandInvocation>>>,
    mismatches: Arc<Mutex<Vec<String>>>,
}

impl FixtureRunner {
    pub fn new(expected: impl IntoIterator<Item = ExpectedInvocation>) -> Self {
        Self {
            expected: Arc::new(Mutex::new(expected.into_iter().collect())),
            ..Self::default()
        }
    }

    pub fn calls(&self) -> Vec<CommandInvocation> {
        self.calls.lock().expect("fixture calls poisoned").clone()
    }

    pub fn assert_finished(&self) -> Result<(), String> {
        let expected = self.expected.lock().expect("fixture queue poisoned");
        let mismatches = self.mismatches.lock().expect("fixture errors poisoned");
        if !mismatches.is_empty() {
            return Err(mismatches.join("; "));
        }
        if !expected.is_empty() {
            return Err(format!(
                "{} expected invocation(s) were unused",
                expected.len()
            ));
        }
        Ok(())
    }
}

impl CommandRunner for FixtureRunner {
    fn run<'a>(
        &'a self,
        invocation: CommandInvocation,
    ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, CommandError>> + Send + 'a>> {
        Box::pin(async move {
            self.calls
                .lock()
                .expect("fixture calls poisoned")
                .push(invocation.clone());
            let Some(expected) = self
                .expected
                .lock()
                .expect("fixture queue poisoned")
                .pop_front()
            else {
                let message = format!("unexpected invocation: {invocation:?}");
                self.mismatches
                    .lock()
                    .expect("fixture errors poisoned")
                    .push(message.clone());
                return Err(spawn_error(message));
            };
            if expected.invocation != invocation {
                let message = format!(
                    "invocation mismatch: expected {:?}, got {:?}",
                    expected.invocation, invocation
                );
                self.mismatches
                    .lock()
                    .expect("fixture errors poisoned")
                    .push(message.clone());
                return Err(spawn_error(message));
            }
            match expected.outcome {
                FixtureOutcome::Output(output) => Ok(output),
                FixtureOutcome::Timeout => Err(CommandError::Timeout {
                    operation: invocation.operation,
                    duration: invocation.timeout,
                }),
                FixtureOutcome::SpawnFailure => {
                    Err(spawn_error("fixture requested a spawn failure"))
                }
            }
        })
    }
}

fn spawn_error(message: impl Into<String>) -> CommandError {
    CommandError::Spawn(std::io::Error::other(message.into()))
}

#[derive(Clone, Debug, Default)]
pub struct RecordingWarnings(Arc<Mutex<Vec<String>>>);

impl RecordingWarnings {
    pub fn messages(&self) -> Vec<String> {
        self.0.lock().expect("warnings poisoned").clone()
    }
}

impl WarningSink for RecordingWarnings {
    fn warn(&self, message: &str) {
        self.0
            .lock()
            .expect("warnings poisoned")
            .push(message.to_owned());
    }
}

pub fn output(stdout: impl Into<Vec<u8>>) -> FixtureOutcome {
    FixtureOutcome::Output(CommandOutput {
        stdout: stdout.into(),
        stderr: Vec::new(),
        status: 0,
    })
}

pub fn nonzero(status: i32) -> FixtureOutcome {
    FixtureOutcome::Output(CommandOutput {
        stdout: Vec::new(),
        stderr: b"fixture failure".to_vec(),
        status,
    })
}

pub fn command_output(stdout: &[u8], stderr: &[u8], status: i32) -> FixtureOutcome {
    FixtureOutcome::Output(CommandOutput {
        stdout: stdout.to_vec(),
        stderr: stderr.to_vec(),
        status,
    })
}

pub fn launchd_absent(user_id: u32) -> FixtureOutcome {
    command_output(&[], launchd_missing_service_line(user_id).as_bytes(), 113)
}

pub fn launchd_missing_service_line(user_id: u32) -> String {
    format!("Could not find service \"{LABEL}\" in domain for user gui: {user_id}")
}

#[derive(Clone, Debug, Default)]
pub struct FakeEnvironment(HashMap<String, OsString>);

impl FakeEnvironment {
    pub fn from_paths(entries: impl IntoIterator<Item = (&'static str, OsString)>) -> Self {
        Self(
            entries
                .into_iter()
                .map(|(key, value)| (key.to_owned(), value))
                .collect(),
        )
    }
}

impl<const N: usize> From<[(&str, &str); N]> for FakeEnvironment {
    fn from(entries: [(&str, &str); N]) -> Self {
        Self(
            entries
                .into_iter()
                .map(|(key, value)| (key.to_owned(), OsString::from(value)))
                .collect(),
        )
    }
}

impl Environment for FakeEnvironment {
    fn var_os(&self, key: &str) -> Option<OsString> {
        self.0.get(key).cloned()
    }
}

pub struct IsolatedRoots {
    entries: [(&'static str, OsString); 3],
}

impl IsolatedRoots {
    pub fn new(base: &Path) -> Self {
        Self::with_xdg_homes(base, base.join("data"), base.join("config"))
    }

    pub fn new_aliased(base: &Path) -> Self {
        let shared = base.join("shared");
        Self::with_xdg_homes(base, shared.clone(), shared)
    }

    fn with_xdg_homes(base: &Path, data_home: PathBuf, config_home: PathBuf) -> Self {
        let home = base.join("home");
        fs::create_dir(&home).expect("create synthetic HOME");
        Self {
            entries: [
                ("HOME", home.into_os_string()),
                ("XDG_DATA_HOME", data_home.into_os_string()),
                ("XDG_CONFIG_HOME", config_home.into_os_string()),
            ],
        }
    }

    pub fn entries(&self) -> &[(&'static str, OsString)] {
        &self.entries
    }

    pub fn data_root(&self) -> PathBuf {
        let environment = FakeEnvironment::from_paths(self.entries().iter().cloned());
        resolve_data_root(current_platform(), &environment).expect("resolve isolated data root")
    }

    pub fn config_root(&self) -> PathBuf {
        let environment = FakeEnvironment::from_paths(self.entries().iter().cloned());
        resolve_config_root(current_platform(), &environment).expect("resolve isolated config root")
    }
}

pub fn create_executable(path: &Path) {
    fs::create_dir_all(path.parent().expect("executable parent"))
        .expect("create executable parent");
    fs::write(path, b"fixture executable\n").expect("write executable");
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).expect("chmod executable");
}

pub struct TestDirectory(PathBuf);

impl TestDirectory {
    pub fn new(label: &str) -> Self {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let number = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "solstone-tmux-{label}-{}-{number}",
            std::process::id()
        ));
        std::fs::create_dir(&path).expect("create test directory");
        Self(path)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct ObserverWireFixture {
    pub id: String,
    pub kind: String,
    pub payload: Value,
    pub provenance: Value,
    pub schema_validation: Value,
}

#[derive(Deserialize)]
struct ObserverWireBundle {
    fixtures: Vec<ObserverWireFixture>,
}

pub fn observer_wire_fixture(id: &str) -> ObserverWireFixture {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("vendor/observer-client-contract/fixtures/wire-behavior.json");
    let bytes = fs::read(path).expect("read observer wire fixture bundle");
    let bundle = serde_json::from_slice::<ObserverWireBundle>(&bytes)
        .expect("parse observer fixture bundle");
    let mut matches = bundle
        .fixtures
        .into_iter()
        .filter(|fixture| fixture.id == id);
    let fixture = matches.next().expect("named observer fixture exists");
    assert!(
        matches.next().is_none(),
        "observer fixture identifier is unique"
    );
    fixture
}

pub fn golden_capture(session: &str) -> CaptureResult {
    let active_window = WindowInfo {
        id: "@7".to_owned(),
        index: 2,
        name: "dev café".to_owned(),
        active: true,
    };
    CaptureResult {
        session: session.to_owned(),
        window: active_window.clone(),
        windows: vec![
            active_window,
            WindowInfo {
                id: "@8".to_owned(),
                index: 3,
                name: "logs".to_owned(),
                active: false,
            },
        ],
        panes: vec![
            PaneInfo {
                id: "%11".to_owned(),
                index: 0,
                left: 0,
                top: 0,
                width: 60,
                height: 24,
                active: true,
                content: "\u{1b}[31mRED\u{1b}[0m café\n".to_owned(),
            },
            PaneInfo {
                id: "%12".to_owned(),
                index: 1,
                left: 61,
                top: 0,
                width: 59,
                height: 24,
                active: false,
                content: "right pane\n".to_owned(),
            },
        ],
    }
}

pub fn sha256(input: &[u8]) -> [u8; 32] {
    const INITIAL: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    const ROUND: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];

    let bit_length = (input.len() as u64) * 8;
    let mut padded = input.to_vec();
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_length.to_be_bytes());

    let mut state = INITIAL;
    for block in padded.chunks_exact(64) {
        let mut schedule = [0_u32; 64];
        for (index, word) in block.chunks_exact(4).enumerate() {
            schedule[index] = u32::from_be_bytes(word.try_into().expect("four-byte word"));
        }
        for index in 16..64 {
            let first = schedule[index - 15].rotate_right(7)
                ^ schedule[index - 15].rotate_right(18)
                ^ (schedule[index - 15] >> 3);
            let second = schedule[index - 2].rotate_right(17)
                ^ schedule[index - 2].rotate_right(19)
                ^ (schedule[index - 2] >> 10);
            schedule[index] = schedule[index - 16]
                .wrapping_add(first)
                .wrapping_add(schedule[index - 7])
                .wrapping_add(second);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = state;
        for index in 0..64 {
            let sum_one = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let choice = (e & f) ^ ((!e) & g);
            let temporary_one = h
                .wrapping_add(sum_one)
                .wrapping_add(choice)
                .wrapping_add(ROUND[index])
                .wrapping_add(schedule[index]);
            let sum_zero = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let temporary_two = sum_zero.wrapping_add(majority);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temporary_one);
            d = c;
            c = b;
            b = a;
            a = temporary_one.wrapping_add(temporary_two);
        }
        for (slot, value) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *slot = slot.wrapping_add(value);
        }
    }

    let mut digest = [0_u8; 32];
    for (chunk, word) in digest.chunks_exact_mut(4).zip(state) {
        chunk.copy_from_slice(&word.to_be_bytes());
    }
    digest
}
