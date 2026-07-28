// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

mod support;

use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::fs::{MetadataExt, symlink};
use std::path::{Path, PathBuf};

use solstone_tmux_observer::command::{
    CommandError, CommandInvocation, CommandOperation, CommandOutput, CommandRunner,
    ServiceOperation,
};
use solstone_tmux_observer::paths::PlatformKind;
use solstone_tmux_observer::service::launchd::{LABEL, artifact_path, render};
use solstone_tmux_observer::service::{
    COMMAND_TIMEOUT, LocalObserver, STATE_FILENAME, ServiceController, TMUX_NOT_FOUND,
};
use support::{
    ExpectedInvocation, FakeEnvironment, FixtureRunner, TestDirectory, command_output,
    create_executable, output,
};

const USER_ID: u32 = 501;
const STATUS_FIVE_ERROR: &[u8] = b"Boot-out failed: 5: Input/output error";

#[test]
fn launchd_plist_has_required_keys() {
    let plist = String::from_utf8(
        render(
            Path::new("/Applications/Solstone/solstone-tmux-observer"),
            OsStr::new("/opt/homebrew/bin:/usr/bin:/bin"),
        )
        .expect("render plist"),
    )
    .expect("UTF-8 plist");
    for required in [
        "<key>Label</key>",
        "<string>com.solstone.tmux-observer</string>",
        "<key>ProgramArguments</key>",
        "<string>/Applications/Solstone/solstone-tmux-observer</string>",
        "<string>run</string>",
        "<key>EnvironmentVariables</key>",
        "<key>PATH</key>",
        "<key>RunAtLoad</key>",
        "<true/>",
        "<key>KeepAlive</key>",
        "<key>SuccessfulExit</key>",
        "<false/>",
        "<key>ThrottleInterval</key>",
        "<integer>5</integer>",
        "<key>ProcessType</key>",
        "<string>Background</string>",
    ] {
        assert!(plist.contains(required), "missing {required}");
    }
}

#[test]
fn launchd_xml_and_arguments_handle_spaces_and_metacharacters() {
    let plist = String::from_utf8(
        render(
            Path::new("/Owner/a b/$bin;&<>'\"/solstone-tmux-observer"),
            OsStr::new("/Owner/a b/$PATH;&<>'\":/usr/bin"),
        )
        .expect("render escaped plist"),
    )
    .expect("UTF-8 plist");
    assert!(plist.contains(
        "<string>/Owner/a b/$bin;&amp;&lt;&gt;&apos;&quot;/solstone-tmux-observer</string>"
    ));
    assert!(plist.contains("<string>/Owner/a b/$PATH;&amp;&lt;&gt;&apos;&quot;:/usr/bin</string>"));
    let arguments = plist
        .find("<key>ProgramArguments</key>")
        .expect("arguments key");
    let binary = plist[arguments..]
        .find("<string>/Owner/a b/$bin;&amp;&lt;&gt;&apos;&quot;/solstone-tmux-observer</string>")
        .expect("binary argument");
    let run = plist[arguments..]
        .find("<string>run</string>")
        .expect("run argument");
    assert!(
        binary < run,
        "binary and run must be separate ordered arguments"
    );
}

#[test]
fn launchd_install_and_uninstall_argv_are_exact() {
    let fixture = ServiceFixture::new("launchd-argv", true);
    let install_runner = FixtureRunner::new(launchd_install_expectations(&fixture));
    fixture.controller(&install_runner).install_blocking();
    install_runner
        .assert_finished()
        .expect("install invocations");
    for call in install_runner.calls() {
        assert!(
            call.args
                .iter()
                .all(|arg| !arg.to_string_lossy().contains("solstone-tmux.service"))
        );
    }

    let uninstall_runner = FixtureRunner::new([expected(
        ServiceOperation::LaunchdBootout,
        vec![
            "bootout".into(),
            domain().into(),
            artifact_path(&fixture.home).into_os_string(),
        ],
        output([]),
    )]);
    fixture.controller(&uninstall_runner).uninstall_blocking();
    uninstall_runner
        .assert_finished()
        .expect("uninstall invocations");
    assert!(!artifact_path(&fixture.home).exists());
    assert!(!fixture.config_root().join("local-observer.json").exists());
}

