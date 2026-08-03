// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

mod support;

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use solstone_tmux::clock::{Clock, TestClock};
use solstone_tmux::health::{DiagnosticCode, HEALTH_FILENAME, HealthWriter};
use solstone_tmux::instance_lock::InstanceLock;
use solstone_tmux::journal::{
    ListingFileStatus, SegmentFile, SegmentItem, SegmentsEnvelope, UploadResult, UploadStatus,
    inventory_files,
};
use solstone_tmux::model::CaptureResult;
use solstone_tmux::name::{DerivedName, derive_component};
use solstone_tmux::observer::{
    CaptureProvider, ObserverConfig, ObserverOperationError, SegmentLifecycle, ShutdownEvent,
    run_observer, shutdown_barrier,
};
use solstone_tmux::paths::ensure_private_directory;
use solstone_tmux::segment::SegmentClose;
use solstone_tmux::sync::{
    SEGMENTS_PER_PASS, SegmentCandidate, StatusBeacon, SyncActivity, SyncFailureClass, SyncJournal,
    SyncOperationError, SyncScheduler, SyncWake,
};
use support::TestDirectory;
use time::{Date, Month, PrimitiveDateTime, Time, UtcOffset};
use tokio::sync::mpsc::error::TryRecvError;
use tokio::sync::{mpsc, oneshot};

const DAY: &str = "20260701";
const STREAM: &str = "host.tmux";
const FILE: &str = "tmux_main_screen.jsonl";
const SCHEDULER_TURNS: usize = 1_024;
const WAIT_CEILING: Duration = Duration::from_secs(5);

#[test]
fn startup_finalization_and_periodic_wakes_converge_on_a_rescan() {
    paused(async {
        let temporary = TestDirectory::new("sync-convergence");
        let clock = clock();
        let wake = SyncWake::default();
        let (journal, mut calls) = FakeJournal::new();
        let (stop, stopped) = oneshot::channel();
        let mut scheduler = scheduler(&temporary, Arc::clone(&clock), wake.clone());
        let task = tokio::spawn(async move {
            let mut journal = journal;
            scheduler
                .run(&mut journal, async move {
                    let _ = stopped.await;
                })
                .await;
        });

        expect_listing(&mut calls, "startup").await;
        wake.segment_closed(&SegmentClose::RemovedEmpty);
        assert_no_listing(&mut calls).await;

        wake.segment_closed(&SegmentClose::Finalized(PathBuf::from("wake-only")));
        expect_listing(&mut calls, "finalization").await;

        advance_both(&clock, Duration::from_secs(60) + Duration::from_millis(1)).await;
        expect_listing(&mut calls, "periodic").await;

        let _ = stop.send(());
        task.await.expect("join scheduler");
    });
}

#[test]
fn one_backoff_owner_advances_holds_resets_and_never_stops_capture() {
    paused(async {
        let temporary = TestDirectory::new("sync-backoff");
        let clock = clock();
        let wake = SyncWake::default();
        let outcomes = [
            failure(SyncFailureClass::Direct),
            failure(SyncFailureClass::Relay),
            failure(SyncFailureClass::Auth),
            failure(SyncFailureClass::Timeout),
            failure(SyncFailureClass::Contract),
            Ok(empty_listing()),
            failure(SyncFailureClass::Direct),
            Ok(empty_listing()),
        ];
        let (journal, mut calls) = FakeJournal::with_list_outcomes("20260710", outcomes);
        let (sync_stop, sync_stopped) = oneshot::channel();
        let mut scheduler = scheduler(&temporary, Arc::clone(&clock), wake.clone());
        let sync_task = tokio::spawn(async move {
            let mut journal = journal;
            scheduler
                .run(&mut journal, async move {
                    let _ = sync_stopped.await;
                })
                .await;
        });

        let capture_polls = Arc::new(AtomicUsize::new(0));
        let rotations = Arc::new(AtomicUsize::new(0));
        let (observer_stop, observer_stopped) = oneshot::channel();
        let (observer_shutdown_barrier, supervisor_shutdown_barrier) = shutdown_barrier();
        drop(supervisor_shutdown_barrier);
        let observer = tokio::spawn(run_observer(
            Arc::new(CountingCapture(Arc::clone(&capture_polls))),
            Box::new(RotatingSegment::new(Arc::clone(&rotations))),
            Arc::clone(&clock) as Arc<dyn Clock>,
            Box::pin(async move {
                let _ = observer_stopped.await;
                ShutdownEvent::Injected
            }),
            observer_shutdown_barrier,
            ObserverConfig {
                capture_interval: Duration::from_secs(5),
                segment_interval: Duration::from_secs(5),
            },
        ));

        expect_listing(&mut calls, "first failure").await;
        wake.segment_closed(&SegmentClose::Finalized(PathBuf::from("coalesced")));
        advance_both(&clock, Duration::from_secs(4)).await;
        assert_no_listing(&mut calls).await;

        for (delay, context) in [
            (5_u64, "5 second retry"),
            (30, "30 second retry"),
            (120, "120 second retry"),
            (300, "300 second retry"),
        ] {
            let previous_polls = capture_polls.load(Ordering::SeqCst);
            let previous_rotations = rotations.load(Ordering::SeqCst);
            let elapsed = if delay == 5 { 1 } else { delay };
            advance_both(
                &clock,
                Duration::from_secs(elapsed) + Duration::from_millis(1),
            )
            .await;
            expect_listing(&mut calls, context).await;
            assert!(capture_polls.load(Ordering::SeqCst) > previous_polls);
            assert!(rotations.load(Ordering::SeqCst) > previous_rotations);
        }

        let previous_polls = capture_polls.load(Ordering::SeqCst);
        let previous_rotations = rotations.load(Ordering::SeqCst);
        advance_both(&clock, Duration::from_secs(300) + Duration::from_millis(1)).await;
        expect_listing(&mut calls, "held retry").await;
        assert!(capture_polls.load(Ordering::SeqCst) > previous_polls);
        assert!(rotations.load(Ordering::SeqCst) > previous_rotations);

        wake.segment_closed(&SegmentClose::Finalized(PathBuf::from("reset")));
        expect_listing(&mut calls, "reset failure").await;
        advance_both(&clock, Duration::from_secs(4)).await;
        assert_no_listing(&mut calls).await;
        advance_both(&clock, Duration::from_secs(1) + Duration::from_millis(1)).await;
        expect_listing(&mut calls, "reset retry").await;

        let _ = sync_stop.send(());
        let _ = observer_stop.send(());
        sync_task.await.expect("join scheduler");
        let exit = observer.await.expect("join observer");
        assert_eq!(exit.exit_code, 0);
    });
}

