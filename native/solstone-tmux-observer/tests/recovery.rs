// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

mod support;

use std::fs;
use std::io::Write;
use std::os::unix::fs::symlink;
use std::path::PathBuf;
use std::time::Duration;

use solstone_tmux_observer::instance_lock::InstanceLock;
use solstone_tmux_observer::recovery::{
    RecoveryAction, RecoveryError, RecoveryOptions, recover_stream, recover_stream_with_options,
};
use solstone_tmux_observer::segment::SegmentState;
use solstone_tmux_observer::storage::SegmentMetadata;
use support::{TestDirectory, golden_capture};
use time::{Date, Month, PrimitiveDateTime, Time, UtcOffset};

// AC 13: valid metadata is the only positive-empty evidence.
#[test]
fn valid_empty_metadata_removes_segment() {
    let setup = incomplete("valid-empty", false);
    let records = recover(&setup);
    assert_eq!(records[0].action, RecoveryAction::Remove);
    assert!(!setup.source.exists());
    assert!(!setup.metadata.exists());
}

// AC 13: missing metadata cannot positively classify even an empty directory.
#[test]
fn missing_metadata_empty_is_retained() {
    let setup = incomplete("missing-empty", false);
    fs::remove_file(&setup.metadata).expect("remove metadata");
    let records = recover(&setup);
    assert_eq!(records[0].action, RecoveryAction::Retain);
    assert!(setup.source.exists());
}

// AC 13: valid nonempty metadata with exact durable bytes finalizes.
#[test]
fn valid_nonempty_finalizes() {
    let setup = incomplete("valid-nonempty", true);
    let finalized = setup.finalized_path();
    let records = recover(&setup);
    assert_eq!(records[0].action, RecoveryAction::Finalized);
    assert!(finalized.is_dir());
    assert!(!setup.source.exists());
    assert!(!setup.metadata.exists());
}

// AC 13: only a torn final line is removed, then the repaired file is synced.
#[test]
fn torn_final_line_is_removed_and_file_fsynced() {
    let setup = incomplete("torn-final", true);
    let jsonl = setup.jsonl();
    let original = fs::read(&jsonl).expect("original JSONL");
    let mut metadata = setup.read_metadata();
    let torn = br#"{"frame_id": 2"#;
    fs::OpenOptions::new()
        .append(true)
        .open(&jsonl)
        .expect("open JSONL")
        .write_all(torn)
        .expect("append torn line");
    let session = metadata.sessions.get_mut("main").expect("main metadata");
    session.durable_offset += torn.len() as u64;
    session.last_frame_id = 2;
    metadata.last_durable_frame_id = 2;
    metadata.durable_frame_count = 2;
    setup.write_metadata(&metadata);

    let records = recover(&setup);

    assert_eq!(records[0].action, RecoveryAction::RepairThenFinalize);
    assert_eq!(
        fs::read(setup.finalized_path().join(setup.filename())).expect("repaired"),
        original
    );
}

// AC 13: invalid JSON before a later durable boundary quarantines.
#[test]
fn earlier_corruption_quarantines() {
    let setup = incomplete("earlier-corruption", true);
    let jsonl = setup.jsonl();
    let mut bytes = fs::read(&jsonl).expect("JSONL");
    bytes[0] = b'!';
    fs::write(&jsonl, bytes).expect("corrupt JSONL");

    let records = recover(&setup);

    assert_eq!(records[0].action, RecoveryAction::Quarantine);
    assert!(
        records[0]
            .candidate
            .extension()
            .is_some_and(|value| value == "failed")
    );
}

// AC 13: missing metadata plus nonempty JSONL retains owner data unchanged.
#[test]
fn missing_metadata_nonempty_retains() {
    let setup = incomplete("missing-nonempty", true);
    let before = fs::read(setup.jsonl()).expect("before");
    fs::remove_file(&setup.metadata).expect("remove metadata");

    let records = recover(&setup);

    assert_eq!(records[0].action, RecoveryAction::Retain);
    assert_eq!(fs::read(setup.jsonl()).expect("after"), before);
}

