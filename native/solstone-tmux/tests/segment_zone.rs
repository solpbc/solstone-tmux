// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

mod support;

use std::fs;
use std::path::Path;
use std::time::Duration;

use std::sync::Arc;

use serde_json::Value;
use solstone_tmux::clock::{Clock, SystemClock, TestClock, Zone, ZoneSource};
use solstone_tmux::config::DEFAULT_SOURCE;
use solstone_tmux::instance_lock::InstanceLock;
use solstone_tmux::journal::INGEST_PATH;
use solstone_tmux::name::derive_component;
use solstone_tmux::observer::{SegmentLifecycle, SegmentManager, stream_directory};
use solstone_tmux::paths::ensure_private_directory;
use solstone_tmux::segment::{SegmentClose, SegmentState};
use solstone_tmux::storage::{CaptureTimeLoad, capture_time_path, load_capture_time};
use solstone_tmux::sync::{JournalSession, SyncWake};
use support::private_link_peer::{PrivateLinkPeer, parse_multipart_parts};
use support::{RecordingWarnings, TestDirectory, golden_capture};
use time::{Date, Month, PrimitiveDateTime, Time, UtcOffset};

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .expect("runtime")
}

fn projection_example(name: &str) -> Vec<u8> {
    let projection: Value = serde_json::from_slice(
        &fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("vendor/observer-client-contract/projection.openapi.json"),
        )
        .expect("read projection"),
    )
    .expect("parse projection");
    let value = match name {
        "upload_normal" => {
            &projection["paths"][INGEST_PATH]["post"]["responses"]["200"]["content"]["application/json"]
                ["examples"]["normal"]["value"]
        }
        _ => panic!("unknown projection example"),
    };
    serde_json::to_vec(value).expect("serialize projection example")
}

struct TestRow {
    clock: SystemClock,
    instant: time::OffsetDateTime,
    expected_tz: Option<&'static str>,
    expected_offset: i32,
    posix_rule: Option<&'static str>,
}

