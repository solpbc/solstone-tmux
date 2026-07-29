// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

mod support;

use std::fs;
use std::os::unix::fs::symlink;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};

use solstone_tmux_observer::journal::{
    ListingFileStatus, SegmentFile, SegmentItem, SegmentsEnvelope, inventory_files,
};
use solstone_tmux_observer::name::{DerivedName, derive_component};
use solstone_tmux_observer::sync::{RetentionOutcome, SegmentCandidate, retain_custodied_segment};
use support::TestDirectory;
use time::{Date, Month};

const STREAM: &str = "host.tmux";
const SEGMENT: &str = "120000_300";
const FILE: &str = "tmux_main_screen.jsonl";

#[test]
fn negative_retention_returns_before_traversing() {
    run(async {
        let temporary = TestDirectory::new("retention-disabled");
        let outside = create_segment(temporary.path(), "outside", "20260701", STREAM, SEGMENT);
        let alias = temporary.path().join("captures");
        symlink(temporary.path().join("outside"), &alias).expect("create captures alias");
        let candidate = SegmentCandidate::new("20260701", STREAM, SEGMENT);
        let listing = listing_for(&outside, &candidate, ListingFileStatus::Present).await;

        let outcome = retain_custodied_segment(
            &alias,
            &stream(),
            today(),
            -1,
            &candidate,
            SEGMENT,
            &listing,
        )
        .await;

        assert_eq!(outcome, RetentionOutcome::Disabled);
        assert!(outside.is_dir());
    });
}

#[test]
fn zero_retention_deletes_older_segments_but_skips_today() {
    run(async {
        let temporary = TestDirectory::new("retention-zero");
        let captures = temporary.path().join("captures");
        let old = create_segment(temporary.path(), "captures", "20260709", STREAM, SEGMENT);
        let current = create_segment(temporary.path(), "captures", "20260710", STREAM, SEGMENT);
        let old_candidate = SegmentCandidate::new("20260709", STREAM, SEGMENT);
        let current_candidate = SegmentCandidate::new("20260710", STREAM, SEGMENT);

        let old_listing = listing_for(&old, &old_candidate, ListingFileStatus::Present).await;
        let current_listing =
            listing_for(&current, &current_candidate, ListingFileStatus::Present).await;
        assert_eq!(
            retain_custodied_segment(
                &captures,
                &stream(),
                today(),
                0,
                &old_candidate,
                SEGMENT,
                &old_listing,
            )
            .await,
            RetentionOutcome::Deleted
        );
        assert_eq!(
            retain_custodied_segment(
                &captures,
                &stream(),
                today(),
                0,
                &current_candidate,
                SEGMENT,
                &current_listing,
            )
            .await,
            RetentionOutcome::Ineligible
        );
        assert!(!old.exists());
        assert!(current.is_dir());
    });
}

#[test]
fn positive_retention_honors_the_cutoff_boundary() {
    run(async {
        let temporary = TestDirectory::new("retention-positive");
        let captures = temporary.path().join("captures");
        let older = create_segment(temporary.path(), "captures", "20260702", STREAM, SEGMENT);
        let cutoff = create_segment(temporary.path(), "captures", "20260703", STREAM, SEGMENT);
        let older_candidate = SegmentCandidate::new("20260702", STREAM, SEGMENT);
        let cutoff_candidate = SegmentCandidate::new("20260703", STREAM, SEGMENT);
        let older_listing =
            listing_for(&older, &older_candidate, ListingFileStatus::Processed).await;
        let cutoff_listing =
            listing_for(&cutoff, &cutoff_candidate, ListingFileStatus::Processed).await;

        assert_eq!(
            retain_custodied_segment(
                &captures,
                &stream(),
                today(),
                7,
                &older_candidate,
                SEGMENT,
                &older_listing,
            )
            .await,
            RetentionOutcome::Deleted
        );
        assert_eq!(
            retain_custodied_segment(
                &captures,
                &stream(),
                today(),
                7,
                &cutoff_candidate,
                SEGMENT,
                &cutoff_listing,
            )
            .await,
            RetentionOutcome::Ineligible
        );
        assert!(!older.exists());
        assert!(cutoff.is_dir());
    });
}

#[test]
fn custody_is_required_before_deletion() {
    run(async {
        let temporary = TestDirectory::new("retention-custody");
        let captures = temporary.path().join("captures");
        let segment = create_segment(temporary.path(), "captures", "20260701", STREAM, SEGMENT);
        let candidate = SegmentCandidate::new("20260701", STREAM, SEGMENT);
        let listing = listing_for(&segment, &candidate, ListingFileStatus::Missing).await;

        let outcome = retain_custodied_segment(
            &captures,
            &stream(),
            today(),
            0,
            &candidate,
            SEGMENT,
            &listing,
        )
        .await;

        assert_eq!(outcome, RetentionOutcome::Retained);
        assert!(segment.is_dir());
    });
}

