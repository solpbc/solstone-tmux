// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};

use crate::command::{CommandInvocation, CommandOperation, CommandRunner, ServiceOperation};

use super::{
    COMMAND_TIMEOUT, ServiceError, ServiceStatus, install_artifact, manager_error,
    remove_owned_regular_file, reports_absent, require_success, utf8_os, utf8_path,
    validate_regular_file,
};

pub const UNIT_NAME: &str = "solstone-tmux.service";
const OWNERSHIP_MARKER: &[u8] = b"Description=Solstone Tmux Observer\n";
const LEGACY_PYTHON_MARKER: &[u8] = b"Description=Solstone Tmux Terminal Observer\n";

pub fn artifact_path(home: &Path) -> PathBuf {
    home.join(".config/systemd/user").join(UNIT_NAME)
}

pub fn render(binary: &Path, service_path: &OsStr) -> Result<Vec<u8>, ServiceError> {
    let binary = escape_unit_value(utf8_path(binary)?)?;
    let service_path = escape_unit_value(utf8_os(service_path)?)?;
    Ok(format!(
        "[Unit]\n\
Description=Solstone Tmux Observer\n\
After=basic.target\n\
StartLimitIntervalSec=300\n\
StartLimitBurst=5\n\
\n\
[Service]\n\
Type=simple\n\
Environment=\"PATH={service_path}\"\n\
ExecStart=\"{binary}\" run\n\
Restart=on-failure\n\
RestartSec=5\n\
\n\
[Install]\n\
WantedBy=default.target\n"
    )
    .into_bytes())
}

pub async fn prepare_install(
    runner: &dyn CommandRunner,
    home: &Path,
    binary: &Path,
    service_path: &OsStr,
) -> Result<(), ServiceError> {
    let path = artifact_path(home);
    let bytes = render(binary, service_path)?;
    let unchanged = validate_unit(&path)?
        && std::fs::read(&path).map_err(|source| ServiceError::Io {
            path: path.clone(),
            source,
        })? == bytes;

    if !unchanged {
        let output = run(
            runner,
            ServiceOperation::SystemdStop,
            &["--user", "stop", UNIT_NAME],
        )
        .await?;
        if output.status != 0 && !reports_absent(&output) {
            return Err(manager_error("systemd stop", &output));
        }
        install_artifact(&path, &bytes)?;
        require_success(
            "systemd daemon-reload",
            run(
                runner,
                ServiceOperation::SystemdDaemonReload,
                &["--user", "daemon-reload"],
            )
            .await?,
        )?;
    }
    Ok(())
}

pub async fn activate(runner: &dyn CommandRunner) -> Result<(), ServiceError> {
    require_success(
        "systemd enable",
        run(
            runner,
            ServiceOperation::SystemdEnableNow,
            &["--user", "enable", "--now", UNIT_NAME],
        )
        .await?,
    )?;
    let active = run(
        runner,
        ServiceOperation::SystemdIsActive,
        &["--user", "is-active", UNIT_NAME],
    )
    .await?;
    if active.status != 0 || String::from_utf8_lossy(&active.stdout).trim() != "active" {
        return Err(manager_error("systemd active check", &active));
    }
    Ok(())
}

pub async fn uninstall(runner: &dyn CommandRunner, home: &Path) -> Result<(), ServiceError> {
    let path = artifact_path(home);
    if !validate_unit(&path)? {
        return Ok(());
    }
    let disabled = run(
        runner,
        ServiceOperation::SystemdDisableNow,
        &["--user", "disable", "--now", UNIT_NAME],
    )
    .await?;
    if disabled.status != 0 && !reports_absent(&disabled) {
        return Err(manager_error("systemd disable", &disabled));
    }
    remove_owned_regular_file(&path, Some(OWNERSHIP_MARKER))?;
    require_success(
        "systemd daemon-reload",
        run(
            runner,
            ServiceOperation::SystemdDaemonReload,
            &["--user", "daemon-reload"],
        )
        .await?,
    )
}

pub async fn status(
    runner: &dyn CommandRunner,
    home: &Path,
) -> Result<ServiceStatus, ServiceError> {
    if !validate_unit(&artifact_path(home))? {
        return Ok(ServiceStatus::Absent);
    }
    let output = run(
        runner,
        ServiceOperation::SystemdIsActive,
        &["--user", "is-active", UNIT_NAME],
    )
    .await?;
    if output.status == 0 && String::from_utf8_lossy(&output.stdout).trim() == "active" {
        Ok(ServiceStatus::Active)
    } else if reports_absent(&output)
        || matches!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "inactive" | "failed" | "activating" | "deactivating"
        )
    {
        Ok(ServiceStatus::Inactive)
    } else {
        Err(manager_error("systemd status", &output))
    }
}

fn validate_unit(path: &Path) -> Result<bool, ServiceError> {
    match validate_regular_file(path, Some(OWNERSHIP_MARKER)) {
        Err(ServiceError::InvalidArtifact(_)) if is_legacy_python_unit(path)? => {
            Err(ServiceError::LegacyPythonUnit(path.to_owned()))
        }
        result => result,
    }
}

fn is_legacy_python_unit(path: &Path) -> Result<bool, ServiceError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(source) => {
            return Err(ServiceError::Io {
                path: path.to_owned(),
                source,
            });
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Ok(false);
    }
    let bytes = fs::read(path).map_err(|source| ServiceError::Io {
        path: path.to_owned(),
        source,
    })?;
    Ok(bytes
        .windows(LEGACY_PYTHON_MARKER.len())
        .any(|window| window == LEGACY_PYTHON_MARKER))
}

fn escape_unit_value(value: &str) -> Result<String, ServiceError> {
    if value.chars().any(char::is_control) {
        return Err(ServiceError::InvalidState(
            "service paths must not contain control characters".to_owned(),
        ));
    }
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '%' => escaped.push_str("%%"),
            _ => escaped.push(character),
        }
    }
    Ok(escaped)
}

async fn run(
    runner: &dyn CommandRunner,
    operation: ServiceOperation,
    args: &[&str],
) -> Result<crate::command::CommandOutput, ServiceError> {
    Ok(runner
        .run(CommandInvocation {
            operation: CommandOperation::Service(operation),
            executable: PathBuf::from("systemctl"),
            args: args.iter().map(OsString::from).collect(),
            timeout: COMMAND_TIMEOUT,
        })
        .await?)
}