#[test]
fn segment_zone_table_driven_wire_envelope() {
    runtime().block_on(async {
        let denver_bytes =
            fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/america-denver.tzif"))
                .expect("read denver tzif");
        let kolkata_bytes =
            fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/asia-kolkata.tzif"))
                .expect("read kolkata tzif");

        let t_jan15 = PrimitiveDateTime::new(
            Date::from_calendar_date(2026, Month::January, 15).expect("date"),
            Time::from_hms(18, 0, 0).expect("time"),
        )
        .assume_utc();

        let t_jul15 = PrimitiveDateTime::new(
            Date::from_calendar_date(2026, Month::July, 15).expect("date"),
            Time::from_hms(18, 0, 0).expect("time"),
        )
        .assume_utc();

        let t_jan15_midnight = PrimitiveDateTime::new(
            Date::from_calendar_date(2026, Month::January, 15).expect("date"),
            Time::from_hms(0, 0, 0).expect("time"),
        )
        .assume_utc();

        let rows = vec![
            TestRow {
                clock: SystemClock::from_zone(
                    Zone::from_tzif("America/Denver", &denver_bytes).expect("denver zone"),
                ),
                instant: t_jan15,
                expected_tz: Some("America/Denver"),
                expected_offset: -25200,
                posix_rule: None,
            },
            TestRow {
                clock: SystemClock::from_zone(
                    Zone::from_tzif("America/Denver", &denver_bytes).expect("denver zone"),
                ),
                instant: t_jul15,
                expected_tz: Some("America/Denver"),
                expected_offset: -21600,
                posix_rule: None,
            },
            TestRow {
                clock: SystemClock::from_zone(
                    Zone::from_tzif("Asia/Kolkata", &kolkata_bytes).expect("kolkata zone"),
                ),
                instant: t_jan15_midnight,
                expected_tz: Some("Asia/Kolkata"),
                expected_offset: 19800,
                posix_rule: None,
            },
            TestRow {
                clock: SystemClock::from_resolved(Err("unavailable".to_owned())).0,
                instant: t_jan15_midnight,
                expected_tz: Some("UTC"),
                expected_offset: 0,
                posix_rule: None,
            },
            TestRow {
                clock: SystemClock::from_zone(
                    Zone::from_posix("PST8PDT,M3.2.0,M11.1.0").expect("posix zone"),
                ),
                instant: t_jan15,
                expected_tz: None,
                expected_offset: -28800,
                posix_rule: Some("PST8PDT,M3.2.0,M11.1.0"),
            },
        ];

        for (i, row) in rows.into_iter().enumerate() {
            let offset = row.clock.offset_at(row.instant);
            assert_eq!(
                offset.whole_seconds(),
                row.expected_offset,
                "row {i} offset mismatch"
            );

            let temporary = TestDirectory::new(&format!("segment-zone-row-{i}"));
            ensure_private_directory(temporary.path()).expect("private root");
            let data_root = temporary.path().join("data");
            let stream = derive_component("test.tmux").expect("stream");
            let stream_dir =
                stream_directory(&data_root, &stream, row.instant, offset).expect("stream dir");

            let (_, stem) = solstone_tmux::clock::local_date_and_time(row.instant, offset);
            let mut segment = SegmentState::create(
                &stream_dir,
                &stem,
                row.instant,
                Duration::ZERO,
                offset,
                row.clock.iana_name().as_deref(),
                Duration::from_secs(300),
            )
            .expect("create segment");

            segment
                .append_capture(&golden_capture("main"), 0.25, Duration::from_secs(1))
                .expect("append capture");

            let close = segment
                .finalize(Duration::from_secs(5))
                .expect("finalize segment");
            let finalized_path = match close {
                SegmentClose::Finalized(path) => path,
                SegmentClose::RemovedEmpty => panic!("expected finalized segment, got empty"),
            };

            let finalized_name = finalized_path
                .file_name()
                .expect("segment name")
                .to_str()
                .expect("segment str")
                .to_owned();
            let stream_path = finalized_path.parent().expect("stream dir parent");
            let day_name = stream_path
                .parent()
                .expect("day dir parent")
                .file_name()
                .expect("day name")
                .to_str()
                .expect("day str")
                .to_owned();

            let zone_file_path = capture_time_path(stream_path, &finalized_name);
            let capture_time = match load_capture_time(&zone_file_path) {
                CaptureTimeLoad::Present(ct) => ct,
                other => panic!("expected Present CaptureTime, got {other:?}"),
            };

            let mut files = Vec::new();
            for entry in fs::read_dir(&finalized_path).expect("read finalized dir") {
                let entry = entry.expect("entry");
                files.push(entry.path());
            }
            files.sort();

            let peer = PrivateLinkPeer::start().await;
            let lock = InstanceLock::acquire(temporary.path()).expect("acquire lock");
            let credential = peer.credential();
            let refresh = solstone_tmux::journal_version::VersionRefreshState::new(
                temporary.path().to_path_buf(),
                temporary.path().to_path_buf(),
                credential.instance_id.clone(),
                &credential.ca_fp_prefix,
                lock.identity().clone(),
            );
            let session =
                JournalSession::start(credential, temporary.path().to_path_buf(), refresh)
                    .await
                    .expect("start session");

            peer.enqueue_response(200, projection_example("upload_normal"));
            session
                .journal()
                .ingest_upload(
                    &day_name,
                    &finalized_name,
                    files,
                    DEFAULT_SOURCE,
                    Some(capture_time),
                )
                .await
                .expect("ingest upload");

            let requests = peer
                .requests()
                .into_iter()
                .filter(|r| {
                    !r.path_without_query().starts_with("/app/network/api/")
                        && r.path_without_query() != "/api/system/about"
                })
                .collect::<Vec<_>>();
            assert_eq!(requests.len(), 1);

            let content_type = requests[0].header("content-type").expect("content type");
            let parts =
                parse_multipart_parts(content_type, requests[0].body()).expect("parse multipart");
            let envelope_bytes = parts[0].body;
            let envelope: Value = serde_json::from_slice(envelope_bytes).expect("envelope json");

            assert_eq!(envelope["source"], DEFAULT_SOURCE);
            let files_arr = envelope["files"].as_array().expect("files array");
            assert_eq!(files_arr.len(), 1);
            let file_obj = files_arr[0].as_object().expect("file obj");
            assert_eq!(file_obj.len(), 1);
            assert_eq!(
                file_obj.get("submitted").expect("submitted"),
                "tmux_main_screen.jsonl"
            );

            let meta_obj = envelope["meta"].as_object().expect("meta object");
            let meta_offset_sec = meta_obj
                .get("utc_offset_seconds")
                .expect("utc_offset_seconds")
                .as_i64()
                .expect("offset i64") as i32;

            let env_day = envelope["day"].as_str().expect("envelope day str");
            let env_seg = envelope["segment"].as_str().expect("envelope segment str");
            let year: i32 = env_day[0..4].parse().expect("year");
            let month: u8 = env_day[4..6].parse().expect("month");
            let day_num: u8 = env_day[6..8].parse().expect("day");
            let hour: u8 = env_seg[0..2].parse().expect("hour");
            let minute: u8 = env_seg[2..4].parse().expect("minute");
            let second: u8 = env_seg[4..6].parse().expect("second");
            let date = Date::from_calendar_date(
                year,
                Month::try_from(month).expect("valid month"),
                day_num,
            )
            .expect("valid date");
            let time = Time::from_hms(hour, minute, second).expect("valid time");
            let civil = PrimitiveDateTime::new(date, time).assume_offset(
                UtcOffset::from_whole_seconds(meta_offset_sec).expect("valid utc offset"),
            );
            assert_eq!(civil, row.instant, "civil time must match injected instant");

            if let Some(expected_tz) = row.expected_tz {
                assert_eq!(meta_obj.len(), 2);
                assert_eq!(
                    meta_obj.get("tz").expect("tz").as_str().expect("tz str"),
                    expected_tz
                );
                assert_eq!(meta_offset_sec, row.expected_offset);
            } else {
                assert_eq!(meta_obj.len(), 1);
                assert!(meta_obj.get("tz").is_none());
                assert_eq!(meta_offset_sec, row.expected_offset);
                let raw_envelope =
                    std::str::from_utf8(envelope_bytes).expect("valid utf-8 envelope");
                assert!(!raw_envelope.contains("Local"));
                assert!(!raw_envelope.contains("Etc/Unknown"));
                if let Some(rule) = row.posix_rule {
                    assert!(!raw_envelope.contains(rule));
                }
            }

            session.shutdown().await.expect("shutdown session");
            peer.shutdown().await;
        }
    });
}

