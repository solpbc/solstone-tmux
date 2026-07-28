// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::fmt;
use std::fs::{self, File};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

pub const LOCK_FILENAME: &str = ".solstone-tmux-observer.lock";

#[derive(Debug)]
pub struct InstanceLock {
    file: File,
    path: PathBuf,
    inode: u64,
}

impl InstanceLock {
    pub fn acquire(data_root: &Path) -> Result<Self, InstanceLockError> {
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
        let descriptor = rustix::fs::open(
            &path,
            rustix::fs::OFlags::CREATE
                | rustix::fs::OFlags::RDWR
                | rustix::fs::OFlags::CLOEXEC
                | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        )
        .map_err(|source| InstanceLockError::Io {
            operation: "open instance lock",
            path: path.clone(),
            source: source.into(),
        })?;
        let file = File::from(descriptor);
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|source| InstanceLockError::Io {
                operation: "set instance lock permissions",
                path: path.clone(),
                source,
            })?;
        let metadata = file.metadata().map_err(|source| InstanceLockError::Io {
            operation: "inspect instance lock",
            path: path.clone(),
            source,
        })?;
        if !metadata.is_file() {
            return Err(InstanceLockError::InvalidTarget(path));
        }
        match rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => Ok(Self {
                inode: metadata.ino(),
                file,
                path,
            }),
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
        self.inode
    }

    pub fn file(&self) -> &File {
        &self.file
    }
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