#[test]
fn one_pass_attempts_at_most_eight_candidates_sequentially() {
    run(async {
        let temporary = TestDirectory::new("sync-bounded");
        for index in 0..(SEGMENTS_PER_PASS + 1) {
            create_segment(
                temporary.path(),
                &format!("12{index:02}00_300"),
                b"fixture\n",
            );
        }
        let clock = clock();
        let (mut journal, mut calls) = FakeJournal::new();
        let mut scheduler = scheduler(&temporary, clock, SyncWake::default());

        let summary = scheduler.run_pass(&mut journal).await;

        assert_eq!(summary.attempted, SEGMENTS_PER_PASS);
        assert!(summary.more_work);
        assert_eq!(
            drain_uploads(&mut calls).len(),
            SEGMENTS_PER_PASS,
            "a pass exceeded its candidate bound"
        );
    });
}

#[test]
fn second_sweep_reuses_custody_without_uploads_across_passes() {
    run(async {
        let temporary = TestDirectory::new("sync-second-sweep-reuse");
        let segments = vec![
            ("20260701", "120000_300"),
            ("20260701", "120100_300"),
            ("20260701", "120200_300"),
            ("20260702", "120000_300"),
            ("20260702", "120100_300"),
            ("20260702", "120200_300"),
            ("20260703", "120000_300"),
            ("20260703", "120100_300"),
            ("20260703", "120200_300"),
        ];
        for (day, segment) in &segments {
            create_segment_in_day(temporary.path(), day, segment, b"fixture\n");
        }
        let (mut journal, mut calls) = FakeJournal::new();
        let mut scheduler = scheduler(&temporary, clock(), SyncWake::default());

        let mut first = scheduler.run_pass(&mut journal).await;
        while first.more_work {
            first = scheduler.run_pass(&mut journal).await;
        }
        let _ = drain_calls(&mut calls);

        let mut attempted = 0;
        let mut second = scheduler.run_pass(&mut journal).await;
        attempted += second.attempted;
        while second.more_work {
            second = scheduler.run_pass(&mut journal).await;
            attempted += second.attempted;
        }
        let second_calls = drain_calls(&mut calls);
        let uploads = second_calls
            .iter()
            .filter_map(|call| match call {
                ObservedCall::Upload(segment) => Some(segment.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        let mut listings_by_day = HashMap::new();
        for call in &second_calls {
            if let ObservedCall::Listing(day) = call {
                *listings_by_day.entry(day).or_insert(0_usize) += 1;
            }
        }

        assert!(
            uploads.is_empty(),
            "second sweep re-uploaded custodied segments: {uploads:?}"
        );
        assert!(listings_by_day.values().all(|count| *count <= 1));
        assert_eq!(attempted, segments.len());
        for (day, segment) in segments {
            assert!(segment_path_in_day(temporary.path(), day, segment).is_dir());
        }
    });
}

#[test]
fn stale_cached_custody_does_not_delete_when_current_listing_disagrees() {
    run(async {
        for mismatched_digest in [false, true] {
            let temporary = TestDirectory::new("sync-stale-custody");
            let segments = [
                "120800_300",
                "120700_300",
                "120600_300",
                "120500_300",
                "120400_300",
                "120300_300",
                "120200_300",
                "120100_300",
                "120000_300",
            ];
            for segment in segments {
                create_segment_in_day(temporary.path(), "20260710", segment, b"fixture\n");
            }
            let clock = clock();
            let (mut journal, mut calls) = FakeJournal::new();
            journal.push_list_outcome(
                "20260710",
                Ok(listing_for_segments(
                    temporary.path(),
                    &segments
                        .iter()
                        .map(|segment| ("20260710", *segment))
                        .collect::<Vec<_>>(),
                )
                .await),
            );
            let mut scheduler = SyncScheduler::new(
                temporary.path().join("captures"),
                stream(),
                0,
                Arc::clone(&clock) as Arc<dyn Clock>,
                SyncWake::default(),
            );

            let first = scheduler.run_pass(&mut journal).await;
            assert!(first.more_work);
            clock.set_wall(clock.wall_now() + time::Duration::days(1));
            let mut stale = if mismatched_digest {
                listing_for_segments(temporary.path(), &[("20260710", "120000_300")]).await
            } else {
                empty_listing()
            };
            if mismatched_digest {
                stale.items[0].files[0].sha256 = "b".repeat(64);
            }
            journal.push_list_outcome("20260710", Ok(stale));
            journal.upload_outcomes.insert(
                "120000_300".to_owned(),
                VecDeque::from([Ok(UploadResult {
                    status: UploadStatus::Conflict,
                    authoritative_key: None,
                })]),
            );

            let second = scheduler.run_pass(&mut journal).await;
            assert!(!second.more_work);
            assert_eq!(drain_uploads(&mut calls), vec!["120000_300".to_owned()]);
            assert!(segment_path_in_day(temporary.path(), "20260710", "120000_300").is_dir());
        }
    });
}

#[test]
fn sweep_cache_is_keyed_by_day_when_rotation_splits_a_day() {
    run(async {
        let temporary = TestDirectory::new("sync-split-day-cache");
        create_segment_in_day(temporary.path(), "20260702", "120100_300", b"fixture\n");
        let (mut journal, mut calls) = FakeJournal::new();
        let mut scheduler = scheduler(&temporary, clock(), SyncWake::default());
        scheduler.run_pass(&mut journal).await;
        let _ = drain_calls(&mut calls);
        std::fs::remove_dir_all(temporary.path().join("captures")).expect("remove cursor fixture");
        scheduler.run_pass(&mut journal).await;
        let _ = drain_calls(&mut calls);

        for (day, segment) in [
            ("20260703", "120000_300"),
            ("20260702", "120200_300"),
            ("20260702", "120100_300"),
            ("20260702", "120000_300"),
            ("20260701", "120000_300"),
        ] {
            create_segment_in_day(temporary.path(), day, segment, b"fixture\n");
        }
        let summary = scheduler.run_pass(&mut journal).await;
        let calls = drain_calls(&mut calls);
        let day_two_listings = calls
            .iter()
            .filter(|call| matches!(call, ObservedCall::Listing(day) if day == "20260702"))
            .count();

        assert_eq!(summary.attempted, 5);
        assert_eq!(day_two_listings, 3);
    });
}

#[test]
fn failed_preupload_listing_falls_through_without_failure_or_diagnostic() {
    run(async {
        let temporary = TestDirectory::new("sync-fail-open-preupload");
        create_segment(temporary.path(), "120000_300", b"fixture\n");
        let (mut journal, mut calls) = FakeJournal::new();
        journal.push_list_outcome(DAY, failure(SyncFailureClass::Timeout));
        let mut scheduler = scheduler(&temporary, clock(), SyncWake::default());

        let summary = scheduler.run_pass(&mut journal).await;

        assert_eq!(summary.failure, None);
        assert_eq!(summary.diagnostic, None);
        assert_eq!(summary.custodied, 1);
        assert_eq!(drain_uploads(&mut calls), vec!["120000_300".to_owned()]);
    });
}

#[test]
fn listings_per_day_do_not_exceed_uploads_plus_one() {
    run(async {
        let temporary = TestDirectory::new("sync-listing-bound");
        for (day, segment) in [
            ("20260702", "120100_300"),
            ("20260702", "120000_300"),
            ("20260701", "120100_300"),
            ("20260701", "120000_300"),
        ] {
            create_segment_in_day(temporary.path(), day, segment, b"fixture\n");
        }
        let (mut journal, mut calls) = FakeJournal::new();
        let mut scheduler = scheduler(&temporary, clock(), SyncWake::default());

        scheduler.run_pass(&mut journal).await;
        let calls = drain_calls(&mut calls);
        let mut listings_by_day = HashMap::new();
        for call in calls {
            if let ObservedCall::Listing(day) = call {
                *listings_by_day.entry(day).or_insert(0_usize) += 1;
            }
        }

        assert_eq!(listings_by_day.get("20260702"), Some(&3));
        assert_eq!(listings_by_day.get("20260701"), Some(&3));
    });
}

#[test]
fn reused_custody_counts_as_sync_and_resets_backoff() {
    run(async {
        let temporary = TestDirectory::new("sync-reused-custody-success");
        ensure_private_directory(temporary.path()).expect("prepare data root");
        create_segment(temporary.path(), "120000_300", b"fixture\n");
        let lock = InstanceLock::acquire(temporary.path()).expect("instance lock");
        let health = HealthWriter::new(temporary.path().to_path_buf(), &lock);
        let (activity, _activity_receiver) = tokio::sync::watch::channel(SyncActivity::Idle);
        let (journal, mut calls) = FakeJournal::new();
        let listing = listing_for_segments(temporary.path(), &[(DAY, "120000_300")]).await;
        let (stop, stopped) = oneshot::channel();
        let mut scheduler = SyncScheduler::new(
            temporary.path().join("captures"),
            stream(),
            -1,
            clock(),
            SyncWake::default(),
        )
        .with_observability(activity, health);
        let task = tokio::spawn(async move {
            let mut journal = journal;
            journal.push_list_outcome(DAY, Ok(listing));
            scheduler
                .run(&mut journal, async move {
                    let _ = stopped.await;
                })
                .await;
        });

        expect_listing(&mut calls, "reused custody").await;
        let snapshot = wait_for_idle_snapshot(temporary.path()).await;
        assert!(!snapshot["last_successful_sync_unix_seconds"].is_null());
        assert!(drain_uploads(&mut calls).is_empty());
        let _ = stop.send(());
        task.await.expect("join reused scheduler");
    });
}

#[test]
fn pending_segments_drains_monotonically_when_custody_is_proven() {
    run(async {
        let temporary = TestDirectory::new("sync-pending-drain");
        ensure_private_directory(temporary.path()).expect("prepare data root");
        for index in 0..(SEGMENTS_PER_PASS + 1) {
            create_segment(
                temporary.path(),
                &format!("12{index:02}00_300"),
                b"fixture\n",
            );
        }
        let lock = InstanceLock::acquire(temporary.path()).expect("instance lock");
        let health = HealthWriter::new(temporary.path().to_path_buf(), &lock);
        let (activity, _activity_receiver) = tokio::sync::watch::channel(SyncActivity::Idle);
        let (journal, mut calls) = FakeJournal::new();
        let (stop, stopped) = oneshot::channel();
        let mut scheduler = SyncScheduler::new(
            temporary.path().join("captures"),
            stream(),
            -1,
            clock(),
            SyncWake::default(),
        )
        .with_observability(activity, health);
        let task = tokio::spawn(async move {
            let mut journal = journal;
            scheduler
                .run(&mut journal, async move {
                    let _ = stopped.await;
                })
                .await;
        });

        let mut uploads = Vec::new();
        while uploads.len() < SEGMENTS_PER_PASS + 1 {
            uploads.push(expect_upload(&mut calls).await);
        }
        let snapshot = wait_for_idle_snapshot(temporary.path()).await;
        assert_eq!(snapshot["pending_segments"], 0);
        let _ = stop.send(());
        task.await.expect("join pending scheduler");
    });
}

#[test]
fn collision_renamed_remote_segment_skips_upload() {
    run(async {
        let temporary = TestDirectory::new("sync-collision-skip");
        create_segment(temporary.path(), "120000_300", b"fixture\n");
        let (mut journal, mut calls) = FakeJournal::new();
        let mut listing = listing_for_segments(temporary.path(), &[(DAY, "120000_300")]).await;
        listing.items[0].key = "120000_301".to_owned();
        listing.items[0].original_key = Some("120000_300".to_owned());
        journal.push_list_outcome(DAY, Ok(listing));
        let mut scheduler = scheduler(&temporary, clock(), SyncWake::default());

        let summary = scheduler.run_pass(&mut journal).await;

        assert_eq!(summary.custodied, 1);
        assert!(drain_uploads(&mut calls).is_empty());
        assert!(segment_path(temporary.path(), "120000_300").is_dir());
    });
}

#[test]
fn changed_local_bytes_force_reupload() {
    run(async {
        let temporary = TestDirectory::new("sync-changed-local-bytes");
        create_segment(temporary.path(), "120000_300", b"first\n");
        let (mut journal, mut calls) = FakeJournal::new();
        let mut scheduler = scheduler(&temporary, clock(), SyncWake::default());

        let first = scheduler.run_pass(&mut journal).await;
        assert_eq!(first.custodied, 1);
        let _ = drain_calls(&mut calls);
        std::fs::write(
            segment_path(temporary.path(), "120000_300").join(FILE),
            b"changed\n",
        )
        .expect("mutate local segment");

        let second = scheduler.run_pass(&mut journal).await;

        assert_eq!(second.attempted, 1);
        assert_eq!(drain_uploads(&mut calls), vec!["120000_300".to_owned()]);
        assert!(segment_path(temporary.path(), "120000_300").is_dir());
    });
}

#[test]
fn listing_loss_self_corrects_by_reuploading_only_the_missing_segment() {
    run(async {
        let temporary = TestDirectory::new("sync-listing-loss");
        let segments = [
            ("20260702", "130000_300"),
            ("20260702", "120000_300"),
            ("20260701", "120000_300"),
        ];
        for (day, segment) in segments {
            create_segment_in_day(temporary.path(), day, segment, b"fixture\n");
        }
        let (mut journal, mut calls) = FakeJournal::new();
        let mut scheduler = scheduler(&temporary, clock(), SyncWake::default());
        let first = scheduler.run_pass(&mut journal).await;
        assert!(!first.more_work);
        let _ = drain_calls(&mut calls);
        journal.push_list_outcome(
            "20260702",
            Ok(listing_for_segments(temporary.path(), &[("20260702", "120000_300")]).await),
        );

        let second = scheduler.run_pass(&mut journal).await;

        assert_eq!(second.attempted, segments.len());
        assert_eq!(drain_uploads(&mut calls), vec!["130000_300".to_owned()]);
    });
}

#[test]
fn scheduler_immediately_drains_the_remainder_of_a_bounded_sweep() {
    run(async {
        let temporary = TestDirectory::new("sync-bounded-drain");
        for index in 0..(SEGMENTS_PER_PASS + 1) {
            create_segment(
                temporary.path(),
                &format!("12{index:02}00_300"),
                b"fixture\n",
            );
        }
        let (journal, mut calls) = FakeJournal::new();
        let (stop, stopped) = oneshot::channel();
        let mut scheduler = scheduler(&temporary, clock(), SyncWake::default());
        let task = tokio::spawn(async move {
            let mut journal = journal;
            scheduler
                .run(&mut journal, async move {
                    let _ = stopped.await;
                })
                .await;
        });

        let mut uploads = Vec::new();
        while uploads.len() < SEGMENTS_PER_PASS + 1 {
            uploads.push(expect_upload(&mut calls).await);
        }
        for _ in 0..SCHEDULER_TURNS {
            tokio::task::yield_now().await;
        }
        uploads.extend(drain_uploads(&mut calls));
        assert_eq!(uploads.len(), SEGMENTS_PER_PASS + 1);

        let _ = stop.send(());
        task.await.expect("join scheduler");
    });
}

#[test]
fn upload_wait_is_bounded_and_channel_closure_short_circuits() {
    paused(async {
        let (silent_sender, mut silent_calls) = mpsc::unbounded_channel::<ObservedCall>();
        let bound = Duration::from_millis(25);
        let started = Instant::now();
        let result = wait_for_upload(&mut silent_calls, bound).await;
        let elapsed = started.elapsed();
        assert_eq!(result, Err(ObservationWaitError::DeadlineExpired));
        assert!(elapsed >= bound, "upload wait returned before its deadline");
        drop(silent_sender);

        let (closed_sender, mut closed_calls) = mpsc::unbounded_channel::<ObservedCall>();
        drop(closed_sender);
        let started = Instant::now();
        let result = wait_for_upload(&mut closed_calls, WAIT_CEILING).await;
        let elapsed = started.elapsed();
        assert_eq!(result, Err(ObservationWaitError::ChannelClosed));
        assert!(
            elapsed < Duration::from_secs(1),
            "closed upload channel did not short-circuit"
        );
    });
}

#[test]
fn retained_outcomes_keep_their_diagnostic_and_never_claim_custody() {
    run(async {
        for code in [
            DiagnosticCode::RequestTooLarge,
            DiagnosticCode::LocalSegmentInvalid,
        ] {
            let temporary = TestDirectory::new("sync-retained-diagnostic");
            create_segment(temporary.path(), "120000_300", b"fixture\n");
            let mut scripted = HashMap::new();
            scripted.insert(
                "120000_300".to_owned(),
                VecDeque::from([Err(SyncOperationError::RetainCandidate(code))]),
            );
            let (mut journal, _calls) = FakeJournal::with_upload_outcomes(scripted);
            let mut scheduler = scheduler(&temporary, clock(), SyncWake::default());

            let summary = scheduler.run_pass(&mut journal).await;

            assert_eq!(summary.diagnostic, Some(code));
            assert_eq!(summary.custodied, 0);
        }
    });
}

#[test]
fn conflict_and_failed_contacts_do_not_claim_successful_custody() {
    run(async {
        for status in [UploadStatus::Conflict, UploadStatus::Failed] {
            let temporary = TestDirectory::new("sync-retained-upload-status");
            create_segment(temporary.path(), "120000_300", b"fixture\n");
            let mut scripted = HashMap::new();
            scripted.insert(
                "120000_300".to_owned(),
                VecDeque::from([Ok(UploadResult {
                    status,
                    authoritative_key: None,
                })]),
            );
            let (mut journal, _calls) = FakeJournal::with_upload_outcomes(scripted);
            let mut scheduler = retaining_scheduler(&temporary);

            let summary = scheduler.run_pass(&mut journal).await;

            assert!(summary.contacted);
            assert_eq!(summary.custodied, 0);
            assert_eq!(
                summary.diagnostic,
                Some(DiagnosticCode::LocalSegmentInvalid)
            );
            assert!(segment_path(temporary.path(), "120000_300").is_dir());
        }
    });
}

#[test]
fn an_unproven_fresh_listing_records_a_diagnostic_and_keeps_the_segment() {
    run(async {
        let temporary = TestDirectory::new("sync-unproven-custody");
        create_segment(temporary.path(), "120000_300", b"fixture\n");
        let mut scripted = HashMap::new();
        scripted.insert(
            "120000_300".to_owned(),
            VecDeque::from([Ok(UploadResult {
                status: UploadStatus::Ok,
                authoritative_key: Some("120000_300".to_owned()),
            })]),
        );
        let (mut journal, _calls) = FakeJournal::with_upload_outcomes(scripted);
        let mut scheduler = retaining_scheduler(&temporary);

        let summary = scheduler.run_pass(&mut journal).await;

        assert!(summary.contacted);
        assert_eq!(summary.custodied, 0);
        assert_eq!(
            summary.diagnostic,
            Some(DiagnosticCode::LocalSegmentInvalid)
        );
        assert!(segment_path(temporary.path(), "120000_300").is_dir());
    });
}

#[test]
fn a_retained_candidate_still_lets_later_candidates_be_attempted() {
    run(async {
        let temporary = TestDirectory::new("sync-retained-then-later");
        create_segment(temporary.path(), "120000_300", b"earlier\n");
        create_segment(temporary.path(), "130000_300", b"later\n");
        let mut scripted = HashMap::new();
        scripted.insert(
            "130000_300".to_owned(),
            VecDeque::from([Ok(UploadResult {
                status: UploadStatus::Conflict,
                authoritative_key: None,
            })]),
        );
        let (mut journal, mut calls) = FakeJournal::with_upload_outcomes(scripted);
        let mut scheduler = retaining_scheduler(&temporary);

        let summary = scheduler.run_pass(&mut journal).await;

        assert_eq!(
            drain_uploads(&mut calls),
            vec!["130000_300".to_owned(), "120000_300".to_owned()]
        );
        assert_eq!(summary.attempted, 2);
        assert_eq!(summary.custodied, 1);
        assert_eq!(
            summary.diagnostic,
            Some(DiagnosticCode::LocalSegmentInvalid)
        );
        assert!(segment_path(temporary.path(), "130000_300").is_dir());
        assert!(!segment_path(temporary.path(), "120000_300").exists());
    });
}

#[test]
fn a_retained_candidate_keeps_operator_visible_error_truth() {
    run(async {
        let temporary = TestDirectory::new("sync-retained-error-truth");
        ensure_private_directory(temporary.path()).expect("prepare data root");
        create_segment(temporary.path(), "120000_300", b"fixture\n");
        let lock = InstanceLock::acquire(temporary.path()).expect("instance lock");
        let health = HealthWriter::new(temporary.path().to_path_buf(), &lock);
        let (activity, _activity_receiver) = tokio::sync::watch::channel(SyncActivity::Idle);
        let mut scripted = HashMap::new();
        scripted.insert(
            "120000_300".to_owned(),
            VecDeque::from([Ok(UploadResult {
                status: UploadStatus::Conflict,
                authoritative_key: None,
            })]),
        );
        let (journal, mut calls) = FakeJournal::with_upload_outcomes(scripted);
        let (stop, stopped) = oneshot::channel();
        let mut scheduler = SyncScheduler::new(
            temporary.path().join("captures"),
            stream(),
            0,
            clock(),
            SyncWake::default(),
        )
        .with_observability(activity, health);
        let task = tokio::spawn(async move {
            let mut journal = journal;
            scheduler
                .run(&mut journal, async move {
                    let _ = stopped.await;
                })
                .await;
        });
        let _ = expect_upload(&mut calls).await;
        let snapshot = wait_for_idle_snapshot(temporary.path()).await;

        assert_eq!(snapshot["last_error_code"], "local_segment_invalid");
        assert_eq!(snapshot["recent_error_count"], 1);
        assert!(!snapshot["last_successful_contact_unix_seconds"].is_null());
        assert!(snapshot["last_successful_sync_unix_seconds"].is_null());
        assert_eq!(snapshot["pending_segments"], 1);
        let _ = stop.send(());
        task.await.expect("join retained scheduler");
    });
}

#[test]
fn health_distinguishes_contact_from_custody_and_decrements_deleted_work() {
    run(async {
        let successful = TestDirectory::new("sync-health-custody");
        ensure_private_directory(successful.path()).expect("prepare data root");
        create_segment(successful.path(), "120000_300", b"fixture\n");
        let lock = InstanceLock::acquire(successful.path()).expect("instance lock");
        let health = HealthWriter::new(successful.path().to_path_buf(), &lock);
        let (activity, _activity_receiver) = tokio::sync::watch::channel(SyncActivity::Idle);
        let (journal, mut calls) = FakeJournal::new();
        let (stop, stopped) = oneshot::channel();
        let mut scheduler = SyncScheduler::new(
            successful.path().join("captures"),
            stream(),
            0,
            clock(),
            SyncWake::default(),
        )
        .with_observability(activity, health);
        let task = tokio::spawn(async move {
            let mut journal = journal;
            scheduler
                .run(&mut journal, async move {
                    let _ = stopped.await;
                })
                .await;
        });
        let _ = expect_upload(&mut calls).await;
        let snapshot = wait_for_idle_snapshot(successful.path()).await;
        assert_eq!(snapshot["pending_segments"], 0);
        assert!(!snapshot["last_successful_contact_unix_seconds"].is_null());
        assert!(!snapshot["last_successful_sync_unix_seconds"].is_null());
        let _ = stop.send(());
        task.await.expect("join successful scheduler");

        let retained = TestDirectory::new("sync-health-contact");
        ensure_private_directory(retained.path()).expect("prepare data root");
        create_segment(retained.path(), "120000_300", b"fixture\n");
        let lock = InstanceLock::acquire(retained.path()).expect("instance lock");
        let health = HealthWriter::new(retained.path().to_path_buf(), &lock);
        let (activity, _activity_receiver) = tokio::sync::watch::channel(SyncActivity::Idle);
        let mut scripted = HashMap::new();
        scripted.insert(
            "120000_300".to_owned(),
            VecDeque::from([Ok(UploadResult {
                status: UploadStatus::Conflict,
                authoritative_key: None,
            })]),
        );
        let (journal, mut calls) = FakeJournal::with_upload_outcomes(scripted);
        let (stop, stopped) = oneshot::channel();
        let mut scheduler = SyncScheduler::new(
            retained.path().join("captures"),
            stream(),
            0,
            clock(),
            SyncWake::default(),
        )
        .with_observability(activity, health);
        let task = tokio::spawn(async move {
            let mut journal = journal;
            scheduler
                .run(&mut journal, async move {
                    let _ = stopped.await;
                })
                .await;
        });
        let _ = expect_upload(&mut calls).await;
        let snapshot = wait_for_idle_snapshot(retained.path()).await;
        assert_eq!(snapshot["pending_segments"], 1);
        assert!(!snapshot["last_successful_contact_unix_seconds"].is_null());
        assert!(snapshot["last_successful_sync_unix_seconds"].is_null());
        let _ = stop.send(());
        task.await.expect("join retained scheduler");
    });
}

#[test]
fn an_unscannable_capture_root_is_not_an_empty_success() {
    run(async {
        let temporary = TestDirectory::new("sync-scan-failure");
        let captures = temporary.path().join("captures");
        std::os::unix::fs::symlink(temporary.path(), &captures).expect("create captures alias");
        let (mut journal, _calls) = FakeJournal::new();
        let mut scheduler = scheduler(&temporary, clock(), SyncWake::default());

        let summary = scheduler.run_pass(&mut journal).await;

        assert_eq!(summary.failure, Some(SyncFailureClass::Contract));
        assert_eq!(
            summary.diagnostic,
            Some(DiagnosticCode::LocalSegmentInvalid)
        );
        assert!(!summary.contacted);
    });
}

#[test]
fn successful_empty_listing_counts_as_contact() {
    run(async {
        let temporary = TestDirectory::new("sync-empty-contact");
        let clock = clock();
        let (mut journal, mut calls) = FakeJournal::new();
        let mut scheduler = scheduler(&temporary, clock, SyncWake::default());

        let summary = scheduler.run_pass(&mut journal).await;

        assert!(summary.contacted);
        assert_eq!(summary.attempted, 0);
        expect_listing(&mut calls, "empty contact").await;
    });
}

#[test]
fn activity_is_working_only_while_a_real_candidate_is_in_flight() {
    run(async {
        let empty = TestDirectory::new("sync-activity-empty");
        let (activity, receiver) = tokio::sync::watch::channel(SyncActivity::Idle);
        let (mut journal, _calls) = FakeJournal::new();
        let mut empty_scheduler =
            scheduler(&empty, clock(), SyncWake::default()).with_activity(activity);
        let summary = empty_scheduler.run_pass(&mut journal).await;
        assert_eq!(summary.attempted, 0);
        assert_eq!(*receiver.borrow(), SyncActivity::Idle);

        let working = TestDirectory::new("sync-activity-working");
        create_segment(working.path(), "120000_300", b"fixture\n");
        let (activity, receiver) = tokio::sync::watch::channel(SyncActivity::Idle);
        let (started, in_flight) = oneshot::channel();
        let (release, released) = oneshot::channel();
        let mut working_scheduler =
            scheduler(&working, clock(), SyncWake::default()).with_activity(activity);
        let task = tokio::spawn(async move {
            let mut journal = GatedJournal {
                started: Some(started),
                release: Some(released),
            };
            working_scheduler.run_pass(&mut journal).await
        });

        in_flight.await.expect("upload started");
        assert_eq!(*receiver.borrow(), SyncActivity::Working);
        let _ = release.send(());
        let summary = task.await.expect("join candidate pass");
        assert_eq!(summary.failure, Some(SyncFailureClass::Timeout));
        assert_eq!(*receiver.borrow(), SyncActivity::Idle);
    });
}

#[test]
fn status_event_is_bounded_to_one_per_heartbeat_interval() {
    paused(async {
        let temporary = TestDirectory::new("sync-status-heartbeat");
        let clock = clock();
        let wake = SyncWake::default();
        let (events, mut received) = mpsc::unbounded_channel();
        let mut scheduler = scheduler(&temporary, Arc::clone(&clock), wake.clone());
        let (stop, stopped) = oneshot::channel();
        let task = tokio::spawn(async move {
            let mut journal = HeartbeatJournal { events };
            scheduler
                .run(&mut journal, async move {
                    let _ = stopped.await;
                })
                .await;
        });

        let first = received.recv().await.expect("startup status event");
        assert_eq!(first.name, "observer");
        wake.segment_closed(&SegmentClose::Finalized(PathBuf::from("coalesced")));
        for _ in 0..SCHEDULER_TURNS {
            tokio::task::yield_now().await;
        }
        assert!(received.try_recv().is_err());

        advance_both(&clock, Duration::from_secs(60) + Duration::from_millis(1)).await;
        received.recv().await.expect("heartbeat status event");
        assert!(received.try_recv().is_err());

        let _ = stop.send(());
        task.await.expect("join scheduler");
    });
}

#[test]
fn poison_segment_cursor_reaches_a_later_valid_candidate_after_backoff() {
    paused(async {
        let temporary = TestDirectory::new("sync-poison-fairness");
        create_segment(temporary.path(), "120500_300", b"poison\n");
        create_segment(temporary.path(), "120000_300", b"valid\n");
        let clock = clock();
        let wake = SyncWake::default();
        let mut scripted = HashMap::new();
        scripted.insert(
            "120500_300".to_owned(),
            VecDeque::from([failure(SyncFailureClass::Contract)]),
        );
        let (journal, mut calls) = FakeJournal::with_upload_outcomes(scripted);
        let (stop, stopped) = oneshot::channel();
        let mut scheduler = scheduler(&temporary, Arc::clone(&clock), wake);
        let task = tokio::spawn(async move {
            let mut journal = journal;
            scheduler
                .run(&mut journal, async move {
                    let _ = stopped.await;
                })
                .await;
        });

        assert_eq!(expect_upload(&mut calls).await, "120500_300");
        advance_both(&clock, Duration::from_secs(5)).await;
        assert_eq!(expect_upload(&mut calls).await, "120000_300");

        let _ = stop.send(());
        task.await.expect("join scheduler");
    });
}

struct HeartbeatJournal {
    events: mpsc::UnboundedSender<StatusBeacon>,
}

impl SyncJournal for HeartbeatJournal {
    fn observer_name(&self) -> Option<&str> {
        Some("observer")
    }

    fn ensure_registered<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<bool, SyncOperationError>> + Send + 'a>> {
        Box::pin(async { Ok(false) })
    }

    fn upload<'a>(
        &'a mut self,
        _candidate: &'a SegmentCandidate,
        _files: Vec<PathBuf>,
    ) -> Pin<Box<dyn Future<Output = Result<UploadResult, SyncOperationError>> + Send + 'a>> {
        Box::pin(async {
            Err(SyncOperationError::RetainCandidate(
                DiagnosticCode::LocalSegmentInvalid,
            ))
        })
    }

    fn segments<'a>(
        &'a mut self,
        _day: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<SegmentsEnvelope, SyncOperationError>> + Send + 'a>>
    {
        Box::pin(async { Ok(empty_listing()) })
    }

    fn status_event<'a>(
        &'a mut self,
        beacon: &'a StatusBeacon,
    ) -> Pin<Box<dyn Future<Output = Result<(), SyncOperationError>> + Send + 'a>> {
        Box::pin(async move {
            let _ = self.events.send(beacon.clone());
            Ok(())
        })
    }
}

