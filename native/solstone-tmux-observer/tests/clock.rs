// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::time::Duration;

use solstone_tmux_observer::clock::{Clock, TestClock, local_date_and_time};
use solstone_tmux_observer::segment::SegmentState;
use support::{TestDirectory, golden_capture};
use time::{Date, Month, PrimitiveDateTime, Time, UtcOffset};

mod support;

fn clock() -> TestClock {
    let date = Date::from_calendar_date(2026, Month::July, 28).expect("valid date");
    let time = Time::from_hms(6, 7, 8).expect("valid time");
    let wall = PrimitiveDateTime::new(date, time).assume_utc();
    TestClock::new(
        wall,
        Duration::from_secs(10),
        UtcOffset::from_hms(-6, 0, 0).expect("valid offset"),
    )
}

#[test]
fn fixed_offset_formats_paths() {
    let clock = clock();
    assert_eq!(
        local_date_and_time(clock.wall_now(), clock.local_offset()),
        ("20260728".to_owned(), "000708".to_owned())
    );
}

#[test]
fn wall_and_monotonic_are_independently_freezable() {
    let clock = clock();
    let original_wall = clock.wall_now();
    clock.set_monotonic(Duration::from_secs(300));
    assert_eq!(clock.wall_now(), original_wall);
    assert_eq!(clock.monotonic_now(), Duration::from_secs(300));

    clock.set_wall(original_wall + time::Duration::hours(12));
    assert_eq!(clock.monotonic_now(), Duration::from_secs(300));
    assert_eq!(clock.wall_now(), original_wall + time::Duration::hours(12));
}

#[test]
fn wall_jump_forward_does_not_rotate() {
    let (temporary, segment, clock) = segment_and_clock("wall-forward");
    clock.set_wall(clock.wall_now() + time::Duration::days(2));
    assert!(!segment.rotation_due(clock.monotonic_now(), Duration::from_secs(300)));
    drop(temporary);
}

#[test]
fn wall_jump_backward_does_not_suppress_rotation() {
    let (temporary, segment, clock) = segment_and_clock("wall-backward");
    clock.set_wall(clock.wall_now() - time::Duration::days(2));
    clock.set_monotonic(Duration::from_secs(310));
    assert!(segment.rotation_due(clock.monotonic_now(), Duration::from_secs(300)));
    drop(temporary);
}

#[test]
fn monotonic_boundary_rotates_with_frozen_wall() {
    let (temporary, segment, clock) = segment_and_clock("mono-boundary");
    let frozen_wall = clock.wall_now();
    clock.set_monotonic(Duration::from_secs(309));
    assert!(!segment.rotation_due(clock.monotonic_now(), Duration::from_secs(300)));
    clock.set_monotonic(Duration::from_secs(310));
    assert!(segment.rotation_due(clock.monotonic_now(), Duration::from_secs(300)));
    assert_eq!(clock.wall_now(), frozen_wall);
    drop(temporary);
}

#[test]
fn all_changed_sessions_share_one_wall_sample() {
    let (temporary, mut segment, clock) = segment_and_clock("shared-wall");
    let wall_sample = clock.wall_now();
    let timestamp = segment.frame_timestamp(wall_sample);
    segment
        .append_capture(&golden_capture("main"), timestamp, Duration::from_secs(11))
        .expect("main append");
    segment
        .append_capture(&golden_capture("other"), timestamp, Duration::from_secs(11))
        .expect("other append");

    for session in segment.metadata().sessions.values() {
        let bytes =
            std::fs::read(segment.incomplete_dir().join(&session.filename)).expect("session JSONL");
        let value: serde_json::Value = serde_json::from_slice(&bytes).expect("frame JSON");
        assert_eq!(value["timestamp"], timestamp);
    }
    drop(temporary);
}

fn segment_and_clock(label: &str) -> (TestDirectory, SegmentState, TestClock) {
    let clock = clock();
    let temporary = TestDirectory::new(label);
    let segment = SegmentState::create(
        &temporary.path().join("stream"),
        clock.wall_now(),
        Duration::from_secs(10),
        clock.local_offset(),
    )
    .expect("segment");
    (temporary, segment, clock)
}
