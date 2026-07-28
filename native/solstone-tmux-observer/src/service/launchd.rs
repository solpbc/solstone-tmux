// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::command::{
    CommandInvocation, CommandOperation, CommandOutput, CommandRunner, ServiceOperation,
};

use super::{
    COMMAND_TIMEOUT, ServiceError, ServiceStatus, install_artifact, manager_error,
    remove_owned_regular_file, require_success, utf8_os, utf8_path, validate_regular_file,
};

pub const LABEL: &str = "com.solstone.tmux-observer";
const QUIESCENCE_TIMEOUT: Duration = Duration::from_secs(5);
const QUIESCENCE_POLL_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum JobState {
    Running,
    Quiescent,
    Absent,
}

pub fn artifact_path(home: &Path) -> PathBuf {
    home.join("Library/LaunchAgents")
        .join(format!("{LABEL}.plist"))
}

pub fn render(binary: &Path, service_path: &OsStr) -> Result<Vec<u8>, ServiceError> {
    let binary = xml_escape(utf8_path(binary)?);
    let service_path = xml_escape(utf8_os(service_path)?);
    Ok(format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
\"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
<plist version=\"1.0\">\n\
<dict>\n\
  <key>Label</key>\n\
  <string>{LABEL}</string>\n\
  <key>ProgramArguments</key>\n\
  <array>\n\
    <string>{binary}</string>\n\
    <string>run</string>\n\
  </array>\n\
  <key>EnvironmentVariables</key>\n\
  <dict>\n\
    <key>PATH</key>\n\
    <string>{service_path}</string>\n\
  </dict>\n\
  <key>RunAtLoad</key>\n\
  <true/>\n\
  <key>KeepAlive</key>\n\
  <dict>\n\
    <key>SuccessfulExit</key>\n\
    <false/>\n\
  </dict>\n\
  <key>ThrottleInterval</key>\n\
  <integer>5</integer>\n\
  <key>ProcessType</key>\n\
  <string>Background</string>\n\
</dict>\n\
</plist>\n"
    )
    .into_bytes())
}

pub struct PreparedInstall {
    path: PathBuf,
    bootstrap: bool,
}

pub async fn prepare_install(
    runner: &dyn CommandRunner,
    home: &Path,
    user_id: u32,
    binary: &Path,
    service_path: &OsStr,
) -> Result<PreparedInstall, ServiceError> {
    let path = artifact_path(home);
    let bytes = render(binary, service_path)?;
    let marker = ownership_marker();
    let present = validate_regular_file(&path, Some(&marker))?;
    let unchanged = present
        && std::fs::read(&path).map_err(|source| ServiceError::Io {
            path: path.clone(),
            source,
        })? == bytes;

    if !unchanged {
        if present {
            graceful_stop(runner, user_id, &path, "install-service").await?;
        }
        install_artifact(&path, &bytes)?;
        enable(runner, user_id).await?;
    }
    Ok(PreparedInstall {
        path,
        bootstrap: !unchanged,
    })
}

pub async fn activate(
    runner: &dyn CommandRunner,
    user_id: u32,
    prepared: PreparedInstall,
) -> Result<(), ServiceError> {
    if prepared.bootstrap {
        bootstrap(runner, user_id, &prepared.path).await?;
    } else {
        let output = print(runner, user_id).await?;
        match classify_print(&output, user_id, "launchd loaded check")? {
            JobState::Running => {
                enable(runner, user_id).await?;
            }
            state @ JobState::Quiescent => {
                unload(runner, user_id, &prepared.path, state, "install-service").await?;
                enable(runner, user_id).await?;
                bootstrap(runner, user_id, &prepared.path).await?;
            }
            JobState::Absent => {
                enable(runner, user_id).await?;
                bootstrap(runner, user_id, &prepared.path).await?;
            }
        }
    }

    verify_loaded(runner, user_id).await
}

pub async fn uninstall(
    runner: &dyn CommandRunner,
    home: &Path,
    user_id: u32,
) -> Result<(), ServiceError> {
    let path = artifact_path(home);
    let marker = ownership_marker();
    if !validate_regular_file(&path, Some(&marker))? {
        return Ok(());
    }
    graceful_stop(runner, user_id, &path, "uninstall-service").await?;
    remove_owned_regular_file(&path, Some(&marker))?;
    Ok(())
}

pub async fn status(
    runner: &dyn CommandRunner,
    home: &Path,
    user_id: u32,
) -> Result<ServiceStatus, ServiceError> {
    let marker = ownership_marker();
    if !validate_regular_file(&artifact_path(home), Some(&marker))? {
        return Ok(ServiceStatus::Absent);
    }
    let output = print(runner, user_id).await?;
    if output.status == 0 {
        Ok(ServiceStatus::Active)
    } else if reports_missing_service(&output, user_id) {
        Ok(ServiceStatus::Inactive)
    } else {
        Err(manager_error("launchd status", &output))
    }
}

async fn graceful_stop(
    runner: &dyn CommandRunner,
    user_id: u32,
    path: &Path,
    retry_command: &'static str,
) -> Result<(), ServiceError> {
    let result = async {
        disable(runner, user_id).await?;
        let output = print(runner, user_id).await?;
        let state = classify_print(&output, user_id, "launchd stop inspection")?;
        unload(runner, user_id, path, state, retry_command).await
    }
    .await;

    let Err(primary) = result else {
        return Ok(());
    };
    match enable(runner, user_id).await {
        Ok(()) => Err(primary),
        Err(reenable) => Err(ServiceError::LaunchdRecovery {
            primary: Box::new(primary),
            reenable: Box::new(reenable),
            retry_command,
        }),
    }
}