struct GatedJournal {
    started: Option<oneshot::Sender<()>>,
    release: Option<oneshot::Receiver<()>>,
}

impl SyncJournal for GatedJournal {
    fn observer_name(&self) -> Option<&str> {
        None
    }

    fn ensure_registered<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<bool, SyncOperationError>> + Send + 'a>> {
        Box::pin(async { Ok(false) })
    }

    fn upload<'a>(
        &'a mut self,
        _candidate: &'a SegmentCandidate,
        _files: Vec<PathBuf>,
    ) -> Pin<Box<dyn Future<Output = Result<UploadResult, SyncOperationError>> + Send + 'a>> {
        Box::pin(async move {
            if let Some(started) = self.started.take() {
                let _ = started.send(());
            }
            if let Some(release) = self.release.take() {
                let _ = release.await;
            }
            Err(SyncOperationError::EndPass(SyncFailureClass::Timeout))
        })
    }

    fn segments<'a>(
        &'a mut self,
        _day: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<SegmentsEnvelope, SyncOperationError>> + Send + 'a>>
    {
        Box::pin(async { Ok(empty_listing()) })
    }

    fn status_event<'a>(
        &'a mut self,
        _beacon: &'a StatusBeacon,
    ) -> Pin<Box<dyn Future<Output = Result<(), SyncOperationError>> + Send + 'a>> {
        Box::pin(async { Ok(()) })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ObservedCall {
    Register,
    Upload(String),
    Listing(String),
}

