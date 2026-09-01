// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

mod support;

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use solstone_tmux::clock::{Clock, TestClock};
use solstone_tmux::config::DEFAULT_SOURCE;
use solstone_tmux::health::{DiagnosticCode, HEALTH_FILENAME, HealthWriter};
use solstone_tmux::instance_lock::InstanceLock;
use solstone_tmux::journal::{
    IngestDayManifest, IngestManifest, ListingFileStatus, LocalFile, ManifestDaySummary,
    ManifestSegment, SegmentFile, SegmentItem, SegmentsEnvelope, UploadResult, UploadStatus,
    inventory_files,
};
use solstone_tmux::model::CaptureResult;
use solstone_tmux::name::{DerivedName, derive_component};
use solstone_tmux::observer::{
    CaptureProvider, ObserverConfig, ObserverOperationError, SegmentLifecycle, ShutdownEvent,
    run_observer, shutdown_barrier,
};
use solstone_tmux::paths::ensure_private_directory;
use solstone_tmux::private_link::PROTOCOL_VERSION_NUMBER;
use solstone_tmux::segment::SegmentClose;
use solstone_tmux::sync::{
    SegmentCandidate, SyncActivity, SyncJournal, SyncOperationError, SyncScheduler, SyncWake,
};
use support::TestDirectory;
use time::{Date, Month, PrimitiveDateTime, Time, UtcOffset};
use tokio::sync::{mpsc, oneshot, watch};

const STREAM: &str = "host.tmux";
const FILE: &str = "tmux_main_screen.jsonl";
const SCHEDULER_TURNS: usize = 1_024;
const HANG_GUARD: Duration = Duration::from_secs(5);

#[test]
fn one_snapshot_attempts_every_candidate_once_and_yields_between_batches() {
    paused(async {
        let temporary = TestDirectory::new("sync-single-snapshot");
        for index in 0..17 {
            create_segment(
                &temporary,
                "20260701",
                &format!("12{index:02}00_300"),
                b"fixture\n",
            );
        }
        let mut scheduler = scheduler(&temporary, SyncWake::default());
        let mut journal = FakeJournal::default();
        let summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;
        let instrumentation = scheduler.instrumentation();

        assert_eq!(summary.attempted, 17);
        assert_eq!(instrumentation.candidate_scans, 1);
        assert_eq!(instrumentation.batches, 3);
        assert_eq!(instrumentation.batch_yields, instrumentation.batches - 1);
        assert_eq!(journal.uploads().len(), 17);
    });
}

/// The health file is the only progress signal an operator has. It used to be
/// written once before the batch loop and once after the sweep, so a sweep of
/// hundreds of candidates held `pending_segments` frozen for its whole duration
/// and a healthy sweep was indistinguishable from a wedged one.
#[test]
fn a_sweep_publishes_progress_after_every_batch_not_only_at_its_boundaries() {
    paused(async {
        let temporary = TestDirectory::new("sync-progress-published");
        for index in 0..17 {
            create_segment(
                &temporary,
                "20260701",
                &format!("12{index:02}00_300"),
                b"fixture\n",
            );
        }
        let mut scheduler = scheduler(&temporary, SyncWake::default());
        let mut journal = FakeJournal::default();
        scheduler.run_sweep(&mut journal, no_shutdown()).await;
        let instrumentation = scheduler.instrumentation();

        assert_eq!(instrumentation.batches, 3);
        // One publish per batch, on top of the sweep's own start/end writes.
        assert!(
            instrumentation.health_writes >= instrumentation.batches,
            "progress published {} times across {} batches -- an operator cannot \
             tell a working sweep from a stuck one",
            instrumentation.health_writes,
            instrumentation.batches
        );
    });
}

#[test]
fn cached_retained_content_is_not_rehashed_before_required_v3_upload() {
    run(async {
        let temporary = TestDirectory::new("sync-cache-reuse");
        for day in ["20260701", "20260702", "20260703"] {
            for index in 0..4 {
                create_segment(
                    &temporary,
                    day,
                    &format!("12{index:02}00_300"),
                    b"fixture\n",
                );
            }
        }
        let mut scheduler = scheduler(&temporary, SyncWake::default());
        let mut journal = FakeJournal::default();
        scheduler.run_sweep(&mut journal, no_shutdown()).await;
        journal.clear_calls();
        let before = scheduler.instrumentation();

        let summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;
        let after = scheduler.instrumentation();

        assert_eq!(summary.custodied, 12);
        assert_eq!(after.hashed_files - before.hashed_files, 0);
        assert_eq!(journal.uploads().len(), 3);
        assert_eq!(journal.listings_by_day().len(), 3);
    });
}

#[test]
fn same_size_content_change_invalidates_only_that_inventory() {
    run(async {
        let temporary = TestDirectory::new("sync-cache-change");
        create_segment(&temporary, "20260701", "120000_300", b"first\n");
        create_segment(&temporary, "20260701", "120100_300", b"other\n");
        let mut scheduler = scheduler(&temporary, SyncWake::default());
        let mut journal = FakeJournal::default();
        scheduler.run_sweep(&mut journal, no_shutdown()).await;
        journal.clear_calls();
        let before = scheduler.instrumentation();
        std::fs::write(
            segment_path(&temporary, "20260701", "120000_300").join(FILE),
            b"later\n",
        )
        .expect("rewrite same-size fixture");

        scheduler.run_sweep(&mut journal, no_shutdown()).await;
        let after = scheduler.instrumentation();

        assert_eq!(journal.uploads().len(), 2);
        assert!(journal.uploads().contains(&"120000_300".to_owned()));
        assert_eq!(after.hashed_files - before.hashed_files, 1);
    });
}

#[test]
fn adding_a_segment_file_invalidates_the_complete_inventory_only_for_that_candidate() {
    run(async {
        let temporary = TestDirectory::new("sync-membership-add");
        create_segment(&temporary, "20260701", "120000_300", b"target\n");
        create_segment(&temporary, "20260701", "120100_300", b"other\n");
        let mut scheduler = scheduler(&temporary, SyncWake::default());
        let mut journal = FakeJournal::default();
        scheduler.run_sweep(&mut journal, no_shutdown()).await;
        journal.clear_calls();
        let before = scheduler.instrumentation();
        std::fs::write(
            segment_path(&temporary, "20260701", "120000_300").join("tmux_aux_screen.jsonl"),
            b"added\n",
        )
        .expect("add valid segment file");

        scheduler.run_sweep(&mut journal, no_shutdown()).await;

        assert_eq!(journal.uploads().len(), 2);
        assert!(journal.uploads().contains(&"120000_300".to_owned()));
        assert_eq!(
            scheduler.instrumentation().hashed_files - before.hashed_files,
            2
        );
        assert_eq!(scheduler.cached_inventories(), 2);
        assert!(segment_path(&temporary, "20260701", "120000_300").is_dir());
    });
}

#[test]
fn removing_a_segment_file_invalidates_the_complete_inventory_only_for_that_candidate() {
    run(async {
        let temporary = TestDirectory::new("sync-membership-remove");
        create_segment(&temporary, "20260701", "120000_300", b"target\n");
        std::fs::write(
            segment_path(&temporary, "20260701", "120000_300").join("tmux_aux_screen.jsonl"),
            b"removed\n",
        )
        .expect("add valid segment file");
        create_segment(&temporary, "20260701", "120100_300", b"other\n");
        let mut scheduler = scheduler(&temporary, SyncWake::default());
        let mut journal = FakeJournal::default();
        scheduler.run_sweep(&mut journal, no_shutdown()).await;
        journal.clear_calls();
        let before = scheduler.instrumentation();
        std::fs::remove_file(
            segment_path(&temporary, "20260701", "120000_300").join("tmux_aux_screen.jsonl"),
        )
        .expect("remove valid segment file");

        scheduler.run_sweep(&mut journal, no_shutdown()).await;

        assert!(
            journal.uploads().len() == 1,
            "v3 uploads before the reconciliation triple"
        );
        assert_eq!(
            scheduler.instrumentation().hashed_files - before.hashed_files,
            1
        );
        assert_eq!(scheduler.cached_inventories(), 2);
        assert!(segment_path(&temporary, "20260701", "120000_300").is_dir());
    });
}

#[test]
fn renaming_a_segment_file_invalidates_the_complete_inventory_only_for_that_candidate() {
    run(async {
        let temporary = TestDirectory::new("sync-membership-rename");
        create_segment(&temporary, "20260701", "120000_300", b"target\n");
        std::fs::write(
            segment_path(&temporary, "20260701", "120000_300").join("tmux_aux_screen.jsonl"),
            b"renamed\n",
        )
        .expect("add valid segment file");
        create_segment(&temporary, "20260701", "120100_300", b"other\n");
        let mut scheduler = scheduler(&temporary, SyncWake::default());
        let mut journal = FakeJournal::default();
        scheduler.run_sweep(&mut journal, no_shutdown()).await;
        journal.clear_calls();
        let before = scheduler.instrumentation();
        std::fs::rename(
            segment_path(&temporary, "20260701", "120000_300").join("tmux_aux_screen.jsonl"),
            segment_path(&temporary, "20260701", "120000_300").join("tmux_renamed_screen.jsonl"),
        )
        .expect("rename valid segment file");

        scheduler.run_sweep(&mut journal, no_shutdown()).await;

        assert_eq!(journal.uploads().len(), 2);
        assert!(journal.uploads().contains(&"120000_300".to_owned()));
        assert_eq!(
            scheduler.instrumentation().hashed_files - before.hashed_files,
            2
        );
        assert_eq!(scheduler.cached_inventories(), 2);
        assert!(segment_path(&temporary, "20260701", "120000_300").is_dir());
    });
}

