// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::collections::BTreeSet;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::instance_lock::InstanceLock;
use crate::name::derive_component;
use crate::storage::{MetadataLifecycle, SegmentMetadata, atomic_write_metadata, sync_directory};

#[derive(Clone, Copy, Debug, Default)]
pub struct RecoveryOptions {
    pub fail_source_rename: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecoveryAction {
    Finalized,
    RepairThenFinalize,
    Retain,
    Quarantine,
    Remove,
    Failed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecoveryRecord {
    pub candidate: PathBuf,
    pub action: RecoveryAction,
    pub detail: String,
}

pub fn recover_stream(
    instance_lock: &InstanceLock,
    data_root: &Path,
    stream_dir: &Path,
) -> Result<Vec<RecoveryRecord>, RecoveryError> {
    recover_stream_with_options(
        instance_lock,
        data_root,
        stream_dir,
        RecoveryOptions::default(),
    )
}

pub fn recover_stream_with_options(
    instance_lock: &InstanceLock,
    data_root: &Path,
    stream_dir: &Path,
    options: RecoveryOptions,
) -> Result<Vec<RecoveryRecord>, RecoveryError> {
    let _held_lock = instance_lock.file();
    recover_stream_inner(data_root, stream_dir, options)
}

pub async fn recover_stream_blocking(
    instance_lock: &InstanceLock,
    data_root: &Path,
    stream_dir: &Path,
) -> Result<Vec<RecoveryRecord>, RecoveryError> {
    let _held_lock = instance_lock.file();
    let data_root = data_root.to_owned();
    let stream_dir = stream_dir.to_owned();
    tokio::task::spawn_blocking(move || {
        recover_stream_inner(&data_root, &stream_dir, RecoveryOptions::default())
    })
    .await
    .map_err(|error| RecoveryError::Task(error.to_string()))?
}

fn recover_stream_inner(
    data_root: &Path,
    stream_dir: &Path,
    options: RecoveryOptions,
) -> Result<Vec<RecoveryRecord>, RecoveryError> {
    validate_stream(data_root, stream_dir)?;
    let mut stems = BTreeSet::new();
    for entry in fs::read_dir(stream_dir).map_err(|source| RecoveryError::Io {
        operation: "scan stream directory",
        path: stream_dir.to_owned(),
        source,
    })? {
        let entry = entry.map_err(|source| RecoveryError::Io {
            operation: "read stream entry",
            path: stream_dir.to_owned(),
            source,
        })?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if let Some(stem) = name.strip_suffix(".incomplete") {
            stems.insert(stem.to_owned());
        } else if let Some(stem) = name.strip_suffix(".incomplete.meta") {
            stems.insert(stem.to_owned());
        }
    }

    let mut records = Vec::new();
    for stem in stems {
        records.push(recover_candidate(stream_dir, &stem, options)?);
    }
    Ok(records)
}

fn recover_candidate(
    stream_dir: &Path,
    stem: &str,
    options: RecoveryOptions,
) -> Result<RecoveryRecord, RecoveryError> {
    if derive_component(stem).map(|name| name.as_str() == stem) != Ok(true) {
        return Ok(record(
            stream_dir.join(format!("{stem}.incomplete")),
            RecoveryAction::Failed,
            "candidate name is not a canonical direct entry",
        ));
    }
    let source = stream_dir.join(format!("{stem}.incomplete"));
    let metadata_path = stream_dir.join(format!("{stem}.incomplete.meta"));
    reject_special_target(&source)?;
    reject_special_target(&metadata_path)?;

    let metadata_bytes = match fs::read(&metadata_path) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(source_error) => {
            return Err(RecoveryError::Io {
                operation: "read segment metadata",
                path: metadata_path,
                source: source_error,
            });
        }
    };
    let source_exists = source.is_dir();

    let Some(metadata_bytes) = metadata_bytes else {
        let nonempty = source_has_nonempty_jsonl(&source)?;
        return Ok(record(
            source,
            RecoveryAction::Retain,
            if nonempty {
                "missing metadata with non-empty JSONL"
            } else {
                "missing metadata cannot prove an empty segment"
            },
        ));
    };
    let metadata = serde_json::from_slice::<SegmentMetadata>(&metadata_bytes)
        .ok()
        .filter(|metadata| metadata_is_consistent(metadata, stem));
    let Some(mut metadata) = metadata else {
        return quarantine(
            stream_dir,
            &source,
            &metadata_path,
            "metadata is torn or contradictory",
        );
    };

    if !source_exists {
        if metadata.lifecycle == MetadataLifecycle::Creating
            || (metadata.durable_frame_count == 0
                && metadata.last_durable_frame_id == 0
                && !metadata.has_durable_frames
                && metadata
                    .sessions
                    .values()
                    .all(|session| session.durable_offset == 0))
        {
            fs::remove_file(&metadata_path).map_err(|source_error| RecoveryError::Io {
                operation: "remove orphan creating metadata",
                path: metadata_path.clone(),
                source: source_error,
            })?;
            sync_directory(stream_dir)?;
            return Ok(record(
                source,
                RecoveryAction::Remove,
                "removed orphan creating metadata",
            ));
        }
        let finalized = stream_dir.join(&metadata.finalized_dir);
        if finalized.is_dir()
            && validate_files(&finalized, &metadata)?.kind == ValidationKind::Exact
        {
            fs::remove_file(&metadata_path).map_err(|source_error| RecoveryError::Io {
                operation: "remove orphan finalized metadata",
                path: metadata_path.clone(),
                source: source_error,
            })?;
            sync_directory(stream_dir)?;
            return Ok(record(
                finalized,
                RecoveryAction::Remove,
                "removed metadata left after finalized rename",
            ));
        }
        return Ok(record(
            source,
            RecoveryAction::Retain,
            "metadata has no matching source or validated finalized target",
        ));
    }

    let finalized = stream_dir.join(&metadata.finalized_dir);
    if finalized.exists() {
        return Ok(record(
            source,
            RecoveryAction::Failed,
            "finalized target collision; source and metadata retained",
        ));
    }
    if options.fail_source_rename {
        return Ok(record(
            source,
            RecoveryAction::Failed,
            "injected source rename failure; source and metadata retained",
        ));
    }

    if metadata.durable_frame_count == 0
        && metadata.last_durable_frame_id == 0
        && !metadata.has_durable_frames
        && metadata
            .sessions
            .values()
            .all(|session| session.durable_offset == 0)
    {
        if directory_is_confirmed_empty(&source)? {
            remove_zero_length_files(&source)?;
            fs::remove_dir(&source).map_err(|source_error| RecoveryError::Io {
                operation: "remove confirmed-empty segment",
                path: source.clone(),
                source: source_error,
            })?;
            fs::remove_file(&metadata_path).map_err(|source_error| RecoveryError::Io {
                operation: "remove confirmed-empty metadata",
                path: metadata_path.clone(),
                source: source_error,
            })?;
            sync_directory(stream_dir)?;
            return Ok(record(
                source,
                RecoveryAction::Remove,
                "valid metadata positively confirmed an empty segment",
            ));
        }
        return quarantine(
            stream_dir,
            &source,
            &metadata_path,
            "empty metadata contradicts JSONL bytes",
        );
    }

    let validation = validate_files(&source, &metadata)?;
    if validation.kind == ValidationKind::Contradictory {
        return quarantine(stream_dir, &source, &metadata_path, &validation.detail);
    }
    let repaired = validation.kind == ValidationKind::Repair;
    if repaired {
        apply_repairs(&source, &validation.repairs)?;
        if let Some(updated) = validation.updated_metadata {
            metadata = updated;
        }
    }
    metadata.lifecycle = MetadataLifecycle::Finalizing;
    let original_metadata = metadata_bytes;
    atomic_write_metadata(&metadata_path, stream_dir, &metadata)?;
    if let Err(source_error) = fs::rename(&source, &finalized) {
        atomic_write_bytes_for_recovery(&metadata_path, stream_dir, &original_metadata)?;
        return Ok(record(
            source,
            RecoveryAction::Failed,
            &format!("source rename failed and was retained: {source_error}"),
        ));
    }
    sync_directory(stream_dir)?;
    fs::remove_file(&metadata_path).map_err(|source_error| RecoveryError::Io {
        operation: "remove recovered metadata",
        path: metadata_path,
        source: source_error,
    })?;
    sync_directory(stream_dir)?;
    Ok(record(
        finalized,
        if repaired {
            RecoveryAction::RepairThenFinalize
        } else {
            RecoveryAction::Finalized
        },
        if repaired {
            "repaired only a torn final line and finalized"
        } else {
            "validated durable bytes and finalized"
        },
    ))
}

fn validate_stream(data_root: &Path, stream_dir: &Path) -> Result<(), RecoveryError> {
    let data_root = data_root
        .canonicalize()
        .map_err(|source| RecoveryError::Io {
            operation: "canonicalize data root",
            path: data_root.to_owned(),
            source,
        })?;
    let metadata = fs::symlink_metadata(stream_dir).map_err(|source| RecoveryError::Io {
        operation: "inspect stream directory",
        path: stream_dir.to_owned(),
        source,
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(RecoveryError::EscapesDataRoot(stream_dir.to_owned()));
    }
    let stream = stream_dir
        .canonicalize()
        .map_err(|source| RecoveryError::Io {
            operation: "canonicalize stream directory",
            path: stream_dir.to_owned(),
            source,
        })?;
    if !stream.starts_with(&data_root) || stream == data_root {
        return Err(RecoveryError::EscapesDataRoot(stream));
    }
    Ok(())
}

fn reject_special_target(path: &Path) -> Result<(), RecoveryError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(RecoveryError::SpecialTarget(path.to_owned()))
        }
        Ok(metadata)
            if path.extension().and_then(|value| value.to_str()) == Some("meta")
                && !metadata.is_file() =>
        {
            Err(RecoveryError::SpecialTarget(path.to_owned()))
        }
        Ok(metadata)
            if path.extension().and_then(|value| value.to_str()) != Some("meta")
                && !metadata.is_dir() =>
        {
            Err(RecoveryError::SpecialTarget(path.to_owned()))
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(RecoveryError::Io {
            operation: "inspect recovery target",
            path: path.to_owned(),
            source,
        }),
    }
}

