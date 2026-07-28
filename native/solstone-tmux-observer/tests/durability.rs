// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

mod support;

use std::fs;
use std::time::Duration;

use solstone_tmux_observer::segment::{AppendOutcome, SegmentState};
use solstone_tmux_observer::serialize::serialize_frame;
use solstone_tmux_observer::storage::{FaultPlan, SegmentMetadata, StorageStage};
use support::{TestDirectory, golden_capture};
use time::{Date, Month, PrimitiveDateTime, Time, UtcOffset};

#[test]
fn append_flush_sync_precede_state_commit() {
    for stage in [
        StorageStage::Append,
        StorageStage::Flush,
        StorageStage::JsonFsync,
        StorageStage::Metadata,
    ] {
        let (temporary, mut segment) = segment_with_faults(&format!("{stage:?}"), [stage]);
        let capture = golden_capture("main");

        assert!(
            segment
                .append_capture(&capture, 0.25, Duration::from_secs(1))
                .is_err()
        );

        assert_eq!(segment.metadata().last_durable_frame_id, 0);
        assert_eq!(segment.metadata().durable_frame_count, 0);
        assert_eq!(jsonl_bytes(&segment), b"");
        assert_metadata_uncommitted(&segment);
        drop(temporary);
    }
}

#[test]
fn append_failure_retries_identical_observation() {
    assert_failure_rolls_back_and_retries(StorageStage::Append);
}

#[test]
fn flush_failure_rolls_back_and_retries() {
    assert_failure_rolls_back_and_retries(StorageStage::Flush);
}

#[test]
fn fsync_failure_rolls_back_and_retries() {
    assert_failure_rolls_back_and_retries(StorageStage::JsonFsync);
}

#[test]
fn metadata_failure_rolls_back_and_retries() {
    assert_failure_rolls_back_and_retries(StorageStage::Metadata);
}

#[test]
fn rollback_failure_poisons_segment() {
    let (_temporary, mut segment) =
        segment_with_faults("poison", [StorageStage::Flush, StorageStage::Rollback]);
    let capture = golden_capture("main");
    let expected = serialize_frame(&capture, 1, 0.25).expect("expected frame");

    let error = segment
        .append_capture(&capture, 0.25, Duration::from_secs(1))
        .expect_err("append and rollback must fail");

    assert!(error.to_string().contains("rollback failed"));
    assert!(segment.is_poisoned());
    assert_eq!(segment.metadata().last_durable_frame_id, 0);
    assert_eq!(jsonl_bytes(&segment), expected);
    assert!(
        segment
            .append_capture(&capture, 0.25, Duration::from_secs(2))
            .is_err()
    );
}

#[test]
fn session_handle_lives_until_segment_close() {
    let (_temporary, mut segment) = segment_with_faults("handles", []);
    let first = golden_capture("main");
    segment
        .append_capture(&first, 0.25, Duration::from_secs(1))
        .expect("first append");
    assert_eq!(segment.open_handle_count(), 1);

    let mut changed = first.clone();
    changed.panes[0].content.push_str("changed");
    segment
        .append_capture(&changed, 0.5, Duration::from_secs(2))
        .expect("second append");
    assert_eq!(segment.open_handle_count(), 1);

    segment
        .finalize(Duration::from_secs(3))
        .expect("finalize segment");
    assert_eq!(segment.open_handle_count(), 0);
}

fn assert_failure_rolls_back_and_retries(stage: StorageStage) {
    let (_temporary, mut segment) = segment_with_faults(&format!("{stage:?}"), [stage]);
    let capture = golden_capture("main");
    let expected = serialize_frame(&capture, 1, 0.25).expect("expected frame");

    assert!(
        segment
            .append_capture(&capture, 0.25, Duration::from_secs(1))
            .is_err()
    );
    assert_eq!(jsonl_bytes(&segment), b"");
    assert_metadata_uncommitted(&segment);

    assert_eq!(
        segment
            .append_capture(&capture, 0.25, Duration::from_secs(1))
            .expect("identical retry"),
        AppendOutcome::Appended { frame_id: 1 }
    );
    assert_eq!(jsonl_bytes(&segment), expected);
    assert_eq!(segment.metadata().last_durable_frame_id, 1);
}

fn assert_metadata_uncommitted(segment: &SegmentState) {
    let bytes = fs::read(segment.metadata_path()).expect("metadata bytes");
    let metadata: SegmentMetadata =
        serde_json::from_slice(&bytes).expect("valid metadata after rollback");
    assert_eq!(metadata.last_durable_frame_id, 0);
    assert_eq!(metadata.durable_frame_count, 0);
    assert!(!metadata.has_durable_frames);
}

fn jsonl_bytes(segment: &SegmentState) -> Vec<u8> {
    fs::read(segment.incomplete_dir().join("tmux_main_screen.jsonl"))
        .expect("real JSONL file after attempted append")
}

fn segment_with_faults<const N: usize>(
    label: &str,
    faults: [StorageStage; N],
) -> (TestDirectory, SegmentState) {
    let temporary = TestDirectory::new(label);
    let stream = temporary.path().join("stream");
    let date = Date::from_calendar_date(2026, Month::July, 28).expect("date");
    let time = Time::from_hms(12, 0, 0).expect("time");
    let wall = PrimitiveDateTime::new(date, time).assume_utc();
    let segment = SegmentState::create_with_faults(
        &stream,
        wall,
        Duration::ZERO,
        UtcOffset::UTC,
        FaultPlan::at(faults),
    )
    .expect("create segment");
    (temporary, segment)
}
