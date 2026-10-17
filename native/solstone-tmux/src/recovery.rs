// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::collections::BTreeSet;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;
use time::{Date, Month, OffsetDateTime, UtcOffset};

use crate::instance_lock::InstanceLock;
use crate::name::derive_component;
use crate::segment::{finalized_name, record_length};
use crate::storage::{
    CaptureTime, MetadataLifecycle, SegmentMetadata, atomic_write_metadata, sync_directory,
    write_capture_time,
};

#[derive(Clone, Copy, Debug, Default)]
pub struct RecoveryOptions {
    pub fail_source_rename: bool,
    pub fail_repair_sync: bool,
    pub fail_rename_parent_sync: bool,
    pub before_empty_removal: Option<fn(&Path) -> std::io::Result<()>>,
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
    segment_interval: Duration,
) -> Result<Vec<RecoveryRecord>, RecoveryError> {
    recover_stream_with_options(
        instance_lock,
        data_root,
        stream_dir,
        segment_interval,
        RecoveryOptions::default(),
    )
}

pub fn recover_stream_with_options(
    instance_lock: &InstanceLock,
    data_root: &Path,
    stream_dir: &Path,
    segment_interval: Duration,
    options: RecoveryOptions,
) -> Result<Vec<RecoveryRecord>, RecoveryError> {
    let _held_lock = instance_lock.file();
    recover_stream_inner(data_root, stream_dir, segment_interval, options)
}

/// Recovers every stream directory under each capture date, not only the
/// configured stream: a default stream follows the hostname, so a renamed
/// machine can leave an interrupted segment under the previous stream name.
pub fn recover_capture_streams(
    instance_lock: &InstanceLock,
    data_root: &Path,
    segment_interval: Duration,
) -> Result<Vec<RecoveryRecord>, RecoveryError> {
    let _held_lock = instance_lock.file();
    let captures = data_root.join("captures");
    match fs::symlink_metadata(&captures) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(RecoveryError::SpecialTarget(captures));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(RecoveryError::Io {
                operation:
                    crate::i18n::MessageId::CliRecoveryRecoverCaptureStreamsInspectCapturesDirectory
                        .text(),
                path: captures,
                source,
            });
        }
    }
    let entries = match fs::read_dir(&captures) {
        Ok(entries) => entries,
        Err(source) => {
            return Err(RecoveryError::Io {
                operation: crate::i18n::MessageId::CliRecoveryRecoverCaptureStreamsScanCaptureDates
                    .text(),
                path: captures,
                source,
            });
        }
    };
    let mut stream_directories = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| RecoveryError::Io {
            operation: crate::i18n::MessageId::CliRecoveryRecoverCaptureStreamsReadCaptureDateEntry
                .text(),
            path: captures.clone(),
            source,
        })?;
        let Some(date) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !valid_date_name(&date) {
            continue;
        }
        let file_type = entry.file_type().map_err(|source| RecoveryError::Io {
            operation:
                crate::i18n::MessageId::CliRecoveryRecoverCaptureStreamsInspectCaptureDateEntry
                    .text(),
            path: entry.path(),
            source,
        })?;
        if file_type.is_symlink() || !file_type.is_dir() {
            return Err(RecoveryError::SpecialTarget(entry.path()));
        }
        let date_dir = entry.path();
        let stream_entries = fs::read_dir(&date_dir).map_err(|source| RecoveryError::Io {
            operation: crate::i18n::MessageId::CliRecoveryRecoverCaptureStreamsScanCaptureStreams
                .text(),
            path: date_dir.clone(),
            source,
        })?;
        for stream_entry in stream_entries {
            let stream_entry = stream_entry.map_err(|source| RecoveryError::Io {
                operation:
                    crate::i18n::MessageId::CliRecoveryRecoverCaptureStreamsReadCaptureStreamEntry
                        .text(),
                path: date_dir.clone(),
                source,
            })?;
            let Some(name) = stream_entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Ok(stream) = derive_component(&name) else {
                continue;
            };
            if stream_entry.file_type().is_ok_and(|kind| kind.is_file()) {
                continue;
            }
            let stream_dir = stream
                .join_checked(&date_dir)
                .map_err(|_| RecoveryError::EscapesDataRoot(date_dir.clone()))?;
            stream_directories.push(stream_dir);
        }
    }
    stream_directories.sort();
    let mut records = Vec::new();
    for stream_dir in stream_directories {
        records.extend(recover_stream_inner(
            data_root,
            &stream_dir,
            segment_interval,
            RecoveryOptions::default(),
        )?);
    }
    Ok(records)
}