fn metadata_is_consistent(metadata: &SegmentMetadata, stem: &str) -> bool {
    if metadata.schema_version != SegmentMetadata::SCHEMA_VERSION
        || metadata.incomplete_dir != format!("{stem}.incomplete")
        || !is_direct_name(&metadata.incomplete_dir)
        || !is_direct_name(&metadata.finalized_dir)
        || metadata.has_durable_frames != (metadata.durable_frame_count > 0)
    {
        return false;
    }
    let mut filenames = BTreeSet::new();
    metadata.sessions.iter().all(|(identity, session)| {
        identity == &session.session
            && derive_component(identity)
                .map(|name| name.session_filename() == session.filename)
                .unwrap_or(false)
            && filenames.insert(session.filename.clone())
    })
}

fn is_direct_name(name: &str) -> bool {
    !name.is_empty()
        && Path::new(name).file_name().and_then(|value| value.to_str()) == Some(name)
        && !name.contains('/')
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ValidationKind {
    Exact,
    Repair,
    Contradictory,
}

struct Validation {
    kind: ValidationKind,
    detail: String,
    repairs: Vec<FileRepair>,
    updated_metadata: Option<SegmentMetadata>,
}

struct FileRepair {
    path: PathBuf,
    offset: u64,
}

fn validate_files(
    directory: &Path,
    metadata: &SegmentMetadata,
) -> Result<Validation, RecoveryError> {
    let expected = metadata
        .sessions
        .values()
        .map(|session| session.filename.as_str())
        .collect::<BTreeSet<_>>();
    for entry in fs::read_dir(directory).map_err(|source| RecoveryError::Io {
        operation: "scan incomplete segment",
        path: directory.to_owned(),
        source,
    })? {
        let entry = entry.map_err(|source| RecoveryError::Io {
            operation: "read incomplete segment entry",
            path: directory.to_owned(),
            source,
        })?;
        let kind = entry.file_type().map_err(|source| RecoveryError::Io {
            operation: "inspect incomplete segment entry",
            path: entry.path(),
            source,
        })?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            return Ok(contradiction("non-UTF-8 filename in segment"));
        };
        if kind.is_symlink() || !kind.is_file() || !expected.contains(name) {
            return Ok(contradiction("unknown or non-regular file in segment"));
        }
    }

    let mut updated = metadata.clone();
    let mut repairs = Vec::new();
    let mut all_ids = Vec::new();
    for session in metadata.sessions.values() {
        let path = directory.join(&session.filename);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(contradiction("referenced JSONL file is missing"));
            }
            Err(source) => {
                return Err(RecoveryError::Io {
                    operation: "read JSONL during recovery",
                    path,
                    source,
                });
            }
        };
        if bytes.len() < session.durable_offset as usize {
            return Ok(contradiction("JSONL is shorter than its durable offset"));
        }
        let recorded = session.durable_offset as usize;
        let prefix = &bytes[..recorded];
        let (valid_offset, mut ids, prefix_torn) = parse_recorded_prefix(prefix);
        if prefix_torn {
            repairs.push(FileRepair {
                path: path.clone(),
                offset: valid_offset as u64,
            });
            let entry = updated
                .sessions
                .get_mut(&session.session)
                .expect("metadata session exists");
            entry.durable_offset = valid_offset as u64;
            entry.last_frame_id = ids.last().copied().unwrap_or(0);
        } else if valid_offset != prefix.len() {
            return Ok(contradiction("earlier JSONL corruption"));
        }
        if !prefix_torn && ids.last().copied().unwrap_or(0) != session.last_frame_id {
            return Ok(contradiction("session frame IDs disagree with metadata"));
        }
        if bytes.len() > recorded {
            let suffix = &bytes[recorded..];
            if suffix.contains(&b'\n') {
                return Ok(contradiction(
                    "complete or multiline bytes exist beyond the durable offset",
                ));
            }
            repairs.push(FileRepair {
                path: path.clone(),
                offset: recorded as u64,
            });
        }
        all_ids.append(&mut ids);
    }
    all_ids.sort_unstable();
    let expected_count = all_ids.len() as u64;
    let expected_last = all_ids.last().copied().unwrap_or(0);
    if all_ids
        .iter()
        .enumerate()
        .any(|(index, frame_id)| *frame_id != index as u64 + 1)
    {
        return Ok(contradiction(
            "global frame IDs are duplicated or nonconsecutive",
        ));
    }
    if repairs.is_empty()
        && (metadata.durable_frame_count != expected_count
            || metadata.last_durable_frame_id != expected_last)
    {
        return Ok(contradiction("global frame IDs disagree with metadata"));
    }
    if !repairs.is_empty() {
        updated.durable_frame_count = expected_count;
        updated.last_durable_frame_id = expected_last;
        updated.has_durable_frames = expected_count > 0;
    }
    Ok(Validation {
        kind: if repairs.is_empty() {
            ValidationKind::Exact
        } else {
            ValidationKind::Repair
        },
        detail: String::new(),
        repairs,
        updated_metadata: Some(updated),
    })
}

