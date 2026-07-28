// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

mod support;

use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use solstone_tmux_observer::command::{CommandInvocation, CommandOperation, ServiceOperation};
use solstone_tmux_observer::paths::PlatformKind;
use solstone_tmux_observer::service::systemd::{UNIT_NAME, artifact_path, render};
use solstone_tmux_observer::service::{
    COMMAND_TIMEOUT, LocalObserver, STATE_FILENAME, ServiceController,
};
use support::{
    ExpectedInvocation, FakeEnvironment, FixtureRunner, TestDirectory, command_output,
    create_executable, output,
};

#[test]
fn systemd_unit_matches_required_sections() {
    let bytes = render(
        Path::new("/opt/solstone/bin/solstone-tmux-observer"),
        OsStr::new("/opt/tmux/bin:/usr/bin:/bin"),
    )
    .expect("render unit");
    let unit = String::from_utf8(bytes).expect("UTF-8 unit");
    assert!(unit.contains(
        "[Unit]\nDescription=Solstone Tmux Local Observer\nAfter=basic.target\n\
StartLimitIntervalSec=300\nStartLimitBurst=5\n"
    ));
    assert!(unit.contains(
        "[Service]\nType=simple\nEnvironment=\"PATH=/opt/tmux/bin:/usr/bin:/bin\"\n\
ExecStart=\"/opt/solstone/bin/solstone-tmux-observer\" run\n\
Restart=on-failure\nRestartSec=5\n"
    ));
    assert!(unit.contains("[Install]\nWantedBy=default.target\n"));
}

#[test]
fn systemd_escaping_handles_spaces_and_metacharacters() {
    let bytes = render(
        Path::new("/owner/a b/$bin;100%/solstone-tmux-observer"),
        OsStr::new("/owner/a b/$PATH;100%:/usr/bin"),
    )
    .expect("render escaped unit");
    let unit = String::from_utf8(bytes).expect("UTF-8 unit");
    assert!(unit.contains("ExecStart=\"/owner/a b/$bin;100%%/solstone-tmux-observer\" run"));
    assert!(unit.contains("Environment=\"PATH=/owner/a b/$PATH;100%%:/usr/bin\""));
    assert!(!unit.contains('\''));
}

#[test]
fn systemd_install_and_uninstall_argv_are_exact() {
    let fixture = ServiceFixture::new("systemd-argv");
    let install_runner = FixtureRunner::new([
        expected(
            ServiceOperation::SystemdStop,
            &["--user", "stop", UNIT_NAME],
            command_output(&[], b"unit not found", 4),
        ),
        expected(
            ServiceOperation::SystemdDaemonReload,
            &["--user", "daemon-reload"],
            output([]),
        ),
        expected(
            ServiceOperation::SystemdEnableNow,
            &["--user", "enable", "--now", UNIT_NAME],
            output([]),
        ),
        expected(
            ServiceOperation::SystemdIsActive,
            &["--user", "is-active", UNIT_NAME],
            output(b"active\n"),
        ),
    ]);
    fixture.controller(&install_runner).install_blocking();
    install_runner
        .assert_finished()
        .expect("install invocations");

    let state_path = fixture.config_root().join(STATE_FILENAME);
    let shared_config = fixture.config_root().join("shared-config.json");
    fs::write(&shared_config, b"keep").expect("shared config");
    let capture = fixture
        .root
        .join("data root/solstone-tmux/captures/keep.jsonl");
    fs::create_dir_all(capture.parent().expect("capture parent")).expect("capture parent");
    fs::write(&capture, b"keep").expect("capture");
    let uninstall_runner = FixtureRunner::new([
        expected(
            ServiceOperation::SystemdDisableNow,
            &["--user", "disable", "--now", UNIT_NAME],
            output([]),
        ),
        expected(
            ServiceOperation::SystemdDaemonReload,
            &["--user", "daemon-reload"],
            output([]),
        ),
    ]);
    fixture.controller(&uninstall_runner).uninstall_blocking();
    uninstall_runner
        .assert_finished()
        .expect("uninstall invocations");
    assert!(!artifact_path(&fixture.home).exists());
    assert!(!state_path.exists());
    assert_eq!(
        fs::read(shared_config).expect("shared config remains"),
        b"keep"
    );
    assert_eq!(fs::read(capture).expect("capture remains"), b"keep");
}

