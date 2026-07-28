// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

pub mod linux;
pub mod macos;

use std::ffi::OsString;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::name::{DerivedName, NameError, derive_component};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlatformKind {
    Linux,
    Macos,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlatformPaths {
    pub data_root: PathBuf,
    pub config_root: PathBuf,
}

pub trait Environment {
    fn var_os(&self, key: &str) -> Option<OsString>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ProcessEnvironment;

impl Environment for ProcessEnvironment {
    fn var_os(&self, key: &str) -> Option<OsString> {
        std::env::var_os(key)
    }
}

#[derive(Debug)]
pub enum PathError {
    MissingHome,
    Name(NameError),
    InvalidTarget(PathBuf),
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

impl fmt::Display for PathError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingHome => write!(
                formatter,
                "HOME is not set; refusing to use a system-wide location"
            ),
            Self::Name(error) => write!(formatter, "invalid stream name: {error}"),
            Self::InvalidTarget(path) => {
                write!(
                    formatter,
                    "{} is a symlink or wrong file type",
                    path.display()
                )
            }
            Self::Io { path, source } => {
                write!(formatter, "{}: {source}", path.display())
            }
        }
    }
}

impl std::error::Error for PathError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Name(error) => Some(error),
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl From<NameError> for PathError {
    fn from(error: NameError) -> Self {
        Self::Name(error)
    }
}

impl PlatformPaths {
    pub fn resolve(
        platform: PlatformKind,
        environment: &dyn Environment,
    ) -> Result<Self, PathError> {
        match platform {
            PlatformKind::Linux => linux::resolve(environment),
            PlatformKind::Macos => macos::resolve(environment),
        }
    }
}

pub fn resolve_stream_paths(
    platform: PlatformKind,
    environment: &dyn Environment,
    stream: &str,
) -> Result<(DerivedName, PlatformPaths), PathError> {
    let stream = derive_component(stream)?;
    let paths = PlatformPaths::resolve(platform, environment)?;
    Ok((stream, paths))
}

pub fn ensure_private_directory(path: &Path) -> Result<(), PathError> {
    if path.as_os_str().is_empty() || path.parent().is_none() {
        return Err(PathError::InvalidTarget(path.to_owned()));
    }
    let mut missing = Vec::new();
    let mut current = path;
    loop {
        match fs::symlink_metadata(current) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    return Err(PathError::InvalidTarget(current.to_owned()));
                }
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                missing.push(current.to_owned());
                current = current
                    .parent()
                    .filter(|parent| !parent.as_os_str().is_empty())
                    .ok_or_else(|| PathError::InvalidTarget(path.to_owned()))?;
            }
            Err(source) => {
                return Err(PathError::Io {
                    path: current.to_owned(),
                    source,
                });
            }
        }
    }

    for directory in missing.iter().rev() {
        fs::create_dir(directory).map_err(|source| PathError::Io {
            path: directory.clone(),
            source,
        })?;
        set_and_verify_mode(directory, 0o700, true)?;
    }
    set_and_verify_mode(path, 0o700, true)?;
    Ok(())
}

static TEMPORARY_COUNTER: AtomicU64 = AtomicU64::new(0);

pub fn write_private_file_atomic(path: &Path, bytes: &[u8]) -> Result<(), PathError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(PathError::InvalidTarget(path.to_owned()));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(PathError::Io {
                path: path.to_owned(),
                source,
            });
        }
    }

    let parent = path
        .parent()
        .ok_or_else(|| PathError::InvalidTarget(path.to_owned()))?;
    let file_name = path
        .file_name()
        .ok_or_else(|| PathError::InvalidTarget(path.to_owned()))?
        .to_string_lossy();
    let counter = TEMPORARY_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temporary = parent.join(format!(
        ".{file_name}.{}.{}.tmp",
        std::process::id(),
        counter
    ));

    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|source| PathError::Io {
                path: temporary.clone(),
                source,
            })?;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|source| PathError::Io {
                path: temporary.clone(),
                source,
            })?;
        file.write_all(bytes).map_err(|source| PathError::Io {
            path: temporary.clone(),
            source,
        })?;
        file.sync_all().map_err(|source| PathError::Io {
            path: temporary.clone(),
            source,
        })?;
        fs::rename(&temporary, path).map_err(|source| PathError::Io {
            path: path.to_owned(),
            source,
        })?;
        set_and_verify_mode(path, 0o600, false)?;
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn set_and_verify_mode(path: &Path, mode: u32, directory: bool) -> Result<(), PathError> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(|source| {
        PathError::Io {
            path: path.to_owned(),
            source,
        }
    })?;
    let metadata = fs::symlink_metadata(path).map_err(|source| PathError::Io {
        path: path.to_owned(),
        source,
    })?;
    let correct_type = if directory {
        metadata.is_dir()
    } else {
        metadata.is_file()
    };
    if metadata.file_type().is_symlink()
        || !correct_type
        || metadata.permissions().mode() & 0o777 != mode
    {
        return Err(PathError::InvalidTarget(path.to_owned()));
    }
    Ok(())
}