#[derive(Debug, PartialEq, Eq)]
enum ObservationWaitError {
    DeadlineExpired,
    ChannelClosed,
}

struct FakeJournal {
    calls: mpsc::UnboundedSender<ObservedCall>,
    list_outcomes: HashMap<String, VecDeque<Result<SegmentsEnvelope, SyncOperationError>>>,
    upload_outcomes: HashMap<String, VecDeque<Result<UploadResult, SyncOperationError>>>,
    uploaded: HashMap<String, HashMap<String, Vec<solstone_tmux::journal::LocalFile>>>,
}

impl FakeJournal {
    fn new() -> (Self, mpsc::UnboundedReceiver<ObservedCall>) {
        Self::with_scripts(HashMap::new(), HashMap::new())
    }

    fn with_list_outcomes(
        day: &str,
        outcomes: impl IntoIterator<Item = Result<SegmentsEnvelope, SyncOperationError>>,
    ) -> (Self, mpsc::UnboundedReceiver<ObservedCall>) {
        let mut list_outcomes = HashMap::new();
        list_outcomes.insert(day.to_owned(), outcomes.into_iter().collect());
        Self::with_scripts(list_outcomes, HashMap::new())
    }

    fn with_upload_outcomes(
        outcomes: HashMap<String, VecDeque<Result<UploadResult, SyncOperationError>>>,
    ) -> (Self, mpsc::UnboundedReceiver<ObservedCall>) {
        Self::with_scripts(HashMap::new(), outcomes)
    }