#[test]
fn fixtures_produce_expected_offsets_and_local_civil_times() {
    let berlin_bytes =
        fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/europe-berlin.tzif"))
            .expect("read berlin tzif");
    let auckland_bytes =
        fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/pacific-auckland.tzif"))
            .expect("read auckland tzif");
    let kolkata_bytes =
        fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/asia-kolkata.tzif"))
            .expect("read kolkata tzif");

    let berlin = Zone::from_tzif("Europe/Berlin", &berlin_bytes).expect("berlin zone");
    let auckland = Zone::from_tzif("Pacific/Auckland", &auckland_bytes).expect("auckland zone");
    let kolkata = Zone::from_tzif("Asia/Kolkata", &kolkata_bytes).expect("kolkata zone");

    let t_berlin1 = PrimitiveDateTime::new(
        Date::from_calendar_date(2026, Month::October, 25).expect("date"),
        Time::from_hms(0, 30, 0).expect("time"),
    )
    .assume_utc();
    let off1 = berlin.offset_at(t_berlin1);
    assert_eq!(off1.whole_seconds(), 7200);
    let (d1, s1) = solstone_tmux::clock::local_date_and_time(t_berlin1, off1);
    assert_eq!(d1, "20261025");
    assert_eq!(s1, "023000");

    let t_berlin2 = PrimitiveDateTime::new(
        Date::from_calendar_date(2026, Month::October, 25).expect("date"),
        Time::from_hms(1, 30, 0).expect("time"),
    )
    .assume_utc();
    let off2 = berlin.offset_at(t_berlin2);
    assert_eq!(off2.whole_seconds(), 3600);
    let (_, s2) = solstone_tmux::clock::local_date_and_time(t_berlin2, off2);
    assert_eq!(s2, "023000");

    let t_auckland = PrimitiveDateTime::new(
        Date::from_calendar_date(2026, Month::September, 29).expect("date"),
        Time::from_hms(18, 25, 0).expect("time"),
    )
    .assume_utc();
    let off_auckland = auckland.offset_at(t_auckland);
    assert_eq!(off_auckland.whole_seconds(), 46800);
    let (d_auckland, s_auckland) =
        solstone_tmux::clock::local_date_and_time(t_auckland, off_auckland);
    assert_eq!(d_auckland, "20260930");
    assert_eq!(s_auckland, "072500");

    let t_kolkata1 = PrimitiveDateTime::new(
        Date::from_calendar_date(2026, Month::September, 29).expect("date"),
        Time::from_hms(18, 20, 0).expect("time"),
    )
    .assume_utc();
    let off_k1 = kolkata.offset_at(t_kolkata1);
    assert_eq!(off_k1.whole_seconds(), 19800);
    let (d_k1, s_k1) = solstone_tmux::clock::local_date_and_time(t_kolkata1, off_k1);
    assert_eq!(d_k1, "20260929");
    assert_eq!(s_k1, "235000");

    let t_kolkata2 = PrimitiveDateTime::new(
        Date::from_calendar_date(2026, Month::September, 29).expect("date"),
        Time::from_hms(18, 25, 0).expect("time"),
    )
    .assume_utc();
    let off_k2 = kolkata.offset_at(t_kolkata2);
    assert_eq!(off_k2.whole_seconds(), 19800);
    let (_, s_k2) = solstone_tmux::clock::local_date_and_time(t_kolkata2, off_k2);
    assert_eq!(s_k2, "235500");
}

