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
    ListingFileStatus, LocalFile, ParsedDescriptor, SegmentFile, SegmentItem, SegmentsEnvelope,
    UploadResult, UploadStatus, decode_upload_response, inventory_files,
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
use solstone_tmux::storage::{
    AtomicWriteFault, set_atomic_write_fault, set_atomic_write_fault_for_prefix,
};
use solstone_tmux::sync::{
    JournalIdentity, SegmentCandidate, SyncActivity, SyncFailureClass, SyncJournal,
    SyncOperationError, SyncScheduler, SyncWake,
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
        let first_summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;
        assert_eq!(first_summary.custodied, 12);
        journal.clear_calls();
        let before = scheduler.instrumentation();

        let summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;
        let after = scheduler.instrumentation();

        assert_eq!(summary.custodied, 0);
        assert_eq!(after.hashed_files - before.hashed_files, 0);
        assert_eq!(journal.uploads().len(), 0);
        assert_eq!(journal.calls, vec![Call::SystemStatus]);
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

        assert_eq!(journal.uploads(), vec!["120000_300".to_owned()]);
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

        assert_eq!(journal.uploads(), vec!["120000_300".to_owned()]);
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

        assert_eq!(journal.uploads(), vec!["120000_300".to_owned()]);
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

        assert_eq!(journal.uploads(), vec!["120000_300".to_owned()]);
        assert_eq!(
            scheduler.instrumentation().hashed_files - before.hashed_files,
            2
        );
        assert_eq!(scheduler.cached_inventories(), 2);
        assert!(segment_path(&temporary, "20260701", "120000_300").is_dir());
    });
}

#[test]
fn missing_receipt_descriptors_rejects_custody() {
    run(async {
        let temporary = TestDirectory::new("sync-missing-receipt-descriptors");
        create_segment(&temporary, "20260701", "120000_300", b"fixture\n");
        let mut scheduler = scheduler(&temporary, SyncWake::default());
        let mut journal = FakeJournal::default();
        journal.upload_outcome(
            "120000_300",
            Ok(UploadResult {
                status: UploadStatus::Ok,
                authoritative_key: Some("120000_300".to_owned()),
                descriptors: None,
            }),
        );
        let segment = segment_path(&temporary, "20260701", "120000_300");
        let before = snapshot_segment_bytes(&segment);

        let summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;

        assert_eq!(journal.uploads(), ["120000_300"]);
        assert_eq!(summary.custodied, 0);
        assert_eq!(summary.diagnostic, None);
        assert_segment_bytes_unchanged(&segment, &before);
    });
}

#[test]
fn retention_day_read_failure_holds_the_day_without_ending_the_sweep() {
    run(async {
        let temporary = TestDirectory::new("sync-failed-retention-listing");
        create_segment(&temporary, "20260701", "120000_300", b"fixture\n");
        let mut scheduler =
            scheduler_with_source(&temporary, SyncWake::default(), DEFAULT_SOURCE, 0);
        let mut journal = FakeJournal::default();
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
        assert_eq!(summary.custodied, 1);
        assert_eq!(summary.failure, None);
        assert_eq!(summary.diagnostic, None);
        assert_segment_bytes_unchanged(&segment, &before);
    });
}