fn parse_recorded_prefix(bytes: &[u8]) -> (usize, Vec<u64>, bool) {
    let mut offset = 0;
    let mut ids = Vec::new();
    for line in bytes.split_inclusive(|byte| *byte == b'\n') {
        if !line.ends_with(b"\n") {
            return (offset, ids, true);
        }
        let Ok(value) = serde_json::from_slice::<Value>(&line[..line.len() - 1]) else {
            return (offset, ids, false);
        };
        let Some(frame_id) = value.get("frame_id").and_then(Value::as_u64) else {
            return (offset, ids, false);
        };
        if ids.last().is_some_and(|previous| *previous >= frame_id) {
            return (offset, ids, false);
        }
        ids.push(frame_id);
        offset += line.len();
    }
    (offset, ids, false)
}

fn contradiction(detail: &str) -> Validation {
    Validation {
        kind: ValidationKind::Contradictory,
        detail: detail.to_owned(),
        repairs: Vec::new(),
        updated_metadata: None,
    }
}

fn apply_repairs(directory: &Path, repairs: &[FileRepair]) -> Result<(), RecoveryError> {
    for repair in repairs {
        if repair.path.parent() != Some(directory) {
            return Err(RecoveryError::EscapesDataRoot(repair.path.clone()));
        }
        let file = OpenOptions::new()
            .write(true)
            .open(&repair.path)
            .map_err(|source| RecoveryError::Io {
                operation: "open JSONL repair",
                path: repair.path.clone(),
                source,
            })?;
        file.set_len(repair.offset)
            .and_then(|()| file.sync_all())
            .map_err(|source| RecoveryError::Io {
                operation: "truncate and fsync JSONL repair",
                path: repair.path.clone(),
                source,
            })?;
    }
    Ok(())
}