#[test]
fn segment_zone_backlog_uses_stored_zone_not_current_zone() {
    runtime().block_on(async {
        let denver_bytes =
            fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/america-denver.tzif"))
                .expect("read denver tzif");
        let kolkata_bytes =
            fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/asia-kolkata.tzif"))
                .expect("read kolkata tzif");

        let t_jan15 = PrimitiveDateTime::new(
            Date::from_calendar_date(2026, Month::January, 15).expect("date"),
            Time::from_hms(18, 0, 0).expect("time"),
        )
        .assume_utc();

        let clock_denver = SystemClock::from_zone(
            Zone::from_tzif("America/Denver", &denver_bytes).expect("denver zone"),
        );
        let offset_denver = clock_denver.offset_at(t_jan15);
        assert_eq!(offset_denver.whole_seconds(), -25200);

        let temporary = TestDirectory::new("segment-zone-backlog");
        ensure_private_directory(temporary.path()).expect("private root");
        let data_root = temporary.path().join("data");
        let stream = derive_component("test.tmux").expect("stream");
        let stream_dir =
            stream_directory(&data_root, &stream, t_jan15, offset_denver).expect("stream dir");

        let (_, stem) = solstone_tmux::clock::local_date_and_time(t_jan15, offset_denver);
        let mut segment = SegmentState::create(
            &stream_dir,
            &stem,
            t_jan15,
            Duration::ZERO,
            offset_denver,
            clock_denver.iana_name().as_deref(),
            Duration::from_secs(300),
        )
        .expect("create segment");

        segment
            .append_capture(&golden_capture("main"), 0.25, Duration::from_secs(1))
            .expect("append capture");

        let close = segment
            .finalize(Duration::from_secs(5))
            .expect("finalize segment");
        let finalized_path = match close {
            SegmentClose::Finalized(path) => path,
            SegmentClose::RemovedEmpty => panic!("expected finalized segment"),
        };

        let clock_kolkata = SystemClock::from_zone(
            Zone::from_tzif("Asia/Kolkata", &kolkata_bytes).expect("kolkata zone"),
        );
        assert_ne!(
            clock_kolkata.offset_at(t_jan15).whole_seconds(),
            -25200,
            "Kolkata offset must differ from Denver offset"
        );

        let finalized_name = finalized_path
            .file_name()
            .expect("segment name")
            .to_str()
            .expect("segment str")
            .to_owned();
        let stream_path = finalized_path.parent().expect("stream dir parent");
        let day_name = stream_path
            .parent()
            .expect("day dir parent")
            .file_name()
            .expect("day name")
            .to_str()
            .expect("day str")
            .to_owned();

        let zone_file_path = capture_time_path(stream_path, &finalized_name);
        let capture_time = match load_capture_time(&zone_file_path) {
            CaptureTimeLoad::Present(ct) => ct,
            other => panic!("expected Present CaptureTime, got {other:?}"),
        };

        let mut files = Vec::new();
        for entry in fs::read_dir(&finalized_path).expect("read finalized dir") {
            let entry = entry.expect("entry");
            files.push(entry.path());
        }
        files.sort();

        let peer = PrivateLinkPeer::start().await;
        let lock = InstanceLock::acquire(temporary.path()).expect("acquire lock");
        let credential = peer.credential();
        let refresh = solstone_tmux::journal_version::VersionRefreshState::new(
            temporary.path().to_path_buf(),
            temporary.path().to_path_buf(),
            credential.instance_id.clone(),
            &credential.ca_fp_prefix,
            lock.identity().clone(),
        );
        let session = JournalSession::start(credential, temporary.path().to_path_buf(), refresh)
            .await
            .expect("start session");

        peer.enqueue_response(200, projection_example("upload_normal"));
        session
            .journal()
            .ingest_upload(
                &day_name,
                &finalized_name,
                files,
                DEFAULT_SOURCE,
                Some(capture_time),
            )
            .await
            .expect("ingest upload");

        let requests = peer
            .requests()
            .into_iter()
            .filter(|r| {
                !r.path_without_query().starts_with("/app/network/api/")
                    && r.path_without_query() != "/api/system/about"
            })
            .collect::<Vec<_>>();
        assert_eq!(requests.len(), 1);

        let content_type = requests[0].header("content-type").expect("content type");
        let parts = parse_multipart_parts(content_type, requests[0].body()).expect("parse parts");
        let envelope: Value = serde_json::from_slice(parts[0].body).expect("parse envelope");

        let meta_obj = envelope["meta"].as_object().expect("meta object");
        assert_eq!(meta_obj.len(), 2);
        assert_eq!(
            meta_obj.get("tz").expect("tz").as_str().expect("tz str"),
            "America/Denver"
        );
        let meta_offset_sec = meta_obj
            .get("utc_offset_seconds")
            .expect("utc_offset_seconds")
            .as_i64()
            .expect("offset i64") as i32;
        assert_eq!(meta_offset_sec, -25200);

        let env_day = envelope["day"].as_str().expect("envelope day str");
        let env_seg = envelope["segment"].as_str().expect("envelope segment str");
        let year: i32 = env_day[0..4].parse().expect("year");
        let month: u8 = env_day[4..6].parse().expect("month");
        let day_num: u8 = env_day[6..8].parse().expect("day");
        let hour: u8 = env_seg[0..2].parse().expect("hour");
        let minute: u8 = env_seg[2..4].parse().expect("minute");
        let second: u8 = env_seg[4..6].parse().expect("second");
        let date =
            Date::from_calendar_date(year, Month::try_from(month).expect("valid month"), day_num)
                .expect("valid date");
        let time = Time::from_hms(hour, minute, second).expect("valid time");
        let civil = PrimitiveDateTime::new(date, time).assume_offset(
            UtcOffset::from_whole_seconds(meta_offset_sec).expect("valid utc offset"),
        );
        assert_eq!(civil, t_jan15, "civil time must match Denver instant");

        session.shutdown().await.expect("shutdown session");
        peer.shutdown().await;
    });
}