fn recover_stream_inner(
    data_root: &Path,
    stream_dir: &Path,
    segment_interval: Duration,
    options: RecoveryOptions,
) -> Result<Vec<RecoveryRecord>, RecoveryError> {
    validate_stream(data_root, stream_dir)?;
    let mut stems = BTreeSet::new();
    for entry in fs::read_dir(stream_dir).map_err(|source| RecoveryError::Io {
        operation: crate::i18n::MessageId::CliRecoveryRecoverStreamInnerScanStreamDirectory.text(),
        path: stream_dir.to_owned(),
        source,
    })? {
        let entry = entry.map_err(|source| RecoveryError::Io {
            operation: crate::i18n::MessageId::CliRecoveryRecoverStreamInnerReadStreamEntry.text(),
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
        records.push(recover_candidate(
            stream_dir,
            &stem,
            segment_interval,
            options,
        )?);
    }
    Ok(records)
}

fn recover_candidate(
    stream_dir: &Path,
    stem: &str,
    segment_interval: Duration,
    options: RecoveryOptions,
) -> Result<RecoveryRecord, RecoveryError> {
    let source = stream_dir.join(format!("{stem}.incomplete"));
    let metadata_path = stream_dir.join(format!("{stem}.incomplete.meta"));
    reject_special_target(&source)?;
    reject_special_target(&metadata_path)?;
    if derive_component(stem).map(|name| name.as_str() == stem) != Ok(true)
        || !valid_time_name(stem)
    {
        return quarantine(
            stream_dir,
            &source,
            &metadata_path,
            crate::i18n::MessageId::CliRecoveryRecoverCandidateCandidateStemIsNotAWellFormedHhmmss
                .text(),
        );
    }

    let metadata_bytes = match fs::read(&metadata_path) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(source_error) => {
            return Err(RecoveryError::Io {
                operation: crate::i18n::MessageId::CliRecoveryRecoverCandidateReadSegmentMetadata
                    .text(),
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
                crate::i18n::MessageId::CliRecoveryRecoverCandidateMissingMetadataWithNonEmptyJsonl
                    .text()
            } else {
                crate::i18n::MessageId::CliRecoveryRecoverCandidateMissingMetadataCannotProveAnEmptySegment.text()
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
            crate::i18n::MessageId::CliRecoveryRecoverCandidateMetadataIsTornOrContradictory.text(),
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
                operation:
                    crate::i18n::MessageId::CliRecoveryRecoverCandidateRemoveOrphanCreatingMetadata
                        .text(),
                path: metadata_path.clone(),
                source: source_error,
            })?;
            sync_directory(stream_dir)?;
            return Ok(record(
                source,
                RecoveryAction::Remove,
                crate::i18n::MessageId::CliRecoveryRecoverCandidateRemovedOrphanCreatingMetadata
                    .text(),
            ));
        }
        let finalized = stream_dir.join(&metadata.finalized_dir);
        let finalized_is_directory = match fs::symlink_metadata(&finalized) {
            Ok(finalized_metadata) => {
                !finalized_metadata.file_type().is_symlink() && finalized_metadata.is_dir()
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(source_error) => {
                return Err(RecoveryError::Io {
                    operation: crate::i18n::MessageId::CliRecoveryRecoverCandidateInspectOrphanFinalizedTarget.text(),
                    path: finalized,
                    source: source_error,
                });
            }
        };
        if finalized_is_directory
            && validate_files(&finalized, &metadata)?.kind == ValidationKind::Exact
        {
            write_capture_time(
                stream_dir,
                &metadata.finalized_dir,
                &CaptureTime {
                    tz: metadata.tz.clone(),
                    utc_offset_seconds: metadata.local_offset_seconds,
                },
            )
            .map_err(RecoveryError::Storage)?;
            fs::remove_file(&metadata_path).map_err(|source_error| RecoveryError::Io {
                operation:
                    crate::i18n::MessageId::CliRecoveryRecoverCandidateRemoveOrphanFinalizedMetadata
                        .text(),
                path: metadata_path.clone(),
                source: source_error,
            })?;
            sync_directory(stream_dir)?;
            return Ok(record(
                finalized,
                RecoveryAction::Remove,
                crate::i18n::MessageId::CliRecoveryRecoverCandidateRemovedMetadataLeftAfterFinalizedRename.text(),
            ));
        }
        return Ok(record(
            source,
            RecoveryAction::Retain,
            crate::i18n::MessageId::CliRecoveryRecoverCandidateMetadataHasNoMatchingSourceOrValidatedFinalized.text(),
        ));
    }

    // Metadata written before lengths were bounded can carry a zero or
    // over-long length; the finalized name always comes from the bound.
    let recorded = Duration::from_nanos(metadata.elapsed_nanos);
    record_length(&mut metadata, recorded, segment_interval);
    let finalized = stream_dir.join(&metadata.finalized_dir);
    if entry_exists(&finalized)? {
        return Ok(record(
            source,
            RecoveryAction::Failed,
            crate::i18n::MessageId::CliRecoveryRecoverCandidateFinalizedTargetCollisionSourceAndMetadataRetained.text(),
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
        if let Some(validated) = validated_empty_files(&source)? {
            if let Some(before_empty_removal) = options.before_empty_removal {
                before_empty_removal(&source).map_err(|source_error| RecoveryError::Io {
                    operation: crate::i18n::MessageId::CliRecoveryRecoverCandidateRunConfirmedEmptyPreRemovalHook.text(),
                    path: source.clone(),
                    source: source_error,
                })?;
            }
            if !remove_validated_empty_files(&source, &validated)? {
                return Ok(record(
                    source,
                    RecoveryAction::Retain,
                    crate::i18n::MessageId::CliRecoveryRecoverCandidateConfirmedEmptySegmentChangedBeforeRemovalRetainedWithout.text(),
                ));
            }
            fs::remove_dir(&source).map_err(|source_error| RecoveryError::Io {
                operation:
                    crate::i18n::MessageId::CliRecoveryRecoverCandidateRemoveConfirmedEmptySegment
                        .text(),
                path: source.clone(),
                source: source_error,
            })?;
            fs::remove_file(&metadata_path).map_err(|source_error| RecoveryError::Io {
                operation:
                    crate::i18n::MessageId::CliRecoveryRecoverCandidateRemoveConfirmedEmptyMetadata
                        .text(),
                path: metadata_path.clone(),
                source: source_error,
            })?;
            sync_directory(stream_dir)?;
            return Ok(record(
                source,
                RecoveryAction::Remove,
                crate::i18n::MessageId::CliRecoveryRecoverCandidateValidMetadataPositivelyConfirmedAnEmptySegment.text(),
            ));
        }
        return quarantine(
            stream_dir,
            &source,
            &metadata_path,
            crate::i18n::MessageId::CliRecoveryRecoverCandidateEmptyMetadataContradictsJsonlBytes
                .text(),
        );
    }

    let validation = validate_files(&source, &metadata)?;
    if validation.kind == ValidationKind::Contradictory {
        return quarantine(stream_dir, &source, &metadata_path, &validation.detail);
    }
    let repaired = validation.kind == ValidationKind::Repair;
    if repaired {
        apply_repairs(&source, &validation.repairs, options)?;
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
            &crate::i18n::render(
                crate::i18n::MessageId::CliRecoveryRecoverCandidateSourceRenameFailedAndWasRetained,
                &[(
                    "source_error",
                    crate::i18n::ArgValue::Text((source_error).to_string()),
                )],
            ),
        ));
    }
    if options.fail_rename_parent_sync {
        return Err(RecoveryError::Io {
            operation:
                crate::i18n::MessageId::CliRecoveryRecoverCandidateFsyncRecoveredRenameParent.text(),
            path: stream_dir.to_owned(),
            source: std::io::Error::other("injected recovered rename parent fsync failure"),
        });
    }
    sync_directory(stream_dir)?;
    write_capture_time(
        stream_dir,
        &metadata.finalized_dir,
        &CaptureTime {
            tz: metadata.tz.clone(),
            utc_offset_seconds: metadata.local_offset_seconds,
        },
    )
    .map_err(RecoveryError::Storage)?;
    fs::remove_file(&metadata_path).map_err(|source_error| RecoveryError::Io {
        operation: crate::i18n::MessageId::CliRecoveryRecoverCandidateRemoveRecoveredMetadata
            .text(),
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
            crate::i18n::MessageId::CliRecoveryRecoverCandidateRepairedOnlyATornFinalLineAndFinalized.text()
        } else {
            crate::i18n::MessageId::CliRecoveryRecoverCandidateValidatedDurableBytesAndFinalized
                .text()
        },
    ))
}

fn validate_stream(data_root: &Path, stream_dir: &Path) -> Result<(), RecoveryError> {
    let data_root = data_root
        .canonicalize()
        .map_err(|source| RecoveryError::Io {
            operation: crate::i18n::MessageId::CliRecoveryValidateStreamCanonicalizeDataRoot.text(),
            path: data_root.to_owned(),
            source,
        })?;
    let metadata = fs::symlink_metadata(stream_dir).map_err(|source| RecoveryError::Io {
        operation: crate::i18n::MessageId::CliRecoveryValidateStreamInspectStreamDirectory.text(),
        path: stream_dir.to_owned(),
        source,
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(RecoveryError::EscapesDataRoot(stream_dir.to_owned()));
    }
    let stream = stream_dir
        .canonicalize()
        .map_err(|source| RecoveryError::Io {
            operation: crate::i18n::MessageId::CliRecoveryValidateStreamCanonicalizeStreamDirectory
                .text(),
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
            operation: crate::i18n::MessageId::CliRecoveryRejectSpecialTargetInspectRecoveryTarget
                .text(),
            path: path.to_owned(),
            source,
        }),
    }
}

fn metadata_is_consistent(metadata: &SegmentMetadata, stem: &str) -> bool {
    let elapsed = Duration::from_nanos(metadata.elapsed_nanos);
    if metadata.schema_version != SegmentMetadata::SCHEMA_VERSION
        || !valid_time_name(stem)
        || metadata.incomplete_dir != format!("{stem}.incomplete")
        || !is_direct_name(&metadata.incomplete_dir)
        || !is_direct_name(&metadata.finalized_dir)
        || metadata.finalized_dir != finalized_name(stem, elapsed)
        || UtcOffset::from_whole_seconds(metadata.local_offset_seconds).is_err()
        || OffsetDateTime::from_unix_timestamp_nanos(metadata.start_wall_unix_nanos).is_err()
        || metadata.has_durable_frames != (metadata.durable_frame_count > 0)
        || metadata.durable_frame_count != metadata.last_durable_frame_id
        || (metadata.lifecycle == MetadataLifecycle::Creating
            && (metadata.durable_frame_count != 0 || !metadata.sessions.is_empty()))
    {
        return false;
    }
    let mut filenames = BTreeSet::new();
    metadata.sessions.iter().all(|(identity, session)| {
        let has_durable_bytes = session.durable_offset > 0;
        let has_frame_id = session.last_frame_id > 0;
        identity == &session.session
            && derive_component(identity)
                .map(|name| name.session_filename() == session.filename)
                .unwrap_or(false)
            && has_durable_bytes == has_frame_id
            && session.last_frame_id <= metadata.last_durable_frame_id
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
        operation: crate::i18n::MessageId::CliRecoveryValidateFilesScanIncompleteSegment.text(),
        path: directory.to_owned(),
        source,
    })? {
        let entry = entry.map_err(|source| RecoveryError::Io {
            operation: crate::i18n::MessageId::CliRecoveryValidateFilesReadIncompleteSegmentEntry
                .text(),
            path: directory.to_owned(),
            source,
        })?;
        let kind =
            entry.file_type().map_err(|source| RecoveryError::Io {
                operation:
                    crate::i18n::MessageId::CliRecoveryValidateFilesInspectIncompleteSegmentEntry
                        .text(),
                path: entry.path(),
                source,
            })?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            return Ok(contradiction(
                crate::i18n::MessageId::CliRecoveryValidateFilesNonUtfFilenameInSegment.text(),
            ));
        };
        if kind.is_symlink() || !kind.is_file() || !expected.contains(name) {
            return Ok(contradiction(
                crate::i18n::MessageId::CliRecoveryValidateFilesUnknownOrNonRegularFileInSegment
                    .text(),
            ));
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
                return Ok(contradiction(
                    crate::i18n::MessageId::CliRecoveryValidateFilesReferencedJsonlFileIsMissing
                        .text(),
                ));
            }
            Err(source) => {
                return Err(RecoveryError::Io {
                    operation:
                        crate::i18n::MessageId::CliRecoveryValidateFilesReadJsonlDuringRecovery
                            .text(),
                    path,
                    source,
                });
            }
        };
        if bytes.len() < session.durable_offset as usize {
            return Ok(contradiction(
                crate::i18n::MessageId::CliRecoveryValidateFilesJsonlIsShorterThanItsDurableOffset
                    .text(),
            ));
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
            return Ok(contradiction(
                crate::i18n::MessageId::CliRecoveryValidateFilesEarlierJsonlCorruption.text(),
            ));
        }
        if !prefix_torn && ids.last().copied().unwrap_or(0) != session.last_frame_id {
            return Ok(contradiction(
                crate::i18n::MessageId::CliRecoveryValidateFilesSessionFrameIdsDisagreeWithMetadata
                    .text(),
            ));
        }
        if bytes.len() > recorded {
            let suffix = &bytes[recorded..];
            if suffix.contains(&b'\n') {
                return Ok(contradiction(
                    crate::i18n::MessageId::CliRecoveryValidateFilesCompleteOrMultilineBytesExistBeyondTheDurable.text(),
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
            crate::i18n::MessageId::CliRecoveryValidateFilesGlobalFrameIdsAreDuplicatedOrNonconsecutive.text(),
        ));
    }
    if repairs.is_empty()
        && (metadata.durable_frame_count != expected_count
            || metadata.last_durable_frame_id != expected_last)
    {
        return Ok(contradiction(
            crate::i18n::MessageId::CliRecoveryValidateFilesGlobalFrameIdsDisagreeWithMetadata
                .text(),
        ));
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

fn apply_repairs(
    directory: &Path,
    repairs: &[FileRepair],
    options: RecoveryOptions,
) -> Result<(), RecoveryError> {
    for repair in repairs {
        if repair.path.parent() != Some(directory) {
            return Err(RecoveryError::EscapesDataRoot(repair.path.clone()));
        }
        let file = OpenOptions::new()
            .write(true)
            .open(&repair.path)
            .map_err(|source| RecoveryError::Io {
                operation: crate::i18n::MessageId::CliRecoveryApplyRepairsOpenJsonlRepair.text(),
                path: repair.path.clone(),
                source,
            })?;
        file.set_len(repair.offset)
            .and_then(|()| {
                if options.fail_repair_sync {
                    Err(std::io::Error::other(
                        "injected repaired JSONL fsync failure",
                    ))
                } else {
                    file.sync_all()
                }
            })
            .map_err(|source| RecoveryError::Io {
                operation:
                    crate::i18n::MessageId::CliRecoveryApplyRepairsTruncateAndFsyncJsonlRepair
                        .text(),
                path: repair.path.clone(),
                source,
            })?;
    }
    Ok(())
}

struct ValidatedEmptyFile {
    path: PathBuf,
    device: u64,
    inode: u64,
    length: u64,
}

fn validated_empty_files(
    directory: &Path,
) -> Result<Option<Vec<ValidatedEmptyFile>>, RecoveryError> {
    let mut validated = Vec::new();
    for entry in fs::read_dir(directory).map_err(|source| RecoveryError::Io {
        operation: crate::i18n::MessageId::CliRecoveryValidatedEmptyFilesScanEmptySegment.text(),
        path: directory.to_owned(),
        source,
    })? {
        let entry = entry.map_err(|source| RecoveryError::Io {
            operation: crate::i18n::MessageId::CliRecoveryValidatedEmptyFilesReadEmptySegmentEntry
                .text(),
            path: directory.to_owned(),
            source,
        })?;
        let metadata = fs::symlink_metadata(entry.path()).map_err(|source| RecoveryError::Io {
            operation:
                crate::i18n::MessageId::CliRecoveryValidatedEmptyFilesInspectEmptySegmentEntry
                    .text(),
            path: entry.path(),
            source,
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() != 0 {
            return Ok(None);
        }
        validated.push(ValidatedEmptyFile {
            path: entry.path(),
            device: metadata.dev(),
            inode: metadata.ino(),
            length: metadata.len(),
        });
    }
    Ok(Some(validated))
}

fn remove_validated_empty_files(
    directory: &Path,
    validated: &[ValidatedEmptyFile],
) -> Result<bool, RecoveryError> {
    let mut seen = BTreeSet::new();
    for entry in fs::read_dir(directory).map_err(|source| RecoveryError::Io {
        operation: crate::i18n::MessageId::CliRecoveryRemoveValidatedEmptyFilesScanZeroLengthFiles
            .text(),
        path: directory.to_owned(),
        source,
    })? {
        let entry = entry.map_err(|source| RecoveryError::Io {
            operation:
                crate::i18n::MessageId::CliRecoveryRemoveValidatedEmptyFilesReadZeroLengthEntry
                    .text(),
            path: directory.to_owned(),
            source,
        })?;
        let path = entry.path();
        let Some(expected) = validated.iter().find(|expected| expected.path == path) else {
            return Ok(false);
        };
        let metadata = fs::symlink_metadata(&path).map_err(|source| RecoveryError::Io {
            operation:
                crate::i18n::MessageId::CliRecoveryRemoveValidatedEmptyFilesReinspectZeroLengthJsonl
                    .text(),
            path: path.clone(),
            source,
        })?;
        if metadata.file_type().is_symlink()
            || !metadata.is_file()
            || metadata.dev() != expected.device
            || metadata.ino() != expected.inode
            || metadata.len() != expected.length
            || !seen.insert(path)
        {
            return Ok(false);
        }
    }
    if seen.len() != validated.len() {
        return Ok(false);
    }
    for expected in validated {
        fs::remove_file(&expected.path).map_err(|source| RecoveryError::Io {
            operation:
                crate::i18n::MessageId::CliRecoveryRemoveValidatedEmptyFilesRemoveZeroLengthJsonl
                    .text(),
            path: expected.path.clone(),
            source,
        })?;
    }
    Ok(true)
}

fn source_has_nonempty_jsonl(source: &Path) -> Result<bool, RecoveryError> {
    if !source.is_dir() {
        return Ok(false);
    }
    for entry in fs::read_dir(source).map_err(|source_error| RecoveryError::Io {
        operation: crate::i18n::MessageId::CliRecoverySourceHasNonemptyJsonlScanMetadataLessSegment
            .text(),
        path: source.to_owned(),
        source: source_error,
    })? {
        let entry = entry.map_err(|source_error| RecoveryError::Io {
            operation: crate::i18n::MessageId::CliRecoverySourceHasNonemptyJsonlReadMetadataLessSegmentEntry.text(),
            path: source.to_owned(),
            source: source_error,
        })?;
        if fs::symlink_metadata(entry.path())
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
        if !entry_exists(&failed)? && !entry_exists(&failed_metadata)? {
            break (failed, failed_metadata);
        }
        number += 1;
    };
    let source_exists = entry_exists(source)?;
    let metadata_exists = entry_exists(metadata)?;
    if source_exists {
        fs::rename(source, &failed).map_err(|source_error| RecoveryError::Io {
            operation: crate::i18n::MessageId::CliRecoveryQuarantineQuarantineSegment.text(),
            path: source.to_owned(),
            source: source_error,
        })?;
    }
    if metadata_exists && let Err(source_error) = fs::rename(metadata, &failed_metadata) {
        if source_exists {
            let _ = fs::rename(&failed, source);
        }
        return Err(RecoveryError::Io {
            operation: crate::i18n::MessageId::CliRecoveryQuarantineQuarantineMetadata.text(),
            path: metadata.to_owned(),
            source: source_error,
        });
    }
    sync_directory(stream_dir)?;
    Ok(record(failed, RecoveryAction::Quarantine, detail))
}

fn entry_exists(path: &Path) -> Result<bool, RecoveryError> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(RecoveryError::Io {
            operation: crate::i18n::MessageId::CliRecoveryEntryExistsInspectRecoveryCollisionTarget
                .text(),
            path: path.to_owned(),
            source,
        }),
    }
}

fn valid_time_name(value: &str) -> bool {
    if value.len() != 6 || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return false;
    }
    let hour = value[0..2].parse::<u8>().ok();
    let minute = value[2..4].parse::<u8>().ok();
    let second = value[4..6].parse::<u8>().ok();
    matches!(
        (hour, minute, second),
        (Some(0..=23), Some(0..=59), Some(0..=59))
    )
}

fn valid_date_name(value: &str) -> bool {
    if value.len() != 8 || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return false;
    }
    let year = value[0..4].parse::<i32>().ok();
    let month = value[4..6]
        .parse::<u8>()
        .ok()
        .and_then(|month| Month::try_from(month).ok());
    let day = value[6..8].parse::<u8>().ok();
    matches!(
        (year, month, day),
        (Some(year), Some(month), Some(day))
            if Date::from_calendar_date(year, month, day).is_ok()
    )
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
            Self::EscapesDataRoot(path) => write!(formatter, "{}", crate::i18n::render(crate::i18n::MessageId::CliRecoveryFmtRecoveryCandidateEscapesTheConfiguredDataRoot, &[("arg0", crate::i18n::ArgValue::Text((path.display()).to_string()))])),
            Self::SpecialTarget(path) => write!(formatter, "{}", crate::i18n::render(crate::i18n::MessageId::CliRecoveryFmtRecoveryCandidateIsASymlinkOrWrongFile, &[("arg0", crate::i18n::ArgValue::Text((path.display()).to_string()))])),
            Self::Io {
                operation,
                path,
                source,
            } => write!(formatter, "{}", crate::i18n::render(crate::i18n::MessageId::CliConfigFmtFailedFor, &[("operation", crate::i18n::ArgValue::Text((operation).to_string())), ("arg0", crate::i18n::ArgValue::Text((path.display()).to_string())), ("source", crate::i18n::ArgValue::Text((source).to_string()))])),
        }
    }
}

impl std::error::Error for RecoveryError {}