#[test]
fn launchd_first_install_skips_bootout_and_persists_before_bootstrap() {
    let fixture = ServiceFixture::new("launchd-first-install", true);
    let artifact = artifact_path(&fixture.home);
    let state_path = fixture.config_root().join(STATE_FILENAME);
    let desired_bytes = expected_plist_bytes(&fixture);
    let expected_tmux = fs::canonicalize(&fixture.tmux).expect("canonical tmux");
    let runner = InspectingRunner {
        inner: FixtureRunner::new(launchd_install_expectations(&fixture)),
        inspect: Box::new(move |invocation| match invocation.operation {
            CommandOperation::Service(ServiceOperation::LaunchdEnable) => {
                assert_eq!(
                    fs::read(&artifact).expect("plist before enable"),
                    desired_bytes
                );
                assert!(!state_path.exists());
            }
            CommandOperation::Service(ServiceOperation::LaunchdBootstrap) => {
                assert_eq!(
                    fs::read(&artifact).expect("plist before bootstrap"),
                    desired_bytes
                );
                assert_local_observer(&state_path, &expected_tmux);
            }
            _ => {}
        }),
    };

    fixture.controller(&runner).install_blocking();

    runner
        .inner
        .assert_finished()
        .expect("first-install invocations");
    assert!(
        !runner.inner.calls().iter().any(|call| matches!(
            call.operation,
            CommandOperation::Service(ServiceOperation::LaunchdBootout)
        )),
        "first install must not boot out an absent job"
    );
}

#[test]
fn unowned_launchd_artifact_is_rejected_before_manager_mutation() {
    let fixture = ServiceFixture::new("launchd-unowned-uninstall", true);
    let artifact = artifact_path(&fixture.home);
    fs::create_dir_all(artifact.parent().expect("plist parent")).expect("plist parent");
    fs::write(&artifact, b"<plist><string>owner label</string></plist>\n").expect("owner plist");
    let runner = FixtureRunner::default();

    let error = runtime()
        .block_on(fixture.controller(&runner).0.uninstall())
        .expect_err("unowned plist must be rejected");

    assert!(
        error
            .to_string()
            .contains("invalid or unowned service artifact")
    );
    assert!(runner.calls().is_empty());
    assert_eq!(
        fs::read(artifact).expect("owner plist preserved"),
        b"<plist><string>owner label</string></plist>\n"
    );
}

#[test]
fn launchd_replacement_boots_out_before_write() {
    let fixture = ServiceFixture::new("launchd-order", true);
    let artifact = artifact_path(&fixture.home);
    let state_path = fixture.config_root().join(STATE_FILENAME);
    fs::create_dir_all(artifact.parent().expect("plist parent")).expect("plist parent");
    let old_bytes =
        render(Path::new("/old/solstone-tmux-observer"), OsStr::new("/old")).expect("old plist");
    fs::write(&artifact, &old_bytes).expect("write old plist");
    let desired_bytes = expected_plist_bytes(&fixture);
    let expected_tmux = fs::canonicalize(&fixture.tmux).expect("canonical tmux");
    let inspected_artifact = artifact.clone();
    let inspected_old_bytes = old_bytes.clone();
    let inspected_desired_bytes = desired_bytes.clone();
    let runner = InspectingRunner {
        inner: FixtureRunner::new(
            [expected(
                ServiceOperation::LaunchdBootout,
                vec![
                    "bootout".into(),
                    domain().into(),
                    artifact.clone().into_os_string(),
                ],
                command_output(&[], b"service not loaded", 3),
            )]
            .into_iter()
            .chain(launchd_install_expectations(&fixture)),
        ),
        inspect: Box::new(move |invocation| match invocation.operation {
            CommandOperation::Service(ServiceOperation::LaunchdBootout) => {
                assert_eq!(
                    fs::read(&inspected_artifact).expect("plist before bootout"),
                    inspected_old_bytes
                );
            }
            CommandOperation::Service(ServiceOperation::LaunchdEnable) => {
                assert_eq!(
                    fs::read(&inspected_artifact).expect("plist before enable"),
                    inspected_desired_bytes
                );
            }
            CommandOperation::Service(ServiceOperation::LaunchdBootstrap) => {
                assert_eq!(
                    fs::read(&inspected_artifact).expect("plist before bootstrap"),
                    inspected_desired_bytes
                );
                assert_local_observer(&state_path, &expected_tmux);
            }
            _ => {}
        }),
    };

    fixture.controller(&runner).install_blocking();

    runner
        .inner
        .assert_finished()
        .expect("replacement invocations");
    assert_eq!(
        fs::read(artifact).expect("new plist"),
        desired_bytes,
        "replacement must install the exact desired plist"
    );
}