fn directory_is_confirmed_empty(directory: &Path) -> Result<bool, RecoveryError> {
    for entry in fs::read_dir(directory).map_err(|source| RecoveryError::Io {
        operation: "scan empty segment",
        path: directory.to_owned(),
        source,
    })? {
        let entry = entry.map_err(|source| RecoveryError::Io {
            operation: "read empty segment entry",
            path: directory.to_owned(),
            source,
        })?;
        let metadata = entry.metadata().map_err(|source| RecoveryError::Io {
            operation: "inspect empty segment entry",
            path: entry.path(),
            source,
        })?;
        if !metadata.is_file() || metadata.len() != 0 {
            return Ok(false);
        }
    }
    Ok(true)
}

fn remove_zero_length_files(directory: &Path) -> Result<(), RecoveryError> {
    for entry in fs::read_dir(directory).map_err(|source| RecoveryError::Io {
        operation: "scan zero-length files",
        path: directory.to_owned(),
        source,
    })? {
        let entry = entry.map_err(|source| RecoveryError::Io {
            operation: "read zero-length entry",
            path: directory.to_owned(),
            source,
        })?;
        fs::remove_file(entry.path()).map_err(|source| RecoveryError::Io {
            operation: "remove zero-length JSONL",
            path: entry.path(),
            source,
        })?;
    }
    Ok(())
}

