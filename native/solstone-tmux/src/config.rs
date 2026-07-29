// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::fmt;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::name::{DerivedName, NameError, derive_component};

pub const CONFIG_FILENAME: &str = "config.json";
pub const DEFAULT_CAPTURE_INTERVAL_SECONDS: u64 = 5;
pub const DEFAULT_SEGMENT_INTERVAL_SECONDS: u64 = 300;
pub const DEFAULT_CACHE_RETENTION_DAYS: i64 = 7;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeConfig {
    pub stream: DerivedName,
    pub capture_interval: Duration,
    pub segment_interval: Duration,
    pub cache_retention_days: i64,
    pub status_indicator: bool,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct ConfigFile {
    stream: Option<String>,
    capture_interval: u64,
    segment_interval: u64,
    cache_retention_days: i64,
    status_indicator: bool,
}

impl Default for ConfigFile {
    fn default() -> Self {
        Self {
            stream: None,
            capture_interval: DEFAULT_CAPTURE_INTERVAL_SECONDS,
            segment_interval: DEFAULT_SEGMENT_INTERVAL_SECONDS,
            cache_retention_days: DEFAULT_CACHE_RETENTION_DAYS,
            status_indicator: true,
        }
    }
}

impl RuntimeConfig {
    pub fn load(config_root: &Path, hostname: &str) -> Result<Self, ConfigError> {
        let path = config_root.join(CONFIG_FILENAME);
        let file = match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_file() {
                    return Err(ConfigError::InvalidTarget(path));
                }
                fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).map_err(
                    |source| ConfigError::Io {
                        operation: "set native config permissions",
                        path: path.clone(),
                        source,
                    },
                )?;
                let bytes = fs::read(&path).map_err(|source| ConfigError::Io {
                    operation: "read native config",
                    path: path.clone(),
                    source,
                })?;
                serde_json::from_slice::<ConfigFile>(&bytes).map_err(|source| {
                    ConfigError::InvalidConfig {
                        path: path.clone(),
                        detail: source.to_string(),
                    }
                })?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => ConfigFile::default(),
            Err(source) => {
                return Err(ConfigError::Io {
                    operation: "inspect native config",
                    path,
                    source,
                });
            }
        };

        Self::from_config_file(&file, hostname)
    }

    pub(crate) fn from_config_file(file: &ConfigFile, hostname: &str) -> Result<Self, ConfigError> {
        if file.capture_interval == 0 {
            return Err(ConfigError::InvalidInterval {
                field: "capture_interval",
            });
        }
        if file.segment_interval == 0 {
            return Err(ConfigError::InvalidInterval {
                field: "segment_interval",
            });
        }
        let stream = file
            .stream
            .as_deref()
            .map_or_else(|| default_stream(hostname), |stream| Ok(stream.to_owned()))
            .and_then(|identity| {
                derive_component(&identity)
                    .map_err(|source| ConfigError::InvalidStream { identity, source })
            })?;

        Ok(Self {
            stream,
            capture_interval: Duration::from_secs(file.capture_interval),
            segment_interval: Duration::from_secs(file.segment_interval),
            cache_retention_days: file.cache_retention_days,
            status_indicator: file.status_indicator,
        })
    }
}

pub fn system_hostname() -> Result<String, ConfigError> {
    rustix::system::uname()
        .nodename()
        .to_str()
        .map(str::to_owned)
        .map_err(|error| ConfigError::InvalidHostname(error.to_string()))
}

fn default_stream(hostname: &str) -> Result<String, ConfigError> {
    let hostname = hostname.trim();
    let parts = hostname
        .split('.')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    let base = if !parts.is_empty()
        && parts
            .iter()
            .all(|part| part.bytes().all(|b| b.is_ascii_digit()))
    {
        parts.join("-")
    } else {
        hostname.split('.').next().unwrap_or_default().to_owned()
    };
    let mut normalized = String::new();
    let mut replaced = false;
    for character in base.trim().to_lowercase().chars() {
        if character.is_whitespace() || matches!(character, '/' | '\\') {
            if !replaced {
                normalized.push('-');
                replaced = true;
            }
        } else {
            normalized.push(character);
            replaced = false;
        }
    }
    let stream = format!("{normalized}.tmux");
    let valid = !normalized.is_empty()
        && !stream.contains("..")
        && stream.bytes().enumerate().all(|(index, byte)| {
            if index == 0 {
                byte.is_ascii_lowercase() || byte.is_ascii_digit()
            } else {
                byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || matches!(byte, b'.' | b'_' | b'-')
            }
        });
    if !valid {
        return Err(ConfigError::InvalidHostname(format!(
            "hostname {hostname:?} cannot form a native stream name"
        )));
    }
    Ok(stream)
}

#[derive(Debug)]
pub enum ConfigError {
    InvalidTarget(PathBuf),
    InvalidConfig {
        path: PathBuf,
        detail: String,
    },
    InvalidStream {
        identity: String,
        source: NameError,
    },
    InvalidInterval {
        field: &'static str,
    },
    InvalidHostname(String),
    Io {
        operation: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidTarget(path) => write!(
                formatter,
                "native config {} must be a regular file, not a symlink or special target",
                path.display()
            ),
            Self::InvalidConfig { path, detail } => write!(
                formatter,
                "native config {} is invalid: {detail}",
                path.display()
            ),
            Self::InvalidStream { identity, source } => write!(
                formatter,
                "native stream name {identity:?} is invalid: {source}; update stream in config.json"
            ),
            Self::InvalidInterval { field } => write!(
                formatter,
                "native config {field} must be greater than zero seconds"
            ),
            Self::InvalidHostname(detail) => write!(
                formatter,
                "could not derive the default native stream from this hostname: {detail}; set stream in config.json"
            ),
            Self::Io {
                operation,
                path,
                source,
            } => write!(
                formatter,
                "{operation} failed for {}: {source}",
                path.display()
            ),
        }
    }
}

impl std::error::Error for ConfigError {}