    fn with_scripts(
        list_outcomes: HashMap<String, VecDeque<Result<SegmentsEnvelope, SyncOperationError>>>,
        upload_outcomes: HashMap<String, VecDeque<Result<UploadResult, SyncOperationError>>>,
    ) -> (Self, mpsc::UnboundedReceiver<ObservedCall>) {
        let (calls, receiver) = mpsc::unbounded_channel();
        (
            Self {
                calls,
                list_outcomes,
                upload_outcomes,
                uploaded: HashMap::new(),
            },
            receiver,
        )
    }

    fn push_list_outcome(
        &mut self,
        day: &str,
        outcome: Result<SegmentsEnvelope, SyncOperationError>,
    ) {
        self.list_outcomes
            .entry(day.to_owned())
            .or_default()
            .push_back(outcome);
    }
}

impl SyncJournal for FakeJournal {
    fn observer_name(&self) -> Option<&str> {
        None
    }

    fn ensure_registered<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<bool, SyncOperationError>> + Send + 'a>> {
        Box::pin(async move {
            let _ = self.calls.send(ObservedCall::Register);
            Ok(false)
        })
    }

    fn upload<'a>(
        &'a mut self,
        candidate: &'a SegmentCandidate,
        files: Vec<PathBuf>,
    ) -> Pin<Box<dyn Future<Output = Result<UploadResult, SyncOperationError>> + Send + 'a>> {
        Box::pin(async move {
            let _ = self
                .calls
                .send(ObservedCall::Upload(candidate.segment().to_owned()));
            if let Some(outcome) = self
                .upload_outcomes
                .get_mut(candidate.segment())
                .and_then(VecDeque::pop_front)
            {
                return outcome;
            }
            if self
                .uploaded
                .get(candidate.day())
                .is_some_and(|segments| segments.contains_key(candidate.segment()))
            {
                return Ok(UploadResult {
                    status: UploadStatus::Duplicate,
                    authoritative_key: Some(candidate.segment().to_owned()),
                });
            }
            let inventory = inventory_files(files).await.map_err(|_| {
                SyncOperationError::RetainCandidate(DiagnosticCode::LocalSegmentInvalid)
            })?;
            self.uploaded
                .entry(candidate.day().to_owned())
                .or_default()
                .insert(candidate.segment().to_owned(), inventory);
            Ok(UploadResult {
                status: UploadStatus::Ok,
                authoritative_key: Some(candidate.segment().to_owned()),
            })
        })
    }

    fn segments<'a>(
        &'a mut self,
        day: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<SegmentsEnvelope, SyncOperationError>> + Send + 'a>>
    {
        Box::pin(async move {
            let _ = self.calls.send(ObservedCall::Listing(day.to_owned()));
            if let Some(outcome) = self
                .list_outcomes
                .get_mut(day)
                .and_then(VecDeque::pop_front)
            {
                return outcome;
            }
            let items = self
                .uploaded
                .get(day)
                .into_iter()
                .flat_map(|segments| segments.iter())
                .map(|(key, files)| SegmentItem {
                    key: key.clone(),
                    observed: false,
                    files: files
                        .iter()
                        .map(|file| SegmentFile {
                            name: file.name.clone(),
                            size: file.size,
                            sha256: file.sha256.clone(),
                            status: ListingFileStatus::Present,
                            submitted_name: None,
                        })
                        .collect(),
                    original_key: None,
                })
                .collect::<Vec<_>>();
            Ok(SegmentsEnvelope {
                total: items.len(),
                items,
                protocol_version: 2,
            })
        })
    }

    fn status_event<'a>(
        &'a mut self,
        _beacon: &'a StatusBeacon,
    ) -> Pin<Box<dyn Future<Output = Result<(), SyncOperationError>> + Send + 'a>> {
        Box::pin(async { Ok(()) })
    }
}