#[test]
fn remote_loss_requires_a_fresh_post_upload_listing_before_custody() {
    run(async {
        let temporary = TestDirectory::new("sync-remote-loss-post-proof");
        create_segment(&temporary, "20260701", "120000_300", b"fixture\n");
        let mut scheduler = scheduler(&temporary, SyncWake::default());
        let mut journal = FakeJournal::default();
        scheduler.run_sweep(&mut journal, no_shutdown()).await;
        journal.remote.clear();
        journal.clear_calls();
        journal.upload_outcome(
            "120000_300",
            Ok(UploadResult {
                status: UploadStatus::Ok,
                authoritative_key: Some("120000_300".to_owned()),
            }),
        );
        let segment = segment_path(&temporary, "20260701", "120000_300");
        let before = snapshot_segment_bytes(&segment);

        let summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;

        assert_eq!(journal.uploads(), ["120000_300"]);
        assert_eq!(summary.custodied, 0);
        assert_eq!(
            summary.diagnostic,
            Some(DiagnosticCode::LocalSegmentInvalid)
        );
        assert_segment_bytes_unchanged(&segment, &before);
    });
}

#[test]
fn failed_fresh_listing_cannot_promote_stale_custody_or_delete_the_segment() {
    run(async {
        let temporary = TestDirectory::new("sync-failed-fresh-listing");
        create_segment(&temporary, "20260701", "120000_300", b"fixture\n");
        let mut scheduler = scheduler(&temporary, SyncWake::default());
        let mut journal = FakeJournal::default();
        scheduler.run_sweep(&mut journal, no_shutdown()).await;
        journal.remote.clear();
        journal.clear_calls();
        journal.list_outcome(
            "20260701",
            Err(SyncOperationError::EndSweep(
                solstone_tmux::sync::SyncFailureClass::Timeout,
            )),
        );
        let segment = segment_path(&temporary, "20260701", "120000_300");
        let before = snapshot_segment_bytes(&segment);
        let summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;

        assert_eq!(journal.uploads(), ["120000_300"]);
        assert_eq!(summary.custodied, 0);
        assert_eq!(
            summary.failure,
            Some(solstone_tmux::sync::SyncFailureClass::Timeout)
        );
        assert_segment_bytes_unchanged(&segment, &before);
    });
}

#[test]
fn cache_prunes_absent_snapshot_candidates_and_missing_entries() {
    run(async {
        let temporary = TestDirectory::new("sync-cache-prune");
        create_segment(&temporary, "20260701", "120000_300", b"fixture\n");
        create_segment(&temporary, "20260701", "120100_300", b"fixture\n");
        let mut scheduler = scheduler(&temporary, SyncWake::default());
        let mut journal = FakeJournal::default();
        scheduler.run_sweep(&mut journal, no_shutdown()).await;
        assert_eq!(scheduler.cached_inventories(), 2);

        std::fs::remove_dir_all(segment_path(&temporary, "20260701", "120100_300"))
            .expect("remove retained segment");
        scheduler.run_sweep(&mut journal, no_shutdown()).await;

        assert_eq!(scheduler.cached_inventories(), 1);
    });
}

#[test]
fn retention_uses_fresh_proof_then_deletes_and_evicts_the_cached_inventory() {
    run(async {
        let temporary = TestDirectory::new("sync-retention-fresh-proof");
        create_segment(&temporary, "20260701", "120000_300", b"fixture\n");
        let mut scheduler = SyncScheduler::new(
            temporary.path().join("captures"),
            stream(),
            DEFAULT_SOURCE.to_owned(),
            0,
            clock(),
            SyncWake::default(),
        );
        let files = inventory_files(
            vec![segment_path(&temporary, "20260701", "120000_300").join(FILE)],
            None,
        )
        .await
        .expect("inventory fixture");
        let mut journal = FakeJournal::default();
        journal
            .remote
            .entry("20260701".to_owned())
            .or_default()
            .insert("120000_300".to_owned(), files);

        let summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;

        assert_eq!(summary.custodied, 1);
        assert!(!segment_path(&temporary, "20260701", "120000_300").exists());
        assert_eq!(scheduler.cached_inventories(), 0);
    });
}

#[test]
fn retention_requires_batch_fresh_proof_and_keeps_a_changed_mismatched_segment() {
    run(async {
        let temporary = TestDirectory::new("sync-retention-batch-fresh");
        let mut journal = FakeJournal::default();
        for index in 0..9 {
            let segment = format!("12{index:02}00_300");
            create_segment(&temporary, "20260701", &segment, b"fixture\n");
            let files = inventory_files(
                vec![segment_path(&temporary, "20260701", &segment).join(FILE)],
                None,
            )
            .await
            .expect("inventory fixture");
            journal
                .remote
                .entry("20260701".to_owned())
                .or_default()
                .insert(segment, files);
        }
        std::fs::write(
            segment_path(&temporary, "20260701", "120800_300").join(FILE),
            b"changed\n",
        )
        .expect("change final batch segment");
        journal.upload_outcome(
            "120800_300",
            Ok(UploadResult {
                status: UploadStatus::Failed,
                authoritative_key: None,
            }),
        );
        let mut scheduler = SyncScheduler::new(
            temporary.path().join("captures"),
            stream(),
            DEFAULT_SOURCE.to_owned(),
            0,
            clock(),
            SyncWake::default(),
        );
        let segment = segment_path(&temporary, "20260701", "120800_300");
        let before = snapshot_segment_bytes(&segment);

        let summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;

        assert_segment_bytes_unchanged(&segment, &before);
        assert!(journal.uploads().contains(&"120800_300".to_owned()));
        assert_eq!(summary.custodied, 8);
    });
}

#[test]
fn finalization_wake_is_latched_for_the_following_sweep() {
    run(async {
        let temporary = TestDirectory::new("sync-latched-wake");
        create_segment(&temporary, "20260701", "120000_300", b"fixture\n");
        let wake = SyncWake::default();
        let mut scheduler = scheduler(&temporary, wake.clone());
        let mut journal = FakeJournal::default();
        scheduler.run_sweep(&mut journal, no_shutdown()).await;
        create_segment(&temporary, "20260701", "120100_300", b"fixture\n");
        wake.segment_closed(&SegmentClose::Finalized(PathBuf::from("new-segment")));
        tokio::time::timeout(HANG_GUARD, wake.wait())
            .await
            .expect("finalization wake was not latched");
        journal.clear_calls();

        let summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;

        assert_eq!(summary.attempted, 2);
        assert_eq!(journal.uploads(), ["120100_300"]);
    });
}

#[test]
fn retention_disabled_second_sweep_reuses_inventory_but_runs_v3_uploads() {
    run(async {
        let temporary = TestDirectory::new("sync-quiet-converged");
        for index in 0..10 {
            create_segment(
                &temporary,
                "20260701",
                &format!("12{index:02}00_300"),
                b"fixture\n",
            );
        }
        let (activity, mut receiver) = tokio::sync::watch::channel(SyncActivity::Idle);
        let mut scheduler = scheduler(&temporary, SyncWake::default()).with_activity(activity);
        let mut journal = FakeJournal::default();
        scheduler.run_sweep(&mut journal, no_shutdown()).await;
        journal.clear_calls();
        receiver.borrow_and_update();
        let before = scheduler.instrumentation();

        scheduler.run_sweep(&mut journal, no_shutdown()).await;
        let after = scheduler.instrumentation();

        assert_eq!(*receiver.borrow(), SyncActivity::Idle);
        assert_eq!(journal.uploads().len(), 1);
        assert_eq!(after.hashed_files - before.hashed_files, 0);
        // sync.rs:1610 deliberately processes candidates newest-first.
        assert_eq!(
            journal.calls,
            vec![
                Call::Upload("120900_300".to_owned()),
                Call::Manifest,
                Call::ManifestDay("20260701".to_owned()),
                Call::Listing("20260701".to_owned()),
            ]
        );
    });
}

#[test]
fn bounded_batches_reflect_the_eight_candidate_limit() {
    run(async {
        for (count, expected_batches) in [(8, 1), (9, 2), (17, 3)] {
            let temporary = TestDirectory::new(&format!("sync-bounded-batches-{count}"));
            for index in 0..count {
                create_segment(
                    &temporary,
                    "20260701",
                    &format!("12{index:02}00_300"),
                    b"fixture\n",
                );
            }
            let mut scheduler = scheduler(&temporary, SyncWake::default());
            let mut journal = FakeJournal::default();
            let summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;

            assert_eq!(summary.attempted, count);
            assert_eq!(scheduler.instrumentation().batches, expected_batches);
        }
    });
}

