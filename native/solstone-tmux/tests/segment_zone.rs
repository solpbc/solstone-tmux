// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

mod support;

use std::fs;
use std::path::Path;
use std::time::Duration;

use serde_json::Value;
use solstone_tmux::clock::{Clock, SystemClock, Zone};
use solstone_tmux::config::DEFAULT_SOURCE;
use solstone_tmux::instance_lock::InstanceLock;
use solstone_tmux::journal::INGEST_PATH;
use solstone_tmux::name::derive_component;
use solstone_tmux::observer::stream_directory;
use solstone_tmux::paths::ensure_private_directory;
use solstone_tmux::segment::{SegmentClose, SegmentState};
use solstone_tmux::storage::{CaptureTimeLoad, capture_time_path, load_capture_time};
use solstone_tmux::sync::JournalSession;
use support::private_link_peer::{PrivateLinkPeer, parse_multipart_parts};
use support::{TestDirectory, golden_capture};
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

            let mut segment = SegmentState::create(
                &stream_dir,
                row.instant,
                Duration::ZERO,
                offset,
                row.clock.iana_name().as_deref(),
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
                .filter(|r| !r.path_without_query().starts_with("/app/network/api/"))
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

        let mut segment = SegmentState::create(
            &stream_dir,
            t_jan15,
            Duration::ZERO,
            offset_denver,
            clock_denver.iana_name().as_deref(),
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
            .filter(|r| !r.path_without_query().starts_with("/app/network/api/"))
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