struct CountingCapture(Arc<AtomicUsize>);

impl CaptureProvider for CountingCapture {
    fn poll<'a>(
        &'a self,
        _wall_unix_seconds: i64,
        _capture_interval: Duration,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<CaptureResult>, ObserverOperationError>> + Send + 'a>>
    {
        Box::pin(async move {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(Vec::new())
        })
    }
}

struct RotatingSegment {
    rotations: Arc<AtomicUsize>,
    last_rotation: Duration,
}

impl RotatingSegment {
    fn new(rotations: Arc<AtomicUsize>) -> Self {
        Self {
            rotations,
            last_rotation: Duration::ZERO,
        }
    }
}

impl SegmentLifecycle for RotatingSegment {
    fn process_poll(
        &mut self,
        _captures: &[CaptureResult],
        _wall_now: time::OffsetDateTime,
        monotonic_now: Duration,
        segment_interval: Duration,
    ) -> Result<(), ObserverOperationError> {
        if monotonic_now.saturating_sub(self.last_rotation) >= segment_interval {
            self.rotations.fetch_add(1, Ordering::SeqCst);
            self.last_rotation = monotonic_now;
        }
        Ok(())
    }

    fn shutdown(
        &mut self,
        _monotonic_now: Duration,
    ) -> Result<SegmentClose, ObserverOperationError> {
        Ok(SegmentClose::RemovedEmpty)
    }
}