struct TravelZoneSource {
    step: usize,
    kolkata: Zone,
    auckland: Zone,
}

impl TravelZoneSource {
    fn new() -> Self {
        let kolkata_bytes =
            fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/asia-kolkata.tzif"))
                .expect("read kolkata tzif");
        let auckland_bytes = fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/pacific-auckland.tzif"),
        )
        .expect("read auckland tzif");
        Self {
            step: 0,
            kolkata: Zone::from_tzif("Asia/Kolkata", &kolkata_bytes).expect("kolkata zone"),
            auckland: Zone::from_tzif("Pacific/Auckland", &auckland_bytes).expect("auckland zone"),
        }
    }
}

impl ZoneSource for TravelZoneSource {
    fn read(&mut self) -> Result<Zone, String> {
        let zone = if self.step == 0 {
            self.kolkata.clone()
        } else {
            self.auckland.clone()
        };
        self.step += 1;
        Ok(zone)
    }
}

#[test]
fn travel_reads_each_segments_zone() {
    let t0 = PrimitiveDateTime::new(
        Date::from_calendar_date(2026, Month::September, 29).expect("date"),
        Time::from_hms(18, 20, 0).expect("time"),
    )
    .assume_utc();
    let clock = Arc::new(TestClock::new(t0, Duration::ZERO, UtcOffset::UTC));
    let temporary = TestDirectory::new("travel-reads-zone");
    let data_root = temporary.path().join("data");
    let stream = derive_component("test.tmux").expect("stream");
    let interval = Duration::from_secs(300);

    let mut manager = SegmentManager::start(
        data_root.clone(),
        stream.clone(),
        clock.as_ref(),
        interval,
        SyncWake::default(),
        Box::new(TravelZoneSource::new()),
        Arc::new(RecordingWarnings::default()),
    )
    .expect("start manager");

    let day_kolkata = data_root
        .join("captures")
        .join("20260929")
        .join("test.tmux");
    let first_incomplete = day_kolkata.join("235000.incomplete");
    assert!(first_incomplete.is_dir());

    let first_meta = manager.segment_mut().metadata();
    assert_eq!(first_meta.tz, Some("Asia/Kolkata".to_owned()));
    assert_eq!(first_meta.local_offset_seconds, 19800);

    manager
        .process_poll(&[golden_capture("main")], t0, Duration::ZERO, interval)
        .expect("poll at t0");

    let t1 = PrimitiveDateTime::new(
        Date::from_calendar_date(2026, Month::September, 29).expect("date"),
        Time::from_hms(18, 25, 0).expect("time"),
    )
    .assume_utc();
    clock.set_wall(t1);
    clock.set_monotonic(interval);

    manager
        .process_poll(&[golden_capture("main")], t1, interval, interval)
        .expect("poll at t1");

    let first_finalized = day_kolkata.join("235000_300");
    assert!(first_finalized.is_dir());
    let first_zone = load_capture_time(&capture_time_path(&day_kolkata, "235000_300"));
    assert_eq!(
        first_zone,
        CaptureTimeLoad::Present(solstone_tmux::storage::CaptureTime {
            tz: Some("Asia/Kolkata".to_owned()),
            utc_offset_seconds: 19800,
        })
    );

    let day_auckland = data_root
        .join("captures")
        .join("20260930")
        .join("test.tmux");
    let second_incomplete = day_auckland.join("072500.incomplete");
    assert!(second_incomplete.is_dir());

    let close = manager
        .segment_mut()
        .finalize(interval * 2)
        .expect("finalize second segment");
    let second_finalized = match close {
        SegmentClose::Finalized(path) => path,
        SegmentClose::RemovedEmpty => panic!("expected finalized segment"),
    };
    assert_eq!(
        second_finalized.file_name().unwrap().to_str().unwrap(),
        "072500_300"
    );
    assert!(second_finalized.starts_with(&day_auckland));

    let second_zone = load_capture_time(&capture_time_path(&day_auckland, "072500_300"));
    assert_eq!(
        second_zone,
        CaptureTimeLoad::Present(solstone_tmux::storage::CaptureTime {
            tz: Some("Pacific/Auckland".to_owned()),
            utc_offset_seconds: 46800,
        })
    );
}

