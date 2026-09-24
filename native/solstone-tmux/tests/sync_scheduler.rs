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
    LocalFile, ParsedDescriptor, UploadResult, UploadStatus, decode_upload_response,
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
use solstone_tmux::storage::{
    AtomicWriteFault, set_atomic_write_fault, set_atomic_write_fault_for_prefix,
};
use solstone_tmux::sync::{
    JournalIdentity, SegmentCandidate, SegmentRemovalObserver, SegmentRemovalStage, SyncActivity,
    SyncFailureClass, SyncJournal, SyncOperationError, SyncScheduler, SyncWake,
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
        assert!(
            instrumentation.health_writes >= instrumentation.batches,
            "progress published {} times across {} batches",
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
    paused(async {
        let temporary = TestDirectory::new("sync-cache-change");
        create_segment(&temporary, "20260701", "120000_300", b"first\n");
        create_segment(&temporary, "20260701", "120100_300", b"other\n");
        let test_clock = clock();
        let mut scheduler =
            scheduler_with_clock(&temporary, SyncWake::default(), Arc::clone(&test_clock));
        let mut journal = FakeJournal::default();
        journal.upload_outcome(
            "120000_300",
            Ok(UploadResult {
                status: UploadStatus::Failed,
                authoritative_key: None,
                descriptors: None,
            }),
        );
        journal.upload_outcome(
            "120100_300",
            Ok(UploadResult {
                status: UploadStatus::Failed,
                authoritative_key: None,
                descriptors: None,
            }),
        );
        scheduler.run_sweep(&mut journal, no_shutdown()).await;
        journal.clear_calls();
        let before = scheduler.instrumentation();
        std::fs::write(
            segment_path(&temporary, "20260701", "120000_300").join(FILE),
            b"later\n",
        )
        .expect("rewrite same-size fixture");

        advance_both(&test_clock, Duration::from_secs(3601)).await;
        scheduler.run_sweep(&mut journal, no_shutdown()).await;
        let after = scheduler.instrumentation();

        assert_eq!(
            journal.uploads(),
            vec!["120100_300".to_owned(), "120000_300".to_owned()]
        );
        assert_eq!(after.hashed_files - before.hashed_files, 1);
    });
}

#[test]
fn adding_a_segment_file_invalidates_the_complete_inventory_only_for_that_candidate() {
    paused(async {
        let temporary = TestDirectory::new("sync-membership-add");
        create_segment(&temporary, "20260701", "120000_300", b"target\n");
        create_segment(&temporary, "20260701", "120100_300", b"other\n");
        let test_clock = clock();
        let mut scheduler =
            scheduler_with_clock(&temporary, SyncWake::default(), Arc::clone(&test_clock));
        let mut journal = FakeJournal::default();
        journal.upload_outcome(
            "120000_300",
            Ok(UploadResult {
                status: UploadStatus::Failed,
                authoritative_key: None,
                descriptors: None,
            }),
        );
        journal.upload_outcome(
            "120100_300",
            Ok(UploadResult {
                status: UploadStatus::Failed,
                authoritative_key: None,
                descriptors: None,
            }),
        );
        scheduler.run_sweep(&mut journal, no_shutdown()).await;
        journal.clear_calls();
        let before = scheduler.instrumentation();
        std::fs::write(
            segment_path(&temporary, "20260701", "120000_300").join("tmux_aux_screen.jsonl"),
            b"added\n",
        )
        .expect("add valid segment file");

        advance_both(&test_clock, Duration::from_secs(3601)).await;
        scheduler.run_sweep(&mut journal, no_shutdown()).await;

        assert_eq!(
            journal.uploads(),
            vec!["120100_300".to_owned(), "120000_300".to_owned()]
        );
        assert_eq!(
            scheduler.instrumentation().hashed_files - before.hashed_files,
            2
        );
        assert_eq!(scheduler.cached_inventories(), 0);
    });
}

#[test]
fn removing_a_segment_file_invalidates_the_complete_inventory_only_for_that_candidate() {
    paused(async {
        let temporary = TestDirectory::new("sync-membership-remove");
        create_segment(&temporary, "20260701", "120000_300", b"target\n");
        std::fs::write(
            segment_path(&temporary, "20260701", "120000_300").join("tmux_aux_screen.jsonl"),
            b"removed\n",
        )
        .expect("add valid segment file");
        create_segment(&temporary, "20260701", "120100_300", b"other\n");
        let test_clock = clock();
        let mut scheduler =
            scheduler_with_clock(&temporary, SyncWake::default(), Arc::clone(&test_clock));
        let mut journal = FakeJournal::default();
        journal.upload_outcome(
            "120000_300",
            Ok(UploadResult {
                status: UploadStatus::Failed,
                authoritative_key: None,
                descriptors: None,
            }),
        );
        journal.upload_outcome(
            "120100_300",
            Ok(UploadResult {
                status: UploadStatus::Failed,
                authoritative_key: None,
                descriptors: None,
            }),
        );
        scheduler.run_sweep(&mut journal, no_shutdown()).await;
        journal.clear_calls();
        let before = scheduler.instrumentation();
        std::fs::remove_file(
            segment_path(&temporary, "20260701", "120000_300").join("tmux_aux_screen.jsonl"),
        )
        .expect("remove valid segment file");

        advance_both(&test_clock, Duration::from_secs(3601)).await;
        scheduler.run_sweep(&mut journal, no_shutdown()).await;

        assert_eq!(
            journal.uploads(),
            vec!["120100_300".to_owned(), "120000_300".to_owned()]
        );
        assert_eq!(
            scheduler.instrumentation().hashed_files - before.hashed_files,
            1
        );
        assert_eq!(scheduler.cached_inventories(), 0);
    });
}

#[test]
fn renaming_a_segment_file_invalidates_the_complete_inventory_only_for_that_candidate() {
    paused(async {
        let temporary = TestDirectory::new("sync-membership-rename");
        create_segment(&temporary, "20260701", "120000_300", b"target\n");
        std::fs::write(
            segment_path(&temporary, "20260701", "120000_300").join("tmux_aux_screen.jsonl"),
            b"renamed\n",
        )
        .expect("add valid segment file");
        create_segment(&temporary, "20260701", "120100_300", b"other\n");
        let test_clock = clock();
        let mut scheduler =
            scheduler_with_clock(&temporary, SyncWake::default(), Arc::clone(&test_clock));
        let mut journal = FakeJournal::default();
        journal.upload_outcome(
            "120000_300",
            Ok(UploadResult {
                status: UploadStatus::Failed,
                authoritative_key: None,
                descriptors: None,
            }),
        );
        journal.upload_outcome(
            "120100_300",
            Ok(UploadResult {
                status: UploadStatus::Failed,
                authoritative_key: None,
                descriptors: None,
            }),
        );
        scheduler.run_sweep(&mut journal, no_shutdown()).await;
        journal.clear_calls();
        let before = scheduler.instrumentation();
        std::fs::rename(
            segment_path(&temporary, "20260701", "120000_300").join("tmux_aux_screen.jsonl"),
            segment_path(&temporary, "20260701", "120000_300").join("tmux_renamed_screen.jsonl"),
        )
        .expect("rename valid segment file");

        advance_both(&test_clock, Duration::from_secs(3601)).await;
        scheduler.run_sweep(&mut journal, no_shutdown()).await;

        assert_eq!(
            journal.uploads(),
            vec!["120100_300".to_owned(), "120000_300".to_owned()]
        );
        assert_eq!(
            scheduler.instrumentation().hashed_files - before.hashed_files,
            2
        );
        assert_eq!(scheduler.cached_inventories(), 0);
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
        journal.upload_outcome(
            "120000_300",
            Ok(UploadResult {
                status: UploadStatus::Failed,
                authoritative_key: None,
                descriptors: None,
            }),
        );
        journal.upload_outcome(
            "120100_300",
            Ok(UploadResult {
                status: UploadStatus::Failed,
                authoritative_key: None,
                descriptors: None,
            }),
        );
        scheduler.run_sweep(&mut journal, no_shutdown()).await;
        assert_eq!(scheduler.cached_inventories(), 2);

        std::fs::remove_dir_all(segment_path(&temporary, "20260701", "120100_300"))
            .expect("remove segment");
        scheduler.run_sweep(&mut journal, no_shutdown()).await;

        assert_eq!(scheduler.cached_inventories(), 1);
    });
}

#[test]
fn retention_deletes_and_evicts_the_cached_inventory() {
    run(async {
        let temporary = TestDirectory::new("sync-retention-fresh-proof");
        create_segment(&temporary, "20260701", "120000_300", b"fixture\n");
        let mut scheduler = scheduler_with_source(&temporary, SyncWake::default(), DEFAULT_SOURCE);
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
        let mut scheduler = scheduler_with_source(&temporary, SyncWake::default(), DEFAULT_SOURCE);
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
fn bounded_batches_reflect_the_eight_candidate_limit() {
    paused(async {
        for (count, expected_batches) in [(1, 1), (8, 1), (9, 2), (16, 2), (17, 3)] {
            let temporary = TestDirectory::new(&format!("sync-batch-{count}"));
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
        let bytes = b"first\n";
        create_segment(&temporary, "20260701", "120000_300", bytes);
        let sha256 = sha256_hex(bytes);
        let size = bytes.len() as u64;
        let ack_val = valid_ack_json(
            &temporary,
            "20260701",
            "120000_300",
            &[(FILE, size, &sha256)],
        );
        write_ack(&temporary, "20260701", "120000_300", &ack_val);

        std::fs::write(
            segment_path(&temporary, "20260701", "120000_300").join(FILE),
            b"other\n",
        )
        .expect("change digest");

        let mut scheduler = scheduler(&temporary, SyncWake::default());
        let mut journal = FakeJournal::default();
        let summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;
        assert_eq!(summary.custodied, 1);
        assert_eq!(journal.uploads(), ["120000_300"]);
    });
}

#[test]
fn ack_invalidation_forces_reupload_before_custody() {
    run(async {
        let temporary = TestDirectory::new("sync-single-remote-loss");
        let bytes = b"fixture\n";
        let sha256 = sha256_hex(bytes);
        let size = bytes.len() as u64;
        for segment in ["120000_300", "120100_300", "120200_300"] {
            create_segment(&temporary, "20260701", segment, bytes);
            let ack_val = valid_ack_json(&temporary, "20260701", segment, &[(FILE, size, &sha256)]);
            write_ack(&temporary, "20260701", segment, &ack_val);
        }

        std::fs::write(
            segment_path(&temporary, "20260701", "120100_300").join(FILE),
            b"modified\n",
        )
        .expect("modify file");

        let mut scheduler = scheduler(&temporary, SyncWake::default());
        let mut journal = FakeJournal::default();
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
        let temporary = TestDirectory::new("sync-invalid-receipt");
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
fn successful_empty_sweep_counts_as_contact() {
    run(async {
        let temporary = TestDirectory::new("sync-empty");
        let mut scheduler = scheduler(&temporary, SyncWake::default());
        let mut journal = FakeJournal::default();
        let summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;
        assert!(summary.contacted);
        assert_eq!(summary.attempted, 0);
        assert_eq!(journal.calls, vec![Call::SystemStatus]);
    });
}

#[test]
fn activity_is_working_only_while_a_real_candidate_is_in_flight() {
    run(async {
        let temporary = TestDirectory::new("sync-activity-real");
        create_segment(&temporary, "20260701", "120000_300", b"fixture\n");
        let (activity, receiver) = watch::channel(SyncActivity::Idle);
        let (entered, started) = oneshot::channel();
        let (mut journal, _uploads) = blocking_journal(BlockingStage::Upload, entered);
        let (stop, shutdown) = watch::channel(false);
        let mut scheduler = scheduler(&temporary, SyncWake::default()).with_activity(activity);
        let task = tokio::spawn(async move { scheduler.run_sweep(&mut journal, shutdown).await });
        started.await.expect("upload began");
        assert_eq!(*receiver.borrow(), SyncActivity::Working);
        stop.send_replace(true);
        let _ = task.await;
    });
}

#[test]
fn delivery_across_batches_has_one_working_interval_and_failures_return_idle() {
    run(async {
        let temporary = TestDirectory::new("sync-activity-batches");
        for index in 0..10 {
            create_segment(
                &temporary,
                "20260701",
                &format!("12{index:02}00_300"),
                b"fixture\n",
            );
        }
        let (activity, receiver) = watch::channel(SyncActivity::Idle);
        let mut scheduler = scheduler(&temporary, SyncWake::default()).with_activity(activity);
        let mut journal = FakeJournal::default();
        journal.upload_outcome(
            "120000_300",
            Err(SyncOperationError::EndSweep(SyncFailureClass::Timeout)),
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
fn startup_finalization_and_periodic_wakes_converge_on_a_rescan() {
    paused(async {
        let temporary = TestDirectory::new("sync-wake-sources");
        let clock = clock();
        let wake = SyncWake::default();
        let (status_probes, mut received) = mpsc::unbounded_channel();
        let journal = BackoffJournal {
            status_probes,
            outcomes: VecDeque::new(),
        };
        let (stop, shutdown) = watch::channel(false);
        let mut scheduler = scheduler(&temporary, wake.clone());
        let task = tokio::spawn(async move {
            let mut journal = journal;
            scheduler.run_with_shutdown(&mut journal, shutdown).await;
        });

        expect_status_probe(&mut received, "startup").await;
        wake.segment_closed(&SegmentClose::Finalized(PathBuf::from("wake")));
        expect_status_probe(&mut received, "finalization").await;
        advance_both(&clock, Duration::from_secs(60) + Duration::from_millis(1)).await;
        expect_status_probe(&mut received, "periodic").await;

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
        let (status_probes, mut received) = mpsc::unbounded_channel();
        let journal = BackoffJournal {
            status_probes,
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

        expect_status_probe(&mut received, "initial failure").await;
        wake.segment_closed(&SegmentClose::Finalized(PathBuf::from("coalesced")));
        advance_both(&clock, Duration::from_secs(4)).await;
        assert_no_status_probe(&mut received).await;

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
            expect_status_probe(&mut received, context).await;
            assert!(captures.load(Ordering::SeqCst) > captures_before);
            assert!(segments.load(Ordering::SeqCst) > segments_before);
        }

        wake.segment_closed(&SegmentClose::Finalized(PathBuf::from("reset")));
        expect_status_probe(&mut received, "post-success failure").await;
        advance_both(&clock, Duration::from_secs(4)).await;
        assert_no_status_probe(&mut received).await;
        advance_both(&clock, Duration::from_secs(1) + Duration::from_millis(1)).await;
        expect_status_probe(&mut received, "reset five-second retry").await;

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
        let mut scheduler = scheduler_with_source(&temporary, SyncWake::default(), DEFAULT_SOURCE)
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
        let mut scheduler = scheduler_with_source(&deleted, SyncWake::default(), DEFAULT_SOURCE)
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
        let mut scheduler = scheduler_with_source(&retained, SyncWake::default(), DEFAULT_SOURCE)
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
        let mut scheduler = scheduler_with_source(&temporary, SyncWake::default(), DEFAULT_SOURCE)
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
        let mut scheduler = scheduler_with_source(&temporary, SyncWake::default(), "studio")
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
        let mut scheduler = scheduler_with_source(&temporary, SyncWake::default(), "studio")
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
        let mut scheduler = scheduler_with_source(&temporary, SyncWake::default(), "studio");
        let summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;

        assert_eq!(summary.custodied, 1);
        assert!(!segment_path(&temporary, "20260701", "120000_300").exists());
        journal.assert_sources("studio");
    });
}

#[test]
fn local_stream_paths_stay_independent_of_configured_source() {
    run(async {
        let temporary = TestDirectory::new("sync-stream-vs-source");
        create_segment(&temporary, "20260701", "120000_300", b"fixture\n");
        let mut scheduler = scheduler_with_source(&temporary, SyncWake::default(), "studio");
        let mut journal = FakeJournal::default();
        scheduler.run_sweep(&mut journal, no_shutdown()).await;

        journal.assert_sources("studio");
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
        let mut scheduler = scheduler_with_source(&temporary, SyncWake::default(), DEFAULT_SOURCE);
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
        let mut scheduler = scheduler_with_source(&temporary, SyncWake::default(), DEFAULT_SOURCE);
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
        let mut scheduler = scheduler_with_source(&temporary, SyncWake::default(), DEFAULT_SOURCE);
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
        let mut scheduler = scheduler_with_source(&temporary, SyncWake::default(), DEFAULT_SOURCE);
        let summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;
        assert_eq!(summary.custodied, 1);
        assert!(!segment_path(&temporary, "20260701", "120000_300").exists());
    });
}

#[test]
fn future_due_times_clamp_to_one_interval() {
    paused(async {
        let temporary = TestDirectory::new("bounds-clamp-interval");
        create_segment(&temporary, "20260701", "120000_300", b"fixture\n");
        let test_clock = clock();
        let mut scheduler1 =
            scheduler_with_clock(&temporary, SyncWake::default(), Arc::clone(&test_clock));
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
            scheduler_with_clock(&temporary, SyncWake::default(), Arc::clone(&test_clock));
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
            scheduler_with_clock(&temporary, SyncWake::default(), Arc::clone(&test_clock));
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
            scheduler_with_clock(&temporary, SyncWake::default(), Arc::clone(&test_clock));
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
            scheduler_with_clock(&temporary, SyncWake::default(), Arc::clone(&test_clock));
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
            scheduler_with_clock(&temporary, SyncWake::default(), Arc::clone(&test_clock));
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
        let _id1 = test_journal_identity();
        let bytes = b"fixture\n";
        let sha256 = sha256_hex(bytes);
        let size = bytes.len() as u64;
        let ack_val = valid_ack_json(
            &temporary,
            "20260701",
            "120000_300",
            &[(FILE, size, &sha256)],
        );
        write_ack(&temporary, "20260701", "120000_300", &ack_val);

        let test_clock = clock();
        let mut id2 = test_journal_identity();
        id2.pairing_generation_hex = "pairgen2".to_owned();
        let mut scheduler2 = scheduler_with_clock_and_identity(
            &temporary,
            SyncWake::default(),
            Arc::clone(&test_clock),
            id2,
        );
        let mut journal = FakeJournal::default();
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
        let seg = segment_path(&temporary, "20260701", "120000_300");
        assert!(seg.exists(), "an unwritten ack keeps the segment");

        let mut scheduler2 = scheduler(&temporary, SyncWake::default());
        journal.clear_calls();
        journal.upload_outcome(
            "120000_300",
            Ok(UploadResult {
                status: UploadStatus::Duplicate,
                authoritative_key: Some("120000_300".to_owned()),
                descriptors: Some(Ok(vec![ParsedDescriptor {
                    submitted: FILE.to_owned(),
                    written: FILE.to_owned(),
                    sha256: sha256_hex(b"fixture\n"),
                    size: b"fixture\n".len() as u64,
                    disposition: "already_held".to_owned(),
                }])),
            }),
        );
        let summary2 = scheduler2.run_sweep(&mut journal, no_shutdown()).await;
        assert_eq!(summary2.attempted, 1);
        assert_eq!(summary2.custodied, 1);
        assert_eq!(journal.uploads(), vec!["120000_300".to_owned()]);
        assert!(
            !seg.exists(),
            "the already-held receipt removes the segment"
        );
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
        let ack_path = temporary
            .path()
            .join("sync-ledger")
            .join("20260701")
            .join(STREAM)
            .join("120000_300")
            .join("ack.json");
        std::fs::create_dir_all(ack_path.parent().unwrap()).unwrap();
        std::fs::write(&ack_path, b"{\"day\":\"20260701").unwrap();

        let mut scheduler = scheduler(&temporary, SyncWake::default());
        let mut journal = FakeJournal::default();
        let summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;
        assert_eq!(summary.attempted, 1);
        assert_eq!(journal.uploads(), vec!["120000_300".to_owned()]);
    });
}

#[test]
fn ack_location_mismatch_is_not_an_ack() {
    run(async {
        let temporary = TestDirectory::new("ack-loc-mismatch");
        create_segment(&temporary, "20260701", "120000_300", b"fixture\n");
        let bytes = b"fixture\n";
        let sha256 = sha256_hex(bytes);
        let size = bytes.len() as u64;
        let mut ack_val = valid_ack_json(
            &temporary,
            "20260701",
            "120000_300",
            &[(FILE, size, &sha256)],
        );
        ack_val["location"] = serde_json::json!("/wrong/location/ack.json");
        write_ack(&temporary, "20260701", "120000_300", &ack_val);

        let mut scheduler = scheduler(&temporary, SyncWake::default());
        let mut journal = FakeJournal::default();
        let summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;
        assert_eq!(summary.attempted, 1);
        assert_eq!(journal.uploads(), vec!["120000_300".to_owned()]);
    });
}

#[test]
fn ack_for_another_journal_is_not_an_ack() {
    run(async {
        let temporary = TestDirectory::new("ack-diff-journal");
        create_segment(&temporary, "20260701", "120000_300", b"fixture\n");
        let bytes = b"fixture\n";
        let sha256 = sha256_hex(bytes);
        let size = bytes.len() as u64;
        let mut ack_val = valid_ack_json(
            &temporary,
            "20260701",
            "120000_300",
            &[(FILE, size, &sha256)],
        );
        ack_val["instance_id"] = serde_json::json!("other-instance");
        write_ack(&temporary, "20260701", "120000_300", &ack_val);

        let mut scheduler = scheduler(&temporary, SyncWake::default());
        let mut journal = FakeJournal::default();
        let summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;
        assert_eq!(summary.attempted, 1);
        assert_eq!(summary.custodied, 1);
        assert_eq!(journal.uploads(), vec!["120000_300".to_owned()]);
        assert!(
            !segment_path(&temporary, "20260701", "120000_300").exists(),
            "this journal's receipt removes the segment"
        );
        assert!(
            !temporary
                .path()
                .join("sync-ledger/20260701")
                .join(STREAM)
                .join("120000_300")
                .exists(),
            "the removed segment leaves no ledger entry"
        );
    });
}

#[test]
fn ack_file_set_mismatch_is_not_an_ack() {
    run(async {
        let temporary = TestDirectory::new("ack-fileset-mismatch");
        create_segment(&temporary, "20260701", "120000_300", b"fixture\n");
        let bytes = b"fixture\n";
        let sha256 = sha256_hex(bytes);
        let size = bytes.len() as u64;
        let mut ack_val = valid_ack_json(
            &temporary,
            "20260701",
            "120000_300",
            &[(FILE, size, &sha256)],
        );
        ack_val["files"] = serde_json::json!([]);
        write_ack(&temporary, "20260701", "120000_300", &ack_val);

        let mut scheduler = scheduler(&temporary, SyncWake::default());
        let mut journal = FakeJournal::default();
        let summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;
        assert_eq!(summary.attempted, 1);
        assert_eq!(journal.uploads(), vec!["120000_300".to_owned()]);
    });
}

// === Confirmed Removal ===

#[test]
fn upload_receipt_removes_the_segment_and_its_ledger_entry() {
    run(async {
        let temporary = TestDirectory::new("upload-receipt-removes");
        create_segment(&temporary, "20260701", "120000_300", b"fixture\n");
        let seg_path = segment_path(&temporary, "20260701", "120000_300");
        let ledger_path = temporary
            .path()
            .join("sync-ledger")
            .join("20260701")
            .join(STREAM)
            .join("120000_300");

        let lock = InstanceLock::acquire(temporary.path()).expect("acquire lock");
        let health_writer = HealthWriter::new(temporary.path().to_path_buf(), &lock);
        let (activity, _rx) = watch::channel(SyncActivity::Idle);
        let mut scheduler =
            scheduler(&temporary, SyncWake::default()).with_observability(activity, health_writer);
        let mut journal = FakeJournal::default();
        let summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;

        assert_eq!(summary.custodied, 1);
        assert_eq!(summary.attempted, 1);
        assert!(!seg_path.exists(), "segment directory must be removed");
        assert!(!ledger_path.exists(), "ledger directory must be removed");

        // The next sweep, through the scheduler loop so it publishes health.
        std::fs::remove_file(temporary.path().join(HEALTH_FILENAME)).expect("clear health");
        journal.clear_calls();
        let (stop, shutdown) = watch::channel(false);
        let task = tokio::spawn(async move {
            scheduler.run_with_shutdown(&mut journal, shutdown).await;
            journal
        });
        let snapshot = wait_for_idle_snapshot(temporary.path()).await;
        stop.send_replace(true);
        let journal = task.await.expect("join scheduler");

        assert!(
            journal.uploads().is_empty(),
            "the next sweep uploads nothing"
        );
        assert_eq!(snapshot["pending_segments"], 0);
    });
}

#[test]
fn matching_local_ack_removes_the_segment_before_any_upload() {
    run(async {
        let temporary = TestDirectory::new("matching-ack-local-finish");
        let bytes = b"fixture\n";
        create_segment(&temporary, "20260701", "120000_300", bytes);
        let seg_path = segment_path(&temporary, "20260701", "120000_300");
        let ledger_path = temporary
            .path()
            .join("sync-ledger")
            .join("20260701")
            .join(STREAM)
            .join("120000_300");
        let sha256 = sha256_hex(bytes);
        let size = bytes.len() as u64;
        let ack_val = valid_ack_json(
            &temporary,
            "20260701",
            "120000_300",
            &[(FILE, size, &sha256)],
        );
        write_ack(&temporary, "20260701", "120000_300", &ack_val);

        let mut scheduler = scheduler(&temporary, SyncWake::default());
        let mut journal = FakeJournal::default();
        let summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;

        assert_eq!(
            summary.attempted, 0,
            "matching ack is removed locally without upload"
        );
        assert_eq!(summary.custodied, 0);
        assert!(journal.uploads().is_empty());
        assert!(
            !seg_path.exists(),
            "segment directory removed by local_finish"
        );
        assert!(
            !ledger_path.exists(),
            "ledger directory removed by local_finish"
        );
    });
}

#[test]
fn segment_removed_on_upload_removes_the_segment_and_the_sweep_continues() {
    run(async {
        let temporary = TestDirectory::new("segment-removed-continues");
        create_segment(&temporary, "20260701", "120000_300", b"first\n");
        create_segment(&temporary, "20260701", "120100_300", b"second\n");
        let seg1 = segment_path(&temporary, "20260701", "120000_300");
        let seg2 = segment_path(&temporary, "20260701", "120100_300");

        let mut scheduler = scheduler(&temporary, SyncWake::default());
        let mut journal = FakeJournal::default();
        journal.upload_outcome("120000_300", Err(SyncOperationError::SegmentRemoved));

        let summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;

        assert_eq!(summary.attempted, 2);
        assert_eq!(summary.custodied, 1);
        assert_eq!(summary.failure, None);
        assert!(
            !seg1.exists(),
            "segment removed on journal is deleted locally"
        );
        assert!(!seg2.exists(), "subsequent segment uploaded and removed");
    });
}

#[test]
fn a_lone_segment_removed_answer_counts_as_contact_not_sync() {
    run(async {
        let temporary = TestDirectory::new("segment-removed-lone");
        create_segment(&temporary, "20260701", "120000_300", b"first\n");
        let seg1 = segment_path(&temporary, "20260701", "120000_300");
        let ledger_path = temporary
            .path()
            .join("sync-ledger")
            .join("20260701")
            .join(STREAM)
            .join("120000_300");

        let lock = InstanceLock::acquire(temporary.path()).expect("acquire lock");
        let health_writer = HealthWriter::new(temporary.path().to_path_buf(), &lock);
        let (activity, _rx) = watch::channel(SyncActivity::Idle);
        let (stop, shutdown) = watch::channel(false);
        let mut scheduler =
            scheduler(&temporary, SyncWake::default()).with_observability(activity, health_writer);
        let mut journal = FakeJournal::default();
        journal.upload_outcome("120000_300", Err(SyncOperationError::SegmentRemoved));

        let task = tokio::spawn(async move {
            scheduler.run_with_shutdown(&mut journal, shutdown).await;
        });

        let snapshot = wait_for_idle_snapshot(temporary.path()).await;
        stop.send_replace(true);
        task.await.expect("join scheduler");

        assert_eq!(snapshot["state"], "connected");
        assert!(snapshot["last_successful_sync_unix_seconds"].is_null());
        assert!(snapshot["last_successful_contact_unix_seconds"].is_number());
        assert!(snapshot["last_error_code"].is_null());
        assert!(
            !seg1.exists(),
            "segment removed on journal is deleted locally"
        );
        assert!(!ledger_path.exists(), "ledger is deleted after removal");
    });
}

#[test]
fn failed_upload_keeps_the_segment_and_defers_it() {
    paused(async {
        let temporary = TestDirectory::new("failed-upload-deferred");
        create_segment(&temporary, "20260701", "120000_300", b"fixture\n");
        let seg = segment_path(&temporary, "20260701", "120000_300");

        let test_clock = clock();
        let mut scheduler =
            scheduler_with_clock(&temporary, SyncWake::default(), Arc::clone(&test_clock));
        let mut journal = FakeJournal::default();
        journal.upload_outcome(
            "120000_300",
            Ok(UploadResult {
                status: UploadStatus::Failed,
                authoritative_key: None,
                descriptors: None,
            }),
        );

        let summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;
        assert_eq!(summary.attempted, 1);
        assert_eq!(summary.custodied, 0);
        assert!(seg.exists(), "segment is kept on upload failure");

        let state_path = temporary
            .path()
            .join("sync-ledger")
            .join("20260701")
            .join(STREAM)
            .join("120000_300")
            .join("state.json");
        assert!(state_path.exists(), "backoff state recorded");

        journal.clear_calls();
        let s_early = scheduler.run_sweep(&mut journal, no_shutdown()).await;
        assert_eq!(s_early.attempted, 0, "segment is deferred during backoff");
    });
}

#[test]
fn strict_subset_ack_with_changed_bytes_is_dropped_and_the_segment_uploads_once() {
    run(async {
        let temporary = TestDirectory::new("strict-subset-changed-bytes");
        let acked = b"first content\n";
        let acked_sha = sha256_hex(acked);
        let other_sha = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let ack_val = valid_ack_json(
            &temporary,
            "20260701",
            "120000_300",
            &[
                (FILE, acked.len() as u64, &acked_sha),
                ("tmux_aux_screen.jsonl", 100, other_sha),
            ],
        );
        write_ack(&temporary, "20260701", "120000_300", &ack_val);
        // Only one of the two acknowledged files is on disk, and its bytes changed.
        create_segment(&temporary, "20260701", "120000_300", b"changed content\n");
        let seg = segment_path(&temporary, "20260701", "120000_300");
        let ledger = temporary
            .path()
            .join("sync-ledger/20260701")
            .join(STREAM)
            .join("120000_300");

        let mut scheduler = scheduler(&temporary, SyncWake::default());
        let mut journal = FakeJournal::default();
        let summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;

        assert_eq!(summary.attempted, 1, "the dropped ack forces one upload");
        assert_eq!(summary.custodied, 1);
        assert_eq!(journal.uploads(), vec!["120000_300".to_owned()]);
        assert!(!seg.exists(), "the upload receipt removes the segment");
        assert!(
            !ledger.exists(),
            "the removed segment leaves no ledger entry"
        );
    });
}

#[test]
fn ack_for_another_identity_is_kept_until_a_new_receipt_removes_the_segment() {
    paused(async {
        let temporary = TestDirectory::new("ack-other-identity-kept");
        let bytes = b"fixture\n";
        create_segment(&temporary, "20260701", "120000_300", bytes);
        let sha256 = sha256_hex(bytes);
        let size = bytes.len() as u64;
        let mut ack_val = valid_ack_json(
            &temporary,
            "20260701",
            "120000_300",
            &[(FILE, size, &sha256)],
        );
        ack_val["instance_id"] = serde_json::json!("different-instance");
        write_ack(&temporary, "20260701", "120000_300", &ack_val);

        let ack_path = temporary
            .path()
            .join("sync-ledger")
            .join("20260701")
            .join(STREAM)
            .join("120000_300")
            .join("ack.json");

        let test_clock = clock();
        let mut scheduler =
            scheduler_with_clock(&temporary, SyncWake::default(), Arc::clone(&test_clock));
        let mut journal = FakeJournal::default();
        journal.upload_outcome(
            "120000_300",
            Ok(UploadResult {
                status: UploadStatus::Failed,
                authoritative_key: None,
                descriptors: None,
            }),
        );
        let summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;

        assert_eq!(
            summary.attempted, 1,
            "re-uploaded because ack is for different identity"
        );
        assert!(ack_path.exists(), "unmatched ack is not deleted");
        let seg = segment_path(&temporary, "20260701", "120000_300");
        assert!(seg.exists(), "a failed upload keeps the segment");

        advance_both(&test_clock, Duration::from_secs(3_600 + 60)).await;
        journal.clear_calls();
        let summary2 = scheduler.run_sweep(&mut journal, no_shutdown()).await;

        assert_eq!(summary2.attempted, 1);
        assert_eq!(summary2.custodied, 1);
        assert_eq!(journal.uploads(), vec!["120000_300".to_owned()]);
        assert!(!seg.exists(), "this journal's receipt removes the segment");
        assert!(!ack_path.exists(), "the removed segment leaves no ack");
    });
}

#[test]
fn local_finish_runs_before_the_backoff_wait() {
    run(async {
        let temporary = TestDirectory::new("local-finish-before-backoff");
        let bytes = b"fixture\n";
        create_segment(&temporary, "20260701", "120000_300", bytes);
        let sha256 = sha256_hex(bytes);
        let size = bytes.len() as u64;
        let ack_val = valid_ack_json(
            &temporary,
            "20260701",
            "120000_300",
            &[(FILE, size, &sha256)],
        );
        write_ack(&temporary, "20260701", "120000_300", &ack_val);

        let (stop, shutdown) = watch::channel(false);
        let mut scheduler = scheduler(&temporary, SyncWake::default());
        let mut journal = FakeJournal::default();
        journal
            .status_outcomes
            .push_back(Err(SyncOperationError::EndSweep(SyncFailureClass::Timeout)));

        let task = tokio::spawn(async move {
            scheduler.run_with_shutdown(&mut journal, shutdown).await;
        });

        // Yield to allow initial sweep + local_finish to run:
        tokio::time::sleep(Duration::from_millis(50)).await;
        let seg_path = segment_path(&temporary, "20260701", "120000_300");
        assert!(
            !seg_path.exists(),
            "local_finish deleted confirmed segment before backoff sleep"
        );

        stop.send_replace(true);
        task.await.expect("join scheduler");
    });
}

#[test]
fn failed_removal_keeps_the_ack_and_the_next_sweep_finishes_it() {
    run(async {
        use std::os::unix::fs::PermissionsExt;
        let temporary = TestDirectory::new("failed-removal-retry");
        create_segment(&temporary, "20260701", "120000_300", b"fixture\n");
        let seg_path = segment_path(&temporary, "20260701", "120000_300");
        let ack_path = temporary
            .path()
            .join("sync-ledger")
            .join("20260701")
            .join(STREAM)
            .join("120000_300")
            .join("ack.json");

        // Make segment directory read-only so unlinkat fails:
        std::fs::set_permissions(&seg_path, std::fs::Permissions::from_mode(0o555)).unwrap();

        let mut scheduler1 = scheduler(&temporary, SyncWake::default());
        let mut journal = FakeJournal::default();
        let summary1 = scheduler1.run_sweep(&mut journal, no_shutdown()).await;

        assert_eq!(summary1.attempted, 1);
        assert_eq!(summary1.custodied, 1);
        assert!(
            seg_path.exists(),
            "segment still on disk after failed unlink"
        );
        assert!(ack_path.exists(), "ack.json preserved after failed unlink");

        // Restore write permissions:
        std::fs::set_permissions(&seg_path, std::fs::Permissions::from_mode(0o755)).unwrap();

        // Next sweep completes local_finish:
        let mut scheduler2 = scheduler(&temporary, SyncWake::default());
        journal.clear_calls();
        let summary2 = scheduler2.run_sweep(&mut journal, no_shutdown()).await;

        assert_eq!(summary2.attempted, 0);
        assert!(!seg_path.exists(), "segment unlinked on retry sweep");
        assert!(!ack_path.exists(), "ledger unlinked on retry sweep");
    });
}

#[test]
fn removal_observer_sees_files_then_directory_unlinked() {
    run(async {
        let temporary = TestDirectory::new("removal-observer-order");
        create_segment(&temporary, "20260701", "120000_300", b"fixture\n");
        let seg_path = segment_path(&temporary, "20260701", "120000_300");
        let upload_file = seg_path.join(FILE);
        let ledger_dir = temporary
            .path()
            .join("sync-ledger")
            .join("20260701")
            .join(STREAM)
            .join("120000_300");
        let ack_path = ledger_dir.join("ack.json");

        let stages = Arc::new(Mutex::new(Vec::new()));
        let stages_clone = Arc::clone(&stages);
        let seg_path_clone = seg_path.clone();
        let upload_file_clone = upload_file.clone();
        let ack_path_clone = ack_path.clone();

        let observer: SegmentRemovalObserver = Arc::new(move |_, stage| {
            match stage {
                SegmentRemovalStage::FilesUnlinked => {
                    assert!(!upload_file_clone.exists(), "upload file must be unlinked");
                    assert!(
                        seg_path_clone.exists(),
                        "segment directory must still exist"
                    );
                    assert!(ack_path_clone.exists(), "ack.json must still exist");
                }
                SegmentRemovalStage::DirectoryUnlinked => {
                    assert!(
                        !seg_path_clone.exists(),
                        "segment directory must be unlinked"
                    );
                    assert!(ack_path_clone.exists(), "ack.json must still exist");
                }
            }
            stages_clone.lock().unwrap().push(stage);
        });

        let mut scheduler =
            scheduler(&temporary, SyncWake::default()).with_removal_observer(observer);
        let mut journal = FakeJournal::default();
        scheduler.run_sweep(&mut journal, no_shutdown()).await;

        let recorded = stages.lock().unwrap().clone();
        assert_eq!(
            recorded,
            vec![
                SegmentRemovalStage::FilesUnlinked,
                SegmentRemovalStage::DirectoryUnlinked,
            ]
        );
        assert!(!ack_path.exists(), "ack.json is gone after sweep returns");
        assert!(
            !ledger_dir.exists(),
            "ledger directory is gone after sweep returns"
        );
    });
}

#[test]
fn empty_segment_directory_is_removed_by_local_finish() {
    run(async {
        let temporary = TestDirectory::new("empty-segment-directory");
        let empty_path = segment_path(&temporary, "20260701", "120000_300");
        std::fs::create_dir_all(&empty_path).unwrap();
        assert!(empty_path.is_dir());

        let mut scheduler = scheduler(&temporary, SyncWake::default());
        let mut journal = FakeJournal::default();
        let summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;

        assert_eq!(summary.attempted, 0);
        assert!(
            !empty_path.exists(),
            "empty segment directory removed during local_finish"
        );
    });
}

#[test]
fn strict_subset_ack_removes_the_segment_without_upload() {
    run(async {
        let temporary = TestDirectory::new("strict-subset-ack");
        let bytes1 = b"file1\n";
        let sha1 = sha256_hex(bytes1);
        let size1 = bytes1.len() as u64;
        let sha2 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let size2 = 100u64;

        // On disk, only file1 remains
        create_segment(&temporary, "20260701", "120000_300", bytes1);
        let seg_path = segment_path(&temporary, "20260701", "120000_300");

        // Ack names two files: file1 and file2
        let ack_val = valid_ack_json(
            &temporary,
            "20260701",
            "120000_300",
            &[(FILE, size1, &sha1), ("other_file.jsonl", size2, sha2)],
        );
        write_ack(&temporary, "20260701", "120000_300", &ack_val);

        let mut scheduler = scheduler(&temporary, SyncWake::default());
        let mut journal = FakeJournal::default();
        let summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;

        assert_eq!(
            summary.attempted, 0,
            "strict subset is removed locally without upload"
        );
        assert_eq!(summary.custodied, 0);
        assert_eq!(summary.diagnostic, None);
        assert!(
            !seg_path.exists(),
            "segment directory removed by local_finish"
        );
    });
}

#[test]
fn empty_sealed_directories_are_removed_with_or_without_an_ack() {
    run(async {
        let temporary = TestDirectory::new("empty-sealed-directories");
        // Empty dir 1: no ack
        let empty1 = segment_path(&temporary, "20260701", "120000_300");
        std::fs::create_dir_all(&empty1).unwrap();

        // Empty dir 2: has ack for files that no longer exist
        let empty2 = segment_path(&temporary, "20260701", "120100_300");
        std::fs::create_dir_all(&empty2).unwrap();
        let ack_val = valid_ack_json(
            &temporary,
            "20260701",
            "120100_300",
            &[(
                FILE,
                10,
                "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            )],
        );
        write_ack(&temporary, "20260701", "120100_300", &ack_val);

        let mut scheduler = scheduler(&temporary, SyncWake::default());
        let mut journal = FakeJournal::default();
        let summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;

        assert_eq!(summary.attempted, 0);
        assert_eq!(summary.diagnostic, None);
        assert!(!empty1.exists());
        assert!(!empty2.exists());
    });
}

#[test]
fn acked_segment_is_removed_before_a_transport_failure_on_an_unacked_one() {
    run(async {
        let temporary = TestDirectory::new("acked-before-transport-failure");
        let bytes1 = b"first\n";
        let sha1 = sha256_hex(bytes1);
        let size1 = bytes1.len() as u64;

        // Acked segment 120000_300
        create_segment(&temporary, "20260701", "120000_300", bytes1);
        let seg1 = segment_path(&temporary, "20260701", "120000_300");
        let ack_val = valid_ack_json(
            &temporary,
            "20260701",
            "120000_300",
            &[(FILE, size1, &sha1)],
        );
        write_ack(&temporary, "20260701", "120000_300", &ack_val);

        // Unacked segment 120100_300
        create_segment(&temporary, "20260701", "120100_300", b"second\n");
        let seg2 = segment_path(&temporary, "20260701", "120100_300");

        let mut scheduler = scheduler(&temporary, SyncWake::default());
        let mut journal = FakeJournal::default();
        journal.upload_outcome(
            "120100_300",
            Err(SyncOperationError::EndSweep(SyncFailureClass::Direct)),
        );

        let summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;

        assert_eq!(summary.failure, Some(SyncFailureClass::Direct));
        assert!(
            !seg1.exists(),
            "acked segment was removed during local_finish"
        );
        assert!(
            seg2.exists(),
            "unacked segment is preserved after transport failure"
        );
    });
}

#[test]
fn missing_ledger_uploads_and_removes_on_an_already_held_receipt() {
    run(async {
        let temporary = TestDirectory::new("missing-ledger-already-held");
        let bytes = b"payload\n";
        create_segment(&temporary, "20260701", "120000_300", bytes);
        let seg = segment_path(&temporary, "20260701", "120000_300");
        let sha256 = sha256_hex(bytes);
        let size = bytes.len() as u64;

        let ledger_root = temporary.path().join("sync-ledger");
        assert!(
            !ledger_root.exists(),
            "sync-ledger should not exist initially"
        );

        let mut journal = FakeJournal::default();
        let descriptors = vec![ParsedDescriptor {
            submitted: FILE.to_owned(),
            written: FILE.to_owned(),
            sha256,
            size,
            disposition: "already_held".to_owned(),
        }];
        journal.upload_outcome(
            "120000_300",
            Ok(UploadResult {
                status: UploadStatus::Duplicate,
                authoritative_key: Some("120000_300".to_owned()),
                descriptors: Some(Ok(descriptors)),
            }),
        );

        let mut scheduler = scheduler(&temporary, SyncWake::default());
        let summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;

        assert_eq!(summary.attempted, 1);
        assert_eq!(summary.custodied, 1);
        assert!(!seg.exists());
    });
}

#[test]
fn earlier_ledger_tree_drops_hold_files_and_uploads_past_terminal_keep() {
    run(async {
        let temporary = TestDirectory::new("earlier-ledger-tree");
        // Segment 1: has valid ack for this journal
        let bytes1 = b"seg1\n";
        create_segment(&temporary, "20260701", "120000_300", bytes1);
        let seg1 = segment_path(&temporary, "20260701", "120000_300");
        let sha1 = sha256_hex(bytes1);
        let size1 = bytes1.len() as u64;
        let ack1 = valid_ack_json(
            &temporary,
            "20260701",
            "120000_300",
            &[(FILE, size1, &sha1)],
        );
        write_ack(&temporary, "20260701", "120000_300", &ack1);

        // Segment 2: state.json with only terminal_keep for this journal
        let bytes2 = b"seg2\n";
        create_segment(&temporary, "20260701", "120100_300", bytes2);
        let seg2 = segment_path(&temporary, "20260701", "120100_300");
        let state2_dir = temporary
            .path()
            .join("sync-ledger/20260701")
            .join(STREAM)
            .join("120100_300");
        std::fs::create_dir_all(&state2_dir).unwrap();
        std::fs::write(
            state2_dir.join("state.json"),
            serde_json::to_string(&serde_json::json!({
                "terminal_keep": {
                    "instance_id": "inst-1",
                    "ca_fp_prefix_hex": "cafp1",
                    "pairing_generation_hex": "pairgen1"
                }
            }))
            .unwrap(),
        )
        .unwrap();

        // hold.json in ledger day
        let day_ledger = temporary.path().join("sync-ledger/20260701");
        let hold_path = day_ledger.join("hold.json");
        std::fs::write(&hold_path, b"{\"hold\": true}").unwrap();

        let mut scheduler = scheduler(&temporary, SyncWake::default());
        let mut journal = FakeJournal::default();
        journal.upload_outcome("120100_300", Err(SyncOperationError::SegmentRemoved));

        let summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;

        assert_eq!(summary.attempted, 1);
        assert_eq!(summary.custodied, 0);
        assert!(!seg1.exists(), "ack-confirmed segment removed locally");
        assert!(!seg2.exists(), "segment_removed removes candidate");
        assert!(!hold_path.exists(), "hold.json was cleaned up");
        assert!(
            !day_ledger.exists(),
            "empty day ledger directory was pruned"
        );
    });
}

#[test]
fn future_attempt_defers_and_strips_the_terminal_keep_marker() {
    run(async {
        let temporary = TestDirectory::new("future-attempt-terminal-keep");
        let bytes = b"deferred\n";
        create_segment(&temporary, "20260701", "120000_300", bytes);
        let seg = segment_path(&temporary, "20260701", "120000_300");

        let state_dir = temporary
            .path()
            .join("sync-ledger/20260701")
            .join(STREAM)
            .join("120000_300");
        std::fs::create_dir_all(&state_dir).unwrap();
        let state_path = state_dir.join("state.json");
        let future_time = clock().wall_now().unix_timestamp() + 3600;
        std::fs::write(
            &state_path,
            serde_json::to_string(&serde_json::json!({
                "next_attempt_unix": future_time,
                "next_attempt_interval_seconds": 3600,
                "terminal_keep": {
                    "instance_id": "inst-1",
                    "ca_fp_prefix_hex": "cafp1",
                    "pairing_generation_hex": "pairgen1"
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let mut scheduler = scheduler(&temporary, SyncWake::default());
        let mut journal = FakeJournal::default();
        let summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;

        assert_eq!(summary.attempted, 0, "deferred segment is not attempted");
        assert!(seg.exists());

        // Verify state.json no longer contains terminal_keep:
        let state_bytes = std::fs::read(&state_path).unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&state_bytes).unwrap();
        assert!(parsed.get("terminal_keep").is_none());
        assert_eq!(parsed["next_attempt_unix"], future_time);
    });
}

#[test]
fn recreated_segment_at_the_same_name_uploads_changed_bytes_and_finishes_identical_bytes_locally() {
    run(async {
        let temporary = TestDirectory::new("recreated-segment-bytes");
        let original_bytes = b"original\n";
        let orig_sha = sha256_hex(original_bytes);
        let orig_size = original_bytes.len() as u64;

        // Old ack on disk for 120000_300:
        let ack_val = valid_ack_json(
            &temporary,
            "20260701",
            "120000_300",
            &[(FILE, orig_size, &orig_sha)],
        );
        write_ack(&temporary, "20260701", "120000_300", &ack_val);

        // Case A: new segment created with DIFFERENT bytes
        let new_bytes = b"different content\n";
        create_segment(&temporary, "20260701", "120000_300", new_bytes);
        let seg = segment_path(&temporary, "20260701", "120000_300");

        let mut scheduler = scheduler(&temporary, SyncWake::default());
        let mut journal = FakeJournal::default();
        let summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;

        assert_eq!(summary.attempted, 1, "different bytes must be uploaded");
        assert_eq!(summary.custodied, 1);
        assert!(!seg.exists());

        // Case B: Same name and identical bytes
        let ack_val2 = valid_ack_json(
            &temporary,
            "20260701",
            "120100_300",
            &[(FILE, orig_size, &orig_sha)],
        );
        write_ack(&temporary, "20260701", "120100_300", &ack_val2);
        create_segment(&temporary, "20260701", "120100_300", original_bytes);
        let seg2 = segment_path(&temporary, "20260701", "120100_300");

        journal.clear_calls();
        let summary2 = scheduler.run_sweep(&mut journal, no_shutdown()).await;

        assert_eq!(summary2.attempted, 0, "identical bytes are removed locally");
        assert_eq!(summary2.custodied, 0);
        assert!(!seg2.exists());
    });
}

#[test]
fn recreated_segment_with_a_file_the_stale_ack_does_not_cover_uploads_once_and_is_removed() {
    let Some(stderr) = child_stderr(
        "recreated_segment_with_a_file_the_stale_ack_does_not_cover_uploads_once_and_is_removed",
        || {
            run(async {
                let temporary = TestDirectory::new("recreated-segment-uncovered-name");
                let original = b"original\n";
                let ack_val = valid_ack_json(
                    &temporary,
                    "20260701",
                    "120000_300",
                    &[(FILE, original.len() as u64, &sha256_hex(original))],
                );
                write_ack(&temporary, "20260701", "120000_300", &ack_val);
                // The later segment keeps the acknowledged file unchanged and adds
                // a file the stale ack never named.
                create_segment(&temporary, "20260701", "120000_300", original);
                let seg = segment_path(&temporary, "20260701", "120000_300");
                std::fs::write(seg.join("tmux_aux_screen.jsonl"), b"new pane\n")
                    .expect("write uncovered file");
                let ledger = temporary
                    .path()
                    .join("sync-ledger/20260701")
                    .join(STREAM)
                    .join("120000_300");

                let acked_names = Arc::new(Mutex::new(Vec::new()));
                let observed_names = Arc::clone(&acked_names);
                let ack_path = ledger.join("ack.json");
                let observer: SegmentRemovalObserver = Arc::new(move |_, stage| {
                    if stage == SegmentRemovalStage::FilesUnlinked {
                        let ack: serde_json::Value = serde_json::from_slice(
                            &std::fs::read(&ack_path).expect("read ack before removal"),
                        )
                        .expect("parse ack before removal");
                        let mut names = ack["files"]
                            .as_array()
                            .expect("ack files")
                            .iter()
                            .map(|file| file["submitted"].as_str().expect("name").to_owned())
                            .collect::<Vec<_>>();
                        names.sort();
                        *observed_names.lock().expect("ack names lock") = names;
                    }
                });
                let mut scheduler =
                    scheduler(&temporary, SyncWake::default()).with_removal_observer(observer);
                let mut journal = FakeJournal::default();
                let summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;

                assert_eq!(summary.attempted, 1, "an uncovered file forces an upload");
                assert_eq!(summary.custodied, 1);
                assert_eq!(journal.uploads(), vec!["120000_300".to_owned()]);
                assert_eq!(
                    *acked_names.lock().expect("ack names lock"),
                    vec!["tmux_aux_screen.jsonl".to_owned(), FILE.to_owned()],
                    "the segment is acked on its own receipt"
                );
                assert!(!seg.exists(), "the receipt removes the segment");
                assert!(
                    !ledger.exists(),
                    "the removed segment leaves no ledger entry"
                );

                for _ in 0..2 {
                    journal.clear_calls();
                    let later = scheduler.run_sweep(&mut journal, no_shutdown()).await;
                    assert_eq!(later.attempted, 0);
                    assert!(journal.uploads().is_empty(), "the segment uploads once");
                }
            });
        },
    ) else {
        return;
    };
    assert_eq!(
        stderr.matches(REMOVAL_FAILURE_LINE).count(),
        0,
        "the removal-failure line must not appear, let alone repeat:\n{stderr}"
    );
}

#[test]
fn a_removal_refused_after_a_change_logs_one_line_and_keeps_the_segment() {
    let Some(stderr) = child_stderr(
        "a_removal_refused_after_a_change_logs_one_line_and_keeps_the_segment",
        || {
            run(async {
                let temporary = TestDirectory::new("refused-removal-logs");
                create_segment(&temporary, "20260701", "120000_300", b"fixture\n");
                let seg = segment_path(&temporary, "20260701", "120000_300");
                let rewritten = b"rewritten while the upload was in flight\n".to_vec();

                let mut scheduler = scheduler(&temporary, SyncWake::default());
                let mut journal = FakeJournal {
                    rewrite_after_receipt: Some((seg.join(FILE), rewritten.clone())),
                    ..FakeJournal::default()
                };
                let summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;

                assert_eq!(summary.custodied, 1);
                assert_eq!(
                    std::fs::read(seg.join(FILE)).expect("read kept file"),
                    rewritten,
                    "a file changed after its receipt keeps the segment"
                );
                eprintln!("-- next sweep --");

                journal.clear_calls();
                let next = scheduler.run_sweep(&mut journal, no_shutdown()).await;
                assert_eq!(next.attempted, 1, "the changed bytes upload again");
                assert_eq!(next.custodied, 1);
                assert!(!seg.exists(), "their receipt removes the segment");
            });
        },
    ) else {
        return;
    };
    let (first_sweep, next_sweep) = stderr
        .split_once("-- next sweep --")
        .expect("child reached the next sweep");
    assert_eq!(
        first_sweep.matches(REMOVAL_FAILURE_LINE).count(),
        1,
        "a refused removal logs one line:\n{stderr}"
    );
    assert_eq!(
        next_sweep.matches(REMOVAL_FAILURE_LINE).count(),
        0,
        "a completed removal logs nothing:\n{stderr}"
    );
}

#[test]
fn local_finish_stops_at_a_shutdown_request_before_the_next_candidate() {
    run(async {
        let temporary = TestDirectory::new("local-finish-shutdown");
        let segments = ["120000_300", "120100_300", "120200_300"];
        for (index, segment) in segments.iter().enumerate() {
            let bytes = format!("fixture {index}\n");
            create_segment(&temporary, "20260701", segment, bytes.as_bytes());
            let ack_val = valid_ack_json(
                &temporary,
                "20260701",
                segment,
                &[(FILE, bytes.len() as u64, &sha256_hex(bytes.as_bytes()))],
            );
            write_ack(&temporary, "20260701", segment, &ack_val);
        }

        let (stop, shutdown) = watch::channel(false);
        let stop = Arc::new(stop);
        let hook_stop = Arc::clone(&stop);
        let mut scheduler =
            scheduler(&temporary, SyncWake::default()).with_delete_hook(Arc::new(move |_| {
                hook_stop.send_replace(true);
            }));
        let mut journal = FakeJournal::default();
        let summary = scheduler.run_sweep(&mut journal, shutdown).await;

        assert!(summary.cancelled, "the shutdown request ends the sweep");
        assert_eq!(summary.attempted, 0);
        assert!(journal.calls.is_empty(), "a stopped sweep calls no journal");
        let kept = segments
            .iter()
            .filter(|segment| segment_path(&temporary, "20260701", segment).exists())
            .count();
        assert_eq!(
            kept, 2,
            "the removal under way finishes and no later candidate starts"
        );
    });
}

#[test]
fn confirmed_removal_keeps_day_stream_and_failed_or_incomplete_siblings() {
    run(async {
        let temporary = TestDirectory::new("confirmed-removal-siblings");
        create_segment(&temporary, "20260701", "120000_300", b"content\n");
        let confirmed_seg = segment_path(&temporary, "20260701", "120000_300");

        let stream_dir = temporary.path().join("captures/20260701").join(STREAM);
        let day_dir = temporary.path().join("captures/20260701");
        let failed_sibling = stream_dir.join("120100_300.failed");
        let incomplete_sibling = stream_dir.join("120200_300.incomplete");
        std::fs::create_dir_all(&failed_sibling).unwrap();
        std::fs::create_dir_all(&incomplete_sibling).unwrap();

        let mut scheduler = scheduler(&temporary, SyncWake::default());
        let mut journal = FakeJournal::default();
        let summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;

        assert_eq!(summary.custodied, 1);
        assert!(!confirmed_seg.exists(), "confirmed segment was removed");
        assert!(day_dir.exists(), "capture day directory preserved");
        assert!(stream_dir.exists(), "capture stream directory preserved");
        assert!(failed_sibling.exists(), ".failed sibling preserved");
        assert!(incomplete_sibling.exists(), ".incomplete sibling preserved");
    });
}

// === Test Helpers ===

const REMOVAL_FAILURE_LINE: &str = "solstone-tmux: confirmed segment was not removed";
const STDERR_CHILD_ENV: &str = "SOLSTONE_TMUX_TEST_STDERR_CHILD";

/// Runs `body` in a fresh process of this test binary, filtered to the test
/// named `test_name`, and returns what that process wrote to stderr. Inside the
/// child process the body runs and this returns `None`.
fn child_stderr(test_name: &str, body: impl FnOnce()) -> Option<String> {
    if std::env::var(STDERR_CHILD_ENV).is_ok_and(|name| name == test_name) {
        body();
        return None;
    }
    let output = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .args([test_name, "--exact", "--nocapture", "--test-threads=1"])
        .env(STDERR_CHILD_ENV, test_name)
        .output()
        .expect("run the test in a child process");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        output.status.success(),
        "child test failed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("test result: ok. 1 passed"),
        "child process did not run {test_name}:\n{stdout}"
    );
    Some(stderr)
}

fn test_journal_identity() -> JournalIdentity {
    JournalIdentity {
        instance_id: "inst-1".to_owned(),
        ca_fp_prefix_hex: "cafp1".to_owned(),
        pairing_generation_hex: "pairgen1".to_owned(),
    }
}

fn scheduler(temporary: &TestDirectory, wake: SyncWake) -> SyncScheduler {
    scheduler_with_source(temporary, wake, DEFAULT_SOURCE)
}

fn scheduler_with_source(temporary: &TestDirectory, wake: SyncWake, source: &str) -> SyncScheduler {
    SyncScheduler::new(
        temporary.path().to_path_buf(),
        stream(),
        source.to_owned(),
        clock(),
        wake,
        test_journal_identity(),
    )
}

fn scheduler_with_clock(
    temporary: &TestDirectory,
    wake: SyncWake,
    clock: Arc<TestClock>,
) -> SyncScheduler {
    scheduler_with_clock_and_identity(temporary, wake, clock, test_journal_identity())
}

fn scheduler_with_clock_and_identity(
    temporary: &TestDirectory,
    wake: SyncWake,
    clock: Arc<TestClock>,
    identity: JournalIdentity,
) -> SyncScheduler {
    SyncScheduler::new(
        temporary.path().to_path_buf(),
        stream(),
        DEFAULT_SOURCE.to_owned(),
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

fn write_ack(temporary: &TestDirectory, day: &str, segment: &str, ack: &serde_json::Value) {
    let path = temporary
        .path()
        .join("sync-ledger")
        .join(day)
        .join(STREAM)
        .join(segment)
        .join("ack.json");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, serde_json::to_string(ack).unwrap()).unwrap();
}

fn valid_ack_json(
    temporary: &TestDirectory,
    day: &str,
    segment: &str,
    files: &[(&str, u64, &str)],
) -> serde_json::Value {
    let ledger_path = temporary
        .path()
        .join("sync-ledger")
        .join(day)
        .join(STREAM)
        .join(segment)
        .join("ack.json");
    serde_json::json!({
        "location": ledger_path.to_string_lossy(),
        "day": day,
        "stream": STREAM,
        "segment": segment,
        "stored_key": segment,
        "instance_id": "inst-1",
        "ca_fp_prefix_hex": "cafp1",
        "pairing_generation_hex": "pairgen1",
        "proof": "upload",
        "files": files.iter().map(|(name, size, sha)| serde_json::json!({
            "submitted": name,
            "written": name,
            "size": size,
            "sha256": sha,
            "disposition": "written",
        })).collect::<Vec<_>>()
    })
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
            scheduler_with_clock(&temporary, SyncWake::default(), Arc::clone(&test_clock));
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

fn no_shutdown() -> tokio::sync::watch::Receiver<bool> {
    let (sender, receiver) = tokio::sync::watch::channel(false);
    std::mem::forget(sender);
    receiver
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

async fn expect_status_probe(status_probes: &mut mpsc::UnboundedReceiver<()>, context: &str) {
    let deadline = std::time::Instant::now() + HANG_GUARD;
    loop {
        if status_probes.try_recv().is_ok() {
            for _ in 0..SCHEDULER_TURNS {
                tokio::task::yield_now().await;
            }
            return;
        }
        if std::time::Instant::now() >= deadline {
            panic!("scheduler did not make the expected status probe: {context}");
        }
        tokio::task::yield_now().await;
        std::thread::sleep(Duration::from_millis(1));
    }
}

async fn assert_no_status_probe(status_probes: &mut mpsc::UnboundedReceiver<()>) {
    for _ in 0..SCHEDULER_TURNS {
        tokio::task::yield_now().await;
    }
    assert!(
        status_probes.try_recv().is_err(),
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

// === Fake & Mock Journals ===

#[derive(Default)]
struct FakeJournal {
    calls: Vec<Call>,
    sources: Vec<String>,
    evidence_for: Option<String>,
    remote: HashMap<String, HashMap<String, Vec<LocalFile>>>,
    upload_outcomes: HashMap<String, VecDeque<Result<UploadResult, SyncOperationError>>>,
    status_outcomes: VecDeque<Result<(), SyncOperationError>>,
    /// Rewrites one file after the receipt is computed, as if the file changed
    /// while the upload was in flight.
    rewrite_after_receipt: Option<(PathBuf, Vec<u8>)>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Call {
    Upload(String),
    SystemStatus,
}

impl FakeJournal {
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
                Call::SystemStatus => None,
            })
            .collect()
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
            if let Some((path, bytes)) = self.rewrite_after_receipt.take() {
                std::fs::write(path, bytes).expect("rewrite file after receipt");
            }
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
    status_probes: mpsc::UnboundedSender<()>,
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

    fn system_status<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<(), SyncOperationError>> + Send + 'a>> {
        Box::pin(async move {
            let _ = self.status_probes.send(());
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

#[derive(Clone, Copy, Eq, PartialEq)]
enum BlockingStage {
    Status,
    Upload,
}

struct BlockingJournal {
    stage: BlockingStage,
    entered: Option<oneshot::Sender<()>>,
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
                descriptors: None,
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
