// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

pub mod launchd;
pub mod systemd;

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::command::{CommandError, CommandRunner};
use crate::paths::{
    Environment, PathError, PlatformKind, PlatformPaths, ensure_private_directory,
    ensure_service_directory,
};
use crate::storage::{StorageError, atomic_write_bytes, sync_directory};

pub const STATE_FILENAME: &str = "local-observer.json";
pub const COMMAND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
pub const TMUX_NOT_FOUND: &str = "tmux was not found. Install tmux, or put its bin directory on PATH and rerun solstone-tmux-observer install-service.";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LocalObserver {
    pub tmux_path: PathBuf,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServiceStatus {
    Active,
    Inactive,
    Absent,
}

impl ServiceStatus {
    pub const fn exit_code(self) -> i32 {
        match self {
            Self::Active => 0,
            Self::Inactive => 3,
            Self::Absent => 4,
        }
    }
}

pub fn status_exit_code(result: Result<ServiceStatus, ServiceError>) -> i32 {
    result.map_or(1, ServiceStatus::exit_code)
}

#[derive(Debug)]
pub enum ServiceError {
    Path(PathError),
    Storage(StorageError),
    Command(CommandError),
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    InvalidArtifact(PathBuf),
    InvalidExecutable(PathBuf),
    InvalidUtf8Path(PathBuf),
    TmuxNotFound,
    Manager {
        action: &'static str,
        status: i32,
        stderr: String,
    },
    LaunchdRecovery {
        primary: Box<ServiceError>,
        reenable: Box<ServiceError>,
        retry_command: &'static str,
    },
    InvalidState(String),
}

impl fmt::Display for ServiceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Path(error) => error.fmt(formatter),
            Self::Storage(error) => error.fmt(formatter),
            Self::Command(error) => error.fmt(formatter),
            Self::Io { path, source } => write!(formatter, "{}: {source}", path.display()),
            Self::InvalidArtifact(path) => write!(
                formatter,
                "refusing to alter invalid or unowned service artifact {}",
                path.display()
            ),
            Self::InvalidExecutable(path) => {
                write!(
                    formatter,
                    "{} is not an executable regular file",
                    path.display()
                )
            }
            Self::InvalidUtf8Path(path) => write!(
                formatter,
                "{} cannot be represented safely in a service definition",
                path.display()
            ),
            Self::TmuxNotFound => formatter.write_str(TMUX_NOT_FOUND),
            Self::Manager {
                action,
                status,
                stderr,
            } => {
                write!(formatter, "{action} failed with exit status {status}")?;
                if !stderr.is_empty() {
                    write!(formatter, ": {stderr}")?;
                }
                Ok(())
            }
            Self::LaunchdRecovery {
                primary,
                reenable,
                retry_command,
            } => write!(
                formatter,
                "{primary}; launchd re-enable recovery also failed: {reenable}; launchd crash \
restart may remain disabled for {}; rerun solstone-tmux-observer {retry_command}",
                launchd::LABEL
            ),
            Self::InvalidState(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for ServiceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Path(error) => Some(error),
            Self::Storage(error) => Some(error),
            Self::Command(error) => Some(error),
            Self::Io { source, .. } => Some(source),
            Self::LaunchdRecovery { primary, .. } => Some(primary.as_ref()),
            _ => None,
        }
    }
}

impl From<PathError> for ServiceError {
    fn from(error: PathError) -> Self {
        Self::Path(error)
    }
}

impl From<StorageError> for ServiceError {
    fn from(error: StorageError) -> Self {
        Self::Storage(error)
    }
}

impl From<CommandError> for ServiceError {
    fn from(error: CommandError) -> Self {
        Self::Command(error)
    }
}

pub struct ServiceController<'a> {
    platform: PlatformKind,
    environment: &'a dyn Environment,
    runner: &'a dyn CommandRunner,
    binary: PathBuf,
    standard_directories: Vec<PathBuf>,
    user_id: u32,
}

impl<'a> ServiceController<'a> {
    pub fn new(
        platform: PlatformKind,
        environment: &'a dyn Environment,
        runner: &'a dyn CommandRunner,
        binary: PathBuf,
    ) -> Self {
        Self {
            platform,
            environment,
            runner,
            binary,
            standard_directories: standard_directories(platform),
            user_id: rustix::process::getuid().as_raw(),
        }
    }

    pub fn with_standard_directories(mut self, directories: Vec<PathBuf>) -> Self {
        self.standard_directories = directories;
        self
    }

    pub fn with_user_id(mut self, user_id: u32) -> Self {
        self.user_id = user_id;
        self
    }