#[test]
fn second_sweep_reuses_inventory_and_reconciles_through_v3_uploads() {
    run(async {
        let temporary = TestDirectory::new("sync-second-custody");
        for day in ["20260701", "20260702"] {
            create_segment(&temporary, day, "120000_300", b"fixture\n");
        }
        let mut scheduler = scheduler(&temporary, SyncWake::default());
        let mut journal = FakeJournal::default();
        scheduler.run_sweep(&mut journal, no_shutdown()).await;
        journal.clear_calls();
        let before = scheduler.instrumentation();
        let summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;
        let after = scheduler.instrumentation();
        assert_eq!(summary.custodied, 2);
        assert_eq!(journal.uploads().len(), 2);
        assert_eq!(journal.listings_by_day().len(), 2);
        assert_eq!(after.hashed_files - before.hashed_files, 0);
        // sync.rs:1610 deliberately processes candidates newest-first.
        assert_eq!(
            journal.calls,
            vec![
                Call::Upload("120000_300".to_owned()),
                Call::Manifest,
                Call::ManifestDay("20260702".to_owned()),
                Call::Listing("20260702".to_owned()),
                Call::Upload("120000_300".to_owned()),
                Call::Manifest,
                Call::ManifestDay("20260701".to_owned()),
                Call::Listing("20260701".to_owned()),
            ]
        );
    });
}

#[test]
fn stale_cached_custody_does_not_delete_when_current_listing_disagrees() {
    run(async {
        let temporary = TestDirectory::new("sync-stale-listing");
        create_segment(&temporary, "20260701", "120000_300", b"fixture\n");
        let mut scheduler = scheduler(&temporary, SyncWake::default());
        let mut journal = FakeJournal::default();
        scheduler.run_sweep(&mut journal, no_shutdown()).await;
        journal.remote.clear();
        journal.clear_calls();
        let segment = segment_path(&temporary, "20260701", "120000_300");
        let before = snapshot_segment_bytes(&segment);

        scheduler.run_sweep(&mut journal, no_shutdown()).await;

        assert_eq!(journal.uploads(), ["120000_300"]);
        assert_segment_bytes_unchanged(&segment, &before);
    });
}

#[test]
fn sweep_cache_is_keyed_by_day_when_rotation_splits_a_day() {
    run(async {
        let temporary = TestDirectory::new("sync-day-listings");
        create_segment(&temporary, "20260701", "120000_300", b"fixture\n");
        create_segment(&temporary, "20260702", "120000_300", b"fixture\n");
        let mut scheduler = scheduler(&temporary, SyncWake::default());
        let mut journal = FakeJournal::default();
        scheduler.run_sweep(&mut journal, no_shutdown()).await;
        journal.clear_calls();
        scheduler.run_sweep(&mut journal, no_shutdown()).await;
        assert_eq!(journal.listings_by_day().len(), 2);
    });
}

#[test]
fn v3_reconciliation_uses_one_complete_triple_per_unproven_upload() {
    run(async {
        let temporary = TestDirectory::new("sync-listing-bound");
        for segment in ["120000_300", "120100_300"] {
            create_segment(&temporary, "20260701", segment, b"fixture\n");
        }
        let mut scheduler = scheduler(&temporary, SyncWake::default());
        let mut journal = FakeJournal::default();
        scheduler.run_sweep(&mut journal, no_shutdown()).await;
        assert_eq!(journal.uploads().len(), 2);
        assert_eq!(journal.reconciliation_calls("20260701"), (2, 2, 2));
    });
}

#[test]
fn pending_segments_reaches_zero_when_custody_is_proven() {
    run(async {
        let temporary = TestDirectory::new("sync-pending");
        ensure_private_directory(temporary.path()).expect("prepare data root");
        for index in 0..9 {
            create_segment(
                &temporary,
                "20260701",
                &format!("12{index:02}00_300"),
                b"fixture\n",
            );
        }
        let mut journal = FakeJournal::default();
        for index in 0..9 {
            let segment = format!("12{index:02}00_300");
            let files = inventory_files(
                vec![segment_path(&temporary, "20260701", &segment).join(FILE)],
                None,
            )
            .await
            .expect("inventory fixture");
            journal
                .remote
                .entry("20260701".to_owned())
                .or_default()
                .insert(segment, files);
        }
        let lock = InstanceLock::acquire(temporary.path()).expect("instance lock");
        let health = HealthWriter::new(temporary.path().to_path_buf(), &lock);
        let (activity, _activity_receiver) = watch::channel(SyncActivity::Idle);
        let (stop, shutdown) = watch::channel(false);
        let mut scheduler =
            scheduler(&temporary, SyncWake::default()).with_observability(activity, health);
        let task = tokio::spawn(async move {
            scheduler.run_with_shutdown(&mut journal, shutdown).await;
        });
        let snapshot = wait_for_idle_snapshot(temporary.path()).await;
        stop.send_replace(true);
        task.await.expect("join scheduler");

        assert_eq!(snapshot["pending_segments"], 0);
        assert!(!snapshot["last_successful_sync_unix_seconds"].is_null());
    });
}

#[test]
fn collision_upload_uses_the_authoritative_renamed_segment_key() {
    run(async {
        let temporary = TestDirectory::new("sync-original-key");
        create_segment(&temporary, "20260701", "120000_300", b"fixture\n");
        let mut journal = FakeJournal::default();
        journal.upload_outcome(
            "120000_300",
            Ok(UploadResult {
                status: UploadStatus::Collision,
                authoritative_key: Some("120000_301".to_owned()),
            }),
        );
        let files = inventory_files(
            vec![segment_path(&temporary, "20260701", "120000_300").join(FILE)],
            None,
        )
        .await
        .expect("inventory fixture");
        journal
            .remote
            .entry("20260701".to_owned())
            .or_default()
            .insert("120000_301".to_owned(), files);
        let mut scheduler = scheduler(&temporary, SyncWake::default());
        let summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;
        assert_eq!(summary.custodied, 1);
        assert_eq!(journal.uploads(), ["120000_300"]);
    });
}

#[test]
fn changed_local_bytes_force_reupload() {
    run(async {
        let temporary = TestDirectory::new("sync-changed-digest");
        create_segment(&temporary, "20260701", "120000_300", b"first\n");
        let mut scheduler = scheduler(&temporary, SyncWake::default());
        let mut journal = FakeJournal::default();
        scheduler.run_sweep(&mut journal, no_shutdown()).await;
        journal.clear_calls();
        std::fs::write(
            segment_path(&temporary, "20260701", "120000_300").join(FILE),
            b"other\n",
        )
        .expect("change digest");
        let summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;
        assert_eq!(summary.custodied, 1);
        assert_eq!(journal.uploads(), ["120000_300"]);
    });
}

#[test]
fn remote_loss_is_reconciled_by_v3_reupload_before_custody() {
    run(async {
        let temporary = TestDirectory::new("sync-single-remote-loss");
        for segment in ["120000_300", "120100_300", "120200_300"] {
            create_segment(&temporary, "20260701", segment, b"fixture\n");
        }
        let mut scheduler = scheduler(&temporary, SyncWake::default());
        let mut journal = FakeJournal::default();
        scheduler.run_sweep(&mut journal, no_shutdown()).await;
        journal
            .remote
            .get_mut("20260701")
            .expect("remote day")
            .remove("120100_300");
        journal.clear_calls();
        let summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;
        assert_eq!(journal.uploads().len(), 2);
        assert!(journal.uploads().contains(&"120100_300".to_owned()));
        assert_eq!(summary.custodied, 3);
        assert_eq!(journal.reconciliation_calls("20260701"), (2, 2, 2));
    });
}

#[test]
fn scheduler_immediately_drains_the_remainder_of_a_bounded_sweep() {
    run(async {
        let temporary = TestDirectory::new("sync-drain-remainder");
        for index in 0..10 {
            create_segment(
                &temporary,
                "20260701",
                &format!("12{index:02}00_300"),
                b"fixture\n",
            );
        }
        let mut scheduler = scheduler(&temporary, SyncWake::default());
        let mut journal = FakeJournal::default();
        let summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;
        assert_eq!(summary.attempted, 10);
        assert_eq!(journal.uploads().len(), 10);
    });
}

#[test]
fn retained_outcomes_keep_their_diagnostic_and_never_claim_custody() {
    run(async {
        let temporary = TestDirectory::new("sync-retained-error");
        create_segment(&temporary, "20260701", "120000_300", b"fixture\n");
        let mut scheduler = scheduler(&temporary, SyncWake::default());
        let mut journal = FakeJournal::default();
        journal.upload_outcome(
            "120000_300",
            Err(SyncOperationError::RetainCandidate(
                DiagnosticCode::LocalSegmentInvalid,
            )),
        );
        let segment = segment_path(&temporary, "20260701", "120000_300");
        let before = snapshot_segment_bytes(&segment);
        let summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;
        assert_eq!(
            summary.diagnostic,
            Some(DiagnosticCode::LocalSegmentInvalid)
        );
        assert_eq!(summary.custodied, 0);
        assert_segment_bytes_unchanged(&segment, &before);
    });
}

#[test]
fn conflict_and_failed_contacts_do_not_claim_successful_custody() {
    run(async {
        for (name, status) in [
            ("sync-conflict", UploadStatus::Conflict),
            ("sync-failed", UploadStatus::Failed),
        ] {
            let temporary = TestDirectory::new(name);
            create_segment(&temporary, "20260701", "120000_300", b"fixture\n");
            let mut scheduler = scheduler(&temporary, SyncWake::default());
            let mut journal = FakeJournal::default();
            journal.upload_outcome(
                "120000_300",
                Ok(UploadResult {
                    status,
                    authoritative_key: None,
                }),
            );
            let segment = segment_path(&temporary, "20260701", "120000_300");
            let before = snapshot_segment_bytes(&segment);
            let summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;

            assert_eq!(summary.custodied, 0);
            assert_eq!(
                summary.diagnostic,
                Some(DiagnosticCode::LocalSegmentInvalid)
            );
            assert_segment_bytes_unchanged(&segment, &before);
        }
    });
}

