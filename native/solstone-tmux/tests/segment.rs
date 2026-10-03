// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

mod support;

use std::fs;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use solstone_tmux::clock::{Zone, ZoneSource};
use solstone_tmux::name::derive_component;
use solstone_tmux::observer::{SegmentLifecycle, SegmentManager};
use solstone_tmux::segment::{
    AppendOutcome, SegmentClose, SegmentError, SegmentState, segment_length,
};
use solstone_tmux::sync::SyncWake;
use solstone_tmux::tmux::StderrWarnings;
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
fn segment_length_is_always_between_one_second_and_the_interval() {
    for interval_ms in [500, 1_000, 2_000, 5_000, 300_000, 600_000] {
        let interval = Duration::from_millis(interval_ms);
        let ceiling = interval.as_secs().max(1);
        let mut elapsed_ms = 0;
        while elapsed_ms <= interval_ms * 4 {
            let length = segment_length(Duration::from_millis(elapsed_ms), interval);
            assert!(
                (1..=ceiling).contains(&length),
                "elapsed {elapsed_ms} ms, interval {interval_ms} ms gave {length}"
            );
            elapsed_ms += 250;
        }
        assert_eq!(segment_length(Duration::MAX, interval), ceiling);
    }
}

#[test]
fn segment_length_keeps_in_range_durations_and_bounds_the_rest() {
    let interval = Duration::from_secs(300);
    for (elapsed, expected) in [
        (Duration::ZERO, 1),
        (Duration::from_millis(999), 1),
        (Duration::from_secs(1), 1),
        (Duration::from_millis(5_400), 5),
        (Duration::from_secs(299), 299),
        (Duration::from_secs(300), 300),
        (Duration::from_secs(305), 300),
        (Duration::from_secs(314), 300),
        (Duration::from_secs(14_400), 300),
    ] {
        assert_eq!(segment_length(elapsed, interval), expected, "{elapsed:?}");
    }
}

#[test]
fn finalize_past_the_interval_names_the_interval() {
    for elapsed in [Duration::from_secs(305), Duration::from_secs(14_400)] {
        let (_temporary, mut segment) = segment("finalize-overshoot");
        segment
            .append_capture(&golden_capture("main"), 0.25, Duration::from_secs(1))
            .expect("append");
        let SegmentClose::Finalized(path) = segment.finalize(elapsed).expect("finalize") else {
            panic!("nonempty segment was removed");
        };
        assert_eq!(key_length(&path), 300, "{elapsed:?}");
        assert!(path.ends_with("120000_300"));
        assert!(path.is_dir());
    }
}

#[test]
fn finalize_within_the_first_second_names_one_second() {
    let (_temporary, mut segment) = segment("finalize-subsecond");
    segment
        .append_capture(&golden_capture("main"), 0.0, Duration::ZERO)
        .expect("append");
    assert_eq!(segment.metadata().finalized_dir, "120000_001");
    let SegmentClose::Finalized(path) = segment
        .finalize(Duration::from_millis(400))
        .expect("finalize")
    else {
        panic!("nonempty segment was removed");
    };
    assert_eq!(key_length(&path), 1);
    assert!(path.is_dir());
}

#[test]
fn late_rotation_poll_seals_the_previous_segment_at_the_interval() {
    let interval = Duration::from_secs(300);
    for late in [Duration::from_secs(305), Duration::from_secs(14_400)] {
        let (temporary, segment) = segment("late-rotation");
        let stream_dir = segment.stream_dir().to_owned();
        let start = segment_wall();
        let mut manager = SegmentManager::new(
            segment,
            temporary.path().join("data"),
            derive_component("host.tmux").expect("stream"),
            SyncWake::default(),
            Box::new(UtcZone),
            Arc::new(StderrWarnings),
            None,
            false,
        );
        manager
            .process_poll(
                &[golden_capture("main")],
                start + time::Duration::seconds(1),
                Duration::from_secs(1),
                interval,
            )
            .expect("first poll");
        manager
            .process_poll(&[golden_capture("main")], start + late, late, interval)
            .expect("late poll");

        let sealed = stream_dir.join("120000_300");
        assert!(sealed.is_dir(), "{late:?}");
        assert_eq!(key_length(&sealed), 300);
    }
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

struct UtcZone;

impl ZoneSource for UtcZone {
    fn read(&mut self) -> Result<Zone, String> {
        Ok(Zone::utc())
    }
}

fn key_length(path: &Path) -> u64 {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .expect("name");
    let (_, length) = name.split_once('_').expect("HHMMSS_LEN");
    length.parse().expect("LEN digits")
}

fn segment_wall() -> time::OffsetDateTime {
    let date = Date::from_calendar_date(2026, Month::July, 28).expect("date");
    let time = Time::from_hms(12, 0, 0).expect("time");
    PrimitiveDateTime::new(date, time).assume_utc()
}

fn segment(label: &str) -> (TestDirectory, SegmentState) {
    let temporary = TestDirectory::new(label);
    let stream = temporary.path().join("stream");
    let segment = SegmentState::create(
        &stream,
        "120000",
        segment_wall(),
        Duration::ZERO,
        UtcOffset::UTC,
        None,
        Duration::from_secs(300),
    )
    .expect("create segment");
    (temporary, segment)
}

#[test]
fn empty_finalized_directory_is_a_collision() {
    let (_temporary, mut segment) = segment("empty-collision");
    segment
        .append_capture(&golden_capture("main"), 0.25, Duration::from_secs(1))
        .expect("append");
    let source = segment.incomplete_dir().to_owned();
    let metadata_path = segment.metadata_path().to_owned();
    let finalized = source.parent().expect("stream").join("120000_005");
    fs::create_dir(&finalized).expect("create empty finalized directory");

    let error = segment
        .finalize(Duration::from_secs(5))
        .expect_err("empty directory collision must fail finalize");

    assert!(matches!(error, SegmentError::Collision(_)));
    assert!(finalized.is_dir());
    assert_eq!(
        fs::read_dir(&finalized)
            .expect("read finalized directory")
            .count(),
        0
    );
    assert!(source.is_dir());
    assert!(metadata_path.exists());
}

#[test]
fn nonempty_finalized_directory_is_a_collision() {
    let (_temporary, mut segment) = segment("nonempty-collision");
    segment
        .append_capture(&golden_capture("main"), 0.25, Duration::from_secs(1))
        .expect("append");
    let source = segment.incomplete_dir().to_owned();
    let metadata_path = segment.metadata_path().to_owned();
    let finalized = source.parent().expect("stream").join("120000_005");
    fs::create_dir(&finalized).expect("create finalized directory");
    let marker = finalized.join("marker.txt");
    fs::write(&marker, b"keep-me").expect("write marker file");

    let error = segment
        .finalize(Duration::from_secs(5))
        .expect_err("nonempty directory collision must fail finalize");

    assert!(matches!(error, SegmentError::Collision(_)));
    assert_eq!(fs::read(&marker).expect("read marker"), b"keep-me");
    assert!(source.is_dir());
    assert!(metadata_path.exists());
}
