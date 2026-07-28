// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

mod support;

use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::fs::symlink;
use std::path::PathBuf;

use solstone_tmux_observer::cli::{CliCommand, command_requires_instance_lock, parse_args};
use solstone_tmux_observer::command::{CommandInvocation, ServiceOperation, TmuxOperation};
use solstone_tmux_observer::paths::PlatformKind;
use solstone_tmux_observer::service::launchd;
use solstone_tmux_observer::service::systemd;
use solstone_tmux_observer::service::{
    COMMAND_TIMEOUT, ServiceController, ServiceStatus, status_exit_code,
};
use support::{
    ExpectedInvocation, FakeEnvironment, FixtureRunner, TestDirectory, command_output, output,
};

#[test]
fn status_codes_are_0_3_4() {
    let temporary = TestDirectory::new("service-status-codes");
    let home = temporary.path().join("owner");
    let environment = environment(&home);

    let absent_runner = FixtureRunner::default();
    let absent = ServiceController::new(
        PlatformKind::Linux,
        &environment,
        &absent_runner,
        PathBuf::from("/unused"),
    );
    assert_eq!(
        runtime().block_on(absent.status()).expect("absent status"),
        ServiceStatus::Absent
    );
    assert_eq!(ServiceStatus::Absent.exit_code(), 4);

    let unit = systemd::artifact_path(&home);
    fs::create_dir_all(unit.parent().expect("unit parent")).expect("unit parent");
    fs::write(
        &unit,
        systemd::render(
            std::path::Path::new("/opt/solstone-tmux-observer"),
            OsStr::new("/usr/bin:/bin"),
        )
        .expect("unit bytes"),
    )
    .expect("write unit");

    let active_runner = FixtureRunner::new([systemd_status(output(b"active\n"))]);
    let active = ServiceController::new(
        PlatformKind::Linux,
        &environment,
        &active_runner,
        PathBuf::from("/unused"),
    );
    assert_eq!(
        runtime().block_on(active.status()).expect("active status"),
        ServiceStatus::Active
    );
    assert_eq!(ServiceStatus::Active.exit_code(), 0);
    active_runner.assert_finished().expect("active query");

    let inactive_runner =
        FixtureRunner::new([systemd_status(command_output(b"inactive\n", &[], 3))]);
    let inactive = ServiceController::new(
        PlatformKind::Linux,
        &environment,
        &inactive_runner,
        PathBuf::from("/unused"),
    );
    assert_eq!(
        runtime()
            .block_on(inactive.status())
            .expect("inactive status"),
        ServiceStatus::Inactive
    );
    assert_eq!(ServiceStatus::Inactive.exit_code(), 3);
    inactive_runner.assert_finished().expect("inactive query");
}

#[test]
fn launchd_loaded_and_not_loaded_statuses_use_same_taxonomy() {
    let temporary = TestDirectory::new("launchd-status");
    let home = temporary.path().join("owner");
    let environment = environment(&home);
    let plist = launchd::artifact_path(&home);
    fs::create_dir_all(plist.parent().expect("plist parent")).expect("plist parent");
    fs::write(
        &plist,
        launchd::render(
            std::path::Path::new("/opt/solstone-tmux-observer"),
            OsStr::new("/usr/bin:/bin"),
        )
        .expect("plist bytes"),
    )
    .expect("write plist");

    let loaded_runner = FixtureRunner::new([launchd_status(output(b"loaded\n"))]);
    let loaded = ServiceController::new(
        PlatformKind::Macos,
        &environment,
        &loaded_runner,
        PathBuf::from("/unused"),
    );
    assert_eq!(
        runtime().block_on(loaded.status()).expect("loaded status"),
        ServiceStatus::Active
    );

    let unloaded_runner =
        FixtureRunner::new([launchd_status(command_output(&[], b"not loaded", 3))]);
    let unloaded = ServiceController::new(
        PlatformKind::Macos,
        &environment,
        &unloaded_runner,
        PathBuf::from("/unused"),
    );
    assert_eq!(
        runtime()
            .block_on(unloaded.status())
            .expect("unloaded status"),
        ServiceStatus::Inactive
    );
}