#[test]
fn traversal_candidate_cannot_escape_its_stream() {
    run(async {
        let temporary = TestDirectory::new("retention-traversal");
        let captures = temporary.path().join("captures");
        let outside = create_segment(
            temporary.path(),
            "captures/20260701",
            "outside",
            STREAM,
            SEGMENT,
        );
        let candidate = SegmentCandidate::new("20260701", STREAM, "../outside");
        let listing = SegmentsEnvelope {
            items: Vec::new(),
            total: 0,
            protocol_version: 2,
        };

        let outcome = retain_custodied_segment(
            &captures,
            &stream(),
            today(),
            0,
            &candidate,
            SEGMENT,
            &listing,
        )
        .await;

        assert_eq!(outcome, RetentionOutcome::Retained);
        assert!(outside.is_dir());
    });
}

#[test]
fn symlink_and_special_file_retain_the_whole_segment() {
    run(async {
        let temporary = TestDirectory::new("retention-file-types");
        let captures = temporary.path().join("captures");
        let symlink_segment = segment_dir(&captures, "20260701", STREAM, "120000_300");
        fs::create_dir_all(&symlink_segment).expect("create symlink segment");
        let referent = temporary.path().join("outside.jsonl");
        fs::write(&referent, b"outside\n").expect("write symlink referent");
        symlink(&referent, symlink_segment.join(FILE)).expect("create segment symlink");

        let socket_segment = segment_dir(&captures, "20260701", STREAM, "120500_300");
        fs::create_dir_all(&socket_segment).expect("create socket segment");
        let socket_path = socket_segment.join(FILE);
        let socket_source = temporary.path().join("s");
        let _listener = UnixListener::bind(&socket_source).expect("bind fixture socket");
        fs::rename(socket_source, &socket_path).expect("move socket into segment");

        for (segment_name, segment) in [
            ("120000_300", symlink_segment),
            ("120500_300", socket_segment),
        ] {
            let candidate = SegmentCandidate::new("20260701", STREAM, segment_name);
            let listing = SegmentsEnvelope {
                items: Vec::new(),
                total: 0,
                protocol_version: 2,
            };
            assert_eq!(
                retain_custodied_segment(
                    &captures,
                    &stream(),
                    today(),
                    0,
                    &candidate,
                    segment_name,
                    &listing,
                )
                .await,
                RetentionOutcome::Retained
            );
            assert!(segment.is_dir());
        }
        assert_eq!(
            fs::read(&referent).expect("read symlink referent"),
            b"outside\n"
        );
    });
}

#[test]
fn reserved_and_unrelated_entries_are_never_touched() {
    run(async {
        let temporary = TestDirectory::new("retention-unrelated");
        let captures = temporary.path().join("captures");
        let finalized = create_segment(temporary.path(), "captures", "20260701", STREAM, SEGMENT);
        let stream_root = finalized.parent().expect("stream root");
        let incomplete = stream_root.join("121000.incomplete");
        let failed = stream_root.join("121500_300.failed");
        let metadata = stream_root.join("121000.incomplete.meta");
        fs::create_dir(&incomplete).expect("create incomplete");
        fs::create_dir(&failed).expect("create failed");
        fs::write(&metadata, b"metadata\n").expect("write metadata");
        let other_stream = create_segment(
            temporary.path(),
            "captures",
            "20260701",
            "other.tmux",
            SEGMENT,
        );
        let non_date = create_segment(temporary.path(), "captures", "not-a-day", STREAM, SEGMENT);
        let candidate = SegmentCandidate::new("20260701", STREAM, SEGMENT);
        let listing = listing_for(&finalized, &candidate, ListingFileStatus::Present).await;

        assert_eq!(
            retain_custodied_segment(
                &captures,
                &stream(),
                today(),
                0,
                &candidate,
                SEGMENT,
                &listing,
            )
            .await,
            RetentionOutcome::Deleted
        );
        assert!(incomplete.is_dir());
        assert!(failed.is_dir());
        assert!(metadata.is_file());
        assert!(other_stream.is_dir());
        assert!(non_date.is_dir());
    });
}

fn run(future: impl std::future::Future<Output = ()>) {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("build test runtime")
        .block_on(future);
}

fn today() -> Date {
    Date::from_calendar_date(2026, Month::July, 10).expect("test date")
}

fn stream() -> DerivedName {
    derive_component(STREAM).expect("stream name")
}

fn create_segment(root: &Path, captures: &str, day: &str, stream: &str, segment: &str) -> PathBuf {
    let path = root.join(captures).join(day).join(stream).join(segment);
    fs::create_dir_all(&path).expect("create segment");
    fs::write(path.join(FILE), b"capture fixture\n").expect("write segment file");
    path
}

fn segment_dir(captures: &Path, day: &str, stream: &str, segment: &str) -> PathBuf {
    captures.join(day).join(stream).join(segment)
}

async fn listing_for(
    segment: &Path,
    candidate: &SegmentCandidate,
    status: ListingFileStatus,
) -> SegmentsEnvelope {
    let paths = fs::read_dir(segment)
        .expect("read fixture segment")
        .map(|entry| entry.expect("read fixture entry").path())
        .collect::<Vec<_>>();
    let local = inventory_files(paths).await.expect("inventory fixture");
    let files = local
        .into_iter()
        .map(|file| SegmentFile {
            name: file.name,
            size: file.size,
            sha256: file.sha256,
            status,
            submitted_name: None,
        })
        .collect();
    SegmentsEnvelope {
        items: vec![SegmentItem {
            key: candidate.segment().to_owned(),
            observed: false,
            files,
            original_key: None,
        }],
        total: 1,
        protocol_version: 2,
    }
}
