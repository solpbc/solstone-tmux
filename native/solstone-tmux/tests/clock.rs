// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::path::Path;
use std::time::Duration;

use solstone_tmux::clock::{Clock, SystemClock, TestClock, Zone, local_date_and_time};
use solstone_tmux::segment::SegmentState;
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
        local_date_and_time(clock.wall_now(), clock.offset_at(clock.wall_now())),
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
        clock.offset_at(clock.wall_now()),
        None,
    )
    .expect("segment");
    (temporary, segment, clock)
}

#[test]
fn denver_tzif_and_posix_rule_answer_2026_from_the_footer() {
    let fixture_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/america-denver.tzif");
    let bytes = std::fs::read(&fixture_path).expect("read fixture");

    let v1_timecnt = u32::from_be_bytes(bytes[32..36].try_into().expect("v1 timecnt"));
    assert_eq!(v1_timecnt, 0);

    let v2_timecnt = u32::from_be_bytes(bytes[86..90].try_into().expect("v2 timecnt"));
    assert_eq!(v2_timecnt, 0);

    assert!(bytes.ends_with(b"\nMST7MDT,M3.2.0,M11.1.0\n"));

    let tzif_zone = Zone::from_tzif("America/Denver", &bytes).expect("tzif zone");
    let tzif_clock = SystemClock::from_zone(tzif_zone);

    let posix_zone = Zone::from_posix("MST7MDT,M3.2.0,M11.1.0").expect("posix zone");
    let posix_clock = SystemClock::from_zone(posix_zone);

    let t_nov_before = PrimitiveDateTime::new(
        Date::from_calendar_date(2026, Month::November, 1).expect("date"),
        Time::from_hms(7, 59, 59).expect("time"),
    )
    .assume_utc();
    let t_nov_at = PrimitiveDateTime::new(
        Date::from_calendar_date(2026, Month::November, 1).expect("date"),
        Time::from_hms(8, 0, 0).expect("time"),
    )
    .assume_utc();
    let t_mar_before = PrimitiveDateTime::new(
        Date::from_calendar_date(2026, Month::March, 8).expect("date"),
        Time::from_hms(8, 59, 59).expect("time"),
    )
    .assume_utc();
    let t_mar_at = PrimitiveDateTime::new(
        Date::from_calendar_date(2026, Month::March, 8).expect("date"),
        Time::from_hms(9, 0, 0).expect("time"),
    )
    .assume_utc();

    let offset_minus_6 = UtcOffset::from_hms(-6, 0, 0).expect("offset -6");
    let offset_minus_7 = UtcOffset::from_hms(-7, 0, 0).expect("offset -7");

    assert_eq!(tzif_clock.offset_at(t_nov_before), offset_minus_6);
    assert_eq!(tzif_clock.offset_at(t_nov_at), offset_minus_7);
    assert_eq!(tzif_clock.offset_at(t_mar_before), offset_minus_7);
    assert_eq!(tzif_clock.offset_at(t_mar_at), offset_minus_6);

    assert_eq!(posix_clock.offset_at(t_nov_before), offset_minus_6);
    assert_eq!(posix_clock.offset_at(t_nov_at), offset_minus_7);
    assert_eq!(posix_clock.offset_at(t_mar_before), offset_minus_7);
    assert_eq!(posix_clock.offset_at(t_mar_at), offset_minus_6);
}

#[test]
fn zone_resolution_failure_warns_once_and_success_does_not() {
    let (err_clock, err_warning) =
        SystemClock::from_resolved(Err("zone data unavailable".to_owned()));
    assert_eq!(usize::from(err_warning.is_some()), 1);
    let sample = PrimitiveDateTime::new(
        Date::from_calendar_date(2026, Month::November, 1).expect("date"),
        Time::from_hms(8, 0, 0).expect("time"),
    )
    .assume_utc();
    assert_eq!(err_clock.offset_at(sample), UtcOffset::UTC);

    let fixture_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/america-denver.tzif");
    let bytes = std::fs::read(&fixture_path).expect("read fixture");
    let ok_zone = Zone::from_tzif("America/Denver", &bytes).expect("tzif zone");
    let (ok_clock, ok_warning) = SystemClock::from_resolved(Ok(ok_zone));
    assert_eq!(usize::from(ok_warning.is_some()), 0);

    let t_nov_before = PrimitiveDateTime::new(
        Date::from_calendar_date(2026, Month::November, 1).expect("date"),
        Time::from_hms(7, 59, 59).expect("time"),
    )
    .assume_utc();
    let t_nov_at = PrimitiveDateTime::new(
        Date::from_calendar_date(2026, Month::November, 1).expect("date"),
        Time::from_hms(8, 0, 0).expect("time"),
    )
    .assume_utc();

    let offset_minus_6 = UtcOffset::from_hms(-6, 0, 0).expect("offset -6");
    let offset_minus_7 = UtcOffset::from_hms(-7, 0, 0).expect("offset -7");

    assert_eq!(ok_clock.offset_at(t_nov_before), offset_minus_6);
    assert_eq!(ok_clock.offset_at(t_nov_at), offset_minus_7);
}

#[test]
fn system_clock_denver_zone_november_fallback() {
    let fixture_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/america-denver.tzif");
    let bytes = std::fs::read(&fixture_path).expect("read fixture");
    let zone = Zone::from_tzif("America/Denver", &bytes).expect("tzif zone");
    let clock = SystemClock::from_zone(zone);

    let t_nov_before = PrimitiveDateTime::new(
        Date::from_calendar_date(2026, Month::November, 1).expect("date"),
        Time::from_hms(7, 59, 59).expect("time"),
    )
    .assume_utc();
    let t_nov_at = PrimitiveDateTime::new(
        Date::from_calendar_date(2026, Month::November, 1).expect("date"),
        Time::from_hms(8, 0, 0).expect("time"),
    )
    .assume_utc();

    let offset_minus_6 = UtcOffset::from_hms(-6, 0, 0).expect("offset -6");
    let offset_minus_7 = UtcOffset::from_hms(-7, 0, 0).expect("offset -7");

    assert_eq!(clock.offset_at(t_nov_before), offset_minus_6);
    assert_eq!(clock.offset_at(t_nov_at), offset_minus_7);
}