async fn unload(
    runner: &dyn CommandRunner,
    user_id: u32,
    path: &Path,
    mut state: JobState,
    retry_command: &'static str,
) -> Result<(), ServiceError> {
    if state == JobState::Running {
        let output = kill(runner, user_id).await?;
        if output.status != 0 && !reports_missing_service(&output, user_id) {
            return Err(manager_error("launchd kill", &output));
        }

        let started = tokio::time::Instant::now();
        let deadline = started + QUIESCENCE_TIMEOUT;
        let mut next_poll = started + QUIESCENCE_POLL_INTERVAL;
        loop {
            tokio::time::sleep_until(next_poll).await;
            next_poll += QUIESCENCE_POLL_INTERVAL;
            let output = print(runner, user_id).await?;
            state = classify_print(&output, user_id, "launchd quiescence check")?;
            if state != JobState::Running {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(ServiceError::InvalidState(format!(
                    "{LABEL} still reports a live pid after {} seconds; rerun \
solstone-tmux-observer {retry_command}",
                    QUIESCENCE_TIMEOUT.as_secs()
                )));
            }
        }
    }

    if state == JobState::Absent {
        return Ok(());
    }
    let output = bootout(runner, user_id, path).await?;
    if output.status == 0 || reports_missing_service(&output, user_id) {
        Ok(())
    } else {
        Err(manager_error("launchd bootout", &output))
    }
}

fn classify_print(
    output: &CommandOutput,
    user_id: u32,
    action: &'static str,
) -> Result<JobState, ServiceError> {
    if output.status == 0 {
        if reports_live_pid(&output.stdout) {
            Ok(JobState::Running)
        } else {
            Ok(JobState::Quiescent)
        }
    } else if reports_missing_service(output, user_id) {
        Ok(JobState::Absent)
    } else {
        Err(manager_error(action, output))
    }
}

fn reports_live_pid(stdout: &[u8]) -> bool {
    String::from_utf8_lossy(stdout).lines().any(|line| {
        let line = line.trim_start();
        let Some((key, value)) = line.split_once(" = ") else {
            return false;
        };
        key == "pid"
            && !value.is_empty()
            && value.bytes().all(|byte| byte.is_ascii_digit())
            && value.bytes().any(|byte| byte != b'0')
    })
}

fn reports_missing_service(output: &CommandOutput, user_id: u32) -> bool {
    if output.status != 113 {
        return false;
    }
    let expected = format!("Could not find service \"{LABEL}\" in domain for user gui: {user_id}");
    output
        .stderr
        .split(|byte| *byte == b'\n')
        .any(|line| line == expected.as_bytes())
}

async fn verify_loaded(runner: &dyn CommandRunner, user_id: u32) -> Result<(), ServiceError> {
    let output = print(runner, user_id).await?;
    if output.status != 0 {
        return Err(manager_error("launchd loaded check", &output));
    }
    if reports_live_pid(&output.stdout) {
        Ok(())
    } else {
        Err(ServiceError::InvalidState(format!(
            "{LABEL} is loaded but does not report a live pid; rerun \
solstone-tmux-observer install-service"
        )))
    }
}

async fn bootout(
    runner: &dyn CommandRunner,
    user_id: u32,
    path: &Path,
) -> Result<CommandOutput, ServiceError> {
    run(
        runner,
        ServiceOperation::LaunchdBootout,
        vec![
            "bootout".into(),
            domain(user_id).into(),
            path.as_os_str().to_owned(),
        ],
    )
    .await
}

async fn disable(runner: &dyn CommandRunner, user_id: u32) -> Result<(), ServiceError> {
    require_success(
        "launchd disable",
        run(
            runner,
            ServiceOperation::LaunchdDisable,
            vec!["disable".into(), target(user_id).into()],
        )
        .await?,
    )
}

async fn enable(runner: &dyn CommandRunner, user_id: u32) -> Result<(), ServiceError> {
    require_success(
        "launchd enable",
        run(
            runner,
            ServiceOperation::LaunchdEnable,
            vec!["enable".into(), target(user_id).into()],
        )
        .await?,
    )
}

async fn kill(runner: &dyn CommandRunner, user_id: u32) -> Result<CommandOutput, ServiceError> {
    run(
        runner,
        ServiceOperation::LaunchdKill,
        vec!["kill".into(), "SIGTERM".into(), target(user_id).into()],
    )
    .await
}

async fn print(runner: &dyn CommandRunner, user_id: u32) -> Result<CommandOutput, ServiceError> {
    run(
        runner,
        ServiceOperation::LaunchdPrint,
        vec!["print".into(), target(user_id).into()],
    )
    .await
}

async fn bootstrap(
    runner: &dyn CommandRunner,
    user_id: u32,
    path: &Path,
) -> Result<(), ServiceError> {
    require_success(
        "launchd bootstrap",
        run(
            runner,
            ServiceOperation::LaunchdBootstrap,
            vec![
                "bootstrap".into(),
                domain(user_id).into(),
                path.as_os_str().to_owned(),
            ],
        )
        .await?,
    )
}

async fn run(
    runner: &dyn CommandRunner,
    operation: ServiceOperation,
    args: Vec<OsString>,
) -> Result<CommandOutput, ServiceError> {
    Ok(runner
        .run(CommandInvocation {
            operation: CommandOperation::Service(operation),
            executable: PathBuf::from("launchctl"),
            args,
            timeout: COMMAND_TIMEOUT,
        })
        .await?)
}

fn ownership_marker() -> Vec<u8> {
    format!("<string>{LABEL}</string>").into_bytes()
}

fn domain(user_id: u32) -> String {
    format!("gui/{user_id}")
}

fn target(user_id: u32) -> String {
    format!("{}/{LABEL}", domain(user_id))
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