struct CountingZoneSource {
    reads: Arc<std::sync::atomic::AtomicUsize>,
    zone: Zone,
}

impl CountingZoneSource {
    fn new() -> Self {
        let berlin_bytes =
            fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/europe-berlin.tzif"))
                .expect("read berlin tzif");
        Self {
            reads: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            zone: Zone::from_tzif("Europe/Berlin", &berlin_bytes).expect("berlin zone"),
        }
    }
}

impl ZoneSource for CountingZoneSource {
    fn read(&mut self) -> Result<Zone, String> {
        let count = self
            .reads
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if count < 2 {
            Ok(self.zone.clone())
        } else {
            Err("simulated zone failure".to_owned())
        }
    }
}

#[test]
fn production_constructor_reads_the_zone_once_per_segment() {
    let t0 = PrimitiveDateTime::new(
        Date::from_calendar_date(2026, Month::October, 25).expect("date"),
        Time::from_hms(0, 0, 0).expect("time"),
    )
    .assume_utc();
    let clock = Arc::new(TestClock::new(t0, Duration::ZERO, UtcOffset::UTC));
    let temporary = TestDirectory::new("counting-zone-source");
    let data_root = temporary.path().join("data");
    let stream = derive_component("test.tmux").expect("stream");
    let interval = Duration::from_secs(300);

    let source = CountingZoneSource::new();
    let reads = Arc::clone(&source.reads);

    let mut manager = SegmentManager::start(
        data_root,
        stream,
        clock.as_ref(),
        interval,
        SyncWake::default(),
        Box::new(source),
        Arc::new(RecordingWarnings::default()),
    )
    .expect("start manager");

    assert_eq!(reads.load(std::sync::atomic::Ordering::Relaxed), 1);

    for i in 1..=5 {
        let t = t0 + Duration::from_secs(i * 5);
        clock.set_wall(t);
        clock.set_monotonic(Duration::from_secs(i * 5));
        manager
            .process_poll(
                &[golden_capture("main")],
                t,
                Duration::from_secs(i * 5),
                interval,
            )
            .expect("poll");
        assert_eq!(reads.load(std::sync::atomic::Ordering::Relaxed), 1);
    }

    let t_rot1 = t0 + interval;
    clock.set_wall(t_rot1);
    clock.set_monotonic(interval);
    manager
        .process_poll(&[golden_capture("main")], t_rot1, interval, interval)
        .expect("poll rotate 1");

    assert_eq!(reads.load(std::sync::atomic::Ordering::Relaxed), 2);

    let t_rot2 = t_rot1 + interval;
    clock.set_wall(t_rot2);
    clock.set_monotonic(interval * 2);
    manager
        .process_poll(&[golden_capture("main")], t_rot2, interval * 2, interval)
        .expect("poll rotate 2");

    assert_eq!(reads.load(std::sync::atomic::Ordering::Relaxed), 3);
    assert_eq!(
        manager.segment_mut().metadata().tz,
        Some("Europe/Berlin".to_owned())
    );
}