fn source_has_nonempty_jsonl(source: &Path) -> Result<bool, RecoveryError> {
    if !source.is_dir() {
        return Ok(false);
    }
    for entry in fs::read_dir(source).map_err(|source_error| RecoveryError::Io {
        operation: "scan metadata-less segment",
        path: source.to_owned(),
        source: source_error,
    })? {
        let entry = entry.map_err(|source_error| RecoveryError::Io {
            operation: "read metadata-less segment entry",
            path: source.to_owned(),
            source: source_error,
        })?;
        if entry
            .metadata()
            .map(|metadata| metadata.len() > 0)
            .unwrap_or(true)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn quarantine(
    stream_dir: &Path,
    source: &Path,
    metadata: &Path,
    detail: &str,
) -> Result<RecoveryRecord, RecoveryError> {
    let stem = source
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_suffix(".incomplete"))
        .unwrap_or("segment");
    let mut number = 0_u64;
    let (failed, failed_metadata) = loop {
        let suffix = if number == 0 {
            String::new()
        } else {
            format!(".{number}")
        };
        let failed = stream_dir.join(format!("{stem}{suffix}.failed"));
        let failed_metadata = stream_dir.join(format!("{stem}{suffix}.failed.meta"));
        if !failed.exists() && !failed_metadata.exists() {
            break (failed, failed_metadata);
        }
        number += 1;
    };
    let source_exists = source.exists();
    let metadata_exists = metadata.exists();
    if source_exists {
        fs::rename(source, &failed).map_err(|source_error| RecoveryError::Io {
            operation: "quarantine segment",
            path: source.to_owned(),
            source: source_error,
        })?;
    }
    if metadata_exists && let Err(source_error) = fs::rename(metadata, &failed_metadata) {
        if source_exists {
            let _ = fs::rename(&failed, source);
        }
        return Err(RecoveryError::Io {
            operation: "quarantine metadata",
            path: metadata.to_owned(),
            source: source_error,
        });
    }
    sync_directory(stream_dir)?;
    Ok(record(failed, RecoveryAction::Quarantine, detail))
}

fn atomic_write_bytes_for_recovery(
    path: &Path,
    parent: &Path,
    bytes: &[u8],
) -> Result<(), RecoveryError> {
    crate::storage::atomic_write_bytes(path, parent, bytes).map_err(RecoveryError::Storage)
}

fn record(candidate: PathBuf, action: RecoveryAction, detail: &str) -> RecoveryRecord {
    RecoveryRecord {
        candidate,
        action,
        detail: detail.to_owned(),
    }
}

#[derive(Debug)]
pub enum RecoveryError {
    Storage(crate::storage::StorageError),
    Task(String),
    EscapesDataRoot(PathBuf),
    SpecialTarget(PathBuf),
    Io {
        operation: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },
}

impl From<crate::storage::StorageError> for RecoveryError {
    fn from(error: crate::storage::StorageError) -> Self {
        Self::Storage(error)
    }
}

impl fmt::Display for RecoveryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Storage(error) => error.fmt(formatter),
            Self::Task(error) => write!(formatter, "blocking recovery task failed: {error}"),
            Self::EscapesDataRoot(path) => write!(
                formatter,
                "recovery candidate escapes the configured data root: {}",
                path.display()
            ),
            Self::SpecialTarget(path) => write!(
                formatter,
                "recovery candidate is a symlink or wrong file type: {}",
                path.display()
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

impl std::error::Error for RecoveryError {}