// AC 13: torn metadata with nonempty JSONL quarantines without truncating JSONL.
#[test]
fn torn_metadata_nonempty_quarantines() {
    let setup = incomplete("torn-metadata", true);
    let before = fs::read(setup.jsonl()).expect("before");
    fs::write(&setup.metadata, b"{").expect("tear metadata");

    let records = recover(&setup);

    assert_eq!(records[0].action, RecoveryAction::Quarantine);
    assert_eq!(
        fs::read(records[0].candidate.join(setup.filename())).expect("quarantined JSONL"),
        before
    );
}

// AC 13: a recorded offset beyond the file is contradictory and quarantines.
#[test]
fn contradictory_offsets_quarantine() {
    let setup = incomplete("contradictory-offset", true);
    let mut metadata = setup.read_metadata();
    metadata
        .sessions
        .get_mut("main")
        .expect("main metadata")
        .durable_offset += 100;
    setup.write_metadata(&metadata);

    let records = recover(&setup);

    assert_eq!(records[0].action, RecoveryAction::Quarantine);
}

// AC 13: a complete valid frame beyond metadata is never truncated as torn.
#[test]
fn complete_bytes_beyond_metadata_quarantine() {
    let setup = incomplete("complete-extra", true);
    let extra = solstone_tmux_observer::serialize::serialize_frame(&golden_capture("main"), 2, 1.0)
        .expect("extra frame");
    fs::OpenOptions::new()
        .append(true)
        .open(setup.jsonl())
        .expect("open JSONL")
        .write_all(&extra)
        .expect("append extra frame");

    let records = recover(&setup);

    assert_eq!(records[0].action, RecoveryAction::Quarantine);
}

// AC 13: a finalized-name collision keeps the source and metadata byte-for-byte.
#[test]
fn rename_collision_keeps_source() {
    let setup = incomplete("rename-collision", true);
    let metadata_before = fs::read(&setup.metadata).expect("metadata before");
    fs::create_dir(setup.finalized_path()).expect("create collision");

    let records = recover(&setup);

    assert_eq!(records[0].action, RecoveryAction::Failed);
    assert!(setup.source.is_dir());
    assert_eq!(
        fs::read(&setup.metadata).expect("metadata after"),
        metadata_before
    );
}

// AC 13: a source rename failure retains both source and authoritative metadata.
#[test]
fn rename_failure_keeps_source() {
    let setup = incomplete("rename-failure", true);
    let metadata_before = fs::read(&setup.metadata).expect("metadata before");
    let instance_lock = InstanceLock::acquire(&setup.data_root).expect("recovery lock");

    let records = recover_stream_with_options(
        &instance_lock,
        &setup.data_root,
        &setup.stream,
        RecoveryOptions {
            fail_source_rename: true,
        },
    )
    .expect("recovery");

    assert_eq!(records[0].action, RecoveryAction::Failed);
    assert!(setup.source.is_dir());
    assert_eq!(
        fs::read(&setup.metadata).expect("metadata after"),
        metadata_before
    );
}

// AC 13: a completed directory rename with lingering valid metadata removes only metadata.
#[test]
fn orphan_metadata_after_final_rename_is_removed() {
    let setup = incomplete("orphan-metadata", true);
    let finalized = setup.finalized_path();
    fs::rename(&setup.source, &finalized).expect("simulate completed rename");

    let records = recover(&setup);

    assert_eq!(records[0].action, RecoveryAction::Remove);
    assert!(finalized.is_dir());
    assert!(!setup.metadata.exists());
}

