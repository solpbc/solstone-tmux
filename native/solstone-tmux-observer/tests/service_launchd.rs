// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

mod support;

use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};

use solstone_tmux_observer::command::{CommandInvocation, CommandOperation, ServiceOperation};
use solstone_tmux_observer::paths::PlatformKind;
use solstone_tmux_observer::service::launchd::{LABEL, artifact_path, render};
use solstone_tmux_observer::service::{COMMAND_TIMEOUT, ServiceController, TMUX_NOT_FOUND};
use support::{
    ExpectedInvocation, FakeEnvironment, FixtureRunner, TestDirectory, command_output,
    create_executable, output,
};

const USER_ID: u32 = 501;

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
    fs::create_dir_all(artifact.parent().expect("plist parent")).expect("plist parent");
    fs::write(
        &artifact,
        render(Path::new("/old/solstone-tmux-observer"), OsStr::new("/old")).expect("old plist"),
    )
    .expect("write old plist");
    let runner = OrderingRunner {
        artifact: artifact.clone(),
        new_binary: fs::canonicalize(&fixture.binary).expect("canonical binary"),
    };
    fixture.controller(&runner).install_blocking();
    let escaped_binary = runner
        .new_binary
        .to_str()
        .expect("UTF-8 binary")
        .replace('&', "&amp;");
    assert!(
        String::from_utf8_lossy(&fs::read(artifact).expect("new plist")).contains(&escaped_binary)
    );
}

#[test]
fn install_is_idempotent_when_launchd_bytes_match() {
    let fixture = ServiceFixture::new("launchd-idempotent", true);
    let first = FixtureRunner::new(launchd_install_expectations(&fixture));
    fixture.controller(&first).install_blocking();
    first.assert_finished().expect("first install");

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

fn launchd_install_expectations(fixture: &ServiceFixture) -> [ExpectedInvocation; 5] {
    [
        expected(
            ServiceOperation::LaunchdBootout,
            vec![
                "bootout".into(),
                domain().into(),
                artifact_path(&fixture.home).into_os_string(),
            ],
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
    environment: FakeEnvironment,
}

impl ServiceFixture {
    fn new(label: &str, with_tmux: bool) -> Self {
        let temporary = TestDirectory::new(label);
        let root = temporary.path().to_owned();
        let home = root.join("owner home;$&");
        let bin = root.join("tmux bin");
        let binary = root.join("observer bin;$&/solstone-tmux-observer");
        create_executable(&binary);
        if with_tmux {
            create_executable(&bin.join("tmux"));
        } else {
            fs::create_dir_all(&bin).expect("empty bin");
        }
        let environment = FakeEnvironment::from_paths([
            ("HOME", home.as_os_str().to_owned()),
            ("PATH", bin.into_os_string()),
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
                CommandOperation::Service(ServiceOperation::LaunchdBootout) => {
                    assert!(
                        String::from_utf8_lossy(&fs::read(&self.artifact).expect("old plist"))
                            .contains("/old/solstone-tmux-observer")
                    );
                }
                CommandOperation::Service(ServiceOperation::LaunchdEnable) => {
                    let escaped_binary = self
                        .new_binary
                        .to_str()
                        .expect("UTF-8 binary")
                        .replace('&', "&amp;");
                    assert!(
                        String::from_utf8_lossy(&fs::read(&self.artifact).expect("new plist"))
                            .contains(&escaped_binary)
                    );
                }
                _ => {}
            }
            Ok(solstone_tmux_observer::command::CommandOutput {
                stdout: Vec::new(),
                stderr: Vec::new(),
                status: 0,
            })
        })
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