    pub async fn install(&self) -> Result<(), ServiceError> {
        let paths = PlatformPaths::resolve(self.platform, self.environment)?;
        let home = required_home(self.environment)?;
        let binary = canonical_executable(&self.binary)?;
        let search_directories = search_directories(self.environment, &self.standard_directories);
        let tmux_path = resolve_tmux(&search_directories)?;
        let service_path = assembled_service_path(&tmux_path, &search_directories)?;
        let state = LocalObserver { tmux_path };
        let previous_state = read_local_observer_state(&paths.config_root)?;

        // Activation can start `run` immediately, so persist first and restore the
        // previous state if activation fails.
        match self.platform {
            PlatformKind::Linux => {
                systemd::prepare_install(self.runner, &home, &binary, &service_path).await?;
                persist_local_observer(&paths.config_root, &state)?;
                if let Err(error) = systemd::activate(self.runner).await {
                    restore_local_observer_state(&paths.config_root, previous_state)?;
                    return Err(error);
                }
            }
            PlatformKind::Macos => {
                let prepared = launchd::prepare_install(
                    self.runner,
                    &home,
                    self.user_id,
                    &binary,
                    &service_path,
                )
                .await?;
                persist_local_observer(&paths.config_root, &state)?;
                if let Err(error) = launchd::activate(self.runner, self.user_id, prepared).await {
                    restore_local_observer_state(&paths.config_root, previous_state)?;
                    return Err(error);
                }
            }
        }
        Ok(())
    }

    pub async fn uninstall(&self) -> Result<(), ServiceError> {
        let paths = PlatformPaths::resolve(self.platform, self.environment)?;
        let home = required_home(self.environment)?;
        match self.platform {
            PlatformKind::Linux => systemd::uninstall(self.runner, &home).await?,
            PlatformKind::Macos => {
                launchd::uninstall(self.runner, &home, self.user_id).await?;
            }
        }
        remove_owned_regular_file(
            &paths.config_root.join(STATE_FILENAME),
            Some(b"\"tmux_path\""),
        )?;
        Ok(())
    }

    pub async fn status(&self) -> Result<ServiceStatus, ServiceError> {
        let home = required_home(self.environment)?;
        match self.platform {
            PlatformKind::Linux => systemd::status(self.runner, &home).await,
            PlatformKind::Macos => launchd::status(self.runner, &home, self.user_id).await,
        }
    }
}

pub fn current_platform() -> PlatformKind {
    #[cfg(target_os = "macos")]
    {
        PlatformKind::Macos
    }
    #[cfg(not(target_os = "macos"))]
    {
        PlatformKind::Linux
    }
}

pub fn load_local_observer(config_root: &Path) -> Result<LocalObserver, ServiceError> {
    let path = config_root.join(STATE_FILENAME);
    if !validate_regular_file(&path, None)? {
        return Err(ServiceError::InvalidState(format!(
            "{} is missing; run solstone-tmux-observer install-service to resolve and persist an absolute tmux path",
            path.display()
        )));
    }
    let bytes = fs::read(&path).map_err(|source| ServiceError::Io {
        path: path.clone(),
        source,
    })?;
    let state: LocalObserver = serde_json::from_slice(&bytes).map_err(|error| {
        ServiceError::InvalidState(format!(
            "{} does not contain valid local observer state: {error}",
            path.display()
        ))
    })?;
    let canonical = canonical_executable(&state.tmux_path)?;
    if canonical != state.tmux_path {
        return Err(ServiceError::InvalidState(format!(
            "{} contains a non-canonical tmux path",
            path.display()
        )));
    }
    Ok(state)
}

pub fn standard_directories(platform: PlatformKind) -> Vec<PathBuf> {
    let values: &[&str] = match platform {
        PlatformKind::Linux => &["/usr/local/bin", "/usr/bin", "/bin"],
        PlatformKind::Macos => &["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin"],
    };
    values.iter().map(PathBuf::from).collect()
}

fn required_home(environment: &dyn Environment) -> Result<PathBuf, ServiceError> {
    environment
        .var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or(ServiceError::Path(PathError::MissingHome))
}

fn search_directories(environment: &dyn Environment, standards: &[PathBuf]) -> Vec<PathBuf> {
    let path = environment.var_os("PATH").unwrap_or_default();
    let mut seen = HashSet::<OsString>::new();
    std::env::split_paths(&path)
        .filter(|entry| !entry.as_os_str().is_empty())
        .chain(standards.iter().cloned())
        .filter(|entry| seen.insert(entry.as_os_str().to_owned()))
        .collect()
}

fn resolve_tmux(directories: &[PathBuf]) -> Result<PathBuf, ServiceError> {
    for directory in directories {
        let candidate = directory.join("tmux");
        if let Ok(path) = canonical_executable(&candidate) {
            return Ok(path);
        }
    }
    Err(ServiceError::TmuxNotFound)
}

fn canonical_executable(path: &Path) -> Result<PathBuf, ServiceError> {
    let canonical =
        fs::canonicalize(path).map_err(|_| ServiceError::InvalidExecutable(path.to_owned()))?;
    if !canonical.is_absolute() {
        return Err(ServiceError::InvalidExecutable(path.to_owned()));
    }
    let metadata = fs::symlink_metadata(&canonical)
        .map_err(|_| ServiceError::InvalidExecutable(path.to_owned()))?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.permissions().mode() & 0o111 == 0
    {
        return Err(ServiceError::InvalidExecutable(path.to_owned()));
    }
    Ok(canonical)
}