struct FallbackZoneSource {
    step: usize,
    berlin: Zone,
}

impl FallbackZoneSource {
    fn new() -> Self {
        let berlin_bytes =
            fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/europe-berlin.tzif"))
                .expect("read berlin tzif");
        Self {
            step: 0,
            berlin: Zone::from_tzif("Europe/Berlin", &berlin_bytes).expect("berlin zone"),
        }
    }
}

impl ZoneSource for FallbackZoneSource {
    fn read(&mut self) -> Result<Zone, String> {
        let res = if self.step == 0 {
            Ok(self.berlin.clone())
        } else {
            Err("zone read failed".to_owned())
        };
        self.step += 1;
        res
    }
}

#[test]
fn failed_read_reuses_the_previous_zone_at_the_new_start() {
    let t0 = PrimitiveDateTime::new(
        Date::from_calendar_date(2026, Month::October, 25).expect("date"),
        Time::from_hms(0, 30, 0).expect("time"),
    )
    .assume_utc();
    let clock = Arc::new(TestClock::new(
        t0,
        Duration::ZERO,
        UtcOffset::from_hms(2, 0, 0).expect("offset +2"),
    ));
    let temporary = TestDirectory::new("failed-read-reuse-zone");
    let data_root = temporary.path().join("data");
    let stream = derive_component("test.tmux").expect("stream");
    let interval = Duration::from_secs(300);
    let warnings = Arc::new(RecordingWarnings::default());

    let mut manager = SegmentManager::start(
        data_root.clone(),
        stream.clone(),
        clock.as_ref(),
        interval,
        SyncWake::default(),
        Box::new(FallbackZoneSource::new()),
        warnings.clone(),
    )
    .expect("start manager");

    manager
        .process_poll(&[golden_capture("main")], t0, Duration::ZERO, interval)
        .expect("poll t0");
    assert_eq!(warnings.messages().len(), 0);

    let t1 = PrimitiveDateTime::new(
        Date::from_calendar_date(2026, Month::October, 25).expect("date"),
        Time::from_hms(1, 30, 0).expect("time"),
    )
    .assume_utc();
    clock.set_wall(t1);
    clock.set_monotonic(interval);

    manager
        .process_poll(&[golden_capture("main")], t1, interval, interval)
        .expect("poll t1");

    let day_stream_dir = data_root
        .join("captures")
        .join("20261025")
        .join("test.tmux");
    let second_incomplete = day_stream_dir.join("023001.incomplete");
    assert!(second_incomplete.is_dir());

    let second_meta = manager.segment_mut().metadata();
    assert_eq!(second_meta.start_wall_unix_nanos, t1.unix_timestamp_nanos());
    assert_eq!(second_meta.local_offset_seconds, 3600);
    assert_eq!(second_meta.tz, Some("Europe/Berlin".to_owned()));
    assert_eq!(warnings.messages().len(), 1);

    let t2 = PrimitiveDateTime::new(
        Date::from_calendar_date(2026, Month::October, 25).expect("date"),
        Time::from_hms(1, 35, 0).expect("time"),
    )
    .assume_utc();
    clock.set_wall(t2);
    clock.set_monotonic(interval * 2);

    manager
        .process_poll(&[golden_capture("main")], t2, interval * 2, interval)
        .expect("poll t2");

    assert_eq!(warnings.messages().len(), 1);
    let third_meta = manager.segment_mut().metadata();
    assert_eq!(third_meta.tz, Some("Europe/Berlin".to_owned()));
    assert_eq!(third_meta.local_offset_seconds, 3600);
}

