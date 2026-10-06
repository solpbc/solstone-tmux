// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::fmt;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::name::{DerivedName, NameError, derive_component};

pub const CONFIG_FILENAME: &str = "config.json";
pub const DEFAULT_CAPTURE_INTERVAL_SECONDS: u64 = 5;
pub const DEFAULT_SEGMENT_INTERVAL_SECONDS: u64 = 300;
pub const DEFAULT_CACHE_RETENTION_DAYS: i64 = 7;
pub const DEFAULT_SOURCE: &str = "tmux";
const MAX_SOURCE_BYTES: usize = 64;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeConfig {
    pub stream: DerivedName,
    pub capture_interval: Duration,
    pub segment_interval: Duration,
    pub cache_retention_days: i64,
    pub status_indicator: bool,
    pub source: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct ConfigFile {
    stream: Option<String>,
    capture_interval: u64,
    segment_interval: u64,
    cache_retention_days: i64,
    status_indicator: bool,
    #[serde(default, skip_serializing_if = "OptionalJson::is_absent")]
    source: OptionalJson,
}

#[derive(Debug, Default)]
enum OptionalJson {
    #[default]
    Absent,
    Present(Value),
}

impl OptionalJson {
    fn is_absent(&self) -> bool {
        matches!(self, Self::Absent)
    }
}

impl<'de> Deserialize<'de> for OptionalJson {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(Self::Present(Value::deserialize(deserializer)?))
    }
}

impl Serialize for OptionalJson {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::Absent => serializer.serialize_none(),
            Self::Present(value) => value.serialize(serializer),
        }
    }
}

impl Default for ConfigFile {
    fn default() -> Self {
        Self {
            stream: None,
            capture_interval: DEFAULT_CAPTURE_INTERVAL_SECONDS,
            segment_interval: DEFAULT_SEGMENT_INTERVAL_SECONDS,
            cache_retention_days: DEFAULT_CACHE_RETENTION_DAYS,
            status_indicator: true,
            source: OptionalJson::Absent,
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
                derive_component(&identity).map_err(|error| ConfigError::InvalidStream {
                    identity,
                    source: error,
                })
            })?;
        let source = configured_source(&file.source)?;

        Ok(Self {
            stream,
            capture_interval: Duration::from_secs(file.capture_interval),
            segment_interval: Duration::from_secs(file.segment_interval),
            cache_retention_days: file.cache_retention_days,
            status_indicator: file.status_indicator,
            source,
        })
    }
}

fn configured_source(value: &OptionalJson) -> Result<String, ConfigError> {
    match value {
        OptionalJson::Absent => Ok(DEFAULT_SOURCE.to_owned()),
        OptionalJson::Present(Value::String(source)) if is_configured_source(source) => {
            Ok(source.clone())
        }
        OptionalJson::Present(_) => Err(ConfigError::InvalidSource),
    }
}

fn is_configured_source(source: &str) -> bool {
    if source.is_empty() || source.len() > MAX_SOURCE_BYTES {
        return false;
    }
    let mut bytes = source.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    (first.is_ascii_lowercase() || first.is_ascii_digit())
        && bytes.all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
        })
}

pub fn system_hostname() -> Result<String, ConfigError> {
    rustix::system::uname()
        .nodename()
        .to_str()
        .map(str::to_owned)
        .map_err(|error| ConfigError::InvalidHostname(error.to_string()))
}

pub(crate) fn default_stream(hostname: &str) -> Result<String, ConfigError> {
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
    InvalidSource,
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
            Self::InvalidSource => write!(
                formatter,
                "native config source must be a nonempty string matching [a-z0-9][a-z0-9_-]* and at most 64 bytes; update source in config.json"
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

#[cfg(test)]
mod tests {
    use super::ConfigFile;

    #[test]
    fn source_free_config_serialization_omits_the_source_key() {
        let bytes = serde_json::to_vec(&ConfigFile::default()).expect("serialize default config");
        assert_eq!(
            bytes,
            br#"{"stream":null,"capture_interval":5,"segment_interval":300,"cache_retention_days":7,"status_indicator":true}"#
        );
    }
}