#[test]
fn an_unproven_fresh_listing_records_a_diagnostic_and_keeps_the_segment() {
    run(async {
        let temporary = TestDirectory::new("sync-unproven-listing");
        create_segment(&temporary, "20260701", "120000_300", b"fixture\n");
        let mut scheduler = scheduler(&temporary, SyncWake::default());
        let mut journal = FakeJournal::default();
        journal.upload_outcome(
            "120000_300",
            Ok(UploadResult {
                status: UploadStatus::Ok,
                authoritative_key: Some("120000_300".to_owned()),
            }),
        );
        let segment = segment_path(&temporary, "20260701", "120000_300");
        let before = snapshot_segment_bytes(&segment);
        let summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;
        assert_eq!(
            summary.diagnostic,
            Some(DiagnosticCode::LocalSegmentInvalid)
        );
        assert_segment_bytes_unchanged(&segment, &before);
    });
}

#[test]
fn a_retained_candidate_still_lets_later_candidates_be_attempted() {
    run(async {
        let temporary = TestDirectory::new("sync-later-candidate");
        create_segment(&temporary, "20260701", "120000_300", b"fixture\n");
        create_segment(&temporary, "20260701", "120100_300", b"fixture\n");
        let mut scheduler = scheduler(&temporary, SyncWake::default());
        let mut journal = FakeJournal::default();
        journal.upload_outcome(
            "120100_300",
            Ok(UploadResult {
                status: UploadStatus::Conflict,
                authoritative_key: None,
            }),
        );
        let retained = segment_path(&temporary, "20260701", "120100_300");
        let before = snapshot_segment_bytes(&retained);
        let summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;
        assert_eq!(summary.attempted, 2);
        assert_eq!(journal.uploads(), ["120100_300", "120000_300"]);
        assert_segment_bytes_unchanged(&retained, &before);
        assert_eq!(summary.custodied, 1);
    });
}

#[test]
fn an_unscannable_capture_root_is_not_an_empty_success() {
    run(async {
        let temporary = TestDirectory::new("sync-invalid-root");
        std::os::unix::fs::symlink(temporary.path(), temporary.path().join("captures"))
            .expect("symlink root");
        let mut scheduler = scheduler(&temporary, SyncWake::default());
        let mut journal = FakeJournal::default();
        let summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;
        assert_eq!(
            summary.diagnostic,
            Some(DiagnosticCode::LocalSegmentInvalid)
        );
    });
}

#[test]
fn successful_empty_listing_counts_as_contact() {
    run(async {
        let temporary = TestDirectory::new("sync-empty");
        let mut scheduler = scheduler(&temporary, SyncWake::default());
        let mut journal = FakeJournal::default();
        let summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;
        assert!(summary.contacted);
        assert_eq!(summary.attempted, 0);
    });
}

#[test]
fn activity_is_working_only_while_a_real_candidate_is_in_flight() {
    run(async {
        let empty = TestDirectory::new("sync-activity-empty");
        let (activity, receiver) = tokio::sync::watch::channel(SyncActivity::Idle);
        let mut empty_scheduler = scheduler(&empty, SyncWake::default()).with_activity(activity);
        let mut empty_journal = FakeJournal::default();
        empty_scheduler
            .run_sweep(&mut empty_journal, no_shutdown())
            .await;
        assert_eq!(*receiver.borrow(), SyncActivity::Idle);

        let working = TestDirectory::new("sync-activity-working");
        create_segment(&working, "20260701", "120000_300", b"fixture\n");
        let (activity, receiver) = tokio::sync::watch::channel(SyncActivity::Idle);
        let (entered, started) = oneshot::channel();
        let (release, released) = oneshot::channel();
        let mut scheduler = scheduler(&working, SyncWake::default()).with_activity(activity);
        let task = tokio::spawn(async move {
            let mut journal = GatedJournal {
                inner: FakeJournal::default(),
                entered: Some(entered),
                release: Some(released),
            };
            scheduler.run_sweep(&mut journal, no_shutdown()).await
        });
        started.await.expect("upload began");
        assert_eq!(*receiver.borrow(), SyncActivity::Working);
        release.send(()).expect("release upload");
        task.await.expect("join sweep");
        assert_eq!(*receiver.borrow(), SyncActivity::Idle);
    });
}

#[test]
fn delivery_across_batches_has_one_working_interval_and_failures_return_idle() {
    run(async {
        let temporary = TestDirectory::new("sync-activity-single-interval");
        for index in 0..10 {
            create_segment(
                &temporary,
                "20260701",
                &format!("12{index:02}00_300"),
                b"fixture\n",
            );
        }
        let (activity, mut receiver) = watch::channel(SyncActivity::Idle);
        let transitions = Arc::new(Mutex::new(vec![SyncActivity::Idle]));
        let recorded = Arc::clone(&transitions);
        let (record_done, mut record_finished) = oneshot::channel();
        let recorder = tokio::spawn(async move {
            loop {
                tokio::select! {
                    changed = receiver.changed() => {
                        if changed.is_err() {
                            return;
                        }
                        recorded.lock().expect("transition lock").push(*receiver.borrow());
                    }
                    _ = &mut record_finished => return,
                }
            }
        });
        let (entered, started) = oneshot::channel();
        let (release, released) = oneshot::channel();
        let mut active_scheduler =
            scheduler(&temporary, SyncWake::default()).with_activity(activity);
        let task = tokio::spawn(async move {
            let mut journal = GatedJournal {
                inner: FakeJournal::default(),
                entered: Some(entered),
                release: Some(released),
            };
            active_scheduler
                .run_sweep(&mut journal, no_shutdown())
                .await
        });
        started.await.expect("first upload began");
        tokio::task::yield_now().await;
        release.send(()).expect("release first upload");
        assert_eq!(task.await.expect("join sweep").custodied, 10);
        let _ = record_done.send(());
        recorder.await.expect("join recorder");

        assert_eq!(
            *transitions.lock().expect("transition lock"),
            vec![
                SyncActivity::Idle,
                SyncActivity::Working,
                SyncActivity::Idle
            ],
        );

        let failed = TestDirectory::new("sync-activity-failure-idle");
        create_segment(&failed, "20260701", "120000_300", b"fixture\n");
        let (activity, receiver) = watch::channel(SyncActivity::Idle);
        let mut scheduler = scheduler(&failed, SyncWake::default()).with_activity(activity);
        let mut journal = FakeJournal::default();
        journal.upload_outcome(
            "120000_300",
            Err(SyncOperationError::EndSweep(
                solstone_tmux::sync::SyncFailureClass::Timeout,
            )),
        );

        let summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;

        assert_eq!(
            summary.failure,
            Some(solstone_tmux::sync::SyncFailureClass::Timeout)
        );
        assert_eq!(*receiver.borrow(), SyncActivity::Idle);
    });
}

#[test]
fn shutdown_cancels_a_pending_empty_scan_listing() {
    run(async {
        let temporary = TestDirectory::new("sync-cancel-empty-listing");
        let (entered, started) = oneshot::channel();
        let (mut journal, uploads) = blocking_journal(BlockingStage::EmptyListing, entered);
        let (stop, shutdown) = watch::channel(false);
        let mut scheduler = scheduler(&temporary, SyncWake::default());
        let task = tokio::spawn(async move { scheduler.run_sweep(&mut journal, shutdown).await });
        started.await.expect("empty listing began");
        stop.send_replace(true);

        let summary = tokio::time::timeout(HANG_GUARD, task)
            .await
            .expect("shutdown must interrupt empty listing")
            .expect("join sweep");
        assert!(summary.cancelled);
        assert_eq!(summary.attempted, 0);
        assert!(uploads.lock().expect("uploads lock").is_empty());
    });
}

#[test]
fn shutdown_cancels_a_pending_upload_and_restores_idle() {
    run(async {
        let temporary = TestDirectory::new("sync-cancel-upload");
        for index in 0..10 {
            create_segment(
                &temporary,
                "20260701",
                &format!("12{index:02}00_300"),
                b"fixture\n",
            );
        }
        let (activity, receiver) = watch::channel(SyncActivity::Idle);
        let (entered, started) = oneshot::channel();
        let (mut journal, uploads) = blocking_journal(BlockingStage::Upload, entered);
        let (stop, shutdown) = watch::channel(false);
        let mut scheduler = scheduler(&temporary, SyncWake::default()).with_activity(activity);
        let task = tokio::spawn(async move { scheduler.run_sweep(&mut journal, shutdown).await });
        started.await.expect("upload began");
        assert_eq!(*receiver.borrow(), SyncActivity::Working);
        stop.send_replace(true);

        let summary = tokio::time::timeout(HANG_GUARD, task)
            .await
            .expect("shutdown must interrupt upload")
            .expect("join sweep");
        assert!(summary.cancelled);
        assert_eq!(summary.attempted, 1);
        assert_eq!(uploads.lock().expect("uploads lock").len(), 1);
        assert_eq!(*receiver.borrow(), SyncActivity::Idle);
    });
}

