// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::Duration;

use time::{OffsetDateTime, UtcOffset};

use crate::model::CaptureResult;
use crate::name::derive_component;
use crate::paths::ensure_private_directory;
use crate::serialize::serialize_frame;
use crate::storage::{
    AppendFrame, DurableStorage, FaultPlan, FileStorage, MetadataLifecycle, SegmentMetadata,
    SessionMetadata, StorageError, atomic_write_metadata, sync_directory,
};

pub struct SegmentState {
    stream_dir: PathBuf,
    incomplete_dir: PathBuf,
    metadata_path: PathBuf,
    metadata: SegmentMetadata,
    storage: Box<dyn DurableStorage>,
    digests: HashMap<String, u64>,
    start_monotonic: Duration,
    poisoned: bool,
    closed: Option<SegmentClose>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AppendOutcome {
    Appended { frame_id: u64 },
    Unchanged,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SegmentClose {
    Finalized(PathBuf),
    RemovedEmpty,
}

impl SegmentState {
    pub fn create(
        stream_dir: &Path,
        start_wall: OffsetDateTime,
        start_monotonic: Duration,
        local_offset: UtcOffset,
    ) -> Result<Self, SegmentError> {
        Self::create_with_faults(
            stream_dir,
            start_wall,
            start_monotonic,
            local_offset,
            FaultPlan::default(),
        )
    }

    pub fn create_with_faults(
        stream_dir: &Path,
        start_wall: OffsetDateTime,
        start_monotonic: Duration,
        local_offset: UtcOffset,
        faults: FaultPlan,
    ) -> Result<Self, SegmentError> {
        ensure_private_directory(stream_dir).map_err(SegmentError::Path)?;
        let local = start_wall.to_offset(local_offset);
        let stem = format!(
            "{:02}{:02}{:02}",
            local.hour(),
            local.minute(),
            local.second()
        );
        let incomplete_name = format!("{stem}.incomplete");
        let metadata_name = format!("{stem}.incomplete.meta");
        let incomplete_dir = stream_dir.join(&incomplete_name);
        let metadata_path = stream_dir.join(metadata_name);
        if fs::symlink_metadata(&incomplete_dir).is_ok()
            || fs::symlink_metadata(&metadata_path).is_ok()
        {
            return Err(SegmentError::Collision(incomplete_dir));
        }
        let mut metadata = SegmentMetadata {
            schema_version: SegmentMetadata::SCHEMA_VERSION,
            lifecycle: MetadataLifecycle::Creating,
            incomplete_dir: incomplete_name,
            finalized_dir: finalized_name(&stem, Duration::ZERO),
            start_wall_unix_nanos: start_wall.unix_timestamp_nanos(),
            local_offset_seconds: local_offset.whole_seconds(),
            elapsed_nanos: 0,
            last_durable_frame_id: 0,
            durable_frame_count: 0,
            has_durable_frames: false,
            sessions: Default::default(),
        };
        atomic_write_metadata(&metadata_path, stream_dir, &metadata)?;
        ensure_private_directory(&incomplete_dir).map_err(SegmentError::Path)?;
        sync_directory(stream_dir)?;
        metadata.lifecycle = MetadataLifecycle::Open;
        atomic_write_metadata(&metadata_path, stream_dir, &metadata)?;
        let storage = FileStorage::with_faults(
            incomplete_dir.clone(),
            metadata_path.clone(),
            stream_dir.to_owned(),
            faults,
        );
        Ok(Self {
            stream_dir: stream_dir.to_owned(),
            incomplete_dir,
            metadata_path,
            metadata,
            storage: Box::new(storage),
            digests: HashMap::new(),
            start_monotonic,
            poisoned: false,
            closed: None,
        })
    }

    pub fn append_capture(
        &mut self,
        capture: &CaptureResult,
        timestamp: f64,
        now_monotonic: Duration,
    ) -> Result<AppendOutcome, SegmentError> {
        if self.poisoned {
            return Err(SegmentError::Poisoned);
        }
        if self.closed.is_some() {
            return Err(SegmentError::Closed);
        }
        let digest = capture_digest(capture);
        if self.digests.get(&capture.session) == Some(&digest) {
            return Ok(AppendOutcome::Unchanged);
        }
        let component = derive_component(&capture.session).map_err(SegmentError::Name)?;
        let filename = component.session_filename();
        let previous_offset = self
            .metadata
            .sessions
            .get(&capture.session)
            .map_or(0, |session| session.durable_offset);
        let frame_id = self
            .metadata
            .last_durable_frame_id
            .checked_add(1)
            .ok_or(SegmentError::FrameIdOverflow)?;
        let bytes =
            serialize_frame(capture, frame_id, timestamp).map_err(SegmentError::Serialize)?;
        let mut proposed = self.metadata.clone();
        let elapsed = now_monotonic.saturating_sub(self.start_monotonic);
        proposed.elapsed_nanos = duration_nanos(elapsed);
        proposed.finalized_dir = finalized_name(segment_stem(&proposed.incomplete_dir), elapsed);
        proposed.last_durable_frame_id = frame_id;
        proposed.durable_frame_count += 1;
        proposed.has_durable_frames = true;
        proposed.sessions.insert(
            capture.session.clone(),
            SessionMetadata {
                session: capture.session.clone(),
                filename: filename.clone(),
                last_frame_id: frame_id,
                durable_offset: previous_offset + bytes.len() as u64,
            },
        );
        let durable = self.storage.append_frame(AppendFrame {
            filename: &filename,
            previous_offset,
            bytes: &bytes,
            proposed_metadata: proposed,
        });
        let durable = match durable {
            Ok(durable) => durable,
            Err(error) => {
                if error.is_poisoned() {
                    self.poisoned = true;
                }
                return Err(SegmentError::Storage(error));
            }
        };

        self.metadata = durable.into_metadata();
        self.digests.insert(capture.session.clone(), digest);
        Ok(AppendOutcome::Appended { frame_id })
    }

    pub fn rotation_due(&self, now_monotonic: Duration, interval: Duration) -> bool {
        now_monotonic.saturating_sub(self.start_monotonic) >= interval
    }

    pub fn frame_timestamp(&self, wall_now: OffsetDateTime) -> f64 {
        let start = OffsetDateTime::from_unix_timestamp_nanos(self.metadata.start_wall_unix_nanos)
            .expect("stored segment wall timestamp was previously valid");
        (wall_now - start).as_seconds_f64()
    }

    pub fn finalize(&mut self, now_monotonic: Duration) -> Result<SegmentClose, SegmentError> {
        if let Some(closed) = &self.closed {
            return Ok(closed.clone());
        }
        if self.poisoned {
            return Err(SegmentError::Poisoned);
        }
        if !self.metadata.has_durable_frames {
            return self.remove_confirmed_empty();
        }

        let elapsed = now_monotonic.saturating_sub(self.start_monotonic);
        self.metadata.elapsed_nanos = duration_nanos(elapsed);
        self.metadata.finalized_dir =
            finalized_name(segment_stem(&self.metadata.incomplete_dir), elapsed);
        self.metadata.lifecycle = MetadataLifecycle::Finalizing;
        self.storage.write_metadata(&self.metadata)?;
        self.storage.sync_and_close()?;

        let finalized = self.stream_dir.join(&self.metadata.finalized_dir);
        if finalized.exists() {
            return Err(SegmentError::Collision(finalized));
        }
        fs::rename(&self.incomplete_dir, &finalized).map_err(|source| SegmentError::Io {
            operation: "rename finalized segment",
            path: self.incomplete_dir.clone(),
            source,
        })?;
        sync_directory(&self.stream_dir)?;
        fs::remove_file(&self.metadata_path).map_err(|source| SegmentError::Io {
            operation: "remove finalized metadata",
            path: self.metadata_path.clone(),
            source,
        })?;
        sync_directory(&self.stream_dir)?;
        let closed = SegmentClose::Finalized(finalized);
        self.closed = Some(closed.clone());
        Ok(closed)
    }

    pub fn remove_confirmed_empty(&mut self) -> Result<SegmentClose, SegmentError> {
        if let Some(closed) = &self.closed {
            return Ok(closed.clone());
        }
        if self.metadata.has_durable_frames
            || self.metadata.durable_frame_count != 0
            || self
                .metadata
                .sessions
                .values()
                .any(|session| session.durable_offset != 0)
        {
            return Err(SegmentError::NotEmpty);
        }
        self.storage.sync_and_close()?;
        for entry in fs::read_dir(&self.incomplete_dir).map_err(|source| SegmentError::Io {
            operation: "scan confirmed-empty segment",
            path: self.incomplete_dir.clone(),
            source,
        })? {
            let entry = entry.map_err(|source| SegmentError::Io {
                operation: "read confirmed-empty entry",
                path: self.incomplete_dir.clone(),
                source,
            })?;
            let file_type = entry.file_type().map_err(|source| SegmentError::Io {
                operation: "inspect confirmed-empty entry",
                path: entry.path(),
                source,
            })?;
            let length = entry
                .metadata()
                .map_err(|source| SegmentError::Io {
                    operation: "inspect confirmed-empty file",
                    path: entry.path(),
                    source,
                })?
                .len();
            if file_type.is_symlink() || !file_type.is_file() || length != 0 {
                return Err(SegmentError::NotEmpty);
            }
            fs::remove_file(entry.path()).map_err(|source| SegmentError::Io {
                operation: "remove zero-length JSONL",
                path: entry.path(),
                source,
            })?;
        }
        fs::remove_dir(&self.incomplete_dir).map_err(|source| SegmentError::Io {
            operation: "remove confirmed-empty segment",
            path: self.incomplete_dir.clone(),
            source,
        })?;
        sync_directory(&self.stream_dir)?;
        fs::remove_file(&self.metadata_path).map_err(|source| SegmentError::Io {
            operation: "remove confirmed-empty metadata",
            path: self.metadata_path.clone(),
            source,
        })?;
        sync_directory(&self.stream_dir)?;
        self.closed = Some(SegmentClose::RemovedEmpty);
        Ok(SegmentClose::RemovedEmpty)
    }

    pub fn metadata(&self) -> &SegmentMetadata {
        &self.metadata
    }

    pub fn incomplete_dir(&self) -> &Path {
        &self.incomplete_dir
    }

    pub fn metadata_path(&self) -> &Path {
        &self.metadata_path
    }

    pub fn stream_dir(&self) -> &Path {
        &self.stream_dir
    }

    pub fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    pub fn open_handle_count(&self) -> usize {
        self.storage.open_handle_count()
    }

    pub fn has_durable_frames(&self) -> bool {
        self.metadata.has_durable_frames
    }

    pub fn start_monotonic(&self) -> Duration {
        self.start_monotonic
    }
}

pub fn capture_digest(capture: &CaptureResult) -> u64 {
    let mut panes = capture.panes.iter().collect::<Vec<_>>();
    panes.sort_by(|left, right| left.id.cmp(&right.id));
    let mut parts = Vec::with_capacity(panes.len() + 1);
    parts.push(capture.window.id.as_str());
    parts.extend(panes.into_iter().map(|pane| pane.content.as_str()));
    let mut hasher = DefaultHasher::new();
    parts.join("\n").hash(&mut hasher);
    hasher.finish()
}

fn duration_nanos(duration: Duration) -> u64 {
    duration.as_nanos().min(u128::from(u64::MAX)) as u64
}

fn segment_stem(incomplete_name: &str) -> &str {
    incomplete_name
        .strip_suffix(".incomplete")
        .expect("segment metadata has an incomplete directory name")
}

fn finalized_name(stem: &str, elapsed: Duration) -> String {
    format!("{stem}_{:03}", elapsed.as_secs())
}

#[derive(Debug)]
pub enum SegmentError {
    Path(crate::paths::PathError),
    Name(crate::name::NameError),
    Serialize(serde_json::Error),
    Storage(StorageError),
    Collision(PathBuf),
    Io {
        operation: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },
    Poisoned,
    Closed,
    NotEmpty,
    FrameIdOverflow,
}

impl From<StorageError> for SegmentError {
    fn from(error: StorageError) -> Self {
        Self::Storage(error)
    }
}

impl SegmentError {
    pub fn is_recoverable_append(&self) -> bool {
        matches!(self, Self::Storage(StorageError::Recoverable(_)))
    }
}

impl fmt::Display for SegmentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Path(error) => error.fmt(formatter),
            Self::Name(error) => error.fmt(formatter),
            Self::Serialize(error) => write!(formatter, "frame serialization failed: {error}"),
            Self::Storage(error) => error.fmt(formatter),
            Self::Collision(path) => {
                write!(formatter, "segment path already exists: {}", path.display())
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
            Self::Poisoned => write!(formatter, "segment is poisoned after a failed rollback"),
            Self::Closed => write!(formatter, "segment is already closed"),
            Self::NotEmpty => write!(formatter, "segment is not metadata-confirmed empty"),
            Self::FrameIdOverflow => write!(formatter, "segment frame_id overflow"),
        }
    }
}

impl std::error::Error for SegmentError {}