fn scheduler(temporary: &TestDirectory, clock: Arc<TestClock>, wake: SyncWake) -> SyncScheduler {
    SyncScheduler::new(temporary.path().join("captures"), stream(), -1, clock, wake)
}

fn stream() -> DerivedName {
    derive_component(STREAM).expect("stream name")
}

fn clock() -> Arc<TestClock> {
    let date = Date::from_calendar_date(2026, Month::July, 10).expect("test date");
    let time = Time::from_hms(12, 0, 0).expect("test time");
    Arc::new(TestClock::new(
        PrimitiveDateTime::new(date, time).assume_utc(),
        Duration::ZERO,
        UtcOffset::UTC,
    ))
}

fn create_segment(root: &Path, segment: &str, bytes: &[u8]) {
    create_segment_in_day(root, DAY, segment, bytes);
}

fn create_segment_in_day(root: &Path, day: &str, segment: &str, bytes: &[u8]) {
    let path = segment_path_in_day(root, day, segment);
    std::fs::create_dir_all(&path).expect("create scheduler segment");
    std::fs::write(path.join(FILE), bytes).expect("write scheduler segment");
}

fn segment_path(root: &Path, segment: &str) -> PathBuf {
    segment_path_in_day(root, DAY, segment)
}

fn segment_path_in_day(root: &Path, day: &str, segment: &str) -> PathBuf {
    root.join("captures").join(day).join(STREAM).join(segment)
}

