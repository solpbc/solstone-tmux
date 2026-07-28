// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use crate::command::{CommandInvocation, CommandOperation, CommandRunner, ServiceOperation};

use super::{
    COMMAND_TIMEOUT, ServiceError, ServiceStatus, install_artifact, manager_error,
    remove_owned_regular_file, reports_absent, require_success, utf8_os, utf8_path,
    validate_regular_file,
};

pub const LABEL: &str = "com.solstone.tmux-observer";
const OWNERSHIP_MARKER: &[u8] = b"<string>com.solstone.tmux-observer</string>";

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
    let present = validate_regular_file(&path, Some(OWNERSHIP_MARKER))?;
    let unchanged = present
        && std::fs::read(&path).map_err(|source| ServiceError::Io {
            path: path.clone(),
            source,
        })? == bytes;

    if !unchanged {
        if present {
            let output = bootout(runner, user_id, &path).await?;
            if output.status != 0 && !reports_absent(&output) {
                return Err(manager_error("launchd bootout", &output));
            }
        }
        install_artifact(&path, &bytes)?;
        require_success(
            "launchd enable",
            run(
                runner,
                ServiceOperation::LaunchdEnable,
                vec!["enable".into(), target(user_id).into()],
            )
            .await?,
        )?;
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
        let loaded = print(runner, user_id).await?;
        if loaded.status == 0 {
            return Ok(());
        }
        if !reports_absent(&loaded) {
            return Err(manager_error("launchd loaded check", &loaded));
        }
        require_success(
            "launchd enable",
            run(
                runner,
                ServiceOperation::LaunchdEnable,
                vec!["enable".into(), target(user_id).into()],
            )
            .await?,
        )?;
        bootstrap(runner, user_id, &prepared.path).await?;
    }

    require_success("launchd loaded check", print(runner, user_id).await?)
}

pub async fn uninstall(
    runner: &dyn CommandRunner,
    home: &Path,
    user_id: u32,
) -> Result<(), ServiceError> {
    let path = artifact_path(home);
    if !validate_regular_file(&path, Some(OWNERSHIP_MARKER))? {
        return Ok(());
    }
    let output = bootout(runner, user_id, &path).await?;
    if output.status != 0 && !reports_absent(&output) {
        return Err(manager_error("launchd bootout", &output));
    }
    remove_owned_regular_file(&path, Some(OWNERSHIP_MARKER))?;
    Ok(())
}

pub async fn status(
    runner: &dyn CommandRunner,
    home: &Path,
    user_id: u32,
) -> Result<ServiceStatus, ServiceError> {
    if !validate_regular_file(&artifact_path(home), Some(OWNERSHIP_MARKER))? {
        return Ok(ServiceStatus::Absent);
    }
    let output = print(runner, user_id).await?;
    if output.status == 0 {
        Ok(ServiceStatus::Active)
    } else if reports_absent(&output) {
        Ok(ServiceStatus::Inactive)
    } else {
        Err(manager_error("launchd status", &output))
    }
}

async fn bootout(
    runner: &dyn CommandRunner,
    user_id: u32,
    path: &Path,
) -> Result<crate::command::CommandOutput, ServiceError> {
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

async fn print(
    runner: &dyn CommandRunner,
    user_id: u32,
) -> Result<crate::command::CommandOutput, ServiceError> {
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
) -> Result<crate::command::CommandOutput, ServiceError> {
    Ok(runner
        .run(CommandInvocation {
            operation: CommandOperation::Service(operation),
            executable: PathBuf::from("launchctl"),
            args,
            timeout: COMMAND_TIMEOUT,
        })
        .await?)
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