#[test]
fn manager_query_error_is_1() {
    let temporary = TestDirectory::new("service-status-manager-error");
    let home = temporary.path().join("owner");
    let environment = environment(&home);
    let unit = systemd::artifact_path(&home);
    fs::create_dir_all(unit.parent().expect("unit parent")).expect("unit parent");
    fs::write(
        &unit,
        systemd::render(
            std::path::Path::new("/opt/solstone-tmux-observer"),
            OsStr::new("/usr/bin:/bin"),
        )
        .expect("unit bytes"),
    )
    .expect("write unit");
    let runner = FixtureRunner::new([systemd_status(command_output(
        &[],
        b"manager unavailable",
        1,
    ))]);
    let controller = ServiceController::new(
        PlatformKind::Linux,
        &environment,
        &runner,
        PathBuf::from("/unused"),
    );
    let result = runtime().block_on(controller.status());
    let error = result.as_ref().expect_err("manager failure");
    assert!(error.to_string().contains("exit status 1"));
    assert_eq!(status_exit_code(result), 1);
}

#[test]
fn invalid_status_artifact_is_exit_1_without_following_referent() {
    let temporary = TestDirectory::new("service-status-invalid");
    let home = temporary.path().join("owner");
    let environment = environment(&home);
    let unit = systemd::artifact_path(&home);
    fs::create_dir_all(unit.parent().expect("unit parent")).expect("unit parent");
    let referent = temporary.path().join("owner value");
    fs::write(&referent, b"do not alter").expect("referent");
    symlink(&referent, &unit).expect("unit symlink");
    let runner = FixtureRunner::default();
    let controller = ServiceController::new(
        PlatformKind::Linux,
        &environment,
        &runner,
        PathBuf::from("/unused"),
    );
    assert!(runtime().block_on(controller.status()).is_err());
    assert_eq!(fs::read(referent).expect("referent bytes"), b"do not alter");
}

#[test]
fn cli_usage_error_is_2_and_service_commands_do_not_lock() {
    assert!(parse_args(["observer".into(), "unknown".into()]).is_err());
    assert!(!command_requires_instance_lock(CliCommand::Status));
    assert!(!command_requires_instance_lock(CliCommand::InstallService));
    assert!(!command_requires_instance_lock(
        CliCommand::UninstallService
    ));
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_solstone-tmux-observer"))
        .arg("unknown")
        .output()
        .expect("run observer CLI");
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("unknown command"));
}

#[test]
fn python_status_contract_is_not_modified() {
    let python_cli = fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../src/solstone_tmux/cli.py"),
    )
    .expect("Python CLI source");
    assert!(python_cli.contains("def cmd_status"));
    assert!(python_cli.contains("return 0"));
}

fn environment(home: &std::path::Path) -> FakeEnvironment {
    FakeEnvironment::from_paths([
        ("HOME", home.as_os_str().to_owned()),
        ("UID", OsString::from("501")),
        ("PATH", OsString::new()),
    ])
}

fn systemd_status(outcome: support::FixtureOutcome) -> ExpectedInvocation {
    ExpectedInvocation {
        invocation: CommandInvocation {
            operation: TmuxOperation::Service(ServiceOperation::SystemdIsActive),
            executable: PathBuf::from("systemctl"),
            args: ["--user", "is-active", systemd::UNIT_NAME]
                .into_iter()
                .map(OsString::from)
                .collect(),
            timeout: COMMAND_TIMEOUT,
        },
        outcome,
    }
}

fn launchd_status(outcome: support::FixtureOutcome) -> ExpectedInvocation {
    ExpectedInvocation {
        invocation: CommandInvocation {
            operation: TmuxOperation::Service(ServiceOperation::LaunchdPrint),
            executable: PathBuf::from("launchctl"),
            args: [
                OsString::from("print"),
                OsString::from(format!("gui/501/{}", launchd::LABEL)),
            ]
            .into_iter()
            .collect(),
            timeout: COMMAND_TIMEOUT,
        },
        outcome,
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("test runtime")
}