#[test]
fn shutdown_cancels_a_pending_postupload_listing_without_starting_later_candidates() {
    run(async {
        let temporary = TestDirectory::new("sync-cancel-postupload-listing");
        for index in 0..10 {
            create_segment(
                &temporary,
                "20260701",
                &format!("12{index:02}00_300"),
                b"fixture\n",
            );
        }
        let (activity, receiver) = watch::channel(SyncActivity::Idle);
        let (entered, started) = oneshot::channel();
        let (mut journal, uploads) = blocking_journal(BlockingStage::PostUploadListing, entered);
        let (stop, shutdown) = watch::channel(false);
        let mut scheduler = scheduler(&temporary, SyncWake::default()).with_activity(activity);
        let task = tokio::spawn(async move { scheduler.run_sweep(&mut journal, shutdown).await });
        started.await.expect("post-upload listing began");
        stop.send_replace(true);

        let summary = tokio::time::timeout(HANG_GUARD, task)
            .await
            .expect("shutdown must interrupt post-upload listing")
            .expect("join sweep");
        assert!(summary.cancelled);
        assert_eq!(summary.attempted, 1);
        assert_eq!(uploads.lock().expect("uploads lock").len(), 1);
        assert_eq!(*receiver.borrow(), SyncActivity::Idle);
    });
}

#[test]
fn startup_finalization_and_periodic_wakes_converge_on_a_rescan() {
    paused(async {
        let temporary = TestDirectory::new("sync-wake-sources");
        let clock = clock();
        let wake = SyncWake::default();
        let (listings, mut received) = mpsc::unbounded_channel();
        let journal = BackoffJournal {
            listings,
            outcomes: VecDeque::new(),
        };
        let (stop, shutdown) = watch::channel(false);
        let mut scheduler = SyncScheduler::new(
            temporary.path().join("captures"),
            stream(),
            DEFAULT_SOURCE.to_owned(),
            -1,
            clock.clone() as Arc<dyn Clock>,
            wake.clone(),
        );
        let task = tokio::spawn(async move {
            let mut journal = journal;
            scheduler.run_with_shutdown(&mut journal, shutdown).await;
        });

        expect_listing(&mut received, "startup").await;
        wake.segment_closed(&SegmentClose::Finalized(PathBuf::from("wake")));
        expect_listing(&mut received, "finalization").await;
        advance_both(&clock, Duration::from_secs(60) + Duration::from_millis(1)).await;
        expect_listing(&mut received, "periodic").await;

        stop.send_replace(true);
        task.await.expect("join scheduler");
    });
}

#[test]
fn one_backoff_owner_advances_holds_resets_and_never_stops_capture() {
    paused(async {
        let temporary = TestDirectory::new("sync-backoff");
        let clock = clock();
        let wake = SyncWake::default();
        let (listings, mut received) = mpsc::unbounded_channel();
        let journal = BackoffJournal {
            listings,
            outcomes: VecDeque::from([
                Err(SyncOperationError::EndSweep(
                    solstone_tmux::sync::SyncFailureClass::Direct,
                )),
                Err(SyncOperationError::EndSweep(
                    solstone_tmux::sync::SyncFailureClass::Relay,
                )),
                Err(SyncOperationError::EndSweep(
                    solstone_tmux::sync::SyncFailureClass::Auth,
                )),
                Err(SyncOperationError::EndSweep(
                    solstone_tmux::sync::SyncFailureClass::Timeout,
                )),
                Err(SyncOperationError::EndSweep(
                    solstone_tmux::sync::SyncFailureClass::Contract,
                )),
                Ok(empty_listing()),
                Err(SyncOperationError::EndSweep(
                    solstone_tmux::sync::SyncFailureClass::Direct,
                )),
                Ok(empty_listing()),
            ]),
        };
        let (stop, shutdown) = watch::channel(false);
        let mut scheduler = SyncScheduler::new(
            temporary.path().join("captures"),
            stream(),
            DEFAULT_SOURCE.to_owned(),
            -1,
            clock.clone() as Arc<dyn Clock>,
            wake.clone(),
        );
        let sync = tokio::spawn(async move {
            let mut journal = journal;
            scheduler.run_with_shutdown(&mut journal, shutdown).await;
        });

        let captures = Arc::new(AtomicUsize::new(0));
        let segments = Arc::new(AtomicUsize::new(0));
        let (observer_stop, observer_shutdown) = oneshot::channel();
        let (observer_barrier, supervisor_barrier) = shutdown_barrier();
        drop(supervisor_barrier);
        let observer = tokio::spawn(run_observer(
            Arc::new(CountingCapture(Arc::clone(&captures))),
            Box::new(CountingSegment(Arc::clone(&segments))),
            Arc::clone(&clock) as Arc<dyn Clock>,
            Box::pin(async move {
                let _ = observer_shutdown.await;
                ShutdownEvent::Injected
            }),
            observer_barrier,
            ObserverConfig {
                capture_interval: Duration::from_secs(5),
                segment_interval: Duration::from_secs(5),
            },
        ));

        expect_listing(&mut received, "initial failure").await;
        wake.segment_closed(&SegmentClose::Finalized(PathBuf::from("coalesced")));
        advance_both(&clock, Duration::from_secs(4)).await;
        assert_no_listing(&mut received).await;

        for (delay, context) in [
            (1_u64, "five-second retry"),
            (30, "thirty-second retry"),
            (120, "two-minute retry"),
            (300, "five-minute retry"),
            (300, "held five-minute retry"),
        ] {
            let captures_before = captures.load(Ordering::SeqCst);
            let segments_before = segments.load(Ordering::SeqCst);
            advance_both(
                &clock,
                Duration::from_secs(delay) + Duration::from_millis(1),
            )
            .await;
            expect_listing(&mut received, context).await;
            assert!(captures.load(Ordering::SeqCst) > captures_before);
            assert!(segments.load(Ordering::SeqCst) > segments_before);
        }

        wake.segment_closed(&SegmentClose::Finalized(PathBuf::from("reset")));
        expect_listing(&mut received, "post-success failure").await;
        advance_both(&clock, Duration::from_secs(4)).await;
        assert_no_listing(&mut received).await;
        advance_both(&clock, Duration::from_secs(1) + Duration::from_millis(1)).await;
        expect_listing(&mut received, "reset five-second retry").await;

        stop.send_replace(true);
        observer_stop.send(()).expect("stop observer");
        sync.await.expect("join sync");
        assert_eq!(observer.await.expect("join observer").exit_code, 0);
    });
}

#[test]
fn reused_custody_counts_as_successful_sync() {
    run(async {
        let temporary = TestDirectory::new("sync-reused-custody");
        ensure_private_directory(temporary.path()).expect("prepare data root");
        create_segment(&temporary, "20260701", "120000_300", b"fixture\n");
        let mut journal = FakeJournal::default();
        let files = inventory_files(
            vec![segment_path(&temporary, "20260701", "120000_300").join(FILE)],
            None,
        )
        .await
        .expect("inventory fixture");
        journal
            .remote
            .entry("20260701".to_owned())
            .or_default()
            .insert("120000_300".to_owned(), files);
        let lock = InstanceLock::acquire(temporary.path()).expect("instance lock");
        let health = HealthWriter::new(temporary.path().to_path_buf(), &lock);
        let (activity, _activity_receiver) = watch::channel(SyncActivity::Idle);
        let (stop, shutdown) = watch::channel(false);
        let mut scheduler =
            scheduler(&temporary, SyncWake::default()).with_observability(activity, health);
        let task = tokio::spawn(async move {
            let mut journal = journal;
            scheduler.run_with_shutdown(&mut journal, shutdown).await;
        });
        let snapshot = wait_for_idle_snapshot(temporary.path()).await;
        stop.send_replace(true);
        task.await.expect("join scheduler");

        assert_eq!(snapshot["pending_segments"], 0);
        assert!(!snapshot["last_successful_contact_unix_seconds"].is_null());
        assert!(!snapshot["last_successful_sync_unix_seconds"].is_null());
    });
}

#[test]
fn a_retained_candidate_keeps_operator_visible_error_truth() {
    run(async {
        let temporary = TestDirectory::new("sync-retained-truth");
        ensure_private_directory(temporary.path()).expect("prepare data root");
        create_segment(&temporary, "20260701", "120000_300", b"fixture\n");
        let lock = InstanceLock::acquire(temporary.path()).expect("instance lock");
        let health = HealthWriter::new(temporary.path().to_path_buf(), &lock);
        let (activity, _activity_receiver) = watch::channel(SyncActivity::Idle);
        let (stop, shutdown) = watch::channel(false);
        let mut scheduler = SyncScheduler::new(
            temporary.path().join("captures"),
            stream(),
            DEFAULT_SOURCE.to_owned(),
            0,
            clock(),
            SyncWake::default(),
        )
        .with_observability(activity, health);
        let mut journal = FakeJournal::default();
        journal.upload_outcome(
            "120000_300",
            Ok(UploadResult {
                status: UploadStatus::Failed,
                authoritative_key: None,
            }),
        );
        let task = tokio::spawn(async move {
            let mut journal = journal;
            scheduler.run_with_shutdown(&mut journal, shutdown).await;
        });
        let snapshot = wait_for_idle_snapshot(temporary.path()).await;
        stop.send_replace(true);
        task.await.expect("join scheduler");

        assert_eq!(snapshot["last_error_code"], "local_segment_invalid");
        assert_eq!(snapshot["recent_error_count"], 1);
        assert!(!snapshot["last_successful_contact_unix_seconds"].is_null());
        assert!(snapshot["last_successful_sync_unix_seconds"].is_null());
        assert_eq!(snapshot["pending_segments"], 1);
    });
}