#[test]
fn unowned_systemd_artifact_is_rejected_before_manager_mutation() {
    let fixture = ServiceFixture::new("systemd-unowned-uninstall");
    let artifact = artifact_path(&fixture.home);
    fs::create_dir_all(artifact.parent().expect("unit parent")).expect("unit parent");
    fs::write(&artifact, b"[Unit]\nDescription=owner unit\n").expect("owner unit");
    let runner = FixtureRunner::default();

    let error = runtime()
        .block_on(fixture.controller(&runner).0.uninstall())
        .expect_err("unowned unit must be rejected");

    assert!(
        error
            .to_string()
            .contains("invalid or unowned service artifact")
    );
    assert!(runner.calls().is_empty());
    assert_eq!(
        fs::read(artifact).expect("owner unit preserved"),
        b"[Unit]\nDescription=owner unit\n"
    );
}

#[test]
fn tmux_is_resolved_absolute_and_persisted() {
    let fixture = ServiceFixture::new("tmux-persisted");
    let runner = FixtureRunner::new(systemd_install_expectations());
    fixture.controller(&runner).install_blocking();
    runner.assert_finished().expect("install invocations");

    let state_path = fixture.config_root().join(STATE_FILENAME);
    let state: LocalObserver =
        serde_json::from_slice(&fs::read(&state_path).expect("state bytes")).expect("state JSON");
    assert_eq!(
        state.tmux_path,
        fs::canonicalize(&fixture.tmux).expect("canonical tmux")
    );
    assert!(state.tmux_path.is_absolute());
    assert_eq!(
        fs::metadata(state_path)
            .expect("state metadata")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[test]
fn install_preserves_preexisting_shared_service_directory_mode() {
    let fixture = ServiceFixture::new("systemd-shared-directory-mode");
    let shared = artifact_path(&fixture.home)
        .parent()
        .expect("unit parent")
        .to_owned();
    fs::create_dir_all(&shared).expect("shared unit directory");
    fs::set_permissions(&shared, fs::Permissions::from_mode(0o755)).expect("shared mode");
    let runner = FixtureRunner::new(systemd_install_expectations());

    fixture.controller(&runner).install_blocking();

    runner.assert_finished().expect("install invocations");
    assert_eq!(
        fs::metadata(shared)
            .expect("shared directory")
            .permissions()
            .mode()
            & 0o777,
        0o755
    );
}

#[test]
fn failed_install_leaves_no_new_persisted_state() {
    let fixture = ServiceFixture::new("systemd-failed-install-state");
    let runner = FixtureRunner::new([
        expected(
            ServiceOperation::SystemdStop,
            &["--user", "stop", UNIT_NAME],
            command_output(&[], b"unit not found", 4),
        ),
        expected(
            ServiceOperation::SystemdDaemonReload,
            &["--user", "daemon-reload"],
            output([]),
        ),
        expected(
            ServiceOperation::SystemdEnableNow,
            &["--user", "enable", "--now", UNIT_NAME],
            command_output(&[], b"enable failed", 1),
        ),
    ]);

    let error = runtime()
        .block_on(fixture.controller(&runner).0.install())
        .expect_err("manager failure must fail install");

    assert!(error.to_string().contains("systemd enable failed"));
    runner.assert_finished().expect("install invocations");
    assert!(!fixture.config_root().join(STATE_FILENAME).exists());
}

#[test]
fn unjoinable_service_path_is_actionable_and_does_not_drop_tmux_directory() {
    let temporary = TestDirectory::new("systemd-unjoinable-path");
    let root = temporary.path();
    let home = root.join("home");
    let binary = root.join("bin/solstone-tmux-observer");
    let tmux_directory = root.join("tmux:bin");
    create_executable(&binary);
    create_executable(&tmux_directory.join("tmux"));
    let environment = FakeEnvironment::from_paths([
        ("HOME", home.into_os_string()),
        ("PATH", OsString::new()),
        ("XDG_CONFIG_HOME", root.join("config").into_os_string()),
        ("XDG_DATA_HOME", root.join("data").into_os_string()),
    ]);
    let runner = FixtureRunner::default();
    let controller = ServiceController::new(PlatformKind::Linux, &environment, &runner, binary)
        .with_standard_directories(vec![tmux_directory]);

    let error = runtime()
        .block_on(controller.install())
        .expect_err("a colon-containing PATH entry cannot be serialized");

    assert!(
        error
            .to_string()
            .contains("could not assemble service PATH")
    );
    assert!(error.to_string().contains("rerun install-service"));
    assert!(runner.calls().is_empty());
}

#[test]
fn systemd_replacement_stops_before_atomic_write() {
    let fixture = ServiceFixture::new("systemd-order");
    let artifact = artifact_path(&fixture.home);
    fs::create_dir_all(artifact.parent().expect("unit parent")).expect("unit parent");
    fs::write(
        &artifact,
        render(Path::new("/old/solstone-tmux-observer"), OsStr::new("/old")).expect("old unit"),
    )
    .expect("write old unit");
    let runner = OrderingRunner {
        artifact: artifact.clone(),
        new_binary: fs::canonicalize(&fixture.binary).expect("canonical binary"),
    };

    fixture.controller(&runner).install_blocking();
    let bytes = fs::read(artifact).expect("new unit");
    assert!(
        String::from_utf8_lossy(&bytes).contains(
            fixture
                .binary
                .canonicalize()
                .expect("canonical binary")
                .to_str()
                .expect("UTF-8 path")
        )
    );
}

#[test]
fn install_is_idempotent() {
    let fixture = ServiceFixture::new("systemd-idempotent");
    let first = FixtureRunner::new(systemd_install_expectations());
    fixture.controller(&first).install_blocking();
    first.assert_finished().expect("first install");

    let second = FixtureRunner::new([
        expected(
            ServiceOperation::SystemdEnableNow,
            &["--user", "enable", "--now", UNIT_NAME],
            output([]),
        ),
        expected(
            ServiceOperation::SystemdIsActive,
            &["--user", "is-active", UNIT_NAME],
            output(b"active\n"),
        ),
    ]);
    fixture.controller(&second).install_blocking();
    second.assert_finished().expect("idempotent install");
}

#[test]
fn rust_service_identity_never_references_python_unit() {
    let fixture = ServiceFixture::new("systemd-identity");
    let runner = FixtureRunner::new(systemd_install_expectations());
    fixture.controller(&runner).install_blocking();
    for call in runner.calls() {
        assert!(
            !call
                .executable
                .to_string_lossy()
                .contains("solstone-tmux.service")
        );
        assert!(
            call.args
                .iter()
                .all(|arg| !arg.to_string_lossy().contains("solstone-tmux.service"))
        );
    }
    assert!(
        !artifact_path(&fixture.home)
            .to_string_lossy()
            .contains("solstone-tmux.service")
    );
    assert!(
        !String::from_utf8_lossy(&fs::read(artifact_path(&fixture.home)).expect("unit"))
            .contains("solstone-tmux.service")
    );
}

fn systemd_install_expectations() -> [ExpectedInvocation; 4] {
    [
        expected(
            ServiceOperation::SystemdStop,
            &["--user", "stop", UNIT_NAME],
            command_output(&[], b"unit not found", 4),
        ),
        expected(
            ServiceOperation::SystemdDaemonReload,
            &["--user", "daemon-reload"],
            output([]),
        ),
        expected(
            ServiceOperation::SystemdEnableNow,
            &["--user", "enable", "--now", UNIT_NAME],
            output([]),
        ),
        expected(
            ServiceOperation::SystemdIsActive,
            &["--user", "is-active", UNIT_NAME],
            output(b"active\n"),
        ),
    ]
}

fn expected(
    operation: ServiceOperation,
    args: &[&str],
    outcome: support::FixtureOutcome,
) -> ExpectedInvocation {
    ExpectedInvocation {
        invocation: CommandInvocation {
            operation: CommandOperation::Service(operation),
            executable: PathBuf::from("systemctl"),
            args: args.iter().map(OsString::from).collect(),
            timeout: COMMAND_TIMEOUT,
        },
        outcome,
    }
}

struct ServiceFixture {
    _temporary: TestDirectory,
    home: PathBuf,
    root: PathBuf,
    binary: PathBuf,
    tmux: PathBuf,
    environment: FakeEnvironment,
}

impl ServiceFixture {
    fn new(label: &str) -> Self {
        let temporary = TestDirectory::new(label);
        let root = temporary.path().to_owned();
        let home = root.join("owner home;$&");
        let bin = root.join("tmux bin");
        let binary = root.join("observer bin;$&/solstone-tmux-observer");
        let tmux = bin.join("tmux");
        create_executable(&binary);
        create_executable(&tmux);
        let environment = FakeEnvironment::from_paths([
            ("HOME", home.as_os_str().to_owned()),
            ("PATH", bin.as_os_str().to_owned()),
            ("XDG_CONFIG_HOME", root.join("config root").into_os_string()),
            ("XDG_DATA_HOME", root.join("data root").into_os_string()),
            ("UID", OsString::from("1001")),
        ]);
        Self {
            _temporary: temporary,
            home,
            root,
            binary,
            tmux,
            environment,
        }
    }

    fn controller<'a>(
        &'a self,
        runner: &'a dyn solstone_tmux_observer::command::CommandRunner,
    ) -> Controller<'a> {
        Controller(
            ServiceController::new(
                PlatformKind::Linux,
                &self.environment,
                runner,
                self.binary.clone(),
            )
            .with_standard_directories(Vec::new()),
        )
    }

    fn config_root(&self) -> PathBuf {
        self.root.join("config root/solstone-tmux")
    }
}

