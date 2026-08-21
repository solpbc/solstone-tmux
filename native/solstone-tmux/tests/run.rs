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

use solstone_tmux::config::CONFIG_FILENAME;
use solstone_tmux::instance_lock::LOCK_FILENAME;
use solstone_tmux::private_link::{CREDENTIALS_FILENAME, acquire_private_state_lock};
use solstone_tmux::service::{LocalObserver, STATE_FILENAME};
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
fn disabled_status_indicator_never_touches_tmux_options() {
    let fixture = RunFixture::new("binary-run-disabled-indicator");
    fixture.install_recording_tmux();
    fixture.write_config_with_indicator("main", 1, 300, false);
    fixture.write_local_observer();
    let status_left = b"owner status \xff left".to_vec();
    let solstone = b"owner value \x00 retained".to_vec();
    fs::write(fixture.status_left_path(), &status_left).expect("write status-left fixture");
    fs::write(fixture.solstone_path(), &solstone).expect("write solstone fixture");

    let mut child = fixture.spawn_run();
    fixture.wait_for_startup_segment_metadata(&mut child);
    fixture.wait_for_tmux_invocation(&mut child);
    let pid = rustix::process::Pid::from_raw(child.id() as i32).expect("positive child pid");
    rustix::process::kill_process(pid, rustix::process::Signal::TERM)
        .expect("signal stub-backed observer");
    let status = child.wait().expect("reap stub-backed observer");

    assert!(status.success(), "observer exit status: {status}");
    let invocations = fixture.tmux_invocations();
    assert!(!invocations.is_empty(), "capture must invoke the tmux seam");
    assert!(
        invocations
            .iter()
            .all(|invocation| invocation == "list-clients"),
        "disabled indicator emitted an option invocation: {invocations:?}"
    );
    assert_eq!(
        fs::read(fixture.status_left_path()).expect("read status-left fixture"),
        status_left
    );
    assert_eq!(
        fs::read(fixture.solstone_path()).expect("read solstone fixture"),
        solstone
    );
}