fn assembled_service_path(
    tmux_path: &Path,
    directories: &[PathBuf],
) -> Result<OsString, ServiceError> {
    let mut assembled = Vec::new();
    if let Some(parent) = tmux_path.parent() {
        assembled.push(parent.to_owned());
    }
    for directory in directories {
        if !assembled.contains(directory) {
            assembled.push(directory.clone());
        }
    }
    std::env::join_paths(assembled).map_err(|error| {
        ServiceError::InvalidState(format!(
            "could not assemble service PATH including the resolved tmux directory: {error}; remove path entries containing ':' and rerun install-service"
        ))
    })
}

fn persist_local_observer(config_root: &Path, state: &LocalObserver) -> Result<(), ServiceError> {
    ensure_private_directory(config_root)?;
    let mut bytes = serde_json::to_vec(state).map_err(|error| {
        ServiceError::InvalidState(format!("could not serialize state: {error}"))
    })?;
    bytes.push(b'\n');
    atomic_write_bytes(&config_root.join(STATE_FILENAME), config_root, &bytes)?;
    Ok(())
}

fn read_local_observer_state(config_root: &Path) -> Result<Option<Vec<u8>>, ServiceError> {
    let path = config_root.join(STATE_FILENAME);
    if !validate_regular_file(&path, Some(b"\"tmux_path\""))? {
        return Ok(None);
    }
    fs::read(&path)
        .map(Some)
        .map_err(|source| ServiceError::Io { path, source })
}

fn restore_local_observer_state(
    config_root: &Path,
    previous: Option<Vec<u8>>,
) -> Result<(), ServiceError> {
    let path = config_root.join(STATE_FILENAME);
    if let Some(bytes) = previous {
        atomic_write_bytes(&path, config_root, &bytes)?;
    } else {
        remove_owned_regular_file(&path, Some(b"\"tmux_path\""))?;
    }
    Ok(())
}

pub(crate) fn validate_regular_file(
    path: &Path,
    required_marker: Option<&[u8]>,
) -> Result<bool, ServiceError> {
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
        return Err(ServiceError::InvalidArtifact(path.to_owned()));
    }
    if let Some(marker) = required_marker {
        let bytes = fs::read(path).map_err(|source| ServiceError::Io {
            path: path.to_owned(),
            source,
        })?;
        if !bytes.windows(marker.len()).any(|window| window == marker) {
            return Err(ServiceError::InvalidArtifact(path.to_owned()));
        }
    }
    Ok(true)
}

pub(crate) fn install_artifact(path: &Path, bytes: &[u8]) -> Result<bool, ServiceError> {
    if validate_regular_file(path, None)?
        && fs::read(path).map_err(|source| ServiceError::Io {
            path: path.to_owned(),
            source,
        })? == bytes
    {
        return Ok(false);
    }
    let parent = path
        .parent()
        .ok_or_else(|| ServiceError::InvalidArtifact(path.to_owned()))?;
    ensure_service_directory(parent)?;
    atomic_write_bytes(path, parent, bytes)?;
    Ok(true)
}

pub(crate) fn remove_owned_regular_file(
    path: &Path,
    required_marker: Option<&[u8]>,
) -> Result<bool, ServiceError> {
    if !validate_regular_file(path, required_marker)? {
        return Ok(false);
    }
    fs::remove_file(path).map_err(|source| ServiceError::Io {
        path: path.to_owned(),
        source,
    })?;
    let parent = path
        .parent()
        .ok_or_else(|| ServiceError::InvalidArtifact(path.to_owned()))?;
    sync_directory(parent)?;
    Ok(true)
}

pub(crate) fn utf8_path(path: &Path) -> Result<&str, ServiceError> {
    path.to_str()
        .ok_or_else(|| ServiceError::InvalidUtf8Path(path.to_owned()))
}

pub(crate) fn utf8_os(value: &OsStr) -> Result<&str, ServiceError> {
    value
        .to_str()
        .ok_or_else(|| ServiceError::InvalidUtf8Path(PathBuf::from(value)))
}

pub(crate) fn manager_error(
    action: &'static str,
    output: &crate::command::CommandOutput,
) -> ServiceError {
    ServiceError::Manager {
        action,
        status: output.status,
        stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
    }
}

pub(crate) fn require_success(
    action: &'static str,
    output: crate::command::CommandOutput,
) -> Result<(), ServiceError> {
    if output.status == 0 {
        Ok(())
    } else {
        Err(manager_error(action, &output))
    }
}

pub(crate) fn reports_absent(output: &crate::command::CommandOutput) -> bool {
    let stderr = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
    output.status == 3
        || output.status == 4
        || stderr.contains("not found")
        || stderr.contains("not loaded")
        || stderr.contains("could not be found")
        || stderr.contains("no such process")
}