#[test]
fn health_distinguishes_contact_from_custody_and_decrements_deleted_work() {
    run(async {
        let deleted = TestDirectory::new("sync-health-deleted");
        ensure_private_directory(deleted.path()).expect("prepare deleted data root");
        create_segment(&deleted, "20260701", "120000_300", b"fixture\n");
        let lock = InstanceLock::acquire(deleted.path()).expect("instance lock");
        let health = HealthWriter::new(deleted.path().to_path_buf(), &lock);
        let (activity, _activity_receiver) = watch::channel(SyncActivity::Idle);
        let (stop, shutdown) = watch::channel(false);
        let mut scheduler = SyncScheduler::new(
            deleted.path().join("captures"),
            stream(),
            DEFAULT_SOURCE.to_owned(),
            0,
            clock(),
            SyncWake::default(),
        )
        .with_observability(activity, health);
        let task = tokio::spawn(async move {
            let mut journal = FakeJournal::default();
            scheduler.run_with_shutdown(&mut journal, shutdown).await;
        });
        let snapshot = wait_for_idle_snapshot(deleted.path()).await;
        stop.send_replace(true);
        task.await.expect("join deleted sync");

        assert_eq!(snapshot["pending_segments"], 0);
        assert!(!snapshot["last_successful_contact_unix_seconds"].is_null());
        assert!(!snapshot["last_successful_sync_unix_seconds"].is_null());
        assert!(!segment_path(&deleted, "20260701", "120000_300").exists());

        let retained = TestDirectory::new("sync-health-retained");
        ensure_private_directory(retained.path()).expect("prepare retained data root");
        create_segment(&retained, "20260701", "120000_300", b"fixture\n");
        let lock = InstanceLock::acquire(retained.path()).expect("instance lock");
        let health = HealthWriter::new(retained.path().to_path_buf(), &lock);
        let (activity, _activity_receiver) = watch::channel(SyncActivity::Idle);
        let (stop, shutdown) = watch::channel(false);
        let mut scheduler = SyncScheduler::new(
            retained.path().join("captures"),
            stream(),
            DEFAULT_SOURCE.to_owned(),
            0,
            clock(),
            SyncWake::default(),
        )
        .with_observability(activity, health);
        let task = tokio::spawn(async move {
            let mut journal = FakeJournal::default();
            journal.upload_outcome(
                "120000_300",
                Ok(UploadResult {
                    status: UploadStatus::Conflict,
                    authoritative_key: None,
                }),
            );
            scheduler.run_with_shutdown(&mut journal, shutdown).await;
        });
        let snapshot = wait_for_idle_snapshot(retained.path()).await;
        stop.send_replace(true);
        task.await.expect("join retained sync");

        assert_eq!(snapshot["pending_segments"], 1);
        assert!(!snapshot["last_successful_contact_unix_seconds"].is_null());
        assert!(snapshot["last_successful_sync_unix_seconds"].is_null());
    });
}

#[test]
fn poison_segment_does_not_block_a_later_valid_candidate() {
    run(async {
        let temporary = TestDirectory::new("sync-poison-later");
        create_segment(&temporary, "20260701", "120000_300", b"valid\n");
        create_segment(&temporary, "20260701", "120100_300", b"poison\n");
        let mut scheduler = scheduler(&temporary, SyncWake::default());
        let mut journal = FakeJournal::default();
        journal.upload_outcome(
            "120100_300",
            Ok(UploadResult {
                status: UploadStatus::Conflict,
                authoritative_key: None,
            }),
        );
        scheduler.run_sweep(&mut journal, no_shutdown()).await;
        assert!(journal.uploads().contains(&"120000_300".to_owned()));
    });
}

#[test]
fn empty_candidate_manifest_contact_uses_the_resolved_source() {
    run(async {
        let temporary = TestDirectory::new("sync-empty-source");
        let mut scheduler = scheduler(&temporary, SyncWake::default());
        let mut journal = FakeJournal::default();
        let summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;
        assert_eq!(summary.attempted, 0);
        assert!(summary.contacted);
        assert_eq!(journal.calls, vec![Call::Manifest]);
        journal.assert_sources(DEFAULT_SOURCE);
    });
}

#[test]
fn default_source_sweep_custodies_and_unlinks_a_retention_candidate() {
    run(async {
        let temporary = TestDirectory::new("sync-default-source-retention");
        ensure_private_directory(temporary.path()).expect("prepare data root");
        create_segment(&temporary, "20260701", "120000_300", b"fixture\n");
        let lock = InstanceLock::acquire(temporary.path()).expect("instance lock");
        let health = HealthWriter::new(temporary.path().to_path_buf(), &lock);
        let (activity, _activity_receiver) = watch::channel(SyncActivity::Idle);
        let (stop, shutdown) = watch::channel(false);
        let mut scheduler =
            scheduler_with_source(&temporary, SyncWake::default(), DEFAULT_SOURCE, 0)
                .with_observability(activity, health);
        let mut journal = FakeJournal::default();
        let task = tokio::spawn(async move {
            scheduler.run_with_shutdown(&mut journal, shutdown).await;
            journal
        });
        let snapshot = wait_for_idle_snapshot(temporary.path()).await;
        stop.send_replace(true);
        let journal = task.await.expect("join scheduler");

        assert_eq!(snapshot["pending_segments"], 0);
        assert!(!snapshot["last_successful_contact_unix_seconds"].is_null());
        assert!(!snapshot["last_successful_sync_unix_seconds"].is_null());
        assert!(!segment_path(&temporary, "20260701", "120000_300").exists());
        journal.assert_sources(DEFAULT_SOURCE);
    });
}

#[test]
fn configured_source_sweep_sends_the_exact_source_on_every_call() {
    run(async {
        let temporary = TestDirectory::new("sync-configured-source");
        ensure_private_directory(temporary.path()).expect("prepare data root");
        create_segment(&temporary, "20260701", "120000_300", b"fixture\n");
        let lock = InstanceLock::acquire(temporary.path()).expect("instance lock");
        let health = HealthWriter::new(temporary.path().to_path_buf(), &lock);
        let (activity, _activity_receiver) = watch::channel(SyncActivity::Idle);
        let (stop, shutdown) = watch::channel(false);
        let mut scheduler = scheduler_with_source(&temporary, SyncWake::default(), "studio", 0)
            .with_observability(activity, health);
        let mut journal = FakeJournal::default();
        let task = tokio::spawn(async move {
            scheduler.run_with_shutdown(&mut journal, shutdown).await;
            journal
        });
        let snapshot = wait_for_idle_snapshot(temporary.path()).await;
        stop.send_replace(true);
        let journal = task.await.expect("join scheduler");

        assert_eq!(snapshot["pending_segments"], 0);
        assert!(!snapshot["last_successful_sync_unix_seconds"].is_null());
        assert!(!segment_path(&temporary, "20260701", "120000_300").exists());
        journal.assert_sources("studio");
    });
}

#[test]
fn source_mismatch_does_not_custody_or_unlink() {
    run(async {
        let temporary = TestDirectory::new("sync-source-mismatch");
        ensure_private_directory(temporary.path()).expect("prepare data root");
        create_segment(&temporary, "20260701", "120000_300", b"fixture\n");
        let files = inventory_files(
            vec![segment_path(&temporary, "20260701", "120000_300").join(FILE)],
            None,
        )
        .await
        .expect("inventory fixture");
        let mut journal = FakeJournal {
            evidence_for: Some(String::new()),
            ..FakeJournal::default()
        };
        journal
            .remote
            .entry("20260701".to_owned())
            .or_default()
            .insert("120000_300".to_owned(), files);
        let lock = InstanceLock::acquire(temporary.path()).expect("instance lock");
        let health = HealthWriter::new(temporary.path().to_path_buf(), &lock);
        let (activity, _activity_receiver) = watch::channel(SyncActivity::Idle);
        let (stop, shutdown) = watch::channel(false);
        let mut scheduler = scheduler_with_source(&temporary, SyncWake::default(), "studio", 0)
            .with_observability(activity, health);
        let segment = segment_path(&temporary, "20260701", "120000_300");
        let before = snapshot_segment_bytes(&segment);
        let task = tokio::spawn(async move {
            scheduler.run_with_shutdown(&mut journal, shutdown).await;
            journal
        });
        let snapshot = wait_for_idle_snapshot(temporary.path()).await;
        stop.send_replace(true);
        let journal = task.await.expect("join scheduler");

        assert_eq!(snapshot["pending_segments"], 1);
        assert!(!snapshot["last_successful_contact_unix_seconds"].is_null());
        assert!(snapshot["last_successful_sync_unix_seconds"].is_null());
        assert_segment_bytes_unchanged(&segment, &before);
        journal.assert_sources("studio");
    });
}