#[test]
fn crash_restart_does_not_duplicate_the_status_indicator() {
    let fixture = RunFixture::new("binary-run-indicator-restart");
    fixture.install_recording_tmux();
    fixture.write_config("main", 1, 300);
    fixture.write_local_observer();
    fs::write(fixture.status_left_path(), b"owner status").expect("write owner status");

    let mut first = fixture.spawn_run();
    fixture.wait_for_startup_segment_metadata(&mut first);
    fixture.wait_for_tmux_invocation(&mut first);
    first.kill().expect("kill first observer");
    first.wait().expect("reap first observer");

    fs::write(&fixture.tmux_log, b"").expect("clear tmux invocation log");
    let mut second = fixture.spawn_run();
    fixture.wait_for_startup_segment_metadata(&mut second);
    fixture.wait_for_tmux_invocation(&mut second);

    let status_left =
        fs::read_to_string(fixture.status_left_path()).expect("read installed status-left");
    assert_eq!(status_left.matches("#{?@solstone").count(), 1);
    assert!(status_left.ends_with("owner status"));

    let pid = rustix::process::Pid::from_raw(second.id() as i32).expect("positive child pid");
    rustix::process::kill_process(pid, rustix::process::Signal::TERM)
        .expect("terminate second observer");
    let status = second.wait().expect("reap second observer");
    assert!(status.success(), "observer exit status: {status}");
    assert_eq!(
        fs::read(fixture.status_left_path()).expect("read restored status-left"),
        b"owner status"
    );
    assert!(!fixture.solstone_path().exists());
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
fn setup_checks_existing_run_lock_before_pairing_or_private_state_creation_with_default_roots() {
    let fixture = RunFixture::new("binary-setup-lock-default-roots");
    assert_setup_checks_existing_run_lock_before_pairing_or_private_state_creation(fixture);
}

#[test]
fn setup_checks_existing_run_lock_before_pairing_or_private_state_creation_with_aliased_roots() {
    let fixture = RunFixture::new_aliased("binary-setup-lock-aliased-roots");
    assert_setup_checks_existing_run_lock_before_pairing_or_private_state_creation(fixture);
}

fn assert_setup_checks_existing_run_lock_before_pairing_or_private_state_creation(
    fixture: RunFixture,
) {
    fs::create_dir_all(fixture.data_root()).expect("data root");
    let _active = solstone_tmux::instance_lock::InstanceLock::acquire(&fixture.data_root())
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
    assert!(!fixture.config_root().join(CREDENTIALS_FILENAME).exists());
    assert!(!fixture.data_root().join("captures").exists());
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
    tmux_log: PathBuf,
    first_child_stderr: PathBuf,
}

impl RunFixture {
    fn new(label: &str) -> Self {
        Self::new_with_roots(label, IsolatedRoots::new)
    }

    fn new_aliased(label: &str) -> Self {
        Self::new_with_roots(label, IsolatedRoots::new_aliased)
    }

    fn new_with_roots(label: &str, make_roots: fn(&Path) -> IsolatedRoots) -> Self {
        let temporary = TestDirectory::new(label);
        let roots = make_roots(temporary.path());
        let tmux = temporary.path().join("stub-bin/tmux");
        let tmux_log = temporary.path().join("tmux-invocations.txt");
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
            tmux_log,
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
        self.write_config_with_indicator(stream, capture_interval, segment_interval, true);
    }

    fn write_config_with_indicator(
        &self,
        stream: &str,
        capture_interval: u64,
        segment_interval: u64,
        status_indicator: bool,
    ) {
        fs::create_dir_all(self.config_root()).expect("config root");
        let bytes = serde_json::to_vec(&serde_json::json!({
            "stream": stream,
            "capture_interval": capture_interval,
            "segment_interval": segment_interval,
            "status_indicator": status_indicator,
        }))
        .expect("config JSON");
        fs::write(self.config_root().join(CONFIG_FILENAME), bytes).expect("write config");
    }

    fn install_recording_tmux(&self) {
        fs::write(
            &self.tmux,
            br#"#!/bin/sh
printf '%s\n' "$1" >> "$SOLSTONE_TMUX_TMUX_LOG"
case "$1:$2:$3" in
  show-options:-gv:status-left) cat "$SOLSTONE_TMUX_STATUS_LEFT" ;;
  show-options:-gv:@solstone) cat "$SOLSTONE_TMUX_SOLSTONE" ;;
  set-option:-g:status-left) printf '%s' "$4" > "$SOLSTONE_TMUX_STATUS_LEFT" ;;
  set-option:-g:@solstone) printf '%s' "$4" > "$SOLSTONE_TMUX_SOLSTONE" ;;
  set-option:-gu:status-left) rm -f "$SOLSTONE_TMUX_STATUS_LEFT" ;;
  set-option:-gu:@solstone) rm -f "$SOLSTONE_TMUX_SOLSTONE" ;;
esac
"#,
        )
        .expect("write recording tmux stub");
        fs::set_permissions(&self.tmux, fs::Permissions::from_mode(0o700))
            .expect("chmod recording tmux stub");
    }

    fn status_left_path(&self) -> PathBuf {
        self.tmux
            .parent()
            .expect("tmux stub parent")
            .join("status-left.bytes")
    }

    fn solstone_path(&self) -> PathBuf {
        self.tmux
            .parent()
            .expect("tmux stub parent")
            .join("solstone.bytes")
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
        let mut command = Command::new(env!("CARGO_BIN_EXE_solstone-tmux"));
        command
            .arg(subcommand)
            .env_clear()
            .envs(self.roots.entries().iter().cloned())
            .env("SOLSTONE_TMUX_TMUX_LOG", &self.tmux_log)
            .env("SOLSTONE_TMUX_STATUS_LEFT", self.status_left_path())
            .env("SOLSTONE_TMUX_SOLSTONE", self.solstone_path());
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

    fn wait_for_tmux_invocation(&self, child: &mut Child) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            assert!(
                child.try_wait().expect("observer status").is_none(),
                "observer exited before a tmux invocation; stderr: {:?}",
                self.read_first_child_stderr()
            );
            if self
                .tmux_invocations()
                .iter()
                .any(|invocation| invocation == "list-clients")
            {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!(
            "tmux was not invoked within five seconds; stderr: {:?}",
            self.read_first_child_stderr()
        );
    }

    fn tmux_invocations(&self) -> Vec<String> {
        fs::read_to_string(&self.tmux_log)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
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