// AC 13: successful repair and rename return only after file and parent syncs.
#[test]
fn repairs_and_parent_renames_are_fsynced() {
    let setup = incomplete("repair-sync", true);
    let jsonl = setup.jsonl();
    fs::OpenOptions::new()
        .append(true)
        .open(&jsonl)
        .expect("open JSONL")
        .write_all(b"{")
        .expect("append torn suffix");

    let records = recover(&setup);

    assert_eq!(records[0].action, RecoveryAction::RepairThenFinalize);
    assert!(records[0].candidate.is_dir());
    assert!(!setup.metadata.exists());
}

// AC 13: symlink candidates and stream paths outside the configured root are rejected.
#[test]
fn symlink_and_escape_candidates_are_rejected() {
    let temporary = TestDirectory::new("recovery-escape");
    let data_root = temporary.path().join("data");
    let stream = data_root.join("stream");
    let outside = temporary.path().join("outside");
    fs::create_dir_all(&stream).expect("stream");
    fs::create_dir(&outside).expect("outside");
    symlink(&outside, stream.join("120000.incomplete")).expect("candidate symlink");
    let instance_lock = InstanceLock::acquire(&data_root).expect("recovery lock");

    assert!(matches!(
        recover_stream(&instance_lock, &data_root, &stream),
        Err(RecoveryError::SpecialTarget(_))
    ));

    let escaped = temporary.path().join("escaped");
    fs::create_dir(&escaped).expect("escaped");
    assert!(matches!(
        recover_stream(&instance_lock, &data_root, &escaped),
        Err(RecoveryError::EscapesDataRoot(_))
    ));
}

struct Incomplete {
    _temporary: TestDirectory,
    data_root: PathBuf,
    stream: PathBuf,
    source: PathBuf,
    metadata: PathBuf,
    filename: String,
    finalized: PathBuf,
}

fn recover(setup: &Incomplete) -> Vec<solstone_tmux_observer::recovery::RecoveryRecord> {
    let instance_lock = InstanceLock::acquire(&setup.data_root).expect("recovery lock");
    recover_stream(&instance_lock, &setup.data_root, &setup.stream).expect("recovery")
}

impl Incomplete {
    fn read_metadata(&self) -> SegmentMetadata {
        serde_json::from_slice(&fs::read(&self.metadata).expect("metadata bytes"))
            .expect("metadata JSON")
    }

    fn write_metadata(&self, metadata: &SegmentMetadata) {
        let mut bytes = serde_json::to_vec(metadata).expect("serialize metadata");
        bytes.push(b'\n');
        fs::write(&self.metadata, bytes).expect("write metadata");
    }

    fn filename(&self) -> String {
        self.filename.clone()
    }

    fn jsonl(&self) -> PathBuf {
        self.source.join(self.filename())
    }

    fn finalized_path(&self) -> PathBuf {
        self.finalized.clone()
    }
}

fn incomplete(label: &str, append: bool) -> Incomplete {
    let temporary = TestDirectory::new(label);
    let data_root = temporary.path().join("data");
    let stream = data_root.join("stream");
    fs::create_dir(&data_root).expect("data root");
    let date = Date::from_calendar_date(2026, Month::July, 28).expect("date");
    let time = Time::from_hms(12, 0, 0).expect("time");
    let wall = PrimitiveDateTime::new(date, time).assume_utc();
    let mut segment =
        SegmentState::create(&stream, wall, Duration::ZERO, UtcOffset::UTC).expect("segment");
    if append {
        segment
            .append_capture(&golden_capture("main"), 0.25, Duration::from_secs(1))
            .expect("append");
    }
    let source = segment.incomplete_dir().to_owned();
    let metadata = segment.metadata_path().to_owned();
    let filename = segment
        .metadata()
        .sessions
        .get("main")
        .map(|session| session.filename.clone())
        .unwrap_or_else(|| "tmux_main_screen.jsonl".to_owned());
    let finalized = stream.join(&segment.metadata().finalized_dir);
    drop(segment);
    Incomplete {
        _temporary: temporary,
        data_root,
        stream,
        source,
        metadata,
        filename,
        finalized,
    }
}