struct Controller<'a>(ServiceController<'a>);

impl Controller<'_> {
    fn install_blocking(&self) {
        runtime()
            .block_on(self.0.install())
            .expect("install service");
    }

    fn uninstall_blocking(&self) {
        runtime()
            .block_on(self.0.uninstall())
            .expect("uninstall service");
    }
}

struct OrderingRunner {
    artifact: PathBuf,
    new_binary: PathBuf,
}

impl solstone_tmux_observer::command::CommandRunner for OrderingRunner {
    fn run<'a>(
        &'a self,
        invocation: CommandInvocation,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<
                        solstone_tmux_observer::command::CommandOutput,
                        solstone_tmux_observer::command::CommandError,
                    >,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            match invocation.operation {
                CommandOperation::Service(ServiceOperation::SystemdStop) => {
                    assert!(
                        String::from_utf8_lossy(&fs::read(&self.artifact).expect("old unit"))
                            .contains("/old/solstone-tmux-observer")
                    );
                }
                CommandOperation::Service(ServiceOperation::SystemdDaemonReload) => {
                    assert!(
                        String::from_utf8_lossy(&fs::read(&self.artifact).expect("new unit"))
                            .contains(self.new_binary.to_str().expect("UTF-8 binary"))
                    );
                }
                _ => {}
            }
            Ok(solstone_tmux_observer::command::CommandOutput {
                stdout: if invocation.operation
                    == CommandOperation::Service(ServiceOperation::SystemdIsActive)
                {
                    b"active\n".to_vec()
                } else {
                    Vec::new()
                },
                stderr: Vec::new(),
                status: 0,
            })
        })
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("test runtime")
}
