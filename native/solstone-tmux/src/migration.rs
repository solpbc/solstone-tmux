// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::fmt;
use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::config::{CONFIG_FILENAME, ConfigFile, RuntimeConfig};
use crate::paths::PlatformKind;
use crate::storage::{atomic_write_bytes, open_regular_readonly, sync_directory};

pub const IMPORTED_LEGACY_FIELDS: [&str; 5] = [
    "stream",
    "capture_interval",
    "segment_interval",
    "cache_retention_days",
    "status_indicator",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MigrationOutcome {
    NotApplicable,
    NativePresent,
    LegacyAbsent,
    Migrated,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MigrationFailure {
    NativeTargetNotRegular,
    LegacyInspect,
    LegacyRead,
    LegacyInvalid,
    LegacyValidation,
    Write,
}

#[derive(Debug)]
pub struct MigrationError {
    failure: MigrationFailure,
    native_path: PathBuf,
}

impl MigrationError {
    fn new(failure: MigrationFailure, native_path: &Path) -> Self {
        Self {
            failure,
            native_path: native_path.to_owned(),
        }
    }
}

impl fmt::Display for MigrationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self.failure {
            MigrationFailure::NativeTargetNotRegular => {
                "native settings target is not a regular file:"
            }
            MigrationFailure::LegacyInspect => {
                "legacy settings could not be inspected while preparing"
            }
            MigrationFailure::LegacyRead => "legacy settings could not be read while preparing",
            MigrationFailure::LegacyInvalid => "legacy settings are invalid for",
            MigrationFailure::LegacyValidation => "legacy settings could not be validated for",
            MigrationFailure::Write => "migrated settings could not be written to",
        };
        write!(formatter, "{message} {}", self.native_path.display())
    }
}

impl std::error::Error for MigrationError {}

pub fn migrate_legacy_config(
    platform: PlatformKind,
    data_root: &Path,
    config_root: &Path,
    hostname: &str,
) -> Result<MigrationOutcome, MigrationError> {
    if platform == PlatformKind::Macos {
        return Ok(MigrationOutcome::NotApplicable);
    }

    let native_path = config_root.join(CONFIG_FILENAME);
    match fs::symlink_metadata(&native_path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(MigrationError::new(
                MigrationFailure::NativeTargetNotRegular,
                &native_path,
            ));
        }
        Ok(_) => return Ok(MigrationOutcome::NativePresent),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => {
            return Err(MigrationError::new(
                MigrationFailure::NativeTargetNotRegular,
                &native_path,
            ));
        }
    }

    let legacy_path = data_root.join("config").join(CONFIG_FILENAME);
    match fs::symlink_metadata(&legacy_path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(MigrationError::new(
                MigrationFailure::LegacyInvalid,
                &native_path,
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(MigrationOutcome::LegacyAbsent);
        }
        Err(_) => {
            return Err(MigrationError::new(
                MigrationFailure::LegacyInspect,
                &native_path,
            ));
        }
    }

    let mut legacy = open_regular_readonly(&legacy_path)
        .map_err(|_| MigrationError::new(MigrationFailure::LegacyRead, &native_path))?;
    let mut bytes = Vec::new();
    legacy
        .read_to_end(&mut bytes)
        .map_err(|_| MigrationError::new(MigrationFailure::LegacyRead, &native_path))?;
    let source: Value = serde_json::from_slice(&bytes)
        .map_err(|_| MigrationError::new(MigrationFailure::LegacyInvalid, &native_path))?;
    let source = source
        .as_object()
        .ok_or_else(|| MigrationError::new(MigrationFailure::LegacyInvalid, &native_path))?;

    let mut projected = Map::new();
    for field in IMPORTED_LEGACY_FIELDS {
        let Some(value) = source.get(field) else {
            continue;
        };
        if field == "stream" && value.as_str() == Some("") {
            continue;
        }
        projected.insert(field.to_owned(), value.clone());
    }
    let config: ConfigFile = serde_json::from_value(Value::Object(projected))
        .map_err(|_| MigrationError::new(MigrationFailure::LegacyInvalid, &native_path))?;
    RuntimeConfig::from_config_file(&config, hostname)
        .map_err(|_| MigrationError::new(MigrationFailure::LegacyValidation, &native_path))?;
    let native_bytes = serde_json::to_vec(&config)
        .map_err(|_| MigrationError::new(MigrationFailure::LegacyValidation, &native_path))?;

    atomic_write_bytes(&native_path, config_root, &native_bytes)
        .and_then(|()| sync_directory(config_root))
        .map_err(|_| MigrationError::new(MigrationFailure::Write, &native_path))?;
    let mode = fs::metadata(&native_path)
        .map_err(|_| MigrationError::new(MigrationFailure::Write, &native_path))?
        .permissions()
        .mode()
        & 0o777;
    if mode != 0o600 {
        return Err(MigrationError::new(MigrationFailure::Write, &native_path));
    }

    Ok(MigrationOutcome::Migrated)
}