struct FlappingZoneSource {
    step: usize,
    zone: Zone,
}

impl ZoneSource for FlappingZoneSource {
    fn read(&mut self) -> Result<Zone, String> {
        let res = match self.step {
            0 => Err("fail 1".into()),
            1 => Err("fail 2".into()),
            2 => Ok(self.zone.clone()),
            3 => Err("fail 3".into()),
            _ => Ok(self.zone.clone()),
        };
        self.step += 1;
        res
    }
}

#[test]
fn zone_failure_warns_on_each_transition_into_failure() {
    let t0 = PrimitiveDateTime::new(
        Date::from_calendar_date(2026, Month::July, 15).expect("date"),
        Time::from_hms(12, 0, 0).expect("time"),
    )
    .assume_utc();
    let clock = Arc::new(TestClock::new(t0, Duration::ZERO, UtcOffset::UTC));
    let temporary = TestDirectory::new("flapping-zone-warnings");
    let data_root = temporary.path().join("data");
    let stream = derive_component("test.tmux").expect("stream");
    let interval = Duration::from_secs(300);

    let warnings = Arc::new(RecordingWarnings::default());
    let flapping = FlappingZoneSource {
        step: 0,
        zone: Zone::fixed(UtcOffset::UTC),
    };

    let mut manager = SegmentManager::start(
        data_root,
        stream,
        clock.as_ref(),
        interval,
        SyncWake::default(),
        Box::new(flapping),
        warnings.clone(),
    )
    .expect("start manager");

    assert_eq!(manager.segment_mut().metadata().tz, Some("UTC".to_owned()));
    assert_eq!(warnings.messages().len(), 1);
    assert!(warnings.messages()[0].contains("fail 1"));

    manager
        .process_poll(&[golden_capture("main")], t0, Duration::ZERO, interval)
        .expect("poll t0");

    let t1 = t0 + interval;
    clock.set_wall(t1);
    clock.set_monotonic(interval);
    manager
        .process_poll(&[golden_capture("main")], t1, interval, interval)
        .expect("poll t1");

    assert_eq!(warnings.messages().len(), 1);

    let t2 = t1 + interval;
    clock.set_wall(t2);
    clock.set_monotonic(interval * 2);
    manager
        .process_poll(&[golden_capture("main")], t2, interval * 2, interval)
        .expect("poll t2");

    assert_eq!(warnings.messages().len(), 1);

    let t3 = t2 + interval;
    clock.set_wall(t3);
    clock.set_monotonic(interval * 3);
    manager
        .process_poll(&[golden_capture("main")], t3, interval * 3, interval)
        .expect("poll t3");

    assert_eq!(warnings.messages().len(), 2);
    assert!(warnings.messages()[1].contains("fail 3"));
}
