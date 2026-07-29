// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::fs::{self, File};
use std::io::{Seek, SeekFrom, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rustix::fd::AsFd;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MetadataLifecycle {
    Creating,
    Open,
    Finalizing,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SessionMetadata {
    pub session: String,
    pub filename: String,
    pub last_frame_id: u64,
    pub durable_offset: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SegmentMetadata {
    pub schema_version: u32,
    pub lifecycle: MetadataLifecycle,
    pub incomplete_dir: String,
    pub finalized_dir: String,
    pub start_wall_unix_nanos: i128,
    pub local_offset_seconds: i32,
    pub elapsed_nanos: u64,
    pub last_durable_frame_id: u64,
    pub durable_frame_count: u64,
    pub has_durable_frames: bool,
    pub sessions: BTreeMap<String, SessionMetadata>,
}

impl SegmentMetadata {
    pub const SCHEMA_VERSION: u32 = 1;

    pub fn to_bytes(&self) -> Result<Vec<u8>, StorageError> {
        let mut bytes = serde_json::to_vec(self).map_err(StorageError::Serialize)?;
        bytes.push(b'\n');
        Ok(bytes)
    }
}

pub struct AppendFrame<'a> {
    pub filename: &'a str,
    pub previous_offset: u64,
    pub bytes: &'a [u8],
    pub proposed_metadata: SegmentMetadata,
}

#[derive(Debug)]
pub struct DurableAppend {
    metadata: SegmentMetadata,
}

impl DurableAppend {
    pub fn accepted(metadata: SegmentMetadata) -> Self {
        Self { metadata }
    }

    pub fn into_metadata(self) -> SegmentMetadata {
        self.metadata
    }
}

pub trait DurableStorage: Send {
    fn append_frame(&mut self, frame: AppendFrame<'_>) -> Result<DurableAppend, StorageError>;
    fn write_metadata(&mut self, metadata: &SegmentMetadata) -> Result<(), StorageError>;
    fn sync_and_close(&mut self) -> Result<(), StorageError>;
    fn open_handle_count(&self) -> usize;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StorageStage {
    Append,
    Flush,
    JsonFsync,
    Metadata,
    Rollback,
}

#[derive(Clone, Debug, Default)]
pub struct FaultPlan {
    stages: VecDeque<StorageStage>,
}

impl FaultPlan {
    pub fn at(stages: impl IntoIterator<Item = StorageStage>) -> Self {
        Self {
            stages: stages.into_iter().collect(),
        }
    }

    fn fail(&mut self, stage: StorageStage) -> std::io::Result<()> {
        if self.stages.front() == Some(&stage) {
            self.stages.pop_front();
            return Err(std::io::Error::other(format!("injected {stage:?} failure")));
        }
        Ok(())
    }
}

pub struct FileStorage {
    segment_dir: PathBuf,
    metadata_path: PathBuf,
    stream_dir: PathBuf,
    handles: BTreeMap<String, File>,
    faults: FaultPlan,
}

impl FileStorage {
    pub fn with_faults(
        segment_dir: PathBuf,
        metadata_path: PathBuf,
        stream_dir: PathBuf,
        faults: FaultPlan,
    ) -> Self {
        Self {
            segment_dir,
            metadata_path,
            stream_dir,
            handles: BTreeMap::new(),
            faults,
        }
    }

    fn handle(&mut self, filename: &str) -> Result<&mut File, StorageError> {
        if !self.handles.contains_key(filename) {
            let path = self.segment_dir.join(filename);
            let descriptor = rustix::fs::open(
                &path,
                rustix::fs::OFlags::CREATE
                    | rustix::fs::OFlags::RDWR
                    | rustix::fs::OFlags::CLOEXEC
                    | rustix::fs::OFlags::NOFOLLOW,
                rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
            )
            .map_err(|source| StorageError::Io {
                stage: "open JSONL",
                path: path.clone(),
                source: source.into(),
            })?;
            let file = File::from(descriptor);
            file.set_permissions(fs::Permissions::from_mode(0o600))
                .map_err(|source| StorageError::Io {
                    stage: "set JSONL permissions",
                    path: path.clone(),
                    source,
                })?;
            let metadata = file.metadata().map_err(|source| StorageError::Io {
                stage: "inspect JSONL",
                path: path.clone(),
                source,
            })?;
            if !metadata.is_file() {
                return Err(StorageError::InvalidTarget(path));
            }
            self.handles.insert(filename.to_owned(), file);
        }
        Ok(self
            .handles
            .get_mut(filename)
            .expect("newly inserted JSONL handle"))
    }

    fn rollback(&mut self, filename: &str, offset: u64) -> Result<(), StorageError> {
        self.faults
            .fail(StorageStage::Rollback)
            .map_err(|source| StorageError::Io {
                stage: "rollback injection",
                path: self.segment_dir.join(filename),
                source,
            })?;
        let path = self.segment_dir.join(filename);
        let Some(file) = self.handles.get_mut(filename) else {
            return Ok(());
        };
        file.set_len(offset).map_err(|source| StorageError::Io {
            stage: "truncate rollback",
            path: path.clone(),
            source,
        })?;
        file.seek(SeekFrom::Start(offset))
            .map_err(|source| StorageError::Io {
                stage: "seek rollback",
                path: path.clone(),
                source,
            })?;
        file.flush().map_err(|source| StorageError::Io {
            stage: "flush rollback",
            path: path.clone(),
            source,
        })?;
        file.sync_all().map_err(|source| StorageError::Io {
            stage: "fsync rollback",
            path,
            source,
        })
    }

    fn append_inner(&mut self, frame: &AppendFrame<'_>) -> Result<(), StorageError> {
        let path = self.segment_dir.join(frame.filename);
        let append_fault = self.faults.fail(StorageStage::Append).err();
        let flush_fault = self.faults.fail(StorageStage::Flush).err();
        let fsync_fault = self.faults.fail(StorageStage::JsonFsync).err();
        let file = self.handle(frame.filename)?;
        let length = file.metadata().map_err(|source| StorageError::Io {
            stage: "inspect append offset",
            path: path.clone(),
            source,
        })?;
        if length.len() != frame.previous_offset {
            return Err(StorageError::OffsetMismatch {
                path,
                expected: frame.previous_offset,
                actual: length.len(),
            });
        }
        file.seek(SeekFrom::Start(frame.previous_offset))
            .map_err(|source| StorageError::Io {
                stage: "seek append",
                path: path.clone(),
                source,
            })?;
        if let Some(source) = append_fault {
            let partial = frame.bytes.len().div_ceil(2);
            file.write_all(&frame.bytes[..partial])
                .map_err(|write_source| StorageError::Io {
                    stage: "partial append",
                    path: path.clone(),
                    source: write_source,
                })?;
            return Err(StorageError::Io {
                stage: "append injection",
                path,
                source,
            });
        }
        file.write_all(frame.bytes)
            .map_err(|source| StorageError::Io {
                stage: "append",
                path: path.clone(),
                source,
            })?;
        if let Some(source) = flush_fault {
            return Err(StorageError::Io {
                stage: "flush injection",
                path: path.clone(),
                source,
            });
        }
        file.flush().map_err(|source| StorageError::Io {
            stage: "flush",
            path: path.clone(),
            source,
        })?;
        if let Some(source) = fsync_fault {
            return Err(StorageError::Io {
                stage: "JSONL fsync injection",
                path: path.clone(),
                source,
            });
        }
        file.sync_all().map_err(|source| StorageError::Io {
            stage: "fsync JSONL",
            path: path.clone(),
            source,
        })?;
        sync_directory(&self.segment_dir)
    }
}

pub fn open_regular_readonly(path: &Path) -> Result<File, StorageError> {
    open_regular_readonly_from(rustix::fs::CWD, path, path)
}

pub fn open_regular_readonly_at(
    directory: &File,
    name: &str,
    path: &Path,
) -> Result<File, StorageError> {
    open_regular_readonly_from(directory, Path::new(name), path)
}

fn open_regular_readonly_from(
    directory: impl AsFd,
    target: &Path,
    path: &Path,
) -> Result<File, StorageError> {
    let descriptor = rustix::fs::openat(
        directory,
        target,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC | rustix::fs::OFlags::NOFOLLOW,
        rustix::fs::Mode::empty(),
    )
    .map_err(|source| StorageError::Io {
        stage: "open regular file",
        path: path.to_owned(),
        source: source.into(),
    })?;
    validate_regular_file(File::from(descriptor), path)
}

fn validate_regular_file(file: File, path: &Path) -> Result<File, StorageError> {
    let metadata = file.metadata().map_err(|source| StorageError::Io {
        stage: "inspect regular file",
        path: path.to_owned(),
        source,
    })?;
    if !metadata.is_file() {
        return Err(StorageError::InvalidTarget(path.to_owned()));
    }
    Ok(file)
}

pub fn open_directory_readonly(path: &Path) -> Result<File, StorageError> {
    let descriptor = rustix::fs::open(
        path,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::CLOEXEC
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::DIRECTORY,
        rustix::fs::Mode::empty(),
    )
    .map_err(|source| StorageError::Io {
        stage: "open directory",
        path: path.to_owned(),
        source: source.into(),
    })?;
    let file = File::from(descriptor);
    let metadata = file.metadata().map_err(|source| StorageError::Io {
        stage: "inspect directory",
        path: path.to_owned(),
        source,
    })?;
    if !metadata.is_dir() {
        return Err(StorageError::InvalidTarget(path.to_owned()));
    }
    Ok(file)
}

impl DurableStorage for FileStorage {
    fn append_frame(&mut self, frame: AppendFrame<'_>) -> Result<DurableAppend, StorageError> {
        let previous_metadata =
            fs::read(&self.metadata_path).map_err(|source| StorageError::Io {
                stage: "read previous metadata",
                path: self.metadata_path.clone(),
                source,
            })?;

        if let Err(append_error) = self.append_inner(&frame) {
            return match self.rollback(frame.filename, frame.previous_offset) {
                Ok(()) => Err(StorageError::Recoverable(Box::new(append_error))),
                Err(rollback_error) => Err(StorageError::Poisoned {
                    operation: Box::new(append_error),
                    rollback: Box::new(rollback_error),
                }),
            };
        }

        let metadata_result = self
            .faults
            .fail(StorageStage::Metadata)
            .map_err(|source| StorageError::Io {
                stage: "metadata injection",
                path: self.metadata_path.clone(),
                source,
            })
            .and_then(|()| {
                atomic_write_metadata(
                    &self.metadata_path,
                    &self.stream_dir,
                    &frame.proposed_metadata,
                )
            });
        if let Err(metadata_error) = metadata_result {
            let json_rollback = self.rollback(frame.filename, frame.previous_offset);
            let metadata_rollback =
                atomic_write_bytes(&self.metadata_path, &self.stream_dir, &previous_metadata);
            return match (json_rollback, metadata_rollback) {
                (Ok(()), Ok(())) => Err(StorageError::Recoverable(Box::new(metadata_error))),
                (json, metadata) => Err(StorageError::Poisoned {
                    operation: Box::new(metadata_error),
                    rollback: Box::new(StorageError::RollbackGroup {
                        json: json.err().map(Box::new),
                        metadata: metadata.err().map(Box::new),
                    }),
                }),
            };
        }

        Ok(DurableAppend::accepted(frame.proposed_metadata))
    }

    fn write_metadata(&mut self, metadata: &SegmentMetadata) -> Result<(), StorageError> {
        atomic_write_metadata(&self.metadata_path, &self.stream_dir, metadata)
    }

    fn sync_and_close(&mut self) -> Result<(), StorageError> {
        let handles = std::mem::take(&mut self.handles);
        for (filename, mut file) in handles {
            let path = self.segment_dir.join(filename);
            file.flush().map_err(|source| StorageError::Io {
                stage: "final flush",
                path: path.clone(),
                source,
            })?;
            file.sync_all().map_err(|source| StorageError::Io {
                stage: "final fsync",
                path,
                source,
            })?;
        }
        Ok(())
    }

    fn open_handle_count(&self) -> usize {
        self.handles.len()
    }
}

#[derive(Debug)]
pub enum StorageError {
    Serialize(serde_json::Error),
    InvalidTarget(PathBuf),
    OffsetMismatch {
        path: PathBuf,
        expected: u64,
        actual: u64,
    },
    Io {
        stage: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },
    Recoverable(Box<StorageError>),
    Poisoned {
        operation: Box<StorageError>,
        rollback: Box<StorageError>,
    },
    RollbackGroup {
        json: Option<Box<StorageError>>,
        metadata: Option<Box<StorageError>>,
    },
}

impl StorageError {
    pub fn is_poisoned(&self) -> bool {
        matches!(self, Self::Poisoned { .. })
    }
}

impl fmt::Display for StorageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Serialize(error) => write!(formatter, "metadata serialization failed: {error}"),
            Self::InvalidTarget(path) => {
                write!(formatter, "invalid file target: {}", path.display())
            }
            Self::OffsetMismatch {
                path,
                expected,
                actual,
            } => write!(
                formatter,
                "{} has length {actual}, expected durable offset {expected}",
                path.display()
            ),
            Self::Io {
                stage,
                path,
                source,
            } => write!(formatter, "{stage} failed for {}: {source}", path.display()),
            Self::Recoverable(error) => write!(formatter, "append rolled back: {error}"),
            Self::Poisoned {
                operation,
                rollback,
            } => write!(
                formatter,
                "append failed and rollback failed: operation={operation}; rollback={rollback}"
            ),
            Self::RollbackGroup { json, metadata } => {
                write!(
                    formatter,
                    "rollback group failed (JSONL={}, metadata={})",
                    json.as_deref()
                        .map(ToString::to_string)
                        .unwrap_or_else(|| "ok".to_owned()),
                    metadata
                        .as_deref()
                        .map(ToString::to_string)
                        .unwrap_or_else(|| "ok".to_owned())
                )
            }
        }
    }
}

impl std::error::Error for StorageError {}

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

pub fn atomic_write_metadata(
    path: &Path,
    parent: &Path,
    metadata: &SegmentMetadata,
) -> Result<(), StorageError> {
    atomic_write_bytes(path, parent, &metadata.to_bytes()?)
}

pub fn atomic_write_bytes(path: &Path, parent: &Path, bytes: &[u8]) -> Result<(), StorageError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(StorageError::InvalidTarget(path.to_owned()));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(StorageError::Io {
                stage: "inspect atomic write target",
                path: path.to_owned(),
                source,
            });
        }
    }
    let name = path
        .file_name()
        .ok_or_else(|| StorageError::InvalidTarget(path.to_owned()))?
        .to_string_lossy();
    let temporary = parent.join(format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let descriptor = rustix::fs::open(
            &temporary,
            rustix::fs::OFlags::CREATE
                | rustix::fs::OFlags::EXCL
                | rustix::fs::OFlags::WRONLY
                | rustix::fs::OFlags::CLOEXEC
                | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        )
        .map_err(|source| StorageError::Io {
            stage: "create metadata temporary",
            path: temporary.clone(),
            source: source.into(),
        })?;
        let mut file = File::from(descriptor);
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|source| StorageError::Io {
                stage: "set metadata permissions",
                path: temporary.clone(),
                source,
            })?;
        file.write_all(bytes).map_err(|source| StorageError::Io {
            stage: "write metadata",
            path: temporary.clone(),
            source,
        })?;
        file.flush().map_err(|source| StorageError::Io {
            stage: "flush metadata",
            path: temporary.clone(),
            source,
        })?;
        file.sync_all().map_err(|source| StorageError::Io {
            stage: "fsync metadata",
            path: temporary.clone(),
            source,
        })?;
        fs::rename(&temporary, path).map_err(|source| StorageError::Io {
            stage: "rename metadata",
            path: path.to_owned(),
            source,
        })?;
        sync_directory(parent)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

pub fn sync_directory(path: &Path) -> Result<(), StorageError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| StorageError::Io {
            stage: "fsync directory",
            path: path.to_owned(),
            source,
        })
}