fn retaining_scheduler(temporary: &TestDirectory) -> SyncScheduler {
    SyncScheduler::new(
        temporary.path().join("captures"),
        stream(),
        0,
        clock(),
        SyncWake::default(),
    )
}

fn empty_listing() -> SegmentsEnvelope {
    SegmentsEnvelope {
        items: Vec::new(),
        total: 0,
        protocol_version: 2,
    }
}

async fn listing_for_segments(root: &Path, segments: &[(&str, &str)]) -> SegmentsEnvelope {
    let mut items = Vec::new();
    for (day, segment) in segments {
        let files = inventory_files(vec![segment_path_in_day(root, day, segment).join(FILE)])
            .await
            .expect("inventory listing fixture");
        items.push(SegmentItem {
            key: (*segment).to_owned(),
            observed: false,
            files: files
                .into_iter()
                .map(|file| SegmentFile {
                    name: file.name,
                    size: file.size,
                    sha256: file.sha256,
                    status: ListingFileStatus::Present,
                    submitted_name: None,
                })
                .collect(),
            original_key: None,
        });
    }
    SegmentsEnvelope {
        total: items.len(),
        items,
        protocol_version: 2,
    }
}

fn failure<T>(class: SyncFailureClass) -> Result<T, SyncOperationError> {
    Err(SyncOperationError::EndPass(class))
}

async fn expect_listing(calls: &mut mpsc::UnboundedReceiver<ObservedCall>, context: &str) {
    match wait_for_call(calls, WAIT_CEILING, |call| match call {
        ObservedCall::Listing(_) => Some(()),
        _ => None,
    })
    .await
    {
        Ok(()) => {}
        Err(ObservationWaitError::DeadlineExpired) => {
            panic!("scheduler did not make the expected listing contact: {context}")
        }
        Err(ObservationWaitError::ChannelClosed) => {
            panic!(
                "scheduler did not make the expected listing contact: {context}: observation channel closed"
            )
        }
    }
}

async fn wait_for_call<T>(
    calls: &mut mpsc::UnboundedReceiver<ObservedCall>,
    bound: Duration,
    mut match_call: impl FnMut(ObservedCall) -> Option<T>,
) -> Result<T, ObservationWaitError> {
    let deadline = Instant::now() + bound;
    loop {
        loop {
            match calls.try_recv() {
                Ok(call) => {
                    if let Some(value) = match_call(call) {
                        return Ok(value);
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    return Err(ObservationWaitError::ChannelClosed);
                }
            }
        }
        if Instant::now() >= deadline {
            return Err(ObservationWaitError::DeadlineExpired);
        }
        tokio::task::yield_now().await;
    }
}

async fn wait_for_upload(
    calls: &mut mpsc::UnboundedReceiver<ObservedCall>,
    bound: Duration,
) -> Result<String, ObservationWaitError> {
    wait_for_call(calls, bound, |call| match call {
        ObservedCall::Upload(segment) => Some(segment),
        _ => None,
    })
    .await
}

async fn expect_upload(calls: &mut mpsc::UnboundedReceiver<ObservedCall>) -> String {
    match wait_for_upload(calls, WAIT_CEILING).await {
        Ok(segment) => segment,
        Err(ObservationWaitError::DeadlineExpired) => {
            panic!("scheduler did not attempt the expected candidate: deadline expired")
        }
        Err(ObservationWaitError::ChannelClosed) => {
            panic!("scheduler did not attempt the expected candidate: observation channel closed")
        }
    }
}

async fn assert_no_listing(calls: &mut mpsc::UnboundedReceiver<ObservedCall>) {
    for _ in 0..SCHEDULER_TURNS {
        tokio::task::yield_now().await;
    }
    while let Ok(call) = calls.try_recv() {
        assert!(
            !matches!(call, ObservedCall::Listing(_)),
            "scheduler bypassed its wake or backoff boundary"
        );
    }
}

fn drain_uploads(calls: &mut mpsc::UnboundedReceiver<ObservedCall>) -> Vec<String> {
    let mut uploads = Vec::new();
    while let Ok(call) = calls.try_recv() {
        if let ObservedCall::Upload(segment) = call {
            uploads.push(segment);
        }
    }
    uploads
}

fn drain_calls(calls: &mut mpsc::UnboundedReceiver<ObservedCall>) -> Vec<ObservedCall> {
    let mut observed = Vec::new();
    while let Ok(call) = calls.try_recv() {
        observed.push(call);
    }
    observed
}

async fn advance_both(clock: &TestClock, duration: Duration) {
    clock.set_monotonic(clock.monotonic_now() + duration);
    clock.set_wall(
        clock.wall_now()
            + time::Duration::seconds(i64::try_from(duration.as_secs()).expect("test duration")),
    );
    tokio::time::advance(duration).await;
    for _ in 0..SCHEDULER_TURNS {
        tokio::task::yield_now().await;
    }
}

async fn wait_for_idle_snapshot(root: &Path) -> serde_json::Value {
    let deadline = Instant::now() + WAIT_CEILING;
    loop {
        if let Ok(bytes) = std::fs::read(root.join(HEALTH_FILENAME))
            && let Ok(snapshot) = serde_json::from_slice::<serde_json::Value>(&bytes)
            && snapshot["sync_in_progress"] == false
        {
            return snapshot;
        }
        if Instant::now() >= deadline {
            panic!("scheduler did not publish its idle health snapshot");
        }
        tokio::task::yield_now().await;
    }
}

fn paused(future: impl Future<Output = ()>) {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("build paused runtime")
        .block_on(async {
            tokio::time::pause();
            let keep_time_manual = tokio::spawn(async {
                loop {
                    tokio::task::yield_now().await;
                }
            });
            future.await;
            keep_time_manual.abort();
            let _ = keep_time_manual.await;
        });
}

fn run(future: impl Future<Output = ()>) {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("build test runtime")
        .block_on(future);
}
