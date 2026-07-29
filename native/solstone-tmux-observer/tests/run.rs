// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

mod support;

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use solstone_tmux_observer::config::CONFIG_FILENAME;
use solstone_tmux_observer::instance_lock::LOCK_FILENAME;
use solstone_tmux_observer::private_link::acquire_private_state_lock;
use solstone_tmux_observer::service::{LocalObserver, STATE_FILENAME};
use support::{IsolatedRoots, TestDirectory};

#[test]
fn production_run_holds_lock_before_config_or_observer_side_effects() {
    let fixture = RunFixture::new("binary-run-lock");
    fixture.write_config("main", 1, 300);
    fixture.write_local_observer();
    let mut first = fixture.spawn_run();
    fixture.wait_for_startup_segment_metadata(&mut first);

    fixture.write_config(&"a".repeat(201), 1, 300);
    let second = fixture.run_output();

    assert_eq!(second.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(stderr.contains("another observer is already using this data root"));
    assert!(!stderr.contains("native stream name"));
    assert!(first.try_wait().expect("first run status").is_none());

    first.kill().expect("stop stub-backed observer");
    first.wait().expect("reap stub-backed observer");
}

#[test]
fn overlong_stream_name_fails_before_capture_directory_creation() {
    let fixture = RunFixture::new("binary-run-overlong-stream");
    fixture.write_config(&"a".repeat(201), 1, 300);

    let output = fixture.run_output();

    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("update stream in config.json"));
    assert!(fixture.data_root().join(LOCK_FILENAME).is_file());
    assert!(!fixture.data_root().join("captures").exists());
}

#[test]
fn setup_checks_existing_run_lock_before_pairing_or_config_creation() {
    let fixture = RunFixture::new("binary-setup-lock");
    fs::create_dir_all(fixture.data_root()).expect("data root");
    let _active =
        solstone_tmux_observer::instance_lock::InstanceLock::acquire(&fixture.data_root())
            .expect("active run lock");
    let pair_input = "sentinel-pair-input";
    let mut command = fixture.command_for("setup");
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("spawn setup");
    child
        .stdin
        .take()
        .expect("setup stdin")
        .write_all(pair_input.as_bytes())
        .expect("write setup input");
    let output = child.wait_with_output().expect("setup output");

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("setup is unavailable"));
    assert!(!stderr.contains(pair_input));
    assert!(!fixture.config_root().exists());
}

#[test]
fn production_run_refuses_private_state_locked_by_setup() {
    let fixture = RunFixture::new("binary-run-private-state-lock");
    fs::create_dir_all(fixture.config_root()).expect("config root");
    let _setup = acquire_private_state_lock(&fixture.config_root()).expect("setup state lock");

    let output = fixture.run_output();

    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("private state is already in use"));
    assert!(!fixture.data_root().join("captures").exists());
}

struct RunFixture {
    _temporary: TestDirectory,
    roots: IsolatedRoots,
    tmux: PathBuf,
    first_child_stderr: PathBuf,
}

impl RunFixture {
    fn new(label: &str) -> Self {
        let temporary = TestDirectory::new(label);
        let roots = IsolatedRoots::new(temporary.path());
        let tmux = temporary.path().join("stub-bin/tmux");
        let first_child_stderr = temporary.path().join("first-child.stderr");
        fs::create_dir_all(tmux.parent().expect("stub parent")).expect("stub parent");
        let source = ["/usr/bin/true", "/bin/true"]
            .into_iter()
            .map(Path::new)
            .find(|candidate| candidate.is_file())
            .expect("system true executable used only as copied tmux stub");
        fs::copy(source, &tmux).expect("copy tmux stub");
        fs::set_permissions(&tmux, fs::Permissions::from_mode(0o700)).expect("chmod tmux stub");
        Self {
            _temporary: temporary,
            roots,
            tmux,
            first_child_stderr,
        }
    }

    fn data_root(&self) -> PathBuf {
        self.roots.data_root()
    }

    fn config_root(&self) -> PathBuf {
        self.roots.config_root()
    }

    fn write_config(&self, stream: &str, capture_interval: u64, segment_interval: u64) {
        fs::create_dir_all(self.config_root()).expect("config root");
        let bytes = serde_json::to_vec(&serde_json::json!({
            "stream": stream,
            "capture_interval": capture_interval,
            "segment_interval": segment_interval,
        }))
        .expect("config JSON");
        fs::write(self.config_root().join(CONFIG_FILENAME), bytes).expect("write config");
    }

    fn write_local_observer(&self) {
        let state = LocalObserver {
            tmux_path: fs::canonicalize(&self.tmux).expect("canonical tmux stub"),
        };
        fs::write(
            self.config_root().join(STATE_FILENAME),
            serde_json::to_vec(&state).expect("state JSON"),
        )
        .expect("write local observer state");
    }

    fn command(&self) -> Command {
        self.command_for("run")
    }

    fn command_for(&self, subcommand: &str) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_solstone-tmux-observer"));
        command
            .arg(subcommand)
            .env_clear()
            .envs(self.roots.entries().iter().cloned());
        command
    }

    fn spawn_run(&self) -> Child {
        let stderr =
            fs::File::create(&self.first_child_stderr).expect("create first child stderr file");
        self.command()
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr))
            .spawn()
            .expect("spawn observer run")
    }

    fn run_output(&self) -> std::process::Output {
        self.command().output().expect("run observer")
    }

    fn wait_for_startup_segment_metadata(&self, child: &mut Child) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            assert!(
                child.try_wait().expect("observer status").is_none(),
                "observer exited before startup segment metadata was observed; first-child stderr: {:?}",
                self.read_first_child_stderr()
            );
            if contains_incomplete_metadata(&self.data_root()) {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!(
            "startup segment metadata was not observed within five seconds; first-child stderr: {:?}",
            self.read_first_child_stderr()
        );
    }

    fn read_first_child_stderr(&self) -> String {
        fs::read(&self.first_child_stderr).map_or_else(
            |error| format!("<could not read first-child stderr: {error}>"),
            |bytes| String::from_utf8_lossy(&bytes).into_owned(),
        )
    }
}

fn contains_incomplete_metadata(path: &Path) -> bool {
    let Ok(entries) = fs::read_dir(path) else {
        return false;
    };
    for entry in entries.flatten() {
        let candidate = entry.path();
        if candidate
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with(".incomplete.meta"))
        {
            return true;
        }
        if candidate.is_dir() && contains_incomplete_metadata(&candidate) {
            return true;
        }
    }
    false
}