#[test]
fn predates_source_configuration_only_deletes_on_matching_configured_source() {
    run(async {
        let temporary = TestDirectory::new("sync-predates-source");
        create_segment(&temporary, "20260701", "120000_300", b"fixture\n");
        let files = inventory_files(
            vec![segment_path(&temporary, "20260701", "120000_300").join(FILE)],
            None,
        )
        .await
        .expect("inventory fixture");
        let mut journal = FakeJournal::default();
        journal
            .remote
            .entry("20260701".to_owned())
            .or_default()
            .insert("120000_300".to_owned(), files);
        let mut scheduler = scheduler_with_source(&temporary, SyncWake::default(), "studio", 0);
        let summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;

        assert_eq!(summary.custodied, 1);
        assert!(!segment_path(&temporary, "20260701", "120000_300").exists());
        assert!(journal.remote.contains_key("20260701"));
        journal.assert_sources("studio");
    });
}

#[test]
fn local_stream_paths_stay_independent_of_configured_source() {
    run(async {
        let temporary = TestDirectory::new("sync-stream-vs-source");
        create_segment(&temporary, "20260701", "120000_300", b"fixture\n");
        let mut scheduler = scheduler_with_source(&temporary, SyncWake::default(), "studio", -1);
        let mut journal = FakeJournal::default();
        scheduler.run_sweep(&mut journal, no_shutdown()).await;

        assert!(segment_path(&temporary, "20260701", "120000_300").is_dir());
        journal.assert_sources("studio");
    });
}

fn no_shutdown() -> tokio::sync::watch::Receiver<bool> {
    let (sender, receiver) = tokio::sync::watch::channel(false);
    std::mem::forget(sender);
    receiver
}

