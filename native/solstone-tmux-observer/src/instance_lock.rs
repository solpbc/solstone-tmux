// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::fmt;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

pub const LOCK_FILENAME: &str = ".solstone-tmux-observer.lock";
static RUN_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RunIdentity {
    pub run_id: String,
    pub lock_inode: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ExistingLock {
    MissingOrInvalid,
    Unlocked(RunIdentity),
    Locked(RunIdentity),
}

#[derive(Debug)]
pub struct InstanceLock {
    file: File,
    path: PathBuf,
    identity: RunIdentity,
}

impl InstanceLock {
    pub fn acquire(data_root: &Path) -> Result<Self, InstanceLockError> {
        Self::open(data_root, true)
    }

    pub fn acquire_existing(data_root: &Path) -> Result<Option<Self>, InstanceLockError> {
        match fs::symlink_metadata(data_root) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(InstanceLockError::InvalidTarget(data_root.to_owned()));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(InstanceLockError::Io {
                    operation: "inspect data root",
                    path: data_root.to_owned(),
                    source,
                });
            }
        }
        let path = data_root.join(LOCK_FILENAME);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                return Err(InstanceLockError::InvalidTarget(path));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(InstanceLockError::Io {
                    operation: "inspect instance lock",
                    path,
                    source,
                });
            }
        }
        Self::open(data_root, false).map(Some)
    }

    fn open(data_root: &Path, create: bool) -> Result<Self, InstanceLockError> {
        let path = data_root.join(LOCK_FILENAME);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                return Err(InstanceLockError::InvalidTarget(path));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(InstanceLockError::Io {
                    operation: "inspect instance lock",
                    path,
                    source,
                });
            }
        }
        let mut flags =
            rustix::fs::OFlags::RDWR | rustix::fs::OFlags::CLOEXEC | rustix::fs::OFlags::NOFOLLOW;
        if create {
            flags |= rustix::fs::OFlags::CREATE;
        }
        let descriptor = rustix::fs::open(
            &path,
            flags,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        )
        .map_err(|source| InstanceLockError::Io {
            operation: "open instance lock",
            path: path.clone(),
            source: source.into(),
        })?;
        let file = File::from(descriptor);
        if create {
            file.set_permissions(fs::Permissions::from_mode(0o600))
                .map_err(|source| InstanceLockError::Io {
                    operation: "set instance lock permissions",
                    path: path.clone(),
                    source,
                })?;
        }
        let metadata = file.metadata().map_err(|source| InstanceLockError::Io {
            operation: "inspect instance lock",
            path: path.clone(),
            source,
        })?;
        if !metadata.is_file() {
            return Err(InstanceLockError::InvalidTarget(path));
        }
        match rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => {
                let identity = RunIdentity {
                    run_id: generate_run_id(),
                    lock_inode: metadata.ino(),
                };
                write_identity(&file, &path, &identity)?;
                Ok(Self {
                    file,
                    path,
                    identity,
                })
            }
            Err(error)
                if error == rustix::io::Errno::AGAIN || error == rustix::io::Errno::WOULDBLOCK =>
            {
                Err(InstanceLockError::AlreadyLocked(path))
            }
            Err(source) => Err(InstanceLockError::Io {
                operation: "acquire instance lock",
                path,
                source: source.into(),
            }),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn inode(&self) -> u64 {
        self.identity.lock_inode
    }

    pub fn file(&self) -> &File {
        &self.file
    }

    pub fn identity(&self) -> &RunIdentity {
        &self.identity
    }
}

pub(crate) fn inspect_existing(data_root: &Path) -> ExistingLock {
    let path = data_root.join(LOCK_FILENAME);
    let descriptor = match rustix::fs::open(
        &path,
        rustix::fs::OFlags::RDWR | rustix::fs::OFlags::CLOEXEC | rustix::fs::OFlags::NOFOLLOW,
        rustix::fs::Mode::empty(),
    ) {
        Ok(descriptor) => descriptor,
        Err(_) => return ExistingLock::MissingOrInvalid,
    };
    let mut file = File::from(descriptor);
    let metadata = match file.metadata() {
        Ok(metadata) if metadata.is_file() => metadata,
        _ => return ExistingLock::MissingOrInvalid,
    };
    let mut bytes = Vec::new();
    if file.read_to_end(&mut bytes).is_err() {
        return ExistingLock::MissingOrInvalid;
    }
    let identity = match serde_json::from_slice::<RunIdentity>(&bytes) {
        Ok(identity) if valid_run_id(&identity.run_id) && identity.lock_inode == metadata.ino() => {
            identity
        }
        _ => return ExistingLock::MissingOrInvalid,
    };
    match rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => {
            let _ = rustix::fs::flock(&file, rustix::fs::FlockOperation::Unlock);
            ExistingLock::Unlocked(identity)
        }
        Err(error)
            if error == rustix::io::Errno::AGAIN || error == rustix::io::Errno::WOULDBLOCK =>
        {
            ExistingLock::Locked(identity)
        }
        Err(_) => ExistingLock::MissingOrInvalid,
    }
}

fn write_identity(
    file: &File,
    path: &Path,
    identity: &RunIdentity,
) -> Result<(), InstanceLockError> {
    let bytes = serde_json::to_vec(identity).map_err(|source| InstanceLockError::Io {
        operation: "serialize instance identity",
        path: path.to_owned(),
        source: std::io::Error::other(source),
    })?;
    let mut file = file;
    file.set_len(0).map_err(|source| InstanceLockError::Io {
        operation: "truncate instance identity",
        path: path.to_owned(),
        source,
    })?;
    file.seek(SeekFrom::Start(0))
        .and_then(|_| file.write_all(&bytes))
        .and_then(|_| file.sync_all())
        .map_err(|source| InstanceLockError::Io {
            operation: "write instance identity",
            path: path.to_owned(),
            source,
        })
}

fn generate_run_id() -> String {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let process = u128::from(std::process::id()) << 64;
    let counter = u128::from(RUN_COUNTER.fetch_add(1, Ordering::Relaxed));
    format!("{:032x}", elapsed ^ process ^ counter)
}

fn valid_run_id(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[derive(Debug)]
pub enum InstanceLockError {
    AlreadyLocked(PathBuf),
    InvalidTarget(PathBuf),
    Io {
        operation: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },
}

impl fmt::Display for InstanceLockError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyLocked(path) => write!(
                formatter,
                "another observer is already using this data root (lock: {})",
                path.display()
            ),
            Self::InvalidTarget(path) => {
                write!(
                    formatter,
                    "instance lock target is not a regular file: {}",
                    path.display()
                )
            }
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

impl std::error::Error for InstanceLockError {}