#[test]
fn launchd_replacement_bootout_error_preserves_artifacts() {
    let fixture = ServiceFixture::new("launchd-bootout-error", true);
    let artifact = artifact_path(&fixture.home);
    fs::create_dir_all(artifact.parent().expect("plist parent")).expect("plist parent");
    let old_bytes =
        render(Path::new("/old/solstone-tmux-observer"), OsStr::new("/old")).expect("old plist");
    fs::write(&artifact, &old_bytes).expect("write old plist");
    let state_path = fixture.config_root().join(STATE_FILENAME);
    fs::create_dir_all(fixture.config_root()).expect("config root");
    let previous_state = b"{\"tmux_path\":\"/previous/tmux\"}\n";
    fs::write(&state_path, previous_state).expect("previous local observer");
    let runner = FixtureRunner::new([expected(
        ServiceOperation::LaunchdBootout,
        vec![
            "bootout".into(),
            domain().into(),
            artifact.clone().into_os_string(),
        ],
        command_output(&[], STATUS_FIVE_ERROR, 5),
    )]);

    let error = runtime()
        .block_on(fixture.controller(&runner).0.install())
        .expect_err("bootout failure must fail replacement");

    assert!(error.to_string().contains("launchd bootout failed"));
    assert!(
        error
            .to_string()
            .contains("Boot-out failed: 5: Input/output error")
    );
    runner
        .assert_finished()
        .expect("failed replacement invocation");
    let calls = runner.calls();
    assert_eq!(calls.len(), 1);
    assert!(matches!(
        calls[0].operation,
        CommandOperation::Service(ServiceOperation::LaunchdBootout)
    ));
    assert_eq!(
        fs::read(artifact).expect("old plist after failed replacement"),
        old_bytes
    );
    assert_eq!(
        fs::read(state_path).expect("previous local observer after failed replacement"),
        previous_state
    );
}

#[test]
fn launchd_install_rejects_invalid_artifacts_before_mutation() {
    #[derive(Clone, Copy)]
    enum InvalidCase {
        UnownedRegular,
        OwnedSymlink,
        DanglingSymlink,
        Directory,
    }

    for (label, case) in [
        ("unowned-regular", InvalidCase::UnownedRegular),
        ("owned-symlink", InvalidCase::OwnedSymlink),
        ("dangling-symlink", InvalidCase::DanglingSymlink),
        ("directory", InvalidCase::Directory),
    ] {
        let fixture = ServiceFixture::new(label, true);
        let artifact = artifact_path(&fixture.home);
        fs::create_dir_all(artifact.parent().expect("plist parent")).expect("plist parent");
        let preserved_bytes = b"preserved owner bytes\n";
        let owned_bytes = render(
            Path::new("/owned/solstone-tmux-observer"),
            OsStr::new("/owned"),
        )
        .expect("owned plist");
        let referent = fixture.root.join("owned referent.plist");
        let dangling = fixture.root.join("missing referent.plist");
        let sentinel = artifact.join("sentinel");

        match case {
            InvalidCase::UnownedRegular => {
                fs::write(&artifact, preserved_bytes).expect("unowned plist");
            }
            InvalidCase::OwnedSymlink => {
                fs::write(&referent, &owned_bytes).expect("owned referent");
                symlink(&referent, &artifact).expect("owned plist symlink");
            }
            InvalidCase::DanglingSymlink => {
                symlink(&dangling, &artifact).expect("dangling plist symlink");
            }
            InvalidCase::Directory => {
                fs::create_dir(&artifact).expect("plist directory");
                fs::write(&sentinel, preserved_bytes).expect("directory sentinel");
            }
        }
        let runner = FixtureRunner::default();

        let error = runtime()
            .block_on(fixture.controller(&runner).0.install())
            .expect_err("invalid plist must be rejected");

        assert!(
            error
                .to_string()
                .contains("invalid or unowned service artifact"),
            "{label}: {error}"
        );
        assert!(runner.calls().is_empty(), "{label}: no launchctl calls");
        match case {
            InvalidCase::UnownedRegular => {
                assert_eq!(fs::read(&artifact).expect("unowned plist"), preserved_bytes);
            }
            InvalidCase::OwnedSymlink => {
                assert!(
                    fs::symlink_metadata(&artifact)
                        .expect("owned symlink metadata")
                        .file_type()
                        .is_symlink()
                );
                assert_eq!(fs::read_link(&artifact).expect("owned symlink"), referent);
                assert_eq!(fs::read(referent).expect("owned referent"), owned_bytes);
            }
            InvalidCase::DanglingSymlink => {
                assert!(
                    fs::symlink_metadata(&artifact)
                        .expect("dangling symlink metadata")
                        .file_type()
                        .is_symlink()
                );
                assert_eq!(
                    fs::read_link(&artifact).expect("dangling symlink"),
                    dangling
                );
                assert!(!dangling.exists());
            }
            InvalidCase::Directory => {
                assert!(fs::metadata(&artifact).expect("plist directory").is_dir());
                assert_eq!(
                    fs::read(sentinel).expect("directory sentinel"),
                    preserved_bytes
                );
            }
        }
    }
}

