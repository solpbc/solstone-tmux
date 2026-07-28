// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

mod support;

use std::fs;
use std::os::unix::fs::symlink;
use std::time::Duration;

use solstone_tmux_observer::segment::{AppendOutcome, SegmentClose, SegmentState};
use support::{TestDirectory, golden_capture};
use time::{Date, Month, PrimitiveDateTime, Time, UtcOffset};

#[test]
fn unchanged_session_is_deduplicated() {
    let (_temporary, mut segment) = segment("dedup");
    let capture = golden_capture("main");
    assert_eq!(
        segment
            .append_capture(&capture, 0.25, Duration::from_secs(1))
            .expect("first append"),
        AppendOutcome::Appended { frame_id: 1 }
    );
    assert_eq!(
        segment
            .append_capture(&capture, 0.5, Duration::from_secs(2))
            .expect("deduplicated append"),
        AppendOutcome::Unchanged
    );
    assert_eq!(segment.metadata().durable_frame_count, 1);
}

#[test]
fn changed_sessions_consume_consecutive_ids() {
    let (_temporary, mut segment) = segment("ids");
    let first = golden_capture("main");
    let second = golden_capture("other");
    assert_eq!(
        segment
            .append_capture(&first, 0.25, Duration::from_secs(1))
            .expect("first append"),
        AppendOutcome::Appended { frame_id: 1 }
    );
    assert_eq!(
        segment
            .append_capture(&second, 0.25, Duration::from_secs(1))
            .expect("second append"),
        AppendOutcome::Appended { frame_id: 2 }
    );
}

#[test]
fn sessions_in_one_poll_share_timestamp() {
    let (_temporary, mut segment) = segment("timestamp");
    segment
        .append_capture(&golden_capture("main"), 1.75, Duration::from_secs(2))
        .expect("main append");
    segment
        .append_capture(&golden_capture("other"), 1.75, Duration::from_secs(2))
        .expect("other append");

    for session in ["main", "other"] {
        let filename = segment
            .metadata()
            .sessions
            .get(session)
            .expect("session metadata")
            .filename
            .clone();
        let bytes = fs::read(segment.incomplete_dir().join(filename)).expect("JSONL");
        let value: serde_json::Value = serde_json::from_slice(&bytes).expect("frame JSON");
        assert_eq!(value["timestamp"], 1.75);
    }
}

#[test]
fn rotation_uses_monotonic_duration() {
    let (_temporary, segment) = segment("rotation");
    assert!(!segment.rotation_due(Duration::from_secs(299), Duration::from_secs(300)));
    assert!(segment.rotation_due(Duration::from_secs(300), Duration::from_secs(300)));
}

#[test]
fn nonempty_segment_finalizes_once() {
    let (_temporary, mut segment) = segment("finalize");
    segment
        .append_capture(&golden_capture("main"), 0.25, Duration::from_secs(1))
        .expect("append");
    let first = segment
        .finalize(Duration::from_secs(5))
        .expect("first finalization");
    let second = segment
        .finalize(Duration::from_secs(10))
        .expect("idempotent finalization");
    assert_eq!(first, second);
    let SegmentClose::Finalized(path) = first else {
        panic!("nonempty segment was removed");
    };
    assert!(path.ends_with("120000_005"));
    assert!(path.is_dir());
    assert!(!segment.metadata_path().exists());
}

#[test]
fn dangling_symlink_finalized_target_preserves_source() {
    let (_temporary, mut segment) = segment("dangling-finalized-target");
    segment
        .append_capture(&golden_capture("main"), 0.25, Duration::from_secs(1))
        .expect("append");
    let source = segment.incomplete_dir().to_owned();
    let finalized = source.parent().expect("stream").join("120000_005");
    symlink(
        finalized
            .parent()
            .expect("stream")
            .join("missing-finalized-target"),
        &finalized,
    )
    .expect("dangling symlink");

    let error = segment
        .finalize(Duration::from_secs(5))
        .expect_err("dangling symlink must be a collision");

    assert!(error.to_string().contains("already exists"));
    assert!(source.is_dir());
    assert!(fs::symlink_metadata(finalized).is_ok());
}

#[test]
fn confirmed_empty_segment_is_removed() {
    let (_temporary, mut segment) = segment("empty");
    let incomplete = segment.incomplete_dir().to_owned();
    let metadata = segment.metadata_path().to_owned();

    assert_eq!(
        segment
            .remove_confirmed_empty()
            .expect("remove empty segment"),
        SegmentClose::RemovedEmpty
    );
    assert!(!incomplete.exists());
    assert!(!metadata.exists());
}

fn segment(label: &str) -> (TestDirectory, SegmentState) {
    let temporary = TestDirectory::new(label);
    let stream = temporary.path().join("stream");
    let date = Date::from_calendar_date(2026, Month::July, 28).expect("date");
    let time = Time::from_hms(12, 0, 0).expect("time");
    let wall = PrimitiveDateTime::new(date, time).assume_utc();
    let segment = SegmentState::create(&stream, wall, Duration::ZERO, UtcOffset::UTC)
        .expect("create segment");
    (temporary, segment)
}