fn empty_listing() -> SegmentsEnvelope {
    SegmentsEnvelope {
        items: Vec::new(),
        total: 0,
        protocol_version: PROTOCOL_VERSION_NUMBER,
    }
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

async fn expect_listing(listings: &mut mpsc::UnboundedReceiver<()>, context: &str) {
    for _ in 0..SCHEDULER_TURNS {
        if listings.try_recv().is_ok() {
            for _ in 0..SCHEDULER_TURNS {
                tokio::task::yield_now().await;
            }
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("scheduler did not make the expected listing contact: {context}");
}

async fn assert_no_listing(listings: &mut mpsc::UnboundedReceiver<()>) {
    for _ in 0..SCHEDULER_TURNS {
        tokio::task::yield_now().await;
    }
    assert!(
        listings.try_recv().is_err(),
        "scheduler bypassed its wake or backoff boundary"
    );
}

async fn wait_for_idle_snapshot(root: &Path) -> serde_json::Value {
    let path = root.join(HEALTH_FILENAME);
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(bytes) = std::fs::read(&path)
                && let Ok(snapshot) = serde_json::from_slice::<serde_json::Value>(&bytes)
                && snapshot["sync_in_progress"] == false
            {
                return snapshot;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("scheduler did not publish its idle health snapshot within 10 seconds")
}

fn scheduler(temporary: &TestDirectory, wake: SyncWake) -> SyncScheduler {
    scheduler_with_source(temporary, wake, DEFAULT_SOURCE, -1)
}

fn scheduler_with_source(
    temporary: &TestDirectory,
    wake: SyncWake,
    source: &str,
    retention_days: i64,
) -> SyncScheduler {
    SyncScheduler::new(
        temporary.path().join("captures"),
        stream(),
        source.to_owned(),
        retention_days,
        clock(),
        wake,
    )
}

fn stream() -> DerivedName {
    derive_component(STREAM).expect("stream")
}

fn clock() -> Arc<TestClock> {
    let date = Date::from_calendar_date(2026, Month::July, 10).expect("date");
    let time = Time::from_hms(12, 0, 0).expect("time");
    Arc::new(TestClock::new(
        PrimitiveDateTime::new(date, time).assume_utc(),
        Duration::ZERO,
        UtcOffset::UTC,
    ))
}

fn create_segment(temporary: &TestDirectory, day: &str, segment: &str, bytes: &[u8]) {
    let path = segment_path(temporary, day, segment);
    std::fs::create_dir_all(&path).expect("create segment");
    std::fs::write(path.join(FILE), bytes).expect("write segment");
}

fn segment_path(temporary: &TestDirectory, day: &str, segment: &str) -> PathBuf {
    temporary
        .path()
        .join("captures")
        .join(day)
        .join(STREAM)
        .join(segment)
}

fn snapshot_segment_bytes(segment: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut snapshot = BTreeMap::new();
    collect_segment_bytes(segment, segment, &mut snapshot);
    snapshot
}

fn collect_segment_bytes(
    segment: &Path,
    directory: &Path,
    snapshot: &mut BTreeMap<PathBuf, Vec<u8>>,
) {
    for entry in std::fs::read_dir(directory).expect("read segment directory") {
        let entry = entry.expect("read segment entry");
        let path = entry.path();
        let relative = path
            .strip_prefix(segment)
            .expect("entry below segment")
            .to_owned();
        let file_type = entry.file_type().expect("inspect segment entry");
        if file_type.is_dir() {
            collect_segment_bytes(segment, &path, snapshot);
        } else if file_type.is_file() {
            assert!(
                snapshot
                    .insert(relative, std::fs::read(path).expect("read segment file"))
                    .is_none(),
                "duplicate segment file"
            );
        } else {
            panic!("unexpected non-file entry in segment");
        }
    }
}

fn assert_segment_bytes_unchanged(segment: &Path, before: &BTreeMap<PathBuf, Vec<u8>>) {
    assert_eq!(
        snapshot_segment_bytes(segment),
        *before,
        "segment bytes changed"
    );
}

#[derive(Default)]
struct FakeJournal {
    calls: Vec<Call>,
    sources: Vec<String>,
    evidence_for: Option<String>,
    remote: HashMap<String, HashMap<String, Vec<LocalFile>>>,
    list_outcomes: HashMap<String, VecDeque<Result<SegmentsEnvelope, SyncOperationError>>>,
    upload_outcomes: HashMap<String, VecDeque<Result<UploadResult, SyncOperationError>>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Call {
    Upload(String),
    Manifest,
    ManifestDay(String),
    Listing(String),
}

impl FakeJournal {
    fn list_outcome(&mut self, day: &str, outcome: Result<SegmentsEnvelope, SyncOperationError>) {
        self.list_outcomes
            .entry(day.to_owned())
            .or_default()
            .push_back(outcome);
    }

    fn upload_outcome(&mut self, segment: &str, outcome: Result<UploadResult, SyncOperationError>) {
        self.upload_outcomes
            .entry(segment.to_owned())
            .or_default()
            .push_back(outcome);
    }
    fn clear_calls(&mut self) {
        self.calls.clear();
        self.sources.clear();
    }

    fn record_source(&mut self, source: &str) {
        self.sources.push(source.to_owned());
    }

    fn evidence_visible(&self, source: &str) -> bool {
        self.evidence_for
            .as_deref()
            .is_none_or(|expected| expected == source)
    }

    fn assert_sources(&self, expected: &str) {
        assert!(
            !self.sources.is_empty(),
            "journal received no source-bearing calls"
        );
        assert!(
            self.sources.iter().all(|source| source == expected),
            "journal sources {:?} did not all equal {expected}",
            self.sources
        );
    }

    fn uploads(&self) -> Vec<String> {
        self.calls
            .iter()
            .filter_map(|call| match call {
                Call::Upload(segment) => Some(segment.clone()),
                Call::Manifest | Call::ManifestDay(_) | Call::Listing(_) => None,
            })
            .collect()
    }

    fn listings_by_day(&self) -> HashMap<String, usize> {
        let mut counts = HashMap::new();
        for call in &self.calls {
            if let Call::Listing(day) = call {
                *counts.entry(day.clone()).or_insert(0) += 1;
            }
        }
        counts
    }

    fn reconciliation_calls(&self, day: &str) -> (usize, usize, usize) {
        let manifests = self
            .calls
            .iter()
            .filter(|call| matches!(call, Call::Manifest))
            .count();
        let manifest_days = self
            .calls
            .iter()
            .filter(|call| matches!(call, Call::ManifestDay(call_day) if call_day == day))
            .count();
        let segments = self
            .calls
            .iter()
            .filter(|call| matches!(call, Call::Listing(call_day) if call_day == day))
            .count();
        (manifests, manifest_days, segments)
    }
}

impl SyncJournal for FakeJournal {
    fn upload<'a>(
        &'a mut self,
        candidate: &'a SegmentCandidate,
        files: Vec<PathBuf>,
        source: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<UploadResult, SyncOperationError>> + Send + 'a>> {
        Box::pin(async move {
            self.record_source(source);
            self.calls
                .push(Call::Upload(candidate.segment().to_owned()));
            if let Some(outcome) = self
                .upload_outcomes
                .get_mut(candidate.segment())
                .and_then(VecDeque::pop_front)
            {
                return outcome;
            }
            let inventory = inventory_files(files, None).await.map_err(|_| {
                SyncOperationError::RetainCandidate(DiagnosticCode::LocalSegmentInvalid)
            })?;
            if self.evidence_visible(source) {
                self.remote
                    .entry(candidate.day().to_owned())
                    .or_default()
                    .insert(candidate.segment().to_owned(), inventory);
            }
            Ok(UploadResult {
                status: UploadStatus::Ok,
                authoritative_key: Some(candidate.segment().to_owned()),
            })
        })
    }

    fn segments<'a>(
        &'a mut self,
        day: &'a str,
        source: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<SegmentsEnvelope, SyncOperationError>> + Send + 'a>>
    {
        Box::pin(async move {
            self.record_source(source);
            self.calls.push(Call::Listing(day.to_owned()));
            if !self.evidence_visible(source) {
                return Ok(empty_listing());
            }
            if let Some(outcome) = self
                .list_outcomes
                .get_mut(day)
                .and_then(VecDeque::pop_front)
            {
                return outcome;
            }
            let items = self
                .remote
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
                protocol_version: PROTOCOL_VERSION_NUMBER,
            })
        })
    }

    fn manifest<'a>(
        &'a mut self,
        source: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<IngestManifest, SyncOperationError>> + Send + 'a>> {
        Box::pin(async move {
            self.record_source(source);
            self.calls.push(Call::Manifest);
            if !self.evidence_visible(source) {
                return Ok(IngestManifest {
                    days: HashMap::new().into_iter().collect(),
                });
            }
            Ok(IngestManifest {
                days: self
                    .remote
                    .iter()
                    .map(|(day, segments)| {
                        (
                            day.clone(),
                            ManifestDaySummary {
                                segments: segments.len(),
                            },
                        )
                    })
                    .collect(),
            })
        })
    }

    fn manifest_day<'a>(
        &'a mut self,
        day: &'a str,
        source: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<IngestDayManifest, SyncOperationError>> + Send + 'a>>
    {
        Box::pin(async move {
            self.record_source(source);
            self.calls.push(Call::ManifestDay(day.to_owned()));
            if !self.evidence_visible(source) {
                return Ok(IngestDayManifest {
                    version: 1,
                    day: day.to_owned(),
                    segments: HashMap::new().into_iter().collect(),
                });
            }
            Ok(IngestDayManifest {
                version: 1,
                day: day.to_owned(),
                segments: self
                    .remote
                    .get(day)
                    .into_iter()
                    .flat_map(|segments| segments.iter())
                    .map(|(key, files)| {
                        (
                            key.clone(),
                            ManifestSegment {
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
                            },
                        )
                    })
                    .collect(),
            })
        })
    }
}

struct BackoffJournal {
    listings: mpsc::UnboundedSender<()>,
    outcomes: VecDeque<Result<SegmentsEnvelope, SyncOperationError>>,
}

impl SyncJournal for BackoffJournal {
    fn upload<'a>(
        &'a mut self,
        _candidate: &'a SegmentCandidate,
        _files: Vec<PathBuf>,
        _source: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<UploadResult, SyncOperationError>> + Send + 'a>> {
        Box::pin(async { unreachable!("backoff fixture scans no candidates") })
    }

    fn segments<'a>(
        &'a mut self,
        _day: &'a str,
        _source: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<SegmentsEnvelope, SyncOperationError>> + Send + 'a>>
    {
        Box::pin(async move {
            let _ = self.listings.send(());
            self.outcomes
                .pop_front()
                .unwrap_or_else(|| Ok(empty_listing()))
        })
    }

    fn manifest<'a>(
        &'a mut self,
        _source: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<IngestManifest, SyncOperationError>> + Send + 'a>> {
        Box::pin(async move {
            let _ = self.listings.send(());
            match self
                .outcomes
                .pop_front()
                .unwrap_or_else(|| Ok(empty_listing()))
            {
                Ok(_) => Ok(IngestManifest {
                    days: HashMap::new().into_iter().collect(),
                }),
                Err(error) => Err(error),
            }
        })
    }

    fn manifest_day<'a>(
        &'a mut self,
        day: &'a str,
        _source: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<IngestDayManifest, SyncOperationError>> + Send + 'a>>
    {
        Box::pin(async move {
            Ok(IngestDayManifest {
                version: 1,
                day: day.to_owned(),
                segments: HashMap::new().into_iter().collect(),
            })
        })
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

struct CountingSegment(Arc<AtomicUsize>);

impl SegmentLifecycle for CountingSegment {
    fn process_poll(
        &mut self,
        _captures: &[CaptureResult],
        _wall_now: time::OffsetDateTime,
        _monotonic_now: Duration,
        _segment_interval: Duration,
    ) -> Result<(), ObserverOperationError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn shutdown(
        &mut self,
        _monotonic_now: Duration,
    ) -> Result<SegmentClose, ObserverOperationError> {
        Ok(SegmentClose::RemovedEmpty)
    }
}

struct GatedJournal {
    inner: FakeJournal,
    entered: Option<oneshot::Sender<()>>,
    release: Option<oneshot::Receiver<()>>,
}

impl SyncJournal for GatedJournal {
    fn upload<'a>(
        &'a mut self,
        candidate: &'a SegmentCandidate,
        files: Vec<PathBuf>,
        source: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<UploadResult, SyncOperationError>> + Send + 'a>> {
        Box::pin(async move {
            if let Some(entered) = self.entered.take() {
                let _ = entered.send(());
            }
            if let Some(release) = self.release.take() {
                let _ = release.await;
            }
            self.inner.upload(candidate, files, source).await
        })
    }

    fn segments<'a>(
        &'a mut self,
        day: &'a str,
        source: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<SegmentsEnvelope, SyncOperationError>> + Send + 'a>>
    {
        self.inner.segments(day, source)
    }

    fn manifest<'a>(
        &'a mut self,
        source: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<IngestManifest, SyncOperationError>> + Send + 'a>> {
        self.inner.manifest(source)
    }

    fn manifest_day<'a>(
        &'a mut self,
        day: &'a str,
        source: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<IngestDayManifest, SyncOperationError>> + Send + 'a>>
    {
        self.inner.manifest_day(day, source)
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum BlockingStage {
    EmptyListing,
    Upload,
    PostUploadListing,
}

struct BlockingJournal {
    stage: BlockingStage,
    entered: Option<oneshot::Sender<()>>,
    segment_calls: usize,
    uploads: Arc<Mutex<Vec<String>>>,
}

fn blocking_journal(
    stage: BlockingStage,
    entered: oneshot::Sender<()>,
) -> (BlockingJournal, Arc<Mutex<Vec<String>>>) {
    let uploads = Arc::new(Mutex::new(Vec::new()));
    (
        BlockingJournal {
            stage,
            entered: Some(entered),
            segment_calls: 0,
            uploads: Arc::clone(&uploads),
        },
        uploads,
    )
}

impl BlockingJournal {
    async fn wait_forever(&mut self) -> ! {
        if let Some(entered) = self.entered.take() {
            let _ = entered.send(());
        }
        std::future::pending().await
    }

    fn blocks_listing(&self) -> bool {
        match self.stage {
            BlockingStage::PostUploadListing => self.segment_calls == 1,
            _ => false,
        }
    }
}

impl SyncJournal for BlockingJournal {
    fn upload<'a>(
        &'a mut self,
        candidate: &'a SegmentCandidate,
        _files: Vec<PathBuf>,
        _source: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<UploadResult, SyncOperationError>> + Send + 'a>> {
        Box::pin(async move {
            self.uploads
                .lock()
                .expect("uploads lock")
                .push(candidate.segment().to_owned());
            if self.stage == BlockingStage::Upload {
                self.wait_forever().await;
            }
            Ok(UploadResult {
                status: UploadStatus::Ok,
                authoritative_key: Some(candidate.segment().to_owned()),
            })
        })
    }

    fn segments<'a>(
        &'a mut self,
        _day: &'a str,
        _source: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<SegmentsEnvelope, SyncOperationError>> + Send + 'a>>
    {
        Box::pin(async move {
            self.segment_calls += 1;
            if self.blocks_listing() {
                self.wait_forever().await;
            }
            Ok(SegmentsEnvelope {
                items: Vec::new(),
                total: 0,
                protocol_version: PROTOCOL_VERSION_NUMBER,
            })
        })
    }

    fn manifest<'a>(
        &'a mut self,
        _source: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<IngestManifest, SyncOperationError>> + Send + 'a>> {
        Box::pin(async move {
            if self.stage == BlockingStage::EmptyListing {
                self.wait_forever().await;
            }
            let days = if self.stage == BlockingStage::PostUploadListing {
                [("20260701".to_owned(), ManifestDaySummary { segments: 1 })]
                    .into_iter()
                    .collect()
            } else {
                HashMap::new().into_iter().collect()
            };
            Ok(IngestManifest { days })
        })
    }

    fn manifest_day<'a>(
        &'a mut self,
        day: &'a str,
        _source: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<IngestDayManifest, SyncOperationError>> + Send + 'a>>
    {
        Box::pin(async move {
            let segments = if self.stage == BlockingStage::PostUploadListing {
                (0..10)
                    .map(|index| {
                        (
                            format!("12{index:02}00_300"),
                            ManifestSegment { files: Vec::new() },
                        )
                    })
                    .collect()
            } else {
                HashMap::new().into_iter().collect()
            };
            Ok(IngestDayManifest {
                version: 1,
                day: day.to_owned(),
                segments,
            })
        })
    }
}

fn run(future: impl Future<Output = ()>) {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime")
        .block_on(future);
}

fn paused(future: impl Future<Output = ()>) {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("paused runtime")
        .block_on(async {
            tokio::time::pause();
            future.await;
        });
}