#[test]
fn install_is_idempotent_when_launchd_bytes_match() {
    let fixture = ServiceFixture::new("launchd-idempotent", true);
    let first = FixtureRunner::new(launchd_install_expectations(&fixture));
    fixture.controller(&first).install_blocking();
    first.assert_finished().expect("first install");
    let artifact = artifact_path(&fixture.home);
    let desired_bytes = expected_plist_bytes(&fixture);
    assert_eq!(fs::read(&artifact).expect("first plist"), desired_bytes);
    let inode_before = fs::metadata(&artifact).expect("first plist metadata").ino();

    let second = FixtureRunner::new([
        expected(
            ServiceOperation::LaunchdPrint,
            vec!["print".into(), target().into()],
            output(b"loaded\n"),
        ),
        expected(
            ServiceOperation::LaunchdKickstart,
            vec!["kickstart".into(), "-k".into(), target().into()],
            output([]),
        ),
        expected(
            ServiceOperation::LaunchdPrint,
            vec!["print".into(), target().into()],
            output(b"loaded\n"),
        ),
    ]);
    fixture.controller(&second).install_blocking();
    second.assert_finished().expect("idempotent install");
    assert_eq!(
        fs::metadata(&artifact)
            .expect("idempotent plist metadata")
            .ino(),
        inode_before,
        "an unchanged plist must not be rewritten"
    );
    assert_eq!(fs::read(artifact).expect("idempotent plist"), desired_bytes);
}

#[test]
fn unchanged_launchd_plist_bootstraps_absent_job_without_rewrite() {
    let fixture = ServiceFixture::new("launchd-unchanged-absent", true);
    let artifact = artifact_path(&fixture.home);
    fs::create_dir_all(artifact.parent().expect("plist parent")).expect("plist parent");
    let desired_bytes = expected_plist_bytes(&fixture);
    fs::write(&artifact, &desired_bytes).expect("desired plist");
    let inode_before = fs::metadata(&artifact)
        .expect("desired plist metadata")
        .ino();
    let runner = FixtureRunner::new([
        expected(
            ServiceOperation::LaunchdPrint,
            vec!["print".into(), target().into()],
            command_output(&[], b"service not loaded", 3),
        ),
        expected(
            ServiceOperation::LaunchdEnable,
            vec!["enable".into(), target().into()],
            output([]),
        ),
        expected(
            ServiceOperation::LaunchdBootstrap,
            vec![
                "bootstrap".into(),
                domain().into(),
                artifact.clone().into_os_string(),
            ],
            output([]),
        ),
        expected(
            ServiceOperation::LaunchdKickstart,
            vec!["kickstart".into(), "-k".into(), target().into()],
            output([]),
        ),
        expected(
            ServiceOperation::LaunchdPrint,
            vec!["print".into(), target().into()],
            output(b"loaded\n"),
        ),
    ]);

    fixture.controller(&runner).install_blocking();

    runner
        .assert_finished()
        .expect("unchanged absent-job invocations");
    assert_eq!(
        fs::metadata(&artifact)
            .expect("unchanged plist metadata")
            .ino(),
        inode_before,
        "an unchanged plist must not be rewritten"
    );
    assert_eq!(fs::read(artifact).expect("unchanged plist"), desired_bytes);
}

#[test]
fn launchd_status_five_is_manager_error_not_inactive() {
    let fixture = ServiceFixture::new("launchd-status-five", true);
    let artifact = artifact_path(&fixture.home);
    fs::create_dir_all(artifact.parent().expect("plist parent")).expect("plist parent");
    fs::write(&artifact, expected_plist_bytes(&fixture)).expect("owned plist");
    let runner = FixtureRunner::new([expected(
        ServiceOperation::LaunchdPrint,
        vec!["print".into(), target().into()],
        command_output(&[], STATUS_FIVE_ERROR, 5),
    )]);

    let error = runtime()
        .block_on(fixture.controller(&runner).0.status())
        .expect_err("status 5 must remain a manager error");

    assert!(error.to_string().contains("launchd status failed"));
    assert!(
        error
            .to_string()
            .contains("Boot-out failed: 5: Input/output error")
    );
    runner.assert_finished().expect("status-five invocation");
}

