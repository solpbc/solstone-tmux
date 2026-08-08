// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

mod support;

use std::fs::{self, OpenOptions};
use std::future::Future;
use std::io::Write;
use std::os::unix::fs::{MetadataExt, symlink};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sha2::{Digest, Sha256};
use solstone_tmux::journal::{
    ListingFileStatus, SegmentFile, SegmentItem, SegmentsEnvelope, inventory_files,
};
use solstone_tmux::name::{DerivedName, derive_component};
use solstone_tmux::observer::{
    LifecycleLock, ObserverExit, ObserverOperationError, ShutdownEvent, ShutdownIndicator,
    SupervisionControl, shutdown_barrier, supervise_observer,
};
use solstone_tmux::sync::{
    RetentionFence, RetentionOutcome, SegmentCandidate, SyncActivity,
    delete_custodied_segment as delete_custodied_segment_fenced,
    delete_custodied_segment_with_hook,
};
use support::TestDirectory;
use time::{Date, Month};

const STREAM: &str = "host.tmux";
const SEGMENT: &str = "120000_300";
const FILE: &str = "tmux_main_screen.jsonl";

async fn delete_custodied_segment(
    captures_root: &Path,
    configured_stream: &DerivedName,
    today: Date,
    retention_days: i64,
    candidate: &SegmentCandidate,
    authoritative_key: &str,
    listing: &SegmentsEnvelope,
) -> RetentionOutcome {
    delete_custodied_segment_fenced(
        captures_root,
        configured_stream,
        today,
        retention_days,
        candidate,
        authoritative_key,
        listing,
        solstone_tmux::sync::SyncInstrumentation::default(),
        Arc::new(RetentionFence::new()),
    )
    .await
}

