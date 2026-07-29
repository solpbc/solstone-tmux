// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

mod support;

use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::fs::{MetadataExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use solstone_tmux_observer::command::{
    CommandError, CommandInvocation, CommandOperation, CommandOutput, CommandRunner,
    ServiceOperation,
};
use solstone_tmux_observer::paths::PlatformKind;
use solstone_tmux_observer::service::launchd::{LABEL, artifact_path, render};
use solstone_tmux_observer::service::{
    COMMAND_TIMEOUT, LocalObserver, STATE_FILENAME, ServiceController, ServiceError, ServiceStatus,
    TMUX_NOT_FOUND,
};
use support::{
    ExpectedInvocation, FakeEnvironment, FixtureOutcome, FixtureRunner, TestDirectory,
    command_output, create_executable, launchd_absent, launchd_missing_service_line, output,
};

const USER_ID: u32 = 501;
const STATUS_FIVE_ERROR: &[u8] = b"Boot-out failed: 5: Input/output error";
const RUNNING: &[u8] = include_bytes!("data/launchd/running.txt");
const QUIESCENT: &[u8] = include_bytes!("data/launchd/loaded-not-running.txt");
const NESTED_PID_ONLY: &[u8] = include_bytes!("data/launchd/nested-pid-only.txt");
const LOCALE_KEY: &str = "LC_ALL";
const LOCALE_VALUE: &str = "UTF-8";

#[test]
fn launchd_plist_has_required_structure_and_locale() {
    let binary = "/Applications/Solstone/solstone-tmux-observer";
    let service_path = "/opt/homebrew/bin:/usr/bin:/bin";
    let plist = read_rendered_launchd_plist(
        &render(Path::new(binary), OsStr::new(service_path)).expect("render plist"),
    );

    assert_eq!(plist.label, LABEL);
    assert_eq!(plist.program_arguments, [binary, "run"]);
    // LC_ALL, rather than LC_CTYPE, is required because a nonempty inherited LC_ALL
    // overrides every other locale category. LC_CTYPE alone therefore cannot guarantee
    // that tmux returns the indicator's UTF-8 bytes unchanged.
    assert_eq!(
        plist.environment_variables,
        expected_environment_variables(service_path)
    );
    assert!(plist.run_at_load);
    assert_eq!(plist.keep_alive, [("SuccessfulExit".to_owned(), false)]);
    assert_eq!(plist.throttle_interval, 5);
    assert_eq!(plist.process_type, "Background");
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
    assert!(binary < run);
}

#[test]
fn launchd_install_and_uninstall_argv_are_exact() {
    let fixture = ServiceFixture::new("launchd-argv", true);
    let install_runner = FixtureRunner::new(launchd_install_expectations(&fixture));
    fixture.controller(&install_runner).install_blocking();
    install_runner
        .assert_finished()
        .expect("install invocations");

    let uninstall_runner = FixtureRunner::new(vec![
        disable(output([])),
        print(running()),
        kill(output([])),
        print(running()),
        print(quiescent()),
        bootout(&fixture, output([])),
    ]);
    fixture.controller(&uninstall_runner).uninstall_blocking();
    uninstall_runner
        .assert_finished()
        .expect("uninstall invocations");

    let calls = install_runner
        .calls()
        .into_iter()
        .chain(uninstall_runner.calls());
    for call in calls {
        assert!(
            call.args
                .iter()
                .all(|arg| !arg.to_string_lossy().contains("solstone-tmux.service"))
        );
    }
    assert!(!artifact_path(&fixture.home).exists());
    assert!(!fixture.config_root().join(STATE_FILENAME).exists());
}

#[test]
fn launchd_first_install_skips_stop_and_persists_before_bootstrap() {
    let fixture = ServiceFixture::new("launchd-first-install", true);
    let artifact = artifact_path(&fixture.home);
    let state_path = fixture.config_root().join(STATE_FILENAME);
    let desired_bytes = expected_plist_bytes(&fixture);
    let expected_tmux = fs::canonicalize(&fixture.tmux).expect("canonical tmux");
    let runner = InspectingRunner {
        inner: FixtureRunner::new(launchd_install_expectations(&fixture)),
        inspect: Box::new(move |invocation| match invocation.operation {
            CommandOperation::Service(ServiceOperation::LaunchdEnable) => {
                assert_eq!(fs::read(&artifact).expect("plist"), desired_bytes);
                assert!(!state_path.exists());
            }
            CommandOperation::Service(ServiceOperation::LaunchdBootstrap) => {
                assert_eq!(fs::read(&artifact).expect("plist"), desired_bytes);
                assert_local_observer(&state_path, &expected_tmux);
            }
            _ => {}
        }),
    };

    fixture.controller(&runner).install_blocking();

    runner.inner.assert_finished().expect("first install");
    assert_eq!(runner.inner.calls().len(), 3);
    assert!(!runner.inner.calls().iter().any(is_stop_operation));
}

#[test]
fn launchd_replacement_stops_running_job_before_write() {
    let fixture = ServiceFixture::new("launchd-replacement-running", true);
    let (old_bytes, old_inode) = write_old_plist(&fixture);
    let desired_bytes = expected_plist_bytes(&fixture);
    let state_path = fixture.config_root().join(STATE_FILENAME);
    let expected_tmux = fs::canonicalize(&fixture.tmux).expect("canonical tmux");
    let artifact = artifact_path(&fixture.home);
    let inspected_artifact = artifact.clone();
    let inspected_old = old_bytes.clone();
    let inspected_desired = desired_bytes.clone();
    let call_index = Arc::new(AtomicUsize::new(0));
    let inspected_index = Arc::clone(&call_index);
    let runner = InspectingRunner {
        inner: FixtureRunner::new(vec![
            disable(output([])),
            print(running()),
            kill(output([])),
            print(running()),
            print(quiescent()),
            bootout(&fixture, output([])),
            enable(output([])),
            bootstrap(&fixture, output([])),
            print(running()),
        ]),
        inspect: Box::new(move |invocation| {
            let index = inspected_index.fetch_add(1, Ordering::Relaxed);
            if index < 6 {
                assert_artifact(
                    &inspected_artifact,
                    &inspected_old,
                    old_inode,
                    "during graceful stop",
                );
                assert!(!state_path.exists(), "state must not precede prepare");
            } else if invocation.operation
                == CommandOperation::Service(ServiceOperation::LaunchdEnable)
            {
                assert_eq!(
                    fs::read(&inspected_artifact).expect("new plist"),
                    inspected_desired
                );
                assert!(!state_path.exists(), "state is persisted after prepare");
            } else {
                assert_eq!(
                    fs::read(&inspected_artifact).expect("new plist"),
                    inspected_desired
                );
                assert_local_observer(&state_path, &expected_tmux);
            }
        }),
    };

    fixture.controller(&runner).install_blocking();

    runner.inner.assert_finished().expect("replacement");
    assert_eq!(call_index.load(Ordering::Relaxed), 9);
    assert_eq!(fs::read(artifact).expect("new plist"), desired_bytes);
}

#[test]
fn launchd_locale_free_owned_plist_is_replaced_through_graceful_stop() {
    let fixture = ServiceFixture::new("launchd-locale-replacement", true);
    let artifact = artifact_path(&fixture.home);
    let desired_bytes = expected_plist_bytes(&fixture);
    let (_, service_path) = expected_plist_inputs(&fixture);
    let service_path = service_path.to_str().expect("UTF-8 service PATH");
    let locale_entry =
        format!("<key>{LOCALE_KEY}</key>\n<string>{LOCALE_VALUE}</string>\n").into_bytes();

    // Derive the pre-fix artifact from render instead of hand-authoring a second plist,
    // so the old and desired artifacts provably differ only by the locale entry. This
    // deliberately depends on that entry remaining contiguous immediately after PATH.
    let occurrences = desired_bytes
        .windows(locale_entry.len())
        .filter(|window| *window == locale_entry)
        .count();
    assert_eq!(
        occurrences, 1,
        "rendered locale entry must occur exactly once"
    );
    let locale_offset = desired_bytes
        .windows(locale_entry.len())
        .position(|window| window == locale_entry)
        .expect("locale entry asserted present");
    let mut old_bytes = desired_bytes.clone();
    old_bytes.drain(locale_offset..locale_offset + locale_entry.len());
    assert_eq!(
        old_bytes.len() + locale_entry.len(),
        desired_bytes.len(),
        "locale-free fixture length"
    );
    let marker = format!("<string>{LABEL}</string>");
    assert!(
        old_bytes
            .windows(marker.len())
            .any(|window| window == marker.as_bytes()),
        "locale-free fixture must remain owned"
    );

    let desired_plist = read_rendered_launchd_plist(&desired_bytes);
    assert_eq!(
        desired_plist.environment_variables,
        expected_environment_variables(service_path)
    );
    let mut expected_old_plist = desired_plist.clone();
    expected_old_plist.environment_variables = vec![("PATH".to_owned(), service_path.to_owned())];
    assert_eq!(
        read_rendered_launchd_plist(&old_bytes),
        expected_old_plist,
        "locale entry must be the only structural difference"
    );

    fs::create_dir_all(artifact.parent().expect("plist parent")).expect("plist parent");
    fs::write(&artifact, &old_bytes).expect("locale-free plist");
    let old_inode = fs::metadata(&artifact).expect("locale-free plist").ino();
    let inspected_artifact = artifact.clone();
    let inspected_old = old_bytes.clone();
    let inspected_desired = desired_bytes.clone();
    let call_index = Arc::new(AtomicUsize::new(0));
    let inspected_index = Arc::clone(&call_index);
    let runner = InspectingRunner {
        inner: FixtureRunner::new(vec![
            disable(output([])),
            print(running()),
            kill(output([])),
            print(running()),
            print(quiescent()),
            bootout(&fixture, output([])),
            enable(output([])),
            bootstrap(&fixture, output([])),
            print(running()),
        ]),
        inspect: Box::new(move |_| {
            if inspected_index.fetch_add(1, Ordering::Relaxed) < 6 {
                assert_artifact(
                    &inspected_artifact,
                    &inspected_old,
                    old_inode,
                    "during locale replacement stop",
                );
            } else {
                assert_eq!(
                    fs::read(&inspected_artifact).expect("locale-bearing plist"),
                    inspected_desired
                );
            }
        }),
    };

    fixture.controller(&runner).install_blocking();

    runner.inner.assert_finished().expect("locale replacement");
    assert_eq!(call_index.load(Ordering::Relaxed), 9);
    assert_eq!(
        fs::read(artifact).expect("locale-bearing plist"),
        desired_bytes
    );
}

#[test]
fn launchd_replacement_stops_quiescent_and_absent_jobs_without_signal() {
    for (label, inspected, tail) in [
        (
            "replacement-quiescent",
            print(quiescent()),
            vec![bootout_placeholder(absent())],
        ),
        (
            "replacement-absent",
            print(absent()),
            Vec::<ExpectedInvocation>::new(),
        ),
    ] {
        let fixture = ServiceFixture::new(label, true);
        let (old_bytes, old_inode) = write_old_plist(&fixture);
        let mut expectations = vec![disable(output([])), inspected];
        expectations.extend(tail.into_iter().map(|mut item| {
            item.invocation.args[2] = artifact_path(&fixture.home).into_os_string();
            item
        }));
        let protected = expectations.len();
        expectations.extend(launchd_install_expectations(&fixture));
        let runner = inspecting_preserved_runner(
            &fixture,
            expectations,
            protected,
            &old_bytes,
            old_inode,
            None,
        );

        fixture.controller(&runner).install_blocking();

        runner.inner.assert_finished().expect(label);
        assert!(
            !runner
                .inner
                .calls()
                .iter()
                .any(|call| call.operation
                    == CommandOperation::Service(ServiceOperation::LaunchdKill))
        );
    }
}

#[test]
fn launchd_uninstall_handles_running_quiescent_absent_and_missing_plist() {
    for (label, expectations, protected) in [
        (
            "uninstall-running",
            vec![
                disable(output([])),
                print(running()),
                kill(output([])),
                print(quiescent()),
                bootout_placeholder(output([])),
            ],
            5,
        ),
        (
            "uninstall-quiescent",
            vec![
                disable(output([])),
                print(quiescent()),
                bootout_placeholder(output([])),
            ],
            3,
        ),
        (
            "uninstall-absent",
            vec![disable(output([])), print(absent())],
            2,
        ),
    ] {
        let fixture = ServiceFixture::new(label, true);
        let runner = FixtureRunner::new(launchd_install_expectations(&fixture));
        fixture.controller(&runner).install_blocking();
        runner.assert_finished().expect("initial install");
        let artifact = artifact_path(&fixture.home);
        let bytes = fs::read(&artifact).expect("owned plist");
        let inode = fs::metadata(&artifact).expect("owned plist").ino();
        let state_path = fixture.config_root().join(STATE_FILENAME);
        let state = fs::read(&state_path).expect("local state");
        let expectations = expectations.into_iter().map(|mut item| {
            if item.invocation.operation
                == CommandOperation::Service(ServiceOperation::LaunchdBootout)
            {
                item.invocation.args[2] = artifact.clone().into_os_string();
            }
            item
        });
        let runner = inspecting_preserved_runner(
            &fixture,
            expectations.collect(),
            protected,
            &bytes,
            inode,
            Some(&state),
        );

        fixture.controller(&runner).uninstall_blocking();

        runner.inner.assert_finished().expect(label);
        assert!(!artifact.exists(), "{label}: plist removed");
        assert!(!state_path.exists(), "{label}: state removed");
    }

    let fixture = ServiceFixture::new("uninstall-missing", true);
    fs::create_dir_all(fixture.config_root()).expect("config root");
    fs::write(
        fixture.config_root().join(STATE_FILENAME),
        b"{\"tmux_path\":\"/previous/tmux\"}\n",
    )
    .expect("state");
    let runner = FixtureRunner::default();
    fixture.controller(&runner).uninstall_blocking();
    assert!(runner.calls().is_empty());
    assert!(!fixture.config_root().join(STATE_FILENAME).exists());
}

#[test]
fn launchd_kill_absence_race_still_confirms_quiescence() {
    for (label, confirmation, expects_bootout) in [
        ("kill-race-quiescent", print(quiescent()), true),
        ("kill-race-absent", print(absent()), false),
    ] {
        let fixture = ServiceFixture::new(label, true);
        let (old_bytes, inode) = write_old_plist(&fixture);
        let mut expectations = vec![
            disable(output([])),
            print(running()),
            kill(absent()),
            confirmation,
        ];
        if expects_bootout {
            expectations.push(bootout(&fixture, output([])));
        }
        let protected = expectations.len();
        expectations.extend(launchd_install_expectations(&fixture));
        let runner =
            inspecting_preserved_runner(&fixture, expectations, protected, &old_bytes, inode, None);

        fixture.controller(&runner).install_blocking();

        runner.inner.assert_finished().expect(label);
        assert_eq!(
            runner
                .inner
                .calls()
                .iter()
                .filter(|call| call.operation
                    == CommandOperation::Service(ServiceOperation::LaunchdPrint))
                .count(),
            3,
            "{label}: decision, confirmation, and final print"
        );
    }
}

#[test]
fn launchd_quiescence_timeout_has_exactly_one_hundred_post_kill_prints() {
    let fixture = ServiceFixture::new("launchd-timeout", true);
    let (old_bytes, inode) = write_old_plist(&fixture);
    let previous_state = write_previous_state(&fixture);
    let mut expectations = vec![disable(output([])), print(running()), kill(output([]))];
    expectations.extend((0..100).map(|_| print(running())));
    expectations.push(enable(output([])));
    let protected = expectations.len();
    let runner = inspecting_preserved_runner(
        &fixture,
        expectations,
        protected,
        &old_bytes,
        inode,
        Some(&previous_state),
    );

    let runtime = runtime();
    let error = runtime
        .block_on(async {
            tokio::time::pause();
            fixture.controller(&runner).0.install().await
        })
        .expect_err("live job must time out");

    assert!(error.to_string().contains("still reports a live pid"));
    assert!(error.to_string().contains("after 5 seconds"));
    assert!(
        error
            .to_string()
            .contains("solstone-tmux-observer install-service")
    );
    runner.inner.assert_finished().expect("timeout queue");
    let calls = runner.inner.calls();
    assert_eq!(
        calls
            .iter()
            .filter(
                |call| call.operation == CommandOperation::Service(ServiceOperation::LaunchdPrint)
            )
            .count(),
        101
    );
    assert!(
        !calls
            .iter()
            .any(|call| call.operation
                == CommandOperation::Service(ServiceOperation::LaunchdBootout))
    );
    assert_preserved_after_failure(&fixture, &old_bytes, inode, &previous_state);
}

#[test]
fn launchd_graceful_stop_failures_reenable_and_preserve_artifacts() {
    let cases = vec![
        (
            "disable",
            vec![
                disable(command_output(&[], b"disable failed", 5)),
                enable(output([])),
            ],
            "launchd disable failed",
        ),
        (
            "inspect",
            vec![
                disable(output([])),
                print(command_output(&[], b"print failed", 5)),
                enable(output([])),
            ],
            "launchd stop inspection failed",
        ),
        (
            "kill-status",
            vec![
                disable(output([])),
                print(running()),
                kill(command_output(&[], b"kill failed", 5)),
                enable(output([])),
            ],
            "launchd kill failed",
        ),
        (
            "kill-command",
            vec![
                disable(output([])),
                print(running()),
                kill(FixtureOutcome::SpawnFailure),
                enable(output([])),
            ],
            "could not start command",
        ),
        (
            "poll",
            vec![
                disable(output([])),
                print(running()),
                kill(output([])),
                print(command_output(&[], b"poll failed", 5)),
                enable(output([])),
            ],
            "launchd quiescence check failed",
        ),
        (
            "bootout",
            vec![
                disable(output([])),
                print(quiescent()),
                bootout_placeholder(command_output(&[], STATUS_FIVE_ERROR, 5)),
                enable(output([])),
            ],
            "launchd bootout failed",
        ),
    ];

    for (label, expectations, primary_message) in cases {
        let fixture = ServiceFixture::new(label, true);
        let (old_bytes, inode) = write_old_plist(&fixture);
        let previous_state = write_previous_state(&fixture);
        let protected = expectations.len();
        let expectations = expectations.into_iter().map(|mut item| {
            if item.invocation.operation
                == CommandOperation::Service(ServiceOperation::LaunchdBootout)
            {
                item.invocation.args[2] = artifact_path(&fixture.home).into_os_string();
            }
            item
        });
        let runner = inspecting_preserved_runner(
            &fixture,
            expectations.collect(),
            protected,
            &old_bytes,
            inode,
            Some(&previous_state),
        );

        let error = runtime()
            .block_on(fixture.controller(&runner).0.install())
            .expect_err("graceful stop failure");

        assert!(
            error.to_string().contains(primary_message),
            "{label}: {error}"
        );
        runner.inner.assert_finished().expect(label);
        assert_preserved_after_failure(&fixture, &old_bytes, inode, &previous_state);
    }
}

#[test]
fn launchd_uninstall_failure_preserves_plist_and_state() {
    let fixture = ServiceFixture::new("uninstall-failure", true);
    let first = FixtureRunner::new(launchd_install_expectations(&fixture));
    fixture.controller(&first).install_blocking();
    first.assert_finished().expect("initial install");
    let artifact = artifact_path(&fixture.home);
    let bytes = fs::read(&artifact).expect("plist");
    let inode = fs::metadata(&artifact).expect("plist").ino();
    let state = fs::read(fixture.config_root().join(STATE_FILENAME)).expect("state");
    let expectations = vec![
        disable(output([])),
        print(running()),
        kill(command_output(&[], b"kill failed", 5)),
        enable(output([])),
    ];
    let protected = expectations.len();
    let runner = inspecting_preserved_runner(
        &fixture,
        expectations,
        protected,
        &bytes,
        inode,
        Some(&state),
    );

    let error = runtime()
        .block_on(fixture.controller(&runner).0.uninstall())
        .expect_err("uninstall stop failure");

    assert!(error.to_string().contains("launchd kill failed"));
    runner.inner.assert_finished().expect("uninstall failure");
    assert_preserved_after_failure(&fixture, &bytes, inode, &state);
}

#[test]
fn launchd_recovery_error_reports_both_failures_and_retry_command() {
    for (label, uninstall, expectations, retry_command) in [
        (
            "combined-install",
            false,
            vec![
                disable(command_output(&[], b"primary failure", 5)),
                enable(command_output(&[], b"re-enable failure", 5)),
            ],
            "install-service",
        ),
        (
            "combined-uninstall",
            true,
            vec![
                disable(output([])),
                print(running()),
                kill(command_output(&[], b"primary failure", 5)),
                enable(command_output(&[], b"re-enable failure", 5)),
            ],
            "uninstall-service",
        ),
    ] {
        let fixture = ServiceFixture::new(label, true);
        let (old_bytes, inode) = write_old_plist(&fixture);
        let previous_state = write_previous_state(&fixture);
        let protected = expectations.len();
        let runner = inspecting_preserved_runner(
            &fixture,
            expectations,
            protected,
            &old_bytes,
            inode,
            Some(&previous_state),
        );

        let result = if uninstall {
            runtime().block_on(fixture.controller(&runner).0.uninstall())
        } else {
            runtime().block_on(fixture.controller(&runner).0.install())
        };
        let error = result.expect_err("combined launchd recovery failure");

        let ServiceError::LaunchdRecovery {
            primary,
            reenable,
            retry_command: actual_retry,
        } = &error
        else {
            panic!("expected LaunchdRecovery, got {error:?}");
        };
        assert!(primary.to_string().contains("primary failure"));
        assert!(reenable.to_string().contains("re-enable failure"));
        assert_eq!(*actual_retry, retry_command);
        let message = error.to_string();
        assert!(message.contains("launchd re-enable recovery also failed"));
        assert!(message.contains("launchd crash restart may remain disabled"));
        assert!(message.contains(LABEL));
        assert!(message.contains(&format!("rerun solstone-tmux-observer {retry_command}")));
        assert!(!message.contains("rolled back"));
        assert_eq!(
            std::error::Error::source(&error)
                .expect("primary source")
                .to_string(),
            primary.to_string()
        );
        runner.inner.assert_finished().expect(label);
        assert_preserved_after_failure(&fixture, &old_bytes, inode, &previous_state);
    }
}

#[test]
fn launchd_replacement_enable_failure_occurs_after_successful_stop() {
    let fixture = ServiceFixture::new("replacement-enable-failure", true);
    let (old_bytes, old_inode) = write_old_plist(&fixture);
    let previous_state = write_previous_state(&fixture);
    let desired = expected_plist_bytes(&fixture);
    let artifact = artifact_path(&fixture.home);
    let state_path = fixture.config_root().join(STATE_FILENAME);
    let inspected_artifact = artifact.clone();
    let runner = InspectingRunner {
        inner: FixtureRunner::new(vec![
            disable(output([])),
            print(quiescent()),
            bootout(&fixture, output([])),
            enable(command_output(&[], b"enable failed", 5)),
        ]),
        inspect: Box::new(move |invocation| {
            if invocation.operation == CommandOperation::Service(ServiceOperation::LaunchdEnable) {
                assert_eq!(fs::read(&inspected_artifact).expect("new plist"), desired);
                assert_eq!(fs::read(&state_path).expect("prior state"), previous_state);
            } else {
                assert_artifact(&inspected_artifact, &old_bytes, old_inode, "during stop");
                assert_eq!(fs::read(&state_path).expect("prior state"), previous_state);
            }
        }),
    };

    let error = runtime()
        .block_on(fixture.controller(&runner).0.install())
        .expect_err("normal enable failure");

    assert!(error.to_string().contains("launchd enable failed"));
    assert!(!matches!(error, ServiceError::LaunchdRecovery { .. }));
    runner
        .inner
        .assert_finished()
        .expect("normal enable failure");
    assert_eq!(
        fs::read(artifact).expect("replacement plist"),
        expected_plist_bytes(&fixture)
    );
}

#[test]
fn unchanged_running_job_is_enabled_and_rechecked_without_restart_or_rewrite() {
    let fixture = ServiceFixture::new("unchanged-running", true);
    let first = FixtureRunner::new(launchd_install_expectations(&fixture));
    fixture.controller(&first).install_blocking();
    first.assert_finished().expect("first install");
    let artifact = artifact_path(&fixture.home);
    let (_, service_path) = expected_plist_inputs(&fixture);
    assert_eq!(
        read_rendered_launchd_plist(&fs::read(&artifact).expect("plist")).environment_variables,
        expected_environment_variables(service_path.to_str().expect("UTF-8 service PATH"))
    );
    let desired = expected_plist_bytes(&fixture);
    let inode = fs::metadata(&artifact).expect("plist").ino();
    let state_path = fixture.config_root().join(STATE_FILENAME);
    let expected_tmux = fs::canonicalize(&fixture.tmux).expect("tmux");
    // Both prints report the same top-level pid, which is the whole claim of this
    // path: the already-running observer was never restarted.
    let runner = InspectingRunner {
        inner: FixtureRunner::new(vec![
            print(output(stdout_with_pid("43210"))),
            enable(output([])),
            print(output(stdout_with_pid("43210"))),
        ]),
        inspect: Box::new(move |_| {
            assert_artifact(&artifact, &desired, inode, "unchanged running");
            assert_local_observer(&state_path, &expected_tmux);
        }),
    };

    fixture.controller(&runner).install_blocking();

    runner.inner.assert_finished().expect("idempotent install");
    let calls = runner.inner.calls();
    assert_eq!(calls.len(), 3);
    assert!(!calls.iter().any(|call| matches!(
        call.operation,
        CommandOperation::Service(
            ServiceOperation::LaunchdDisable
                | ServiceOperation::LaunchdKill
                | ServiceOperation::LaunchdBootout
                | ServiceOperation::LaunchdBootstrap
        )
    )));
}

#[test]
fn unchanged_running_job_rejects_a_changed_pid_without_bootout_or_rewrite() {
    let fixture = ServiceFixture::new("unchanged-running-pid-change", true);
    write_desired_plist(&fixture);
    let artifact = artifact_path(&fixture.home);
    let bytes = fs::read(&artifact).expect("plist");
    let inode = fs::metadata(&artifact).expect("plist").ino();
    let previous_state = write_previous_state(&fixture);
    // The job restarted underneath install-service between the two prints. Nothing in
    // this path asked it to, so the install must fail loudly rather than report success
    // for a process it never observed starting.
    let runner = FixtureRunner::new(vec![
        print(output(stdout_with_pid("43210"))),
        enable(output([])),
        print(output(stdout_with_pid("98765"))),
    ]);

    let error = runtime()
        .block_on(fixture.controller(&runner).0.install())
        .expect_err("changed pid");

    assert!(matches!(&error, ServiceError::InvalidState(_)));
    let message = error.to_string();
    assert!(message.contains(LABEL), "{message}");
    assert!(message.contains("43210"), "{message}");
    assert!(message.contains("98765"), "{message}");
    assert!(
        message.contains("solstone-tmux-observer install-service"),
        "{message}"
    );
    runner.assert_finished().expect("changed pid calls");
    let calls = runner.calls();
    assert_eq!(calls.len(), 3);
    assert!(
        !calls.iter().any(|call| matches!(
            call.operation,
            CommandOperation::Service(
                ServiceOperation::LaunchdDisable
                    | ServiceOperation::LaunchdKill
                    | ServiceOperation::LaunchdBootout
                    | ServiceOperation::LaunchdBootstrap
            )
        )),
        "a changed pid must not be escalated into a restart"
    );
    assert_preserved_after_failure(&fixture, &bytes, inode, &previous_state);
}

#[test]
fn launchd_nested_pid_is_not_the_job_pid_and_never_signals() {
    // The job block carries no top-level `pid`, but nested dictionaries carry three
    // decoys, including a verbatim `pid = 777` one level down under `spawn statistics`.
    // A depth-blind scan would call this running, kill a job that is not there, and on
    // uninstall destroy a live observer's segment on the strength of another
    // dictionary's field.
    let fixture = ServiceFixture::new("nested-pid-only", true);
    write_desired_plist(&fixture);
    let artifact = artifact_path(&fixture.home);
    let bytes = fs::read(&artifact).expect("plist");
    let inode = fs::metadata(&artifact).expect("plist").ino();

    let install = FixtureRunner::new(vec![
        print(nested_pid_only()),
        bootout(&fixture, output([])),
        enable(output([])),
        bootstrap(&fixture, output([])),
        print(running()),
    ]);
    fixture.controller(&install).install_blocking();
    install.assert_finished().expect("nested pid install");
    assert_artifact(&artifact, &bytes, inode, "nested pid no rewrite");
    assert!(
        !install
            .calls()
            .iter()
            .any(|call| call.operation == CommandOperation::Service(ServiceOperation::LaunchdKill)),
        "a nested pid must never be signalled"
    );

    let uninstall = FixtureRunner::new(vec![
        disable(output([])),
        print(nested_pid_only()),
        bootout(&fixture, output([])),
    ]);
    runtime()
        .block_on(fixture.controller(&uninstall).0.uninstall())
        .expect("nested pid uninstall");
    uninstall.assert_finished().expect("nested pid uninstall");
    assert!(
        !uninstall
            .calls()
            .iter()
            .any(|call| call.operation == CommandOperation::Service(ServiceOperation::LaunchdKill)),
        "uninstall must not signal on a nested pid"
    );
    assert!(!artifact.exists());
}

#[test]
fn unchanged_quiescent_job_boots_out_without_disable_then_restarts() {
    let fixture = ServiceFixture::new("unchanged-quiescent", true);
    write_desired_plist(&fixture);
    let artifact = artifact_path(&fixture.home);
    let desired = expected_plist_bytes(&fixture);
    let inode = fs::metadata(&artifact).expect("plist").ino();
    let state_path = fixture.config_root().join(STATE_FILENAME);
    let expected_tmux = fs::canonicalize(&fixture.tmux).expect("tmux");
    let runner = InspectingRunner {
        inner: FixtureRunner::new(vec![
            print(quiescent()),
            bootout(&fixture, output([])),
            enable(output([])),
            bootstrap(&fixture, output([])),
            print(running()),
        ]),
        inspect: Box::new(move |_| {
            assert_artifact(&artifact, &desired, inode, "unchanged quiescent");
            assert_local_observer(&state_path, &expected_tmux);
        }),
    };

    fixture.controller(&runner).install_blocking();

    runner.inner.assert_finished().expect("quiescent restart");
    assert!(!runner.inner.calls().iter().any(|call| matches!(
        call.operation,
        CommandOperation::Service(ServiceOperation::LaunchdDisable | ServiceOperation::LaunchdKill)
    )));
}

#[test]
fn unchanged_quiescent_bootout_failure_has_no_reenable_recovery() {
    let fixture = ServiceFixture::new("unchanged-quiescent-failure", true);
    write_desired_plist(&fixture);
    let artifact = artifact_path(&fixture.home);
    let bytes = fs::read(&artifact).expect("plist");
    let inode = fs::metadata(&artifact).expect("plist").ino();
    let previous_state = write_previous_state(&fixture);
    let state_path = fixture.config_root().join(STATE_FILENAME);
    let expected_tmux = fs::canonicalize(&fixture.tmux).expect("tmux");
    let inspected_artifact = artifact.clone();
    let inspected_bytes = bytes.clone();
    let runner = InspectingRunner {
        inner: FixtureRunner::new(vec![
            print(quiescent()),
            bootout(&fixture, command_output(&[], STATUS_FIVE_ERROR, 5)),
        ]),
        inspect: Box::new(move |_| {
            assert_artifact(
                &inspected_artifact,
                &inspected_bytes,
                inode,
                "unchanged bootout failure",
            );
            assert_local_observer(&state_path, &expected_tmux);
        }),
    };

    let error = runtime()
        .block_on(fixture.controller(&runner).0.install())
        .expect_err("bootout failure");

    assert!(error.to_string().contains("launchd bootout failed"));
    assert_eq!(runner.inner.calls().len(), 2);
    runner.inner.assert_finished().expect("no recovery enable");
    assert_preserved_after_failure(&fixture, &bytes, inode, &previous_state);
}

#[test]
fn unchanged_absent_job_enables_and_bootstraps_without_rewrite() {
    let fixture = ServiceFixture::new("unchanged-absent", true);
    write_desired_plist(&fixture);
    let artifact = artifact_path(&fixture.home);
    let desired = expected_plist_bytes(&fixture);
    let inode = fs::metadata(&artifact).expect("plist").ino();
    let runner = FixtureRunner::new(vec![
        print(absent()),
        enable(output([])),
        bootstrap(&fixture, output([])),
        print(running()),
    ]);

    fixture.controller(&runner).install_blocking();

    runner.assert_finished().expect("absent restart");
    assert_artifact(&artifact, &desired, inode, "unchanged absent");
    assert!(!runner.calls().iter().any(is_stop_operation));
}

#[test]
fn unchanged_running_and_absent_enable_failures_restore_previous_state() {
    for (label, first_print) in [
        ("running-enable-failure", print(running())),
        ("absent-enable-failure", print(absent())),
    ] {
        let fixture = ServiceFixture::new(label, true);
        write_desired_plist(&fixture);
        let artifact = artifact_path(&fixture.home);
        let bytes = fs::read(&artifact).expect("plist");
        let inode = fs::metadata(&artifact).expect("plist").ino();
        let previous_state = write_previous_state(&fixture);
        let state_path = fixture.config_root().join(STATE_FILENAME);
        let expected_tmux = fs::canonicalize(&fixture.tmux).expect("tmux");
        let inspected_artifact = artifact.clone();
        let inspected_bytes = bytes.clone();
        let runner = InspectingRunner {
            inner: FixtureRunner::new(vec![
                first_print,
                enable(command_output(&[], b"enable failed", 5)),
            ]),
            inspect: Box::new(move |_| {
                assert_artifact(
                    &inspected_artifact,
                    &inspected_bytes,
                    inode,
                    "unchanged enable failure",
                );
                assert_local_observer(&state_path, &expected_tmux);
            }),
        };

        let error = runtime()
            .block_on(fixture.controller(&runner).0.install())
            .expect_err("enable failure");

        assert!(error.to_string().contains("launchd enable failed"));
        runner.inner.assert_finished().expect(label);
        assert_preserved_after_failure(&fixture, &bytes, inode, &previous_state);
    }
}

#[test]
fn unchanged_initial_print_failure_restores_previous_state() {
    let fixture = ServiceFixture::new("unchanged-print-failure", true);
    write_desired_plist(&fixture);
    let artifact = artifact_path(&fixture.home);
    let bytes = fs::read(&artifact).expect("plist");
    let inode = fs::metadata(&artifact).expect("plist").ino();
    let previous_state = write_previous_state(&fixture);
    let state_path = fixture.config_root().join(STATE_FILENAME);
    let expected_tmux = fs::canonicalize(&fixture.tmux).expect("tmux");
    let inspected_artifact = artifact.clone();
    let inspected_bytes = bytes.clone();
    let runner = InspectingRunner {
        inner: FixtureRunner::new(vec![print(command_output(&[], STATUS_FIVE_ERROR, 5))]),
        inspect: Box::new(move |_| {
            assert_artifact(
                &inspected_artifact,
                &inspected_bytes,
                inode,
                "unchanged print failure",
            );
            assert_local_observer(&state_path, &expected_tmux);
        }),
    };

    let error = runtime()
        .block_on(fixture.controller(&runner).0.install())
        .expect_err("loaded check failure");

    assert!(error.to_string().contains("launchd loaded check failed"));
    runner.inner.assert_finished().expect("loaded check");
    assert_preserved_after_failure(&fixture, &bytes, inode, &previous_state);
}

#[test]
fn launchd_final_loaded_check_requires_pid_after_bootstrap() {
    let fixture = ServiceFixture::new("final-pid-required", true);
    let previous_state = write_previous_state(&fixture);
    let artifact = artifact_path(&fixture.home);
    let desired = expected_plist_bytes(&fixture);
    let state_path = fixture.config_root().join(STATE_FILENAME);
    let expected_tmux = fs::canonicalize(&fixture.tmux).expect("tmux");
    let inspected_previous = previous_state.clone();
    let runner = InspectingRunner {
        inner: FixtureRunner::new(vec![
            enable(output([])),
            bootstrap(&fixture, output([])),
            print(quiescent()),
        ]),
        inspect: Box::new(move |invocation| {
            assert_eq!(fs::read(&artifact).expect("installed plist"), desired);
            if invocation.operation == CommandOperation::Service(ServiceOperation::LaunchdEnable) {
                assert_eq!(
                    fs::read(&state_path).expect("prior state before prepare completes"),
                    inspected_previous
                );
            } else {
                assert_local_observer(&state_path, &expected_tmux);
            }
        }),
    };

    let error = runtime()
        .block_on(fixture.controller(&runner).0.install())
        .expect_err("loaded without pid");

    assert!(matches!(&error, ServiceError::InvalidState(_)));
    assert!(error.to_string().contains(LABEL));
    assert!(error.to_string().contains("does not report a live pid"));
    assert!(
        error
            .to_string()
            .contains("solstone-tmux-observer install-service")
    );
    runner.inner.assert_finished().expect("pid-required calls");
    assert_eq!(
        fs::read(fixture.config_root().join(STATE_FILENAME)).expect("restored state"),
        previous_state
    );
}

#[test]
fn launchd_final_loaded_manager_failure_restores_previous_state() {
    let fixture = ServiceFixture::new("final-loaded-manager-failure", true);
    let previous_state = write_previous_state(&fixture);
    let artifact = artifact_path(&fixture.home);
    let desired = expected_plist_bytes(&fixture);
    let state_path = fixture.config_root().join(STATE_FILENAME);
    let expected_tmux = fs::canonicalize(&fixture.tmux).expect("tmux");
    let inspected_previous = previous_state.clone();
    let runner = InspectingRunner {
        inner: FixtureRunner::new(vec![
            enable(output([])),
            bootstrap(&fixture, output([])),
            print(command_output(&[], STATUS_FIVE_ERROR, 5)),
        ]),
        inspect: Box::new(move |invocation| {
            assert_eq!(fs::read(&artifact).expect("installed plist"), desired);
            if invocation.operation == CommandOperation::Service(ServiceOperation::LaunchdEnable) {
                assert_eq!(
                    fs::read(&state_path).expect("prior state before prepare completes"),
                    inspected_previous
                );
            } else {
                assert_local_observer(&state_path, &expected_tmux);
            }
        }),
    };

    let error = runtime()
        .block_on(fixture.controller(&runner).0.install())
        .expect_err("loaded check manager failure");

    assert!(error.to_string().contains("launchd loaded check failed"));
    runner.inner.assert_finished().expect("final loaded check");
    assert_eq!(
        fs::read(fixture.config_root().join(STATE_FILENAME)).expect("restored state"),
        previous_state
    );
}

#[test]
fn launchd_bootstrap_failure_restores_previous_state() {
    let fixture = ServiceFixture::new("bootstrap-failure", true);
    let previous_state = write_previous_state(&fixture);
    let artifact = artifact_path(&fixture.home);
    let desired = expected_plist_bytes(&fixture);
    let state_path = fixture.config_root().join(STATE_FILENAME);
    let expected_tmux = fs::canonicalize(&fixture.tmux).expect("tmux");
    let inspected_previous = previous_state.clone();
    let runner = InspectingRunner {
        inner: FixtureRunner::new(vec![
            enable(output([])),
            bootstrap(&fixture, command_output(&[], b"bootstrap failed", 5)),
        ]),
        inspect: Box::new(move |invocation| {
            assert_eq!(fs::read(&artifact).expect("installed plist"), desired);
            if invocation.operation == CommandOperation::Service(ServiceOperation::LaunchdEnable) {
                assert_eq!(
                    fs::read(&state_path).expect("prior state before prepare completes"),
                    inspected_previous
                );
            } else {
                assert_local_observer(&state_path, &expected_tmux);
            }
        }),
    };

    let error = runtime()
        .block_on(fixture.controller(&runner).0.install())
        .expect_err("bootstrap manager failure");

    assert!(error.to_string().contains("launchd bootstrap failed"));
    runner.inner.assert_finished().expect("bootstrap failure");
    assert_eq!(
        fs::read(fixture.config_root().join(STATE_FILENAME)).expect("restored state"),
        previous_state
    );
}

#[test]
fn launchd_pid_classifier_rejects_zero_and_non_digit_pid_values() {
    for value in ["0", "0000", "", "abc", "12a", "-1"] {
        let fixture = ServiceFixture::new("invalid-pid", true);
        write_desired_plist(&fixture);
        let artifact = artifact_path(&fixture.home);
        let bytes = fs::read(&artifact).expect("plist");
        let inode = fs::metadata(&artifact).expect("plist").ino();
        let runner = FixtureRunner::new(vec![
            print(output(stdout_with_pid(value))),
            bootout(&fixture, output([])),
            enable(output([])),
            bootstrap(&fixture, output([])),
            print(running()),
        ]);

        fixture.controller(&runner).install_blocking();

        runner.assert_finished().expect("invalid pid path");
        assert_artifact(&artifact, &bytes, inode, "invalid pid no rewrite");
        assert_eq!(
            runner
                .calls()
                .iter()
                .filter(|call| call.operation
                    == CommandOperation::Service(ServiceOperation::LaunchdBootout))
                .count(),
            1,
            "{value:?} must be quiescent"
        );
    }
}

#[test]
fn launchd_pid_classifier_accepts_thirty_digit_nonzero_value() {
    let fixture = ServiceFixture::new("large-pid", true);
    write_desired_plist(&fixture);
    // Held as a digit string end to end, so a value far past u64 classifies as running
    // and still compares equal across the two prints without any parse.
    const HUGE: &str = "123456789012345678901234567890";
    let runner = FixtureRunner::new(vec![
        print(output(stdout_with_pid(HUGE))),
        enable(output([])),
        print(output(stdout_with_pid(HUGE))),
    ]);

    fixture.controller(&runner).install_blocking();

    runner.assert_finished().expect("large pid");
    assert!(!runner.calls().iter().any(|call| matches!(
        call.operation,
        CommandOperation::Service(
            ServiceOperation::LaunchdBootout
                | ServiceOperation::LaunchdBootstrap
                | ServiceOperation::LaunchdKill
        )
    )));
}

#[test]
fn launchd_absence_requires_status_113_and_an_exact_complete_line() {
    let fixture = ServiceFixture::new("absence-classifier", true);
    write_desired_plist(&fixture);
    let exact = launchd_missing_service_line(USER_ID);
    let positive_stderr = format!("diagnostic before\n{exact}\ndiagnostic after\n");
    let positive = FixtureRunner::new(vec![print(command_output(
        &[],
        positive_stderr.as_bytes(),
        113,
    ))]);
    assert_eq!(
        runtime()
            .block_on(fixture.controller(&positive).0.status())
            .expect("exact complete line"),
        ServiceStatus::Inactive
    );
    positive.assert_finished().expect("positive absence");

    let cases = vec![
        ("wrong-label", exact.replace(LABEL, "wrong.label"), 113),
        ("wrong-uid", exact.replace(&USER_ID.to_string(), "999"), 113),
        ("prefix", format!("prefix {exact}"), 113),
        ("suffix", format!("{exact} suffix"), 113),
        ("empty", String::new(), 113),
        ("generic", "service not loaded".to_owned(), 113),
        ("unrelated", "unrelated launchctl failure".to_owned(), 113),
        ("status-3", exact.clone(), 3),
        ("status-4", exact.clone(), 4),
        ("status-5", exact, 5),
    ];
    for (label, stderr, status) in cases {
        let runner =
            FixtureRunner::new(vec![print(command_output(&[], stderr.as_bytes(), status))]);
        let error = runtime()
            .block_on(fixture.controller(&runner).0.status())
            .expect_err("negative absence");
        assert!(
            error.to_string().contains("launchd status failed"),
            "{label}: {error}"
        );
        runner.assert_finished().expect(label);
    }
}

#[test]
fn launchd_status_zero_remains_active_without_pid() {
    let fixture = ServiceFixture::new("status-no-pid", true);
    write_desired_plist(&fixture);
    let runner = FixtureRunner::new(vec![print(quiescent())]);

    assert_eq!(
        runtime()
            .block_on(fixture.controller(&runner).0.status())
            .expect("status zero"),
        ServiceStatus::Active
    );
    runner.assert_finished().expect("status query");
}

#[test]
fn launchd_install_and_uninstall_reject_invalid_or_unowned_artifacts_before_mutation() {
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
        for install in [true, false] {
            let fixture = ServiceFixture::new(label, true);
            let artifact = artifact_path(&fixture.home);
            fs::create_dir_all(artifact.parent().expect("plist parent")).expect("plist parent");
            let preserved = b"preserved owner bytes\n";
            let owned =
                render(Path::new("/owned/observer"), OsStr::new("/owned")).expect("owned plist");
            let referent = fixture.root.join("owned referent.plist");
            let dangling = fixture.root.join("missing referent.plist");
            let sentinel = artifact.join("sentinel");
            match case {
                InvalidCase::UnownedRegular => fs::write(&artifact, preserved).expect("unowned"),
                InvalidCase::OwnedSymlink => {
                    fs::write(&referent, &owned).expect("referent");
                    symlink(&referent, &artifact).expect("symlink");
                }
                InvalidCase::DanglingSymlink => symlink(&dangling, &artifact).expect("dangling"),
                InvalidCase::Directory => {
                    fs::create_dir(&artifact).expect("directory");
                    fs::write(&sentinel, preserved).expect("sentinel");
                }
            }
            let runner = FixtureRunner::default();
            let result = if install {
                runtime().block_on(fixture.controller(&runner).0.install())
            } else {
                runtime().block_on(fixture.controller(&runner).0.uninstall())
            };
            let error = result.expect_err("invalid artifact");
            assert!(error.to_string().contains("invalid or unowned"));
            assert!(runner.calls().is_empty());
            match case {
                InvalidCase::UnownedRegular => {
                    assert_eq!(fs::read(&artifact).expect("unowned"), preserved);
                }
                InvalidCase::OwnedSymlink => {
                    assert!(
                        fs::symlink_metadata(&artifact)
                            .expect("symlink")
                            .file_type()
                            .is_symlink()
                    );
                    assert_eq!(fs::read(referent).expect("referent"), owned);
                }
                InvalidCase::DanglingSymlink => {
                    assert!(
                        fs::symlink_metadata(&artifact)
                            .expect("dangling")
                            .file_type()
                            .is_symlink()
                    );
                    assert!(!dangling.exists());
                }
                InvalidCase::Directory => {
                    assert_eq!(fs::read(sentinel).expect("sentinel"), preserved);
                }
            }
        }
    }
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

fn launchd_install_expectations(fixture: &ServiceFixture) -> Vec<ExpectedInvocation> {
    vec![
        enable(output([])),
        bootstrap(fixture, output([])),
        print(running()),
    ]
}

fn expected(
    operation: ServiceOperation,
    args: Vec<OsString>,
    outcome: FixtureOutcome,
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

fn disable(outcome: FixtureOutcome) -> ExpectedInvocation {
    expected(
        ServiceOperation::LaunchdDisable,
        vec!["disable".into(), target().into()],
        outcome,
    )
}

fn enable(outcome: FixtureOutcome) -> ExpectedInvocation {
    expected(
        ServiceOperation::LaunchdEnable,
        vec!["enable".into(), target().into()],
        outcome,
    )
}

fn kill(outcome: FixtureOutcome) -> ExpectedInvocation {
    expected(
        ServiceOperation::LaunchdKill,
        vec!["kill".into(), "SIGTERM".into(), target().into()],
        outcome,
    )
}

fn print(outcome: FixtureOutcome) -> ExpectedInvocation {
    expected(
        ServiceOperation::LaunchdPrint,
        vec!["print".into(), target().into()],
        outcome,
    )
}

fn bootout(fixture: &ServiceFixture, outcome: FixtureOutcome) -> ExpectedInvocation {
    expected(
        ServiceOperation::LaunchdBootout,
        vec![
            "bootout".into(),
            domain().into(),
            artifact_path(&fixture.home).into_os_string(),
        ],
        outcome,
    )
}

fn bootout_placeholder(outcome: FixtureOutcome) -> ExpectedInvocation {
    expected(
        ServiceOperation::LaunchdBootout,
        vec!["bootout".into(), domain().into(), OsString::new()],
        outcome,
    )
}

fn bootstrap(fixture: &ServiceFixture, outcome: FixtureOutcome) -> ExpectedInvocation {
    expected(
        ServiceOperation::LaunchdBootstrap,
        vec![
            "bootstrap".into(),
            domain().into(),
            artifact_path(&fixture.home).into_os_string(),
        ],
        outcome,
    )
}

fn running() -> FixtureOutcome {
    output(RUNNING)
}

fn quiescent() -> FixtureOutcome {
    output(QUIESCENT)
}

fn absent() -> FixtureOutcome {
    launchd_absent(USER_ID)
}

fn nested_pid_only() -> FixtureOutcome {
    output(NESTED_PID_ONLY)
}

fn stdout_with_pid(value: &str) -> Vec<u8> {
    String::from_utf8(RUNNING.to_vec())
        .expect("fixture UTF-8")
        .replace("\tpid = 43210", &format!("\tpid = {value}"))
        .into_bytes()
}

fn is_stop_operation(call: &CommandInvocation) -> bool {
    matches!(
        call.operation,
        CommandOperation::Service(
            ServiceOperation::LaunchdDisable
                | ServiceOperation::LaunchdKill
                | ServiceOperation::LaunchdBootout
        )
    )
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RenderedLaunchdPlist {
    label: String,
    program_arguments: Vec<String>,
    environment_variables: Vec<(String, String)>,
    run_at_load: bool,
    keep_alive: Vec<(String, bool)>,
    throttle_interval: i64,
    process_type: String,
}
fn read_rendered_launchd_plist(bytes: &[u8]) -> RenderedLaunchdPlist {
    let text = std::str::from_utf8(bytes).expect("rendered plist must be UTF-8");
    let mut parser = PlistLines(text.lines());
    parser.expect(r#"<?xml version="1.0" encoding="UTF-8"?>"#);
    parser.expect(
        r#"<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">"#,
    );
    parser.expect(r#"<plist version="1.0">"#);
    parser.expect("<dict>");
    let plist = RenderedLaunchdPlist {
        label: parser.keyed("Label", |parser| parser.tagged("string")),
        program_arguments: parser.keyed("ProgramArguments", PlistLines::string_array),
        environment_variables: parser.keyed("EnvironmentVariables", |parser| {
            parser.dictionary(|parser| parser.tagged("string"))
        }),
        run_at_load: parser.keyed("RunAtLoad", PlistLines::boolean),
        keep_alive: parser.keyed("KeepAlive", |parser| parser.dictionary(PlistLines::boolean)),
        throttle_interval: parser.keyed("ThrottleInterval", PlistLines::integer),
        process_type: parser.keyed("ProcessType", |parser| parser.tagged("string")),
    };
    parser.expect("</dict>");
    parser.expect("</plist>");
    assert!(
        parser.0.next().is_none(),
        "unexpected trailing plist content"
    );
    plist
}
struct PlistLines<'a>(std::str::Lines<'a>);
impl<'a> PlistLines<'a> {
    fn next(&mut self, context: &str) -> &'a str {
        self.0
            .next()
            .unwrap_or_else(|| panic!("unexpected end of plist while reading {context}"))
            .trim()
    }
    fn expect(&mut self, expected: &str) {
        assert_eq!(self.next(expected), expected, "unexpected plist line");
    }
    fn keyed<T>(&mut self, key: &str, read: impl FnOnce(&mut Self) -> T) -> T {
        assert_eq!(self.tagged("key"), key, "unexpected plist key");
        read(self)
    }
    fn tagged(&mut self, tag: &str) -> String {
        let line = self.next(tag);
        let opening = format!("<{tag}>");
        let closing = format!("</{tag}>");
        let value = line
            .strip_prefix(&opening)
            .and_then(|value| value.strip_suffix(&closing))
            .unwrap_or_else(|| panic!("expected {opening}value{closing}, got {line:?}"));
        decode_xml_text(value)
    }
    fn boolean(&mut self) -> bool {
        match self.next("boolean") {
            "<true/>" => true,
            "<false/>" => false,
            line => panic!("expected plist boolean, got {line:?}"),
        }
    }
    fn integer(&mut self) -> i64 {
        self.tagged("integer")
            .parse()
            .expect("plist integer must be valid")
    }
    fn string_array(&mut self) -> Vec<String> {
        self.expect("<array>");
        let mut values = Vec::new();
        while self.0.clone().next().map(str::trim) != Some("</array>") {
            values.push(self.tagged("string"));
        }
        self.expect("</array>");
        values
    }
    fn dictionary<T>(&mut self, mut read_value: impl FnMut(&mut Self) -> T) -> Vec<(String, T)> {
        self.expect("<dict>");
        let mut values = Vec::new();
        while self.0.clone().next().map(str::trim) != Some("</dict>") {
            values.push((self.tagged("key"), read_value(self)));
        }
        self.expect("</dict>");
        values
    }
}
fn decode_xml_text(value: &str) -> String {
    let entities = ["amp;", "lt;", "gt;", "quot;", "apos;"];
    for suffix in value.split('&').skip(1) {
        assert!(
            entities.iter().any(|entity| suffix.starts_with(entity)),
            "unexpected XML entity in {value:?}"
        );
    }
    value
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

fn expected_environment_variables(service_path: &str) -> Vec<(String, String)> {
    vec![
        ("PATH".to_owned(), service_path.to_owned()),
        (LOCALE_KEY.to_owned(), LOCALE_VALUE.to_owned()),
    ]
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

    fn controller<'a>(&'a self, runner: &'a dyn CommandRunner) -> Controller<'a> {
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
    let (binary, service_path) = expected_plist_inputs(fixture);
    render(&binary, &service_path).expect("desired plist")
}

fn expected_plist_inputs(fixture: &ServiceFixture) -> (PathBuf, OsString) {
    let binary = fs::canonicalize(&fixture.binary).expect("canonical binary");
    let tmux = fs::canonicalize(&fixture.tmux).expect("canonical tmux");
    let tmux_parent = tmux.parent().expect("tmux parent").to_owned();
    let mut service_directories = vec![tmux_parent];
    if !service_directories.contains(&fixture.path_entry) {
        service_directories.push(fixture.path_entry.clone());
    }
    let service_path = std::env::join_paths(service_directories).expect("service PATH");
    (binary, service_path)
}

fn write_old_plist(fixture: &ServiceFixture) -> (Vec<u8>, u64) {
    let artifact = artifact_path(&fixture.home);
    fs::create_dir_all(artifact.parent().expect("plist parent")).expect("plist parent");
    let bytes =
        render(Path::new("/old/solstone-tmux-observer"), OsStr::new("/old")).expect("old plist");
    fs::write(&artifact, &bytes).expect("old plist");
    let inode = fs::metadata(&artifact).expect("old plist").ino();
    (bytes, inode)
}

fn write_desired_plist(fixture: &ServiceFixture) {
    let artifact = artifact_path(&fixture.home);
    fs::create_dir_all(artifact.parent().expect("plist parent")).expect("plist parent");
    fs::write(artifact, expected_plist_bytes(fixture)).expect("desired plist");
}

fn write_previous_state(fixture: &ServiceFixture) -> Vec<u8> {
    let state = b"{\"tmux_path\":\"/previous/tmux\"}\n".to_vec();
    fs::create_dir_all(fixture.config_root()).expect("config root");
    fs::write(fixture.config_root().join(STATE_FILENAME), &state).expect("previous state");
    state
}

fn assert_artifact(path: &Path, bytes: &[u8], inode: u64, context: &str) {
    assert_eq!(fs::read(path).expect("plist bytes"), bytes, "{context}");
    assert_eq!(
        fs::metadata(path).expect("plist metadata").ino(),
        inode,
        "{context}: inode"
    );
}

fn assert_local_observer(path: &Path, expected_tmux: &Path) {
    let mut expected = serde_json::to_vec(&LocalObserver {
        tmux_path: expected_tmux.to_owned(),
    })
    .expect("expected local observer JSON");
    expected.push(b'\n');
    assert_eq!(fs::read(path).expect("local observer"), expected);
}

fn inspecting_preserved_runner(
    fixture: &ServiceFixture,
    expectations: Vec<ExpectedInvocation>,
    protected_calls: usize,
    plist_bytes: &[u8],
    plist_inode: u64,
    state_bytes: Option<&[u8]>,
) -> InspectingRunner {
    let artifact = artifact_path(&fixture.home);
    let state_path = fixture.config_root().join(STATE_FILENAME);
    let plist_bytes = plist_bytes.to_vec();
    let state_bytes = state_bytes.map(<[u8]>::to_vec);
    let index = AtomicUsize::new(0);
    InspectingRunner {
        inner: FixtureRunner::new(expectations),
        inspect: Box::new(move |_| {
            if index.fetch_add(1, Ordering::Relaxed) < protected_calls {
                assert_artifact(&artifact, &plist_bytes, plist_inode, "protected call");
                match &state_bytes {
                    Some(expected) => {
                        assert_eq!(fs::read(&state_path).expect("state bytes"), *expected)
                    }
                    None => assert!(!state_path.exists(), "state must remain absent"),
                }
            }
        }),
    }
}

fn assert_preserved_after_failure(
    fixture: &ServiceFixture,
    plist_bytes: &[u8],
    plist_inode: u64,
    state_bytes: &[u8],
) {
    assert_artifact(
        &artifact_path(&fixture.home),
        plist_bytes,
        plist_inode,
        "after failure",
    );
    assert_eq!(
        fs::read(fixture.config_root().join(STATE_FILENAME)).expect("state after failure"),
        state_bytes
    );
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