#[test]
fn minimal_path_missing_tmux_is_actionable() {
    let fixture = ServiceFixture::new("launchd-no-tmux", false);
    let runner = FixtureRunner::default();
    let error = runtime()
        .block_on(fixture.controller(&runner).0.install())
        .expect_err("tmux must be required");
    assert_eq!(error.to_string(), TMUX_NOT_FOUND);
    assert!(runner.calls().is_empty());
}

fn launchd_install_expectations(fixture: &ServiceFixture) -> [ExpectedInvocation; 4] {
    [
        expected(
            ServiceOperation::LaunchdEnable,
            vec!["enable".into(), target().into()],
            output([]),
        ),
        expected(
            ServiceOperation::LaunchdBootstrap,
            vec![
                "bootstrap".into(),
                domain().into(),
                artifact_path(&fixture.home).into_os_string(),
            ],
            output([]),
        ),
        expected(
            ServiceOperation::LaunchdKickstart,
            vec!["kickstart".into(), "-k".into(), target().into()],
            output([]),
        ),
        expected(
            ServiceOperation::LaunchdPrint,
            vec!["print".into(), target().into()],
            output(b"loaded\n"),
        ),
    ]
}

fn expected(
    operation: ServiceOperation,
    args: Vec<OsString>,
    outcome: support::FixtureOutcome,
) -> ExpectedInvocation {
    ExpectedInvocation {
        invocation: CommandInvocation {
            operation: CommandOperation::Service(operation),
            executable: PathBuf::from("launchctl"),
            args,
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
    path_entry: PathBuf,
    environment: FakeEnvironment,
}

impl ServiceFixture {
    fn new(label: &str, with_tmux: bool) -> Self {
        let temporary = TestDirectory::new(label);
        let root = temporary.path().to_owned();
        let home = root.join("owner home;$&");
        let bin = root.join("tmux bin");
        let tmux = bin.join("tmux");
        let binary = root.join("observer bin;$&/solstone-tmux-observer");
        create_executable(&binary);
        if with_tmux {
            create_executable(&tmux);
        } else {
            fs::create_dir_all(&bin).expect("empty bin");
        }
        let environment = FakeEnvironment::from_paths([
            ("HOME", home.as_os_str().to_owned()),
            ("PATH", bin.as_os_str().to_owned()),
            (
                "XDG_CONFIG_HOME",
                root.join("ignored config").into_os_string(),
            ),
            ("UID", OsString::from(USER_ID.to_string())),
        ]);
        Self {
            _temporary: temporary,
            home,
            root,
            binary,
            tmux,
            path_entry: bin,
            environment,
        }
    }

    fn controller<'a>(
        &'a self,
        runner: &'a dyn solstone_tmux_observer::command::CommandRunner,
    ) -> Controller<'a> {
        Controller(
            ServiceController::new(
                PlatformKind::Macos,
                &self.environment,
                runner,
                self.binary.clone(),
            )
            .with_user_id(USER_ID)
            .with_standard_directories(Vec::new()),
        )
    }

    fn config_root(&self) -> PathBuf {
        self.root
            .join("owner home;$&/Library/Application Support/solstone-tmux")
    }
}

fn expected_plist_bytes(fixture: &ServiceFixture) -> Vec<u8> {
    let binary = fs::canonicalize(&fixture.binary).expect("canonical binary");
    let tmux = fs::canonicalize(&fixture.tmux).expect("canonical tmux");
    let tmux_parent = tmux.parent().expect("tmux parent").to_owned();
    let mut service_directories = vec![tmux_parent];
    if !service_directories.contains(&fixture.path_entry) {
        service_directories.push(fixture.path_entry.clone());
    }
    let service_path = std::env::join_paths(service_directories).expect("service PATH");
    render(&binary, &service_path).expect("desired plist")
}

fn assert_local_observer(path: &Path, expected_tmux: &Path) {
    assert!(
        fs::metadata(path)
            .expect("local observer before bootstrap")
            .is_file()
    );
    let state: LocalObserver =
        serde_json::from_slice(&fs::read(path).expect("local observer bytes before bootstrap"))
            .expect("local observer JSON before bootstrap");
    assert_eq!(state.tmux_path, expected_tmux);
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

struct InspectingRunner {
    inner: FixtureRunner,
    inspect: Box<dyn Fn(&CommandInvocation) + Send + Sync>,
}

impl CommandRunner for InspectingRunner {
    fn run<'a>(
        &'a self,
        invocation: CommandInvocation,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<CommandOutput, CommandError>> + Send + 'a>,
    > {
        (self.inspect)(&invocation);
        self.inner.run(invocation)
    }
}

fn domain() -> String {
    format!("gui/{USER_ID}")
}

fn target() -> String {
    format!("{}/{LABEL}", domain())
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("test runtime")
}