#[test]
fn upload_timeout_and_status_timeout_end_the_sweep() {
    run(async {
        let temporary = TestDirectory::new("sync-upload-timeout");
        create_segment(&temporary, "20260701", "120000_300", b"fixture\n");
        let mut upload_scheduler = scheduler(&temporary, SyncWake::default());
        let mut journal = FakeJournal::default();
        journal.upload_outcome(
            "120000_300",
            Err(SyncOperationError::EndSweep(
                solstone_tmux::sync::SyncFailureClass::Timeout,
            )),
        );
        let summary = upload_scheduler
            .run_sweep(&mut journal, no_shutdown())
            .await;
        assert_eq!(
            summary.failure,
            Some(solstone_tmux::sync::SyncFailureClass::Timeout)
        );

        let empty = TestDirectory::new("sync-status-timeout");
        let mut empty_scheduler = scheduler(&empty, SyncWake::default());
        let mut status_journal = FakeJournal::default();
        status_journal
            .status_outcomes
            .push_back(Err(SyncOperationError::EndSweep(
                solstone_tmux::sync::SyncFailureClass::Timeout,
            )));
        let status_summary = empty_scheduler
            .run_sweep(&mut status_journal, no_shutdown())
            .await;
        assert_eq!(
            status_summary.failure,
            Some(solstone_tmux::sync::SyncFailureClass::Timeout)
        );
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
fn retention_deletes_and_evicts_the_cached_inventory() {
    run(async {
        let temporary = TestDirectory::new("sync-retention-fresh-proof");
        create_segment(&temporary, "20260701", "120000_300", b"fixture\n");
        let mut scheduler =
            scheduler_with_source(&temporary, SyncWake::default(), DEFAULT_SOURCE, 0);
        let mut journal = FakeJournal::default();

        let summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;

        assert_eq!(summary.custodied, 1);
        assert!(!segment_path(&temporary, "20260701", "120000_300").exists());
        assert_eq!(scheduler.cached_inventories(), 0);
    });
}

#[test]
fn retention_keeps_a_failed_upload_segment() {
    run(async {
        let temporary = TestDirectory::new("sync-retention-batch-fresh");
        let mut journal = FakeJournal::default();
        for index in 0..9 {
            let segment = format!("12{index:02}00_300");
            create_segment(&temporary, "20260701", &segment, b"fixture\n");
        }
        journal.upload_outcome(
            "120800_300",
            Ok(UploadResult {
                status: UploadStatus::Failed,
                authoritative_key: None,
                descriptors: None,
            }),
        );
        let mut scheduler =
            scheduler_with_source(&temporary, SyncWake::default(), DEFAULT_SOURCE, 0);
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

        assert_eq!(summary.attempted, 1);
        assert_eq!(journal.uploads(), ["120100_300"]);
    });
}

#[test]
fn retention_disabled_second_sweep_reuses_inventory_and_makes_no_uploads() {
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
        let first_summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;
        assert_eq!(first_summary.custodied, 10);
        journal.clear_calls();
        receiver.borrow_and_update();
        let before = scheduler.instrumentation();

        let summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;
        let after = scheduler.instrumentation();

        assert_eq!(*receiver.borrow(), SyncActivity::Idle);
        assert_eq!(summary.custodied, 0);
        assert_eq!(journal.uploads().len(), 0);
        assert_eq!(after.hashed_files - before.hashed_files, 0);
        assert_eq!(journal.calls, vec![Call::SystemStatus]);
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
fn second_sweep_reuses_inventory_and_checks_status() {
    run(async {
        let temporary = TestDirectory::new("sync-second-custody");
        for day in ["20260701", "20260702"] {
            create_segment(&temporary, day, "120000_300", b"fixture\n");
        }
        let mut scheduler = scheduler(&temporary, SyncWake::default());
        let mut journal = FakeJournal::default();
        let first = scheduler.run_sweep(&mut journal, no_shutdown()).await;
        assert_eq!(first.custodied, 2);
        journal.clear_calls();
        let before = scheduler.instrumentation();
        let summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;
        let after = scheduler.instrumentation();
        assert_eq!(summary.custodied, 0);
        assert_eq!(journal.uploads().len(), 0);
        assert_eq!(after.hashed_files - before.hashed_files, 0);
        assert_eq!(journal.calls, vec![Call::SystemStatus]);
    });
}

#[test]
fn stale_hold_does_not_delete_when_current_listing_disagrees() {
    run(async {
        let temporary = TestDirectory::new("sync-stale-listing");
        create_segment(&temporary, "20260701", "120000_300", b"fixture\n");
        let mut scheduler =
            scheduler_with_source(&temporary, SyncWake::default(), DEFAULT_SOURCE, -1);
        let mut journal = FakeJournal::default();
        scheduler.run_sweep(&mut journal, no_shutdown()).await;
        journal.remote.clear();
        journal.clear_calls();
        let segment = segment_path(&temporary, "20260701", "120000_300");
        let before = snapshot_segment_bytes(&segment);

        let mut scheduler_retention =
            scheduler_with_source(&temporary, SyncWake::default(), DEFAULT_SOURCE, 0);
        let summary = scheduler_retention
            .run_sweep(&mut journal, no_shutdown())
            .await;

        assert_eq!(summary.custodied, 0);
        assert_segment_bytes_unchanged(&segment, &before);
    });
}

#[test]
fn retention_listings_are_keyed_by_day() {
    run(async {
        let temporary = TestDirectory::new("sync-day-listings");
        create_segment(&temporary, "20260701", "120000_300", b"fixture\n");
        create_segment(&temporary, "20260702", "120000_300", b"fixture\n");
        let mut scheduler =
            scheduler_with_source(&temporary, SyncWake::default(), DEFAULT_SOURCE, 0);
        let mut journal = FakeJournal::default();
        let summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;
        assert_eq!(summary.custodied, 2);
        assert_eq!(journal.listings_by_day().len(), 2);
    });
}

#[test]
fn retention_reconciliation_uses_one_listing_per_day() {
    run(async {
        let temporary = TestDirectory::new("sync-listing-bound");
        for segment in ["120000_300", "120100_300"] {
            create_segment(&temporary, "20260701", segment, b"fixture\n");
        }
        let mut scheduler =
            scheduler_with_source(&temporary, SyncWake::default(), DEFAULT_SOURCE, 0);
        let mut journal = FakeJournal::default();
        let summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;
        assert_eq!(journal.uploads().len(), 2);
        assert_eq!(summary.custodied, 2);
        assert_eq!(journal.listings_by_day().get("20260701"), Some(&1));
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
        let lock = InstanceLock::acquire(temporary.path()).expect("instance lock");
        let health = HealthWriter::new(temporary.path().to_path_buf(), &lock);
        let (activity, _activity_receiver) = watch::channel(SyncActivity::Idle);
        let (stop, shutdown) = watch::channel(false);
        let mut scheduler =
            scheduler(&temporary, SyncWake::default()).with_observability(activity, health);
        let mut journal = FakeJournal::default();
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
        let files = inventory_files(
            vec![segment_path(&temporary, "20260701", "120000_300").join(FILE)],
            None,
        )
        .await
        .expect("inventory fixture");
        let descriptors = files
            .iter()
            .map(|f| ParsedDescriptor {
                submitted: f.name.clone(),
                written: f.name.clone(),
                sha256: f.sha256.clone(),
                size: f.size,
                disposition: "written".to_owned(),
            })
            .collect();
        journal.upload_outcome(
            "120000_300",
            Ok(UploadResult {
                status: UploadStatus::Collision,
                authoritative_key: Some("120000_301".to_owned()),
                descriptors: Some(Ok(descriptors)),
            }),
        );
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
fn ack_invalidation_forces_reupload_before_custody() {
    run(async {
        let temporary = TestDirectory::new("sync-single-remote-loss");
        for segment in ["120000_300", "120100_300", "120200_300"] {
            create_segment(&temporary, "20260701", segment, b"fixture\n");
        }
        let mut scheduler = scheduler(&temporary, SyncWake::default());
        let mut journal = FakeJournal::default();
        let first = scheduler.run_sweep(&mut journal, no_shutdown()).await;
        assert_eq!(first.custodied, 3);
        journal.clear_calls();
        std::fs::write(
            segment_path(&temporary, "20260701", "120100_300").join(FILE),
            b"modified\n",
        )
        .expect("modify file");
        let summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;
        assert_eq!(journal.uploads(), vec!["120100_300".to_owned()]);
        assert_eq!(summary.custodied, 1);
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
            Err(SyncOperationError::RetainCandidate {
                diagnostic: DiagnosticCode::LocalSegmentInvalid,
                answer: "local:local_segment_invalid".to_owned(),
            }),
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
            ensure_private_directory(temporary.path()).expect("prepare data root");
            create_segment(&temporary, "20260701", "120000_300", b"fixture\n");
            let lock = InstanceLock::acquire(temporary.path()).expect("instance lock");
            let health = HealthWriter::new(temporary.path().to_path_buf(), &lock);
            let (activity, _activity_receiver) = watch::channel(SyncActivity::Idle);
            let (stop, shutdown) = watch::channel(false);
            let mut scheduler =
                scheduler(&temporary, SyncWake::default()).with_observability(activity, health);
            let mut journal = FakeJournal::default();
            journal.upload_outcome(
                "120000_300",
                Ok(UploadResult {
                    status,
                    authoritative_key: None,
                    descriptors: None,
                }),
            );
            let segment = segment_path(&temporary, "20260701", "120000_300");
            let before = snapshot_segment_bytes(&segment);
            let task = tokio::spawn(async move {
                let mut journal = journal;
                scheduler.run_with_shutdown(&mut journal, shutdown).await;
            });
            let snapshot = wait_for_idle_snapshot(temporary.path()).await;
            stop.send_replace(true);
            task.await.expect("join scheduler");

            assert_eq!(snapshot["last_error_code"], serde_json::Value::Null);
            assert_eq!(
                snapshot["last_successful_sync_unix_seconds"],
                serde_json::Value::Null
            );
            assert!(snapshot["pending_segments"].as_u64().unwrap_or(0) >= 1);
            assert_segment_bytes_unchanged(&segment, &before);
        }
    });
}

#[test]
fn invalid_upload_receipt_records_no_diagnostic_and_keeps_the_segment() {
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
                descriptors: None,
            }),
        );
        let segment = segment_path(&temporary, "20260701", "120000_300");
        let before = snapshot_segment_bytes(&segment);
        let summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;
        assert_eq!(summary.diagnostic, None);
        assert_eq!(summary.custodied, 0);
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
                descriptors: None,
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
fn shutdown_cancels_a_pending_status_probe() {
    run(async {
        let temporary = TestDirectory::new("sync-cancel-status-probe");
        let (entered, started) = oneshot::channel();
        let (mut journal, uploads) = blocking_journal(BlockingStage::Status, entered);
        let (stop, shutdown) = watch::channel(false);
        let mut scheduler = scheduler(&temporary, SyncWake::default());
        let task = tokio::spawn(async move { scheduler.run_sweep(&mut journal, shutdown).await });
        started.await.expect("status probe began");
        stop.send_replace(true);

        let summary = tokio::time::timeout(HANG_GUARD, task)
            .await
            .expect("shutdown must interrupt status probe")
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
fn shutdown_cancels_a_pending_retention_listing_without_starting_later_candidates() {
    run(async {
        let temporary = TestDirectory::new("sync-cancel-retention-listing");
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
        let (mut journal, _uploads) = blocking_journal(BlockingStage::RetentionListing, entered);
        let (stop, shutdown) = watch::channel(false);
        let mut scheduler =
            scheduler_with_source(&temporary, SyncWake::default(), DEFAULT_SOURCE, 0)
                .with_activity(activity);
        let task = tokio::spawn(async move { scheduler.run_sweep(&mut journal, shutdown).await });
        started.await.expect("retention listing began");
        stop.send_replace(true);

        let summary = tokio::time::timeout(HANG_GUARD, task)
            .await
            .expect("shutdown must interrupt retention listing")
            .expect("join sweep");
        assert!(summary.cancelled);
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
        let mut scheduler = scheduler(&temporary, wake.clone());
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
                Ok(()),
                Err(SyncOperationError::EndSweep(
                    solstone_tmux::sync::SyncFailureClass::Direct,
                )),
                Ok(()),
            ]),
        };
        let (stop, shutdown) = watch::channel(false);
        let mut scheduler = scheduler(&temporary, wake.clone());
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
fn valid_receipt_counts_as_custody() {
    run(async {
        let temporary = TestDirectory::new("sync-reused-custody");
        ensure_private_directory(temporary.path()).expect("prepare data root");
        create_segment(&temporary, "20260701", "120000_300", b"fixture\n");
        let lock = InstanceLock::acquire(temporary.path()).expect("instance lock");
        let health = HealthWriter::new(temporary.path().to_path_buf(), &lock);
        let (activity, _activity_receiver) = watch::channel(SyncActivity::Idle);
        let (stop, shutdown) = watch::channel(false);
        let mut scheduler =
            scheduler(&temporary, SyncWake::default()).with_observability(activity, health);
        let mut journal = FakeJournal::default();
        let task = tokio::spawn(async move {
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
        let mut scheduler =
            scheduler_with_source(&temporary, SyncWake::default(), DEFAULT_SOURCE, 0)
                .with_observability(activity, health);
        let mut journal = FakeJournal::default();
        journal.upload_outcome(
            "120000_300",
            Err(SyncOperationError::RetainCandidate {
                diagnostic: DiagnosticCode::LocalSegmentInvalid,
                answer: "local:local_segment_invalid".to_owned(),
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
        let mut scheduler = scheduler_with_source(&deleted, SyncWake::default(), DEFAULT_SOURCE, 0)
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
        let mut scheduler =
            scheduler_with_source(&retained, SyncWake::default(), DEFAULT_SOURCE, 0)
                .with_observability(activity, health);
        let task = tokio::spawn(async move {
            let mut journal = FakeJournal::default();
            journal.upload_outcome(
                "120000_300",
                Ok(UploadResult {
                    status: UploadStatus::Conflict,
                    authoritative_key: None,
                    descriptors: None,
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
                descriptors: None,
            }),
        );
        scheduler.run_sweep(&mut journal, no_shutdown()).await;
        assert!(journal.uploads().contains(&"120000_300".to_owned()));
    });
}

#[test]
fn empty_candidate_status_probe_is_liveness_contact() {
    run(async {
        let temporary = TestDirectory::new("sync-empty-source");
        let mut scheduler = scheduler(&temporary, SyncWake::default());
        let mut journal = FakeJournal::default();
        let summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;
        assert_eq!(summary.attempted, 0);
        assert!(summary.contacted);
        assert_eq!(journal.calls, vec![Call::SystemStatus]);
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
        let mut journal = FakeJournal {
            evidence_for: Some("other-source".to_owned()),
            ..FakeJournal::default()
        };
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
        let mut journal = FakeJournal::default();
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
    let deadline = std::time::Instant::now() + HANG_GUARD;
    loop {
        if listings.try_recv().is_ok() {
            for _ in 0..SCHEDULER_TURNS {
                tokio::task::yield_now().await;
            }
            return;
        }
        if std::time::Instant::now() >= deadline {
            panic!("scheduler did not make the expected listing contact: {context}");
        }
        tokio::task::yield_now().await;
        // yield_now does not wait for spawn_blocking candidate scans.
        std::thread::sleep(Duration::from_millis(1));
    }
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

#[test]
fn receipt_sha256_mismatch_is_not_an_ack() {
    run_receipt_matrix_test(
        "receipt-sha256-mismatch",
        |_sha256, size| {
            let bad_sha = "0".repeat(64);
            let json = format!(
                r#"{{"status":"ok","segment":"120000_300","file_descriptors":[{{"submitted":"{FILE}","written":"{FILE}","size":{size},"sha256":"{bad_sha}","disposition":"written"}}]}}"#
            );
            decode_upload_response(json.as_bytes()).map_err(|_| unreachable!())
        },
        Duration::from_secs(3600),
    );
}

#[test]
fn receipt_size_mismatch_is_not_an_ack() {
    run_receipt_matrix_test(
        "receipt-size-mismatch",
        |sha256, size| {
            let json = format!(
                r#"{{"status":"ok","segment":"120000_300","file_descriptors":[{{"submitted":"{FILE}","written":"{FILE}","size":{},"sha256":"{sha256}","disposition":"written"}}]}}"#,
                size + 1
            );
            decode_upload_response(json.as_bytes()).map_err(|_| unreachable!())
        },
        Duration::from_secs(3600),
    );
}

#[test]
fn receipt_missing_descriptor_is_not_an_ack() {
    run_receipt_matrix_test(
        "receipt-missing-descriptor",
        |_sha256, _size| {
            let json = r#"{"status":"ok","segment":"120000_300","file_descriptors":[]}"#;
            decode_upload_response(json.as_bytes()).map_err(|_| unreachable!())
        },
        Duration::from_secs(3600),
    );
}

#[test]
fn receipt_extra_descriptor_is_not_an_ack() {
    run_receipt_matrix_test(
        "receipt-extra-descriptor",
        |sha256, size| {
            let json = format!(
                r#"{{"status":"ok","segment":"120000_300","file_descriptors":[{{"submitted":"{FILE}","written":"{FILE}","size":{size},"sha256":"{sha256}","disposition":"written"}},{{"submitted":"extra.jsonl","written":"extra.jsonl","size":10,"sha256":"{sha256}","disposition":"written"}}]}}"#
            );
            decode_upload_response(json.as_bytes()).map_err(|_| unreachable!())
        },
        Duration::from_secs(3600),
    );
}

#[test]
fn receipt_duplicate_submitted_is_not_an_ack() {
    run_receipt_matrix_test(
        "receipt-duplicate-submitted",
        |sha256, size| {
            let json = format!(
                r#"{{"status":"ok","segment":"120000_300","file_descriptors":[{{"submitted":"{FILE}","written":"{FILE}","size":{size},"sha256":"{sha256}","disposition":"written"}},{{"submitted":"{FILE}","written":"{FILE}","size":{size},"sha256":"{sha256}","disposition":"written"}}]}}"#
            );
            decode_upload_response(json.as_bytes()).map_err(|_| unreachable!())
        },
        Duration::from_secs(3600),
    );
}

#[test]
fn receipt_without_descriptors_is_not_an_ack() {
    run_receipt_matrix_test(
        "receipt-without-descriptors",
        |_sha256, _size| {
            let json = r#"{"status":"ok","segment":"120000_300"}"#;
            decode_upload_response(json.as_bytes()).map_err(|_| unreachable!())
        },
        Duration::from_secs(3600),
    );
}

#[test]
fn receipt_unknown_disposition_is_not_an_ack() {
    run_receipt_matrix_test(
        "receipt-unknown-disposition",
        |sha256, size| {
            let json = format!(
                r#"{{"status":"ok","segment":"120000_300","file_descriptors":[{{"submitted":"{FILE}","written":"{FILE}","size":{size},"sha256":"{sha256}","disposition":"unknown_value"}}]}}"#
            );
            decode_upload_response(json.as_bytes()).map_err(|_| unreachable!())
        },
        Duration::from_secs(3600),
    );
}

#[test]
fn receipt_missing_disposition_is_not_an_ack() {
    run_receipt_matrix_test(
        "receipt-missing-disposition",
        |sha256, size| {
            let json = format!(
                r#"{{"status":"ok","segment":"120000_300","file_descriptors":[{{"submitted":"{FILE}","written":"{FILE}","size":{size},"sha256":"{sha256}"}}]}}"#
            );
            decode_upload_response(json.as_bytes()).map_err(|_| unreachable!())
        },
        Duration::from_secs(3600),
    );
}

#[test]
fn receipt_uppercase_sha256_is_not_an_ack() {
    run_receipt_matrix_test(
        "receipt-uppercase-sha256",
        |sha256, size| {
            let upper_sha = sha256.to_ascii_uppercase();
            let json = format!(
                r#"{{"status":"ok","segment":"120000_300","file_descriptors":[{{"submitted":"{FILE}","written":"{FILE}","size":{size},"sha256":"{upper_sha}","disposition":"written"}}]}}"#
            );
            decode_upload_response(json.as_bytes()).map_err(|_| unreachable!())
        },
        Duration::from_secs(3600),
    );
}

#[test]
fn receipt_sha256_not_a_string_is_not_an_ack() {
    run_receipt_matrix_test(
        "receipt-sha256-not-a-string",
        |_sha256, size| {
            let json = format!(
                r#"{{"status":"ok","segment":"120000_300","file_descriptors":[{{"submitted":"{FILE}","written":"{FILE}","size":{size},"sha256":12345,"disposition":"written"}}]}}"#
            );
            decode_upload_response(json.as_bytes()).map_err(|_| unreachable!())
        },
        Duration::from_secs(3600),
    );
}

#[test]
fn receipt_received_not_written_waits_a_day() {
    run_receipt_matrix_test(
        "receipt-received-not-written",
        |sha256, size| {
            let json = format!(
                r#"{{"status":"ok","segment":"120000_300","file_descriptors":[{{"submitted":"{FILE}","written":"{FILE}","size":{size},"sha256":"{sha256}","disposition":"received_not_written"}}]}}"#
            );
            decode_upload_response(json.as_bytes()).map_err(|_| unreachable!())
        },
        Duration::from_secs(86400),
    );
}

#[test]
fn valid_ok_receipt_acks() {
    run(async {
        let temporary = TestDirectory::new("valid-ok-receipt");
        create_segment(&temporary, "20260701", "120000_300", b"fixture\n");
        let mut scheduler =
            scheduler_with_source(&temporary, SyncWake::default(), DEFAULT_SOURCE, 0);
        let mut journal = FakeJournal::default();
        let summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;
        assert_eq!(summary.custodied, 1);
        assert!(!segment_path(&temporary, "20260701", "120000_300").exists());
    });
}

#[test]
fn valid_duplicate_receipt_stores_existing_segment() {
    run(async {
        let temporary = TestDirectory::new("valid-dup-receipt");
        create_segment(&temporary, "20260701", "120000_300", b"fixture\n");
        let files = inventory_files(
            vec![segment_path(&temporary, "20260701", "120000_300").join(FILE)],
            None,
        )
        .await
        .expect("inventory");
        let descriptors = files
            .iter()
            .map(|f| ParsedDescriptor {
                submitted: f.name.clone(),
                written: f.name.clone(),
                sha256: f.sha256.clone(),
                size: f.size,
                disposition: "already_held".to_owned(),
            })
            .collect();
        let mut journal = FakeJournal::default();
        journal.upload_outcome(
            "120000_300",
            Ok(UploadResult {
                status: UploadStatus::Duplicate,
                authoritative_key: Some("120000_299".to_owned()),
                descriptors: Some(Ok(descriptors)),
            }),
        );
        journal
            .remote
            .entry("20260701".to_owned())
            .or_default()
            .insert("120000_299".to_owned(), files);
        let mut scheduler =
            scheduler_with_source(&temporary, SyncWake::default(), DEFAULT_SOURCE, 0);
        let summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;
        assert_eq!(summary.custodied, 1);
        assert!(!segment_path(&temporary, "20260701", "120000_300").exists());
    });
}

#[test]
fn valid_collision_receipt_stores_segment_key() {
    run(async {
        let temporary = TestDirectory::new("valid-collision-receipt");
        create_segment(&temporary, "20260701", "120000_300", b"fixture\n");
        let files = inventory_files(
            vec![segment_path(&temporary, "20260701", "120000_300").join(FILE)],
            None,
        )
        .await
        .expect("inventory");
        let descriptors = files
            .iter()
            .map(|f| ParsedDescriptor {
                submitted: f.name.clone(),
                written: f.name.clone(),
                sha256: f.sha256.clone(),
                size: f.size,
                disposition: "written".to_owned(),
            })
            .collect();
        let mut journal = FakeJournal::default();
        journal.upload_outcome(
            "120000_300",
            Ok(UploadResult {
                status: UploadStatus::Collision,
                authoritative_key: Some("120000_301".to_owned()),
                descriptors: Some(Ok(descriptors)),
            }),
        );
        journal
            .remote
            .entry("20260701".to_owned())
            .or_default()
            .insert("120000_301".to_owned(), files);
        let mut scheduler =
            scheduler_with_source(&temporary, SyncWake::default(), DEFAULT_SOURCE, 0);
        let summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;
        assert_eq!(summary.custodied, 1);
        assert!(!segment_path(&temporary, "20260701", "120000_300").exists());
    });
}

#[test]
fn valid_receipt_allows_written_name_to_differ() {
    run(async {
        let temporary = TestDirectory::new("valid-written-diff");
        create_segment(&temporary, "20260701", "120000_300", b"fixture\n");
        let files = inventory_files(
            vec![segment_path(&temporary, "20260701", "120000_300").join(FILE)],
            None,
        )
        .await
        .expect("inventory");
        let descriptors = files
            .iter()
            .map(|f| ParsedDescriptor {
                submitted: f.name.clone(),
                written: "remapped_name.jsonl".to_owned(),
                sha256: f.sha256.clone(),
                size: f.size,
                disposition: "written".to_owned(),
            })
            .collect();
        let mut journal = FakeJournal::default();
        journal.upload_outcome(
            "120000_300",
            Ok(UploadResult {
                status: UploadStatus::Ok,
                authoritative_key: Some("120000_300".to_owned()),
                descriptors: Some(Ok(descriptors)),
            }),
        );
        journal.list_outcome(
            "20260701",
            Ok(SegmentsEnvelope {
                items: vec![SegmentItem {
                    key: "120000_300".to_owned(),
                    observed: false,
                    files: vec![SegmentFile {
                        name: "remapped_name.jsonl".to_owned(),
                        size: files[0].size,
                        sha256: files[0].sha256.clone(),
                        status: ListingFileStatus::Present,
                        submitted_name: Some(FILE.to_owned()),
                    }],
                    original_key: None,
                }],
                total: 1,
                protocol_version: PROTOCOL_VERSION_NUMBER,
            }),
        );
        let mut scheduler =
            scheduler_with_source(&temporary, SyncWake::default(), DEFAULT_SOURCE, 0);
        let summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;
        assert_eq!(summary.custodied, 1);
        assert!(!segment_path(&temporary, "20260701", "120000_300").exists());
    });
}

#[test]
fn idle_acked_tree_probes_status_once_per_sweep() {
    run(async {
        let temporary = TestDirectory::new("idle-acked-status");
        create_segment(&temporary, "20260701", "120000_300", b"fixture\n");
        let mut scheduler = scheduler(&temporary, SyncWake::default());
        let mut journal = FakeJournal::default();
        let first = scheduler.run_sweep(&mut journal, no_shutdown()).await;
        assert_eq!(first.custodied, 1);
        journal.clear_calls();

        let summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;
        assert_eq!(summary.attempted, 0);
        assert_eq!(journal.calls, vec![Call::SystemStatus]);
    });
}

#[test]
fn idle_status_revoked_publishes_revoked() {
    run(async {
        let temporary = TestDirectory::new("idle-status-revoked");
        ensure_private_directory(temporary.path()).expect("prepare data root");
        let lock = InstanceLock::acquire(temporary.path()).expect("instance lock");
        let health = HealthWriter::new(temporary.path().to_path_buf(), &lock);
        let (activity, _rx) = watch::channel(SyncActivity::Idle);
        let (stop, shutdown) = watch::channel(false);
        let mut scheduler =
            scheduler(&temporary, SyncWake::default()).with_observability(activity, health);
        let mut journal = FakeJournal::default();
        journal
            .status_outcomes
            .push_back(Err(SyncOperationError::EndSweep(SyncFailureClass::Auth)));
        let task = tokio::spawn(async move {
            let mut journal = journal;
            scheduler.run_with_shutdown(&mut journal, shutdown).await;
        });
        let snapshot = wait_for_idle_snapshot(temporary.path()).await;
        stop.send_replace(true);
        task.await.expect("join scheduler");
        assert_eq!(
            snapshot["last_error_code"],
            DiagnosticCode::JournalRevoked.as_str()
        );
    });
}

#[test]
fn idle_status_failure_does_not_advance_contact() {
    run(async {
        let temporary = TestDirectory::new("idle-status-failure");
        ensure_private_directory(temporary.path()).expect("prepare data root");
        let lock = InstanceLock::acquire(temporary.path()).expect("instance lock");
        let health = HealthWriter::new(temporary.path().to_path_buf(), &lock);
        let (activity, _rx) = watch::channel(SyncActivity::Idle);
        let (stop, shutdown) = watch::channel(false);
        let mut scheduler =
            scheduler(&temporary, SyncWake::default()).with_observability(activity, health);
        let mut journal = FakeJournal::default();
        journal
            .status_outcomes
            .push_back(Err(SyncOperationError::EndSweep(SyncFailureClass::Timeout)));
        let task = tokio::spawn(async move {
            let mut journal = journal;
            scheduler.run_with_shutdown(&mut journal, shutdown).await;
        });
        let snapshot = wait_for_idle_snapshot(temporary.path()).await;
        stop.send_replace(true);
        task.await.expect("join scheduler");
        assert_eq!(
            snapshot["last_successful_sync_unix_seconds"],
            serde_json::Value::Null
        );
    });
}

#[test]
fn unacked_segment_uploads_before_any_segments_read() {
    run(async {
        let temporary = TestDirectory::new("unacked-upload-first");
        create_segment(&temporary, "20260701", "120000_300", b"fixture\n");
        let mut scheduler =
            scheduler_with_source(&temporary, SyncWake::default(), DEFAULT_SOURCE, 0);
        let mut journal = FakeJournal::default();
        scheduler.run_sweep(&mut journal, no_shutdown()).await;
        assert_eq!(journal.calls[0], Call::Upload("120000_300".to_owned()));
        assert_eq!(journal.calls[1], Call::Listing("20260701".to_owned()));
    });
}

#[test]
fn retention_proves_two_of_three_and_rechecks_the_third() {
    run(async {
        let temporary = TestDirectory::new("retention-two-of-three");
        for seg in ["120000_300", "120100_300", "120200_300"] {
            create_segment(&temporary, "20260701", seg, b"fixture\n");
        }
        let mut scheduler1 =
            scheduler_with_source(&temporary, SyncWake::default(), DEFAULT_SOURCE, -1);
        let mut journal = FakeJournal::default();
        scheduler1.run_sweep(&mut journal, no_shutdown()).await;
        journal
            .remote
            .get_mut("20260701")
            .unwrap()
            .remove("120200_300");
        journal.clear_calls();

        let mut scheduler2 =
            scheduler_with_source(&temporary, SyncWake::default(), DEFAULT_SOURCE, 0);
        let summary = scheduler2.run_sweep(&mut journal, no_shutdown()).await;
        assert_eq!(summary.attempted, 0);
        assert!(!segment_path(&temporary, "20260701", "120000_300").exists());
        assert!(!segment_path(&temporary, "20260701", "120100_300").exists());
        assert!(segment_path(&temporary, "20260701", "120200_300").exists());
    });
}

#[test]
fn retention_next_day_read_deletes_the_unproven_segment() {
    paused(async {
        let temporary = TestDirectory::new("retention-next-day");
        create_segment(&temporary, "20260701", "120000_300", b"fixture\n");
        let test_clock = clock();
        let mut scheduler1 =
            scheduler_with_clock(&temporary, SyncWake::default(), Arc::clone(&test_clock), 0);
        let mut journal = FakeJournal::default();
        journal.list_outcome("20260701", Ok(empty_listing()));
        let s1 = scheduler1.run_sweep(&mut journal, no_shutdown()).await;
        assert_eq!(s1.attempted, 1);
        assert!(segment_path(&temporary, "20260701", "120000_300").exists());

        journal.clear_calls();
        let files = inventory_files(
            vec![segment_path(&temporary, "20260701", "120000_300").join(FILE)],
            None,
        )
        .await
        .unwrap();
        journal
            .remote
            .entry("20260701".to_owned())
            .or_default()
            .insert("120000_300".to_owned(), files);

        advance_both(&test_clock, Duration::from_secs(86401)).await;
        let mut scheduler2 =
            scheduler_with_clock(&temporary, SyncWake::default(), Arc::clone(&test_clock), 0);
        let s2 = scheduler2.run_sweep(&mut journal, no_shutdown()).await;
        assert_eq!(s2.attempted, 0);
        assert!(!segment_path(&temporary, "20260701", "120000_300").exists());
    });
}

#[test]
fn changed_bytes_after_ack_upload_instead_of_delete() {
    run(async {
        let temporary = TestDirectory::new("retention-changed-bytes");
        create_segment(&temporary, "20260701", "120000_300", b"first\n");
        let mut scheduler =
            scheduler_with_source(&temporary, SyncWake::default(), DEFAULT_SOURCE, -1);
        let mut journal = FakeJournal::default();
        scheduler.run_sweep(&mut journal, no_shutdown()).await;
        assert_eq!(journal.uploads(), vec!["120000_300".to_owned()]);

        std::fs::write(
            segment_path(&temporary, "20260701", "120000_300").join(FILE),
            b"modified\n",
        )
        .unwrap();
        journal.clear_calls();

        let mut scheduler_ret =
            scheduler_with_source(&temporary, SyncWake::default(), DEFAULT_SOURCE, 0);
        let summary = scheduler_ret.run_sweep(&mut journal, no_shutdown()).await;
        assert_eq!(summary.attempted, 1);
        assert_eq!(journal.uploads(), vec!["120000_300".to_owned()]);
    });
}

#[test]
fn retention_listing_failure_still_uploads_a_new_segment() {
    run(async {
        let temporary = TestDirectory::new("retention-listing-failure");
        create_segment(&temporary, "20260701", "120000_300", b"fixture 1\n");
        create_segment(&temporary, "20260702", "120000_300", b"fixture 2\n");
        let mut scheduler =
            scheduler_with_source(&temporary, SyncWake::default(), DEFAULT_SOURCE, 0);
        let mut journal = FakeJournal::default();
        journal.list_outcome(
            "20260701",
            Err(SyncOperationError::EndSweep(SyncFailureClass::Timeout)),
        );
        let summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;
        assert_eq!(summary.attempted, 2);
        assert_eq!(journal.uploads().len(), 2);
    });
}

#[test]
fn retention_proves_the_acks_stored_key() {
    run(async {
        let temporary = TestDirectory::new("retention-proves-stored-key");
        create_segment(&temporary, "20260701", "120000_300", b"fixture\n");
        let files = inventory_files(
            vec![segment_path(&temporary, "20260701", "120000_300").join(FILE)],
            None,
        )
        .await
        .expect("inventory");
        let descriptors = files
            .iter()
            .map(|f| ParsedDescriptor {
                submitted: f.name.clone(),
                written: f.name.clone(),
                sha256: f.sha256.clone(),
                size: f.size,
                disposition: "written".to_owned(),
            })
            .collect();
        let mut journal = FakeJournal::default();
        journal.upload_outcome(
            "120000_300",
            Ok(UploadResult {
                status: UploadStatus::Collision,
                authoritative_key: Some("120000_301".to_owned()),
                descriptors: Some(Ok(descriptors)),
            }),
        );
        journal
            .remote
            .entry("20260701".to_owned())
            .or_default()
            .insert("120000_301".to_owned(), files);
        let mut scheduler =
            scheduler_with_source(&temporary, SyncWake::default(), DEFAULT_SOURCE, 0);
        let summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;
        assert_eq!(summary.custodied, 1);
        assert!(!segment_path(&temporary, "20260701", "120000_300").exists());
    });
}

#[test]
fn retention_listing_at_the_local_key_does_not_delete() {
    run(async {
        let temporary = TestDirectory::new("retention-local-key-no-del");
        create_segment(&temporary, "20260701", "120000_300", b"fixture\n");
        let files = inventory_files(
            vec![segment_path(&temporary, "20260701", "120000_300").join(FILE)],
            None,
        )
        .await
        .expect("inventory");
        let descriptors = files
            .iter()
            .map(|f| ParsedDescriptor {
                submitted: f.name.clone(),
                written: f.name.clone(),
                sha256: f.sha256.clone(),
                size: f.size,
                disposition: "written".to_owned(),
            })
            .collect();
        let mut journal = FakeJournal::default();
        journal.upload_outcome(
            "120000_300",
            Ok(UploadResult {
                status: UploadStatus::Collision,
                authoritative_key: Some("120000_301".to_owned()),
                descriptors: Some(Ok(descriptors)),
            }),
        );
        journal
            .remote
            .entry("20260701".to_owned())
            .or_default()
            .insert("120000_300".to_owned(), files);
        let mut scheduler =
            scheduler_with_source(&temporary, SyncWake::default(), DEFAULT_SOURCE, 0);
        let _ = scheduler.run_sweep(&mut journal, no_shutdown()).await;
        assert!(segment_path(&temporary, "20260701", "120000_300").exists());
    });
}

#[test]
fn future_due_times_clamp_to_one_interval() {
    paused(async {
        let temporary = TestDirectory::new("bounds-clamp-interval");
        create_segment(&temporary, "20260701", "120000_300", b"fixture\n");
        let test_clock = clock();
        let mut scheduler1 =
            scheduler_with_clock(&temporary, SyncWake::default(), Arc::clone(&test_clock), -1);
        let mut journal = FakeJournal::default();
        journal.upload_outcome(
            "120000_300",
            Ok(UploadResult {
                status: UploadStatus::Failed,
                authoritative_key: None,
                descriptors: None,
            }),
        );
        scheduler1.run_sweep(&mut journal, no_shutdown()).await;

        let state_path = temporary
            .path()
            .join("sync-ledger")
            .join("20260701")
            .join(STREAM)
            .join("120000_300")
            .join("state.json");
        let content = std::fs::read_to_string(&state_path).unwrap();
        let mut val: serde_json::Value = serde_json::from_str(&content).unwrap();
        val["next_attempt_unix"] =
            serde_json::json!(test_clock.wall_now().unix_timestamp() + 100 * 86400);
        val["next_attempt_interval_seconds"] = serde_json::json!(3600);
        std::fs::write(&state_path, serde_json::to_string(&val).unwrap()).unwrap();

        let mut scheduler2 =
            scheduler_with_clock(&temporary, SyncWake::default(), Arc::clone(&test_clock), -1);
        journal.clear_calls();
        let summary = scheduler2.run_sweep(&mut journal, no_shutdown()).await;
        assert_eq!(summary.attempted, 0);
        assert!(journal.uploads().is_empty());
    });
}

#[test]
fn fresh_scheduler_honors_persisted_bounds() {
    paused(async {
        let temporary = TestDirectory::new("bounds-persisted");
        create_segment(&temporary, "20260701", "120000_300", b"fixture\n");
        let test_clock = clock();
        let mut scheduler1 =
            scheduler_with_clock(&temporary, SyncWake::default(), Arc::clone(&test_clock), -1);
        let mut journal = FakeJournal::default();
        journal.upload_outcome(
            "120000_300",
            Ok(UploadResult {
                status: UploadStatus::Failed,
                authoritative_key: None,
                descriptors: None,
            }),
        );
        scheduler1.run_sweep(&mut journal, no_shutdown()).await;

        let mut scheduler2 =
            scheduler_with_clock(&temporary, SyncWake::default(), Arc::clone(&test_clock), -1);
        journal.clear_calls();
        let summary = scheduler2.run_sweep(&mut journal, no_shutdown()).await;
        assert_eq!(summary.attempted, 0);
        assert!(journal.uploads().is_empty());
    });
}

#[test]
fn deferred_and_terminal_keep_publish_one_status_probe() {
    paused(async {
        let temporary = TestDirectory::new("bounds-status-probe");
        create_segment(&temporary, "20260701", "120000_300", b"fixture\n");
        let test_clock = clock();
        let mut scheduler =
            scheduler_with_clock(&temporary, SyncWake::default(), Arc::clone(&test_clock), -1);
        let mut journal = FakeJournal::default();
        journal.upload_outcome(
            "120000_300",
            Ok(UploadResult {
                status: UploadStatus::Failed,
                authoritative_key: None,
                descriptors: None,
            }),
        );
        scheduler.run_sweep(&mut journal, no_shutdown()).await;
        journal.clear_calls();

        let summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;
        assert_eq!(summary.attempted, 0);
        assert_eq!(journal.calls, vec![Call::SystemStatus]);
    });
}

#[test]
fn three_identical_receipts_wait_a_day_and_a_new_answer_restores_the_hour() {
    paused(async {
        let temporary = TestDirectory::new("bounds-three-identical");
        let bytes = b"fixture\n";
        create_segment(&temporary, "20260701", "120000_300", bytes);
        let test_clock = clock();
        let mut scheduler =
            scheduler_with_clock(&temporary, SyncWake::default(), Arc::clone(&test_clock), -1);
        let mut journal = FakeJournal::default();

        let bad_json = r#"{"status":"ok","segment":"120000_300","file_descriptors":[]}"#;
        let bad_res = decode_upload_response(bad_json.as_bytes()).unwrap();

        journal.upload_outcome("120000_300", Ok(bad_res.clone()));
        scheduler.run_sweep(&mut journal, no_shutdown()).await;
        journal.clear_calls();

        advance_both(&test_clock, Duration::from_secs(3601)).await;
        journal.upload_outcome("120000_300", Ok(bad_res.clone()));
        scheduler.run_sweep(&mut journal, no_shutdown()).await;
        journal.clear_calls();

        advance_both(&test_clock, Duration::from_secs(3601)).await;
        journal.upload_outcome("120000_300", Ok(bad_res.clone()));
        scheduler.run_sweep(&mut journal, no_shutdown()).await;
        journal.clear_calls();

        advance_both(&test_clock, Duration::from_secs(3601)).await;
        let s_early = scheduler.run_sweep(&mut journal, no_shutdown()).await;
        assert_eq!(s_early.attempted, 0);

        advance_both(&test_clock, Duration::from_secs(86400)).await;
        journal.upload_outcome(
            "120000_300",
            Ok(UploadResult {
                status: UploadStatus::Conflict,
                authoritative_key: None,
                descriptors: None,
            }),
        );
        let s_diff = scheduler.run_sweep(&mut journal, no_shutdown()).await;
        assert_eq!(s_diff.attempted, 1);
        journal.clear_calls();

        advance_both(&test_clock, Duration::from_secs(3601)).await;
        journal.upload_outcome("120000_300", Ok(bad_res));
        let s_restored = scheduler.run_sweep(&mut journal, no_shutdown()).await;
        assert_eq!(s_restored.attempted, 1);
    });
}

#[test]
fn repaired_credential_reacks_under_the_new_generation() {
    run(async {
        let temporary = TestDirectory::new("repaired-credential");
        create_segment(&temporary, "20260701", "120000_300", b"fixture\n");
        let id1 = test_journal_identity();
        let test_clock = clock();
        let mut scheduler1 = scheduler_with_clock_and_identity(
            &temporary,
            SyncWake::default(),
            Arc::clone(&test_clock),
            id1,
            -1,
        );
        let mut journal = FakeJournal::default();
        let s1 = scheduler1.run_sweep(&mut journal, no_shutdown()).await;
        assert_eq!(s1.custodied, 1);
        journal.clear_calls();

        let mut id2 = test_journal_identity();
        id2.pairing_generation_hex = "pairgen2".to_owned();
        let mut scheduler2 = scheduler_with_clock_and_identity(
            &temporary,
            SyncWake::default(),
            Arc::clone(&test_clock),
            id2,
            -1,
        );
        let s2 = scheduler2.run_sweep(&mut journal, no_shutdown()).await;
        assert_eq!(s2.attempted, 1);
        assert_eq!(journal.uploads(), vec!["120000_300".to_owned()]);
    });
}

#[test]
fn ack_write_failure_before_rename_reuploads_on_the_next_scheduler() {
    run(async {
        let temporary = TestDirectory::new("ack-write-fail-reupload");
        create_segment(&temporary, "20260701", "120000_300", b"fixture\n");
        let mut scheduler1 = scheduler(&temporary, SyncWake::default());
        let mut journal = FakeJournal::default();

        set_atomic_write_fault_for_prefix(
            &temporary.path().join("sync-ledger"),
            AtomicWriteFault::FailBeforeRename,
            1,
        );
        let summary = scheduler1.run_sweep(&mut journal, no_shutdown()).await;
        set_atomic_write_fault(None);
        assert_eq!(summary.custodied, 0);

        let mut scheduler2 = scheduler(&temporary, SyncWake::default());
        journal.clear_calls();
        let summary2 = scheduler2.run_sweep(&mut journal, no_shutdown()).await;
        assert_eq!(summary2.attempted, 1);
        assert_eq!(journal.uploads(), vec!["120000_300".to_owned()]);
    });
}

#[test]
fn three_ack_write_failures_record_private_state_io() {
    run(async {
        let temporary = TestDirectory::new("ack-three-failures");
        create_segment(&temporary, "20260701", "120000_300", b"fixture 1\n");
        create_segment(&temporary, "20260701", "120100_300", b"fixture 2\n");
        create_segment(&temporary, "20260701", "120200_300", b"fixture 3\n");
        let mut scheduler = scheduler(&temporary, SyncWake::default());
        let mut journal = FakeJournal::default();

        set_atomic_write_fault_for_prefix(
            &temporary.path().join("sync-ledger"),
            AtomicWriteFault::FailBeforeRename,
            3,
        );
        let summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;
        set_atomic_write_fault(None);
        assert_eq!(summary.diagnostic, Some(DiagnosticCode::PrivateStateIo));
    });
}

#[test]
fn truncated_ack_is_not_an_ack() {
    run(async {
        let temporary = TestDirectory::new("ack-truncated");
        create_segment(&temporary, "20260701", "120000_300", b"fixture\n");
        let mut scheduler1 = scheduler(&temporary, SyncWake::default());
        let mut journal = FakeJournal::default();
        scheduler1.run_sweep(&mut journal, no_shutdown()).await;

        let ack_path = temporary
            .path()
            .join("sync-ledger")
            .join("20260701")
            .join(STREAM)
            .join("120000_300")
            .join("ack.json");
        std::fs::write(&ack_path, b"{\"day\":\"20260701").unwrap();

        let mut scheduler2 = scheduler(&temporary, SyncWake::default());
        journal.clear_calls();
        let summary = scheduler2.run_sweep(&mut journal, no_shutdown()).await;
        assert_eq!(summary.attempted, 1);
        assert_eq!(journal.uploads(), vec!["120000_300".to_owned()]);
    });
}

#[test]
fn ack_location_mismatch_is_not_an_ack() {
    run(async {
        let temporary = TestDirectory::new("ack-loc-mismatch");
        create_segment(&temporary, "20260701", "120000_300", b"fixture\n");
        let mut scheduler1 = scheduler(&temporary, SyncWake::default());
        let mut journal = FakeJournal::default();
        scheduler1.run_sweep(&mut journal, no_shutdown()).await;

        let ack_path = temporary
            .path()
            .join("sync-ledger")
            .join("20260701")
            .join(STREAM)
            .join("120000_300")
            .join("ack.json");
        let content = std::fs::read_to_string(&ack_path).unwrap();
        let mut val: serde_json::Value = serde_json::from_str(&content).unwrap();
        val["location"] = serde_json::json!("/wrong/location/ack.json");
        std::fs::write(&ack_path, serde_json::to_string(&val).unwrap()).unwrap();

        let mut scheduler2 = scheduler(&temporary, SyncWake::default());
        journal.clear_calls();
        let summary = scheduler2.run_sweep(&mut journal, no_shutdown()).await;
        assert_eq!(summary.attempted, 1);
        assert_eq!(journal.uploads(), vec!["120000_300".to_owned()]);
    });
}

#[test]
fn ack_for_another_journal_is_not_an_ack() {
    run(async {
        let temporary = TestDirectory::new("ack-diff-journal");
        create_segment(&temporary, "20260701", "120000_300", b"fixture\n");
        let mut scheduler1 = scheduler(&temporary, SyncWake::default());
        let mut journal = FakeJournal::default();
        scheduler1.run_sweep(&mut journal, no_shutdown()).await;

        let ack_path = temporary
            .path()
            .join("sync-ledger")
            .join("20260701")
            .join(STREAM)
            .join("120000_300")
            .join("ack.json");
        let content = std::fs::read_to_string(&ack_path).unwrap();
        let mut val: serde_json::Value = serde_json::from_str(&content).unwrap();
        val["instance_id"] = serde_json::json!("other-instance");
        std::fs::write(&ack_path, serde_json::to_string(&val).unwrap()).unwrap();

        let mut scheduler2 = scheduler(&temporary, SyncWake::default());
        journal.clear_calls();
        let summary = scheduler2.run_sweep(&mut journal, no_shutdown()).await;
        assert_eq!(summary.attempted, 1);
        assert_eq!(journal.uploads(), vec!["120000_300".to_owned()]);
    });
}

#[test]
fn ack_file_set_mismatch_is_not_an_ack() {
    run(async {
        let temporary = TestDirectory::new("ack-fileset-mismatch");
        create_segment(&temporary, "20260701", "120000_300", b"fixture\n");
        let mut scheduler1 = scheduler(&temporary, SyncWake::default());
        let mut journal = FakeJournal::default();
        scheduler1.run_sweep(&mut journal, no_shutdown()).await;

        let ack_path = temporary
            .path()
            .join("sync-ledger")
            .join("20260701")
            .join(STREAM)
            .join("120000_300")
            .join("ack.json");
        let content = std::fs::read_to_string(&ack_path).unwrap();
        let mut val: serde_json::Value = serde_json::from_str(&content).unwrap();
        val["files"] = serde_json::json!([]);
        std::fs::write(&ack_path, serde_json::to_string(&val).unwrap()).unwrap();

        let mut scheduler2 = scheduler(&temporary, SyncWake::default());
        journal.clear_calls();
        let summary = scheduler2.run_sweep(&mut journal, no_shutdown()).await;
        assert_eq!(summary.attempted, 1);
        assert_eq!(journal.uploads(), vec!["120000_300".to_owned()]);
    });
}

fn test_journal_identity() -> JournalIdentity {
    JournalIdentity {
        instance_id: "inst-1".to_owned(),
        ca_fp_prefix_hex: "cafp1".to_owned(),
        pairing_generation_hex: "pairgen1".to_owned(),
    }
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
        temporary.path().to_path_buf(),
        stream(),
        source.to_owned(),
        retention_days,
        clock(),
        wake,
        test_journal_identity(),
    )
}

fn scheduler_with_clock(
    temporary: &TestDirectory,
    wake: SyncWake,
    clock: Arc<TestClock>,
    retention_days: i64,
) -> SyncScheduler {
    scheduler_with_clock_and_identity(
        temporary,
        wake,
        clock,
        test_journal_identity(),
        retention_days,
    )
}

fn scheduler_with_clock_and_identity(
    temporary: &TestDirectory,
    wake: SyncWake,
    clock: Arc<TestClock>,
    identity: JournalIdentity,
    retention_days: i64,
) -> SyncScheduler {
    SyncScheduler::new(
        temporary.path().to_path_buf(),
        stream(),
        DEFAULT_SOURCE.to_owned(),
        retention_days,
        clock,
        wake,
        identity,
    )
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let hash = hasher.finalize();
    let mut hex = String::with_capacity(64);
    for byte in hash {
        use std::fmt::Write;
        let _ = write!(hex, "{:02x}", byte);
    }
    hex
}

fn run_receipt_matrix_test(
    test_name: &str,
    make_response: impl Fn(&str, u64) -> Result<UploadResult, SyncOperationError>,
    expected_wait: Duration,
) {
    paused(async move {
        let temporary = TestDirectory::new(test_name);
        let bytes = b"test payload\n";
        create_segment(&temporary, "20260701", "120000_300", bytes);
        let sha256 = sha256_hex(bytes);
        let size = bytes.len() as u64;

        let test_clock = clock();
        let mut scheduler =
            scheduler_with_clock(&temporary, SyncWake::default(), Arc::clone(&test_clock), -1);
        let mut journal = FakeJournal::default();
        let outcome = make_response(&sha256, size);
        journal.upload_outcome("120000_300", outcome);

        let summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;
        assert_eq!(summary.attempted, 1);
        assert_eq!(journal.uploads(), vec!["120000_300".to_owned()]);
        journal.clear_calls();

        let before_wait = expected_wait - Duration::from_secs(60);
        advance_both(&test_clock, before_wait).await;
        let summary2 = scheduler.run_sweep(&mut journal, no_shutdown()).await;
        assert_eq!(
            summary2.attempted, 0,
            "segment should be deferred before boundary"
        );
        assert!(journal.uploads().is_empty());

        advance_both(&test_clock, Duration::from_secs(120)).await;
        let summary3 = scheduler.run_sweep(&mut journal, no_shutdown()).await;
        assert_eq!(
            summary3.attempted, 1,
            "segment should be retried after boundary"
        );
        assert_eq!(journal.uploads(), vec!["120000_300".to_owned()]);
    });
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
    status_outcomes: VecDeque<Result<(), SyncOperationError>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Call {
    Upload(String),
    Listing(String),
    SystemStatus,
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
                Call::Listing(_) | Call::SystemStatus => None,
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
            if !self.evidence_visible(source) {
                return Ok(UploadResult {
                    status: UploadStatus::Failed,
                    authoritative_key: None,
                    descriptors: None,
                });
            }
            let inventory = inventory_files(files, None).await.map_err(|_| {
                SyncOperationError::RetainCandidate {
                    diagnostic: DiagnosticCode::LocalSegmentInvalid,
                    answer: "local:local_segment_invalid".to_owned(),
                }
            })?;
            let descriptors = inventory
                .iter()
                .map(|f| ParsedDescriptor {
                    submitted: f.name.clone(),
                    written: f.name.clone(),
                    sha256: f.sha256.clone(),
                    size: f.size,
                    disposition: "written".to_owned(),
                })
                .collect();
            if self.evidence_visible(source) {
                self.remote
                    .entry(candidate.day().to_owned())
                    .or_default()
                    .insert(candidate.segment().to_owned(), inventory);
            }
            Ok(UploadResult {
                status: UploadStatus::Ok,
                authoritative_key: Some(candidate.segment().to_owned()),
                descriptors: Some(Ok(descriptors)),
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

    fn system_status<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<(), SyncOperationError>> + Send + 'a>> {
        Box::pin(async move {
            self.calls.push(Call::SystemStatus);
            if let Some(outcome) = self.status_outcomes.pop_front() {
                outcome
            } else {
                Ok(())
            }
        })
    }
}

struct BackoffJournal {
    listings: mpsc::UnboundedSender<()>,
    outcomes: VecDeque<Result<(), SyncOperationError>>,
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
        Box::pin(async { unreachable!("backoff fixture makes no segment listings") })
    }

    fn system_status<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<(), SyncOperationError>> + Send + 'a>> {
        Box::pin(async move {
            let _ = self.listings.send(());
            self.outcomes.pop_front().unwrap_or(Ok(()))
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

    fn system_status<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<(), SyncOperationError>> + Send + 'a>> {
        self.inner.system_status()
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum BlockingStage {
    Status,
    Upload,
    RetentionListing,
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
}

impl SyncJournal for BlockingJournal {
    fn upload<'a>(
        &'a mut self,
        candidate: &'a SegmentCandidate,
        files: Vec<PathBuf>,
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
            let descriptors = if self.stage == BlockingStage::RetentionListing {
                let inventory = inventory_files(files, None).await.map_err(|_| {
                    SyncOperationError::RetainCandidate {
                        diagnostic: DiagnosticCode::LocalSegmentInvalid,
                        answer: "local:local_segment_invalid".to_owned(),
                    }
                })?;
                Some(Ok(inventory
                    .into_iter()
                    .map(|f| ParsedDescriptor {
                        submitted: f.name.clone(),
                        written: f.name,
                        sha256: f.sha256,
                        size: f.size,
                        disposition: "written".to_owned(),
                    })
                    .collect()))
            } else {
                None
            };
            Ok(UploadResult {
                status: UploadStatus::Ok,
                authoritative_key: Some(candidate.segment().to_owned()),
                descriptors,
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
            if self.stage == BlockingStage::RetentionListing {
                self.wait_forever().await;
            }
            Ok(SegmentsEnvelope {
                items: Vec::new(),
                total: 0,
                protocol_version: PROTOCOL_VERSION_NUMBER,
            })
        })
    }

    fn system_status<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<(), SyncOperationError>> + Send + 'a>> {
        Box::pin(async move {
            if self.stage == BlockingStage::Status {
                self.wait_forever().await;
            }
            Ok(())
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