#[test]
fn negative_retention_returns_before_traversing() {
    run(async {
        let temporary = TestDirectory::new("retention-disabled");
        let outside = create_segment(temporary.path(), "outside", "20260701", STREAM, SEGMENT);
        let alias = temporary.path().join("captures");
        symlink(temporary.path().join("outside"), &alias).expect("create captures alias");
        let candidate = SegmentCandidate::new("20260701", STREAM, SEGMENT);
        let listing = listing_for(&outside, &candidate, ListingFileStatus::Present).await;

        let outcome = delete_custodied_segment(
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
            delete_custodied_segment(
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
            delete_custodied_segment(
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
            delete_custodied_segment(
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
            delete_custodied_segment(
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

        let outcome = delete_custodied_segment(
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
fn retention_reinventory_hashes_are_counted() {
    run(async {
        let temporary = TestDirectory::new("retention-instrumentation");
        let captures = temporary.path().join("captures");
        let segment = create_segment(temporary.path(), "captures", "20260701", STREAM, SEGMENT);
        let candidate = SegmentCandidate::new("20260701", STREAM, SEGMENT);
        let listing = listing_for(&segment, &candidate, ListingFileStatus::Present).await;
        let instrumentation = solstone_tmux::sync::SyncInstrumentation::default();

        let outcome = delete_custodied_segment_fenced(
            &captures,
            &stream(),
            today(),
            0,
            &candidate,
            SEGMENT,
            &listing,
            instrumentation.clone(),
            Arc::new(RetentionFence::new()),
        )
        .await;

        assert_eq!(outcome, RetentionOutcome::Deleted);
        assert!(instrumentation.snapshot().hashed_files >= 2);
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

        let outcome = delete_custodied_segment(
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
                delete_custodied_segment(
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
            delete_custodied_segment(
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

#[test]
fn replacement_between_inspection_and_unlink_is_retained() {
    run(async {
        let temporary = TestDirectory::new("retention-replacement");
        let captures = temporary.path().join("captures");
        let segment = create_segment(temporary.path(), "captures", "20260701", STREAM, SEGMENT);
        let candidate = SegmentCandidate::new("20260701", STREAM, SEGMENT);
        let listing = listing_for(&segment, &candidate, ListingFileStatus::Present).await;
        let target = segment.join(FILE);
        let hook_target = target.clone();
        let hook = Arc::new(move |index| {
            if index == 0 {
                let incoming = hook_target.with_file_name(".incoming");
                fs::write(&incoming, b"replacement\n").expect("write replacement");
                fs::rename(incoming, &hook_target).expect("install replacement");
            }
        });

        let outcome = delete_custodied_segment_with_hook(
            &captures,
            &stream(),
            today(),
            0,
            &candidate,
            (SEGMENT, &listing),
            hook,
            solstone_tmux::sync::SyncInstrumentation::default(),
            Arc::new(RetentionFence::new()),
        )
        .await;

        assert_eq!(outcome, RetentionOutcome::Retained);
        assert!(segment.is_dir());
        assert_eq!(
            fs::read(&target).expect("read restored fixture"),
            b"capture fixture\n"
        );
        assert_eq!(
            fs::read(segment.join(".retention-conflict-0")).expect("read preserved replacement"),
            b"replacement\n"
        );
    });
}

#[test]
fn mid_deletion_failure_restores_every_removed_file() {
    run(async {
        let temporary = TestDirectory::new("retention-rollback");
        let captures = temporary.path().join("captures");
        let segment = create_segment(temporary.path(), "captures", "20260701", STREAM, SEGMENT);
        let auxiliary = segment.join("tmux_aux_screen.jsonl");
        fs::write(&auxiliary, b"auxiliary fixture\n").expect("write auxiliary fixture");
        let candidate = SegmentCandidate::new("20260701", STREAM, SEGMENT);
        let listing = listing_for(&segment, &candidate, ListingFileStatus::Processed).await;
        let main = segment.join(FILE);
        let hook_main = main.clone();
        let hook = Arc::new(move |index| {
            if index == 1 {
                let incoming = hook_main.with_file_name(".incoming");
                fs::write(&incoming, b"replacement\n").expect("write replacement");
                fs::rename(incoming, &hook_main).expect("install replacement");
            }
        });

        let outcome = delete_custodied_segment_with_hook(
            &captures,
            &stream(),
            today(),
            0,
            &candidate,
            (SEGMENT, &listing),
            hook,
            solstone_tmux::sync::SyncInstrumentation::default(),
            Arc::new(RetentionFence::new()),
        )
        .await;

        assert_eq!(outcome, RetentionOutcome::Retained);
        assert!(segment.is_dir());
        assert_eq!(
            fs::read(auxiliary).expect("read restored auxiliary"),
            b"auxiliary fixture\n"
        );
        assert_eq!(
            fs::read(main).expect("read restored main"),
            b"capture fixture\n"
        );
        assert_eq!(
            fs::read(segment.join(".retention-conflict-1")).expect("read preserved replacement"),
            b"replacement\n"
        );
    });
}

#[test]
fn in_place_mutation_before_unlink_is_retained() {
    run(async {
        let temporary = TestDirectory::new("retention-in-place-mutation");
        let captures = temporary.path().join("captures");
        let segment = create_segment(temporary.path(), "captures", "20260701", STREAM, SEGMENT);
        let candidate = SegmentCandidate::new("20260701", STREAM, SEGMENT);
        let listing = listing_for(&segment, &candidate, ListingFileStatus::Present).await;
        let target = sorted_segment_files(&segment)
            .into_iter()
            .next()
            .expect("fixture file");
        let original = fs::read(&target).expect("read original fixture");
        let original_metadata = fs::metadata(&target).expect("inspect original fixture");
        let original_digest = sha256_hex(&original);
        let mutated = b"mutated fixture\n".to_vec();
        assert_eq!(mutated.len(), original.len());
        assert_ne!(mutated, original);
        let timestamps = timestamps(&original_metadata);
        let hook_target = target.clone();
        let hook_mutated = mutated.clone();
        let hook = Arc::new(move |index| {
            if index == 0 {
                write_in_place_and_restore_timestamps(&hook_target, &hook_mutated, timestamps);
            }
        });

        let outcome = delete_custodied_segment_with_hook(
            &captures,
            &stream(),
            today(),
            0,
            &candidate,
            (SEGMENT, &listing),
            hook,
            solstone_tmux::sync::SyncInstrumentation::default(),
            Arc::new(RetentionFence::new()),
        )
        .await;

        assert_eq!(outcome, RetentionOutcome::Retained);
        let mutated_metadata = fs::metadata(&target).expect("inspect mutated fixture");
        let mutated_on_disk = fs::read(&target).expect("read mutated fixture");
        assert_eq!(
            (
                original_metadata.dev(),
                original_metadata.ino(),
                original_metadata.len()
            ),
            (
                mutated_metadata.dev(),
                mutated_metadata.ino(),
                mutated_metadata.len()
            )
        );
        assert_ne!(sha256_hex(&mutated_on_disk), original_digest);
        assert_eq!(mutated_on_disk, mutated);
        assert!(segment.is_dir());
    });
}

#[test]
fn late_byte_mismatch_rolls_back_prior_unlinks() {
    run(async {
        let temporary = TestDirectory::new("retention-late-byte-mismatch");
        let captures = temporary.path().join("captures");
        let segment = create_segment(temporary.path(), "captures", "20260701", STREAM, SEGMENT);
        fs::write(
            segment.join("tmux_aux_screen.jsonl"),
            b"auxiliary fixture\n",
        )
        .expect("write auxiliary fixture");
        let candidate = SegmentCandidate::new("20260701", STREAM, SEGMENT);
        let listing = listing_for(&segment, &candidate, ListingFileStatus::Processed).await;
        let sorted_paths = sorted_segment_files(&segment);
        assert_eq!(sorted_paths.len(), 2);
        let first = sorted_paths[0].clone();
        let second = sorted_paths[1].clone();
        let original_first = fs::read(&first).expect("read first fixture");
        let original_second = fs::read(&second).expect("read second fixture");
        let original_second_metadata = fs::metadata(&second).expect("inspect second fixture");
        let original_second_digest = sha256_hex(&original_second);
        let mut mutated_second = original_second.clone();
        mutated_second[0] ^= 1;
        let timestamps = timestamps(&original_second_metadata);
        let hook_first = first.clone();
        let hook_second = second.clone();
        let hook_mutated_second = mutated_second.clone();
        let hook = Arc::new(move |index| {
            if index == 1 {
                assert!(!hook_first.exists(), "first sorted file should be unlinked");
                write_in_place_and_restore_timestamps(
                    &hook_second,
                    &hook_mutated_second,
                    timestamps,
                );
            }
        });

        let outcome = delete_custodied_segment_with_hook(
            &captures,
            &stream(),
            today(),
            0,
            &candidate,
            (SEGMENT, &listing),
            hook,
            solstone_tmux::sync::SyncInstrumentation::default(),
            Arc::new(RetentionFence::new()),
        )
        .await;

        assert_eq!(outcome, RetentionOutcome::Retained);
        assert_eq!(
            fs::read(&first).expect("read restored first fixture"),
            original_first
        );
        let mutated_second_metadata =
            fs::metadata(&second).expect("inspect mutated second fixture");
        let mutated_second_on_disk = fs::read(&second).expect("read mutated second fixture");
        assert_eq!(
            (
                original_second_metadata.dev(),
                original_second_metadata.ino(),
                original_second_metadata.len()
            ),
            (
                mutated_second_metadata.dev(),
                mutated_second_metadata.ino(),
                mutated_second_metadata.len()
            )
        );
        assert_ne!(sha256_hex(&mutated_second_on_disk), original_second_digest);
        assert_eq!(mutated_second_on_disk, mutated_second);
        assert!(segment.is_dir());
    });
}

#[tokio::test(start_paused = true)]
async fn retention_fence_keeps_the_lock_until_a_gated_unlink_finishes() {
    let temporary = TestDirectory::new("retention-fence");
    let captures = temporary.path().join("captures");
    let segment = create_segment(temporary.path(), "captures", "20260701", STREAM, SEGMENT);
    let candidate = SegmentCandidate::new("20260701", STREAM, SEGMENT);
    let listing = listing_for(&segment, &candidate, ListingFileStatus::Present).await;
    let fence = Arc::new(RetentionFence::new());
    let (entered_sender, entered_receiver) = tokio::sync::oneshot::channel();
    let entered_sender = Arc::new(Mutex::new(Some(entered_sender)));
    let (release_sender, release_receiver) = std::sync::mpsc::channel();
    let release_receiver = Arc::new(Mutex::new(Some(release_receiver)));
    let hook_entered = Arc::clone(&entered_sender);
    let hook_release = Arc::clone(&release_receiver);
    let hook = Arc::new(move |index| {
        if index == 0 {
            if let Some(sender) = hook_entered.lock().expect("entered lock poisoned").take() {
                let _ = sender.send(());
            }
            if let Some(receiver) = hook_release.lock().expect("release lock poisoned").take() {
                let _ = receiver.recv();
            }
        }
    });
    let sync_fence = Arc::clone(&fence);
    let sync = async move {
        let _ = delete_custodied_segment_with_hook(
            &captures,
            &stream(),
            today(),
            0,
            &candidate,
            (SEGMENT, &listing),
            hook,
            solstone_tmux::sync::SyncInstrumentation::default(),
            sync_fence,
        )
        .await;
        Ok(())
    };
    let (observer_release, observer_wait) = tokio::sync::oneshot::channel();
    let observer = async move {
        let _ = observer_wait.await;
        ObserverExit {
            exit_code: 0,
            shutdown_event: Some(ShutdownEvent::SigTerm),
            failures: Vec::new(),
        }
    };
    let log = Arc::new(Mutex::new(Vec::new()));
    let (_activity, activity) = tokio::sync::watch::channel(SyncActivity::Idle);
    let (sync_stop, _sync_shutdown) = tokio::sync::watch::channel(false);
    let (observer_stop, _observer_shutdown) =
        tokio::sync::watch::channel::<Option<ShutdownEvent>>(None);
    let (observer_barrier, supervisor_barrier) = shutdown_barrier();
    drop(observer_barrier);
    let supervision = tokio::spawn(supervise_observer(
        observer,
        sync,
        Box::new(RecordingIndicator(Arc::clone(&log))),
        Box::new(RecordingLock(Arc::clone(&log))),
        SupervisionControl {
            activity,
            sync_stop,
            observer_stop,
            shutdown_barrier: supervisor_barrier,
            retention_fence: Arc::clone(&fence),
        },
    ));

    entered_receiver.await.expect("unlink entered hook");
    observer_release.send(()).expect("request shutdown");
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(15)).await;
    tokio::task::yield_now().await;
    assert!(log.lock().expect("log poisoned").is_empty());

    release_sender.send(()).expect("release unlink");
    let exit = supervision.await.expect("join supervision");
    assert_eq!(exit.exit_code, 1);
    assert_eq!(
        exit.failures,
        [solstone_tmux::health::DiagnosticCode::SyncTaskTimedOut
            .message()
            .to_owned()]
    );
    assert_eq!(*log.lock().expect("log poisoned"), ["indicator", "lock"]);
    assert!(!segment.exists());

    fs::create_dir_all(&segment).expect("recreate released segment");
    fs::write(segment.join(FILE), b"replacement after release\n").expect("write replacement");
    tokio::task::yield_now().await;
    assert!(segment.join(FILE).is_file());
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

struct RecordingIndicator(Arc<Mutex<Vec<&'static str>>>);

impl ShutdownIndicator for RecordingIndicator {
    fn restore<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<(), ObserverOperationError>> + Send + 'a>> {
        Box::pin(async move {
            self.0.lock().expect("log poisoned").push("indicator");
            Ok(())
        })
    }
}

struct RecordingLock(Arc<Mutex<Vec<&'static str>>>);

impl LifecycleLock for RecordingLock {}

impl Drop for RecordingLock {
    fn drop(&mut self) {
        self.0.lock().expect("log poisoned").push("lock");
    }
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
    let local = inventory_files(paths, None)
        .await
        .expect("inventory fixture");
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

fn sorted_segment_files(segment: &Path) -> Vec<PathBuf> {
    let mut paths = fs::read_dir(segment)
        .expect("read fixture segment")
        .map(|entry| entry.expect("read fixture entry").path())
        .collect::<Vec<_>>();
    paths.sort();
    paths
}

fn timestamps(metadata: &fs::Metadata) -> (i64, i64, i64, i64) {
    (
        metadata.atime(),
        metadata.atime_nsec(),
        metadata.mtime(),
        metadata.mtime_nsec(),
    )
}

fn write_in_place_and_restore_timestamps(
    path: &Path,
    bytes: &[u8],
    (atime, atime_nsec, mtime, mtime_nsec): (i64, i64, i64, i64),
) {
    assert_eq!(
        fs::metadata(path)
            .expect("inspect fixture before mutation")
            .len(),
        u64::try_from(bytes.len()).expect("fixture length")
    );
    {
        let mut file = OpenOptions::new()
            .write(true)
            .open(path)
            .expect("open fixture for mutation");
        file.write_all(bytes).expect("mutate fixture");
        file.sync_all().expect("sync fixture mutation");
    }
    rustix::fs::utimensat(
        rustix::fs::CWD,
        path,
        &rustix::fs::Timestamps {
            last_access: rustix::fs::Timespec {
                tv_sec: atime,
                tv_nsec: atime_nsec,
            },
            last_modification: rustix::fs::Timespec {
                tv_sec: mtime,
                tv_nsec: mtime_nsec,
            },
        },
        rustix::fs::AtFlags::empty(),
    )
    .expect("restore fixture timestamps");
    assert_eq!(
        timestamps(&fs::metadata(path).expect("inspect restored fixture timestamps")),
        (atime, atime_nsec, mtime, mtime_nsec)
    );
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
