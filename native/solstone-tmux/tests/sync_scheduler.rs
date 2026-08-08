// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

mod support;

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use solstone_tmux::clock::TestClock;
use solstone_tmux::health::DiagnosticCode;
use solstone_tmux::journal::{
    ListingFileStatus, LocalFile, SegmentFile, SegmentItem, SegmentsEnvelope, UploadResult,
    UploadStatus, inventory_files,
};
use solstone_tmux::name::{DerivedName, derive_component};
use solstone_tmux::segment::SegmentClose;
use solstone_tmux::sync::{
    SegmentCandidate, StatusBeacon, SyncActivity, SyncJournal, SyncOperationError, SyncScheduler,
    SyncWake,
};
use support::TestDirectory;
use time::{Date, Month, PrimitiveDateTime, Time, UtcOffset};
use tokio::sync::{oneshot, watch};

const STREAM: &str = "host.tmux";
const FILE: &str = "tmux_main_screen.jsonl";

#[test]
fn one_snapshot_attempts_every_candidate_once_and_yields_between_batches() {
    paused(async {
        let temporary = TestDirectory::new("sync-single-snapshot");
        for day in ["20260701", "20260702", "20260703"] {
            for index in 0..6 {
                create_segment(
                    &temporary,
                    day,
                    &format!("12{index:02}00_300"),
                    b"fixture\n",
                );
            }
        }
        let (eighth, eighth_started) = oneshot::channel();
        let (release, released) = oneshot::channel();
        let interleaved = Arc::new(AtomicBool::new(false));
        let ninth_saw_interleave = Arc::new(AtomicBool::new(false));
        let mut scheduler = scheduler(&temporary, SyncWake::default());
        let task = tokio::spawn({
            let interleaved = Arc::clone(&interleaved);
            let ninth_saw_interleave = Arc::clone(&ninth_saw_interleave);
            async move {
                let mut journal = InterleavingJournal {
                    inner: FakeJournal::default(),
                    uploads: 0,
                    eighth: Some(eighth),
                    release: Some(released),
                    interleaved,
                    ninth_saw_interleave,
                };
                let summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;
                (summary, journal.inner, scheduler.instrumentation())
            }
        });
        eighth_started.await.expect("eighth upload began");
        let competing = {
            let interleaved = Arc::clone(&interleaved);
            tokio::spawn(async move {
                tokio::task::yield_now().await;
                interleaved.store(true, Ordering::SeqCst);
            })
        };
        release.send(()).expect("release eighth upload");
        let (summary, journal, instrumentation) = task.await.expect("join sweep");
        competing.await.expect("join competing task");

        assert_eq!(summary.attempted, 18);
        assert_eq!(instrumentation.candidate_scans, 1);
        assert_eq!(journal.uploads().len(), 18);
        assert!(
            ninth_saw_interleave.load(Ordering::SeqCst),
            "the competing task must run at the bounded batch boundary"
        );
    });
}

#[test]
fn cached_retained_content_is_not_rehashed_or_reuploaded() {
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
        assert!(journal.uploads().is_empty());
        assert_eq!(
            journal.listings_by_day(),
            HashMap::from([
                ("20260701".to_owned(), 1),
                ("20260702".to_owned(), 1),
                ("20260703".to_owned(), 1),
            ])
        );
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

        assert_eq!(journal.uploads(), ["120000_300"]);
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

        assert_eq!(journal.uploads(), ["120000_300"]);
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
            journal.uploads().is_empty(),
            "the retained subset remains proven"
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

        assert_eq!(journal.uploads(), ["120000_300"]);
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

        let summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;

        assert_eq!(journal.uploads(), ["120000_300"]);
        assert_eq!(summary.custodied, 0);
        assert_eq!(
            summary.diagnostic,
            Some(DiagnosticCode::LocalSegmentInvalid)
        );
        assert!(segment_path(&temporary, "20260701", "120000_300").is_dir());
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
            Err(SyncOperationError::EndPass(
                solstone_tmux::sync::SyncFailureClass::Timeout,
            )),
        );
        journal.upload_outcome(
            "120000_300",
            Err(SyncOperationError::EndPass(
                solstone_tmux::sync::SyncFailureClass::Timeout,
            )),
        );

        let summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;

        assert_eq!(journal.uploads(), ["120000_300"]);
        assert_eq!(summary.custodied, 0);
        assert_eq!(
            summary.failure,
            Some(solstone_tmux::sync::SyncFailureClass::Timeout)
        );
        assert!(segment_path(&temporary, "20260701", "120000_300").is_dir());
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
            0,
            clock(),
            SyncWake::default(),
        );

        let summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;

        assert!(
            journal.listings_by_day()["20260701"] >= 2,
            "the second bounded group must obtain fresh retention proof"
        );
        assert_eq!(journal.uploads(), ["120800_300"]);
        assert_eq!(summary.custodied, 8);
        assert!(!segment_path(&temporary, "20260701", "120000_300").exists());
        assert!(segment_path(&temporary, "20260701", "120800_300").is_dir());
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
        tokio::time::timeout(Duration::from_millis(50), wake.wait())
            .await
            .expect("finalization wake was not latched");
        journal.clear_calls();

        let summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;

        assert_eq!(summary.attempted, 2);
        assert_eq!(journal.uploads(), ["120100_300"]);
    });
}

#[test]
fn converged_retention_disabled_sweep_stays_idle() {
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

        scheduler.run_sweep(&mut journal, no_shutdown()).await;

        assert_eq!(*receiver.borrow(), SyncActivity::Idle);
        assert!(
            !receiver
                .has_changed()
                .expect("activity sender remains open"),
            "converged custody must not request syncing"
        );
        assert!(journal.uploads().is_empty());
    });
}

#[test]
fn one_pass_attempts_at_most_eight_candidates_sequentially() {
    run(async {
        let temporary = TestDirectory::new("sync-bounded-groups");
        for index in 0..9 {
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
        assert_eq!(summary.attempted, 9);
        assert_eq!(scheduler.instrumentation().candidate_scans, 1);
        assert_eq!(journal.uploads().len(), 9);
    });
}

#[test]
fn second_sweep_reuses_custody_without_uploads_across_passes() {
    run(async {
        let temporary = TestDirectory::new("sync-second-custody");
        for day in ["20260701", "20260702"] {
            create_segment(&temporary, day, "120000_300", b"fixture\n");
        }
        let mut scheduler = scheduler(&temporary, SyncWake::default());
        let mut journal = FakeJournal::default();
        scheduler.run_sweep(&mut journal, no_shutdown()).await;
        journal.clear_calls();
        let summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;
        assert_eq!(summary.custodied, 2);
        assert!(journal.uploads().is_empty());
        assert_eq!(journal.listings_by_day().len(), 2);
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

        scheduler.run_sweep(&mut journal, no_shutdown()).await;

        assert_eq!(journal.uploads(), ["120000_300"]);
        assert!(segment_path(&temporary, "20260701", "120000_300").is_dir());
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
fn failed_preupload_listing_falls_through_without_failure_or_diagnostic() {
    run(async {
        let temporary = TestDirectory::new("sync-preupload-failure");
        create_segment(&temporary, "20260701", "120000_300", b"fixture\n");
        let mut scheduler = scheduler(&temporary, SyncWake::default());
        let mut journal = FakeJournal::default();
        journal.list_outcome(
            "20260701",
            Err(SyncOperationError::EndPass(
                solstone_tmux::sync::SyncFailureClass::Timeout,
            )),
        );
        let summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;
        assert_eq!(summary.failure, None);
        assert_eq!(journal.uploads(), ["120000_300"]);
    });
}

#[test]
fn listings_per_day_do_not_exceed_uploads_plus_one() {
    run(async {
        let temporary = TestDirectory::new("sync-listing-bound");
        for segment in ["120000_300", "120100_300"] {
            create_segment(&temporary, "20260701", segment, b"fixture\n");
        }
        let mut scheduler = scheduler(&temporary, SyncWake::default());
        let mut journal = FakeJournal::default();
        scheduler.run_sweep(&mut journal, no_shutdown()).await;
        assert!(journal.listings_by_day()["20260701"] <= journal.uploads().len() + 1);
    });
}

#[test]
fn pending_segments_drains_monotonically_when_custody_is_proven() {
    run(async {
        let temporary = TestDirectory::new("sync-pending");
        for index in 0..9 {
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
        assert_eq!(summary.custodied, 9);
    });
}

#[test]
fn collision_renamed_remote_segment_skips_upload() {
    run(async {
        let temporary = TestDirectory::new("sync-original-key");
        create_segment(&temporary, "20260701", "120000_300", b"fixture\n");
        let local = inventory_files(
            vec![segment_path(&temporary, "20260701", "120000_300").join(FILE)],
            None,
        )
        .await
        .expect("inventory fixture");
        let mut journal = FakeJournal::default();
        journal.list_outcome(
            "20260701",
            Ok(listing_with_original_key("120000_301", "120000_300", local)),
        );
        let mut scheduler = scheduler(&temporary, SyncWake::default());
        let summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;
        assert_eq!(summary.custodied, 1);
        assert!(journal.uploads().is_empty());
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
fn listing_loss_self_corrects_by_reuploading_only_the_missing_segment() {
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
        scheduler.run_sweep(&mut journal, no_shutdown()).await;
        assert_eq!(journal.uploads(), ["120100_300"]);
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
        let summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;
        assert_eq!(
            summary.diagnostic,
            Some(DiagnosticCode::LocalSegmentInvalid)
        );
        assert_eq!(summary.custodied, 0);
    });
}

#[test]
fn conflict_and_failed_contacts_do_not_claim_successful_custody() {
    run(async {
        let temporary = TestDirectory::new("sync-conflict");
        create_segment(&temporary, "20260701", "120000_300", b"fixture\n");
        let mut scheduler = scheduler(&temporary, SyncWake::default());
        let mut journal = FakeJournal::default();
        journal.upload_outcome(
            "120000_300",
            Ok(UploadResult {
                status: UploadStatus::Conflict,
                authoritative_key: None,
            }),
        );
        let summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;
        assert_eq!(summary.custodied, 0);
        assert_eq!(
            summary.diagnostic,
            Some(DiagnosticCode::LocalSegmentInvalid)
        );
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
        let summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;
        assert_eq!(
            summary.diagnostic,
            Some(DiagnosticCode::LocalSegmentInvalid)
        );
        assert!(segment_path(&temporary, "20260701", "120000_300").is_dir());
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
        let summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;
        assert_eq!(summary.attempted, 2);
        assert!(journal.uploads().contains(&"120000_300".to_owned()));
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
            Err(SyncOperationError::EndPass(
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
fn shutdown_cancels_a_pending_registration_before_scanning_or_uploading() {
    run(async {
        let temporary = TestDirectory::new("sync-cancel-registration");
        for index in 0..10 {
            create_segment(
                &temporary,
                "20260701",
                &format!("12{index:02}00_300"),
                b"fixture\n",
            );
        }
        let (entered, started) = oneshot::channel();
        let (mut journal, uploads) = blocking_journal(BlockingStage::Registration, entered);
        let (stop, shutdown) = watch::channel(false);
        let mut scheduler = scheduler(&temporary, SyncWake::default());
        let task = tokio::spawn(async move { scheduler.run_sweep(&mut journal, shutdown).await });
        started.await.expect("registration began");
        stop.send_replace(true);

        let summary = tokio::time::timeout(Duration::from_millis(100), task)
            .await
            .expect("shutdown must interrupt registration")
            .expect("join sweep");
        assert!(summary.cancelled);
        assert_eq!(summary.attempted, 0);
        assert!(uploads.lock().expect("uploads lock").is_empty());
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

        let summary = tokio::time::timeout(Duration::from_millis(100), task)
            .await
            .expect("shutdown must interrupt empty listing")
            .expect("join sweep");
        assert!(summary.cancelled);
        assert_eq!(summary.attempted, 0);
        assert!(uploads.lock().expect("uploads lock").is_empty());
    });
}

#[test]
fn shutdown_cancels_a_pending_preupload_listing_without_starting_later_candidates() {
    run(async {
        let temporary = TestDirectory::new("sync-cancel-preupload-listing");
        for index in 0..10 {
            create_segment(
                &temporary,
                "20260701",
                &format!("12{index:02}00_300"),
                b"fixture\n",
            );
        }
        let (entered, started) = oneshot::channel();
        let (mut journal, uploads) = blocking_journal(BlockingStage::PreUploadListing, entered);
        let (stop, shutdown) = watch::channel(false);
        let mut scheduler = scheduler(&temporary, SyncWake::default());
        let task = tokio::spawn(async move { scheduler.run_sweep(&mut journal, shutdown).await });
        started.await.expect("pre-upload listing began");
        stop.send_replace(true);

        let summary = tokio::time::timeout(Duration::from_millis(100), task)
            .await
            .expect("shutdown must interrupt pre-upload listing")
            .expect("join sweep");
        assert!(summary.cancelled);
        assert_eq!(summary.attempted, 1);
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

        let summary = tokio::time::timeout(Duration::from_millis(100), task)
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

        let summary = tokio::time::timeout(Duration::from_millis(100), task)
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
fn shutdown_cancels_a_pending_status_event() {
    run(async {
        let temporary = TestDirectory::new("sync-cancel-status-event");
        let (entered, started) = oneshot::channel();
        let (mut journal, uploads) = blocking_journal(BlockingStage::StatusEvent, entered);
        let (stop, stopped) = oneshot::channel();
        let mut scheduler = scheduler(&temporary, SyncWake::default());
        let task = tokio::spawn(async move {
            scheduler
                .run(&mut journal, async move {
                    let _ = stopped.await;
                })
                .await;
        });
        started.await.expect("status event began");
        stop.send(()).expect("request shutdown");

        tokio::time::timeout(Duration::from_millis(100), task)
            .await
            .expect("shutdown must interrupt status event")
            .expect("join scheduler");
        assert!(uploads.lock().expect("uploads lock").is_empty());
    });
}

#[test]
fn startup_finalization_and_periodic_wakes_converge_on_a_rescan() {
    run(async {
        let temporary = TestDirectory::new("sync-wake-sources");
        create_segment(&temporary, "20260701", "120000_300", b"fixture\n");
        let wake = SyncWake::default();
        let mut scheduler = scheduler(&temporary, wake.clone());
        let mut journal = FakeJournal::default();
        scheduler.run_sweep(&mut journal, no_shutdown()).await;
        let startup_scans = scheduler.instrumentation().candidate_scans;
        wake.segment_closed(&SegmentClose::Finalized(PathBuf::from("wake")));
        wake.wait().await;
        scheduler.run_sweep(&mut journal, no_shutdown()).await;
        scheduler.run_sweep(&mut journal, no_shutdown()).await;
        assert_eq!(
            scheduler.instrumentation().candidate_scans,
            startup_scans + 2
        );
    });
}

#[test]
fn one_backoff_owner_advances_holds_resets_and_never_stops_capture() {
    run(async {
        let temporary = TestDirectory::new("sync-backoff-error");
        create_segment(&temporary, "20260701", "120000_300", b"fixture\n");
        let mut scheduler = scheduler(&temporary, SyncWake::default());
        let mut journal = FakeJournal::default();
        journal.upload_outcome(
            "120000_300",
            Err(SyncOperationError::EndPass(
                solstone_tmux::sync::SyncFailureClass::Timeout,
            )),
        );
        let failed = scheduler.run_sweep(&mut journal, no_shutdown()).await;
        assert_eq!(
            failed.failure,
            Some(solstone_tmux::sync::SyncFailureClass::Timeout)
        );
        let succeeded = scheduler.run_sweep(&mut journal, no_shutdown()).await;
        assert_eq!(succeeded.custodied, 1);
    });
}

#[test]
fn reused_custody_counts_as_successful_sync() {
    run(async {
        let temporary = TestDirectory::new("sync-reused-custody");
        create_segment(&temporary, "20260701", "120000_300", b"fixture\n");
        let mut scheduler = scheduler(&temporary, SyncWake::default());
        let mut journal = FakeJournal::default();
        scheduler.run_sweep(&mut journal, no_shutdown()).await;
        journal.clear_calls();
        let summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;
        assert_eq!(summary.custodied, 1);
        assert!(summary.contacted);
        assert!(journal.uploads().is_empty());
    });
}

#[test]
fn a_retained_candidate_keeps_operator_visible_error_truth() {
    run(async {
        let temporary = TestDirectory::new("sync-retained-truth");
        create_segment(&temporary, "20260701", "120000_300", b"fixture\n");
        let mut scheduler = scheduler(&temporary, SyncWake::default());
        let mut journal = FakeJournal::default();
        journal.upload_outcome(
            "120000_300",
            Ok(UploadResult {
                status: UploadStatus::Failed,
                authoritative_key: None,
            }),
        );
        let summary = scheduler.run_sweep(&mut journal, no_shutdown()).await;
        assert_eq!(
            summary.diagnostic,
            Some(DiagnosticCode::LocalSegmentInvalid)
        );
        assert!(summary.contacted);
        assert_eq!(summary.custodied, 0);
    });
}

#[test]
fn health_distinguishes_contact_from_custody_and_decrements_deleted_work() {
    run(async {
        let temporary = TestDirectory::new("sync-contact-custody");
        create_segment(&temporary, "20260701", "120000_300", b"fixture\n");
        let mut custody_scheduler = scheduler(&temporary, SyncWake::default());
        let mut journal = FakeJournal::default();
        let first = custody_scheduler
            .run_sweep(&mut journal, no_shutdown())
            .await;
        assert!(first.contacted);
        assert_eq!(first.custodied, 1);
        let empty = TestDirectory::new("sync-empty-contact");
        let mut empty_scheduler = scheduler(&empty, SyncWake::default());
        let empty_summary = empty_scheduler.run_sweep(&mut journal, no_shutdown()).await;
        assert!(empty_summary.contacted);
        assert_eq!(empty_summary.custodied, 0);
    });
}

#[test]
fn status_event_is_bounded_to_one_per_heartbeat_interval() {
    // Status-event cadence is exercised through the scheduler run loop, not a direct sweep.
    run(async {
        let temporary = TestDirectory::new("sync-status-cadence");
        let mut scheduler = scheduler(&temporary, SyncWake::default());
        let mut journal = StatusJournal { events: 0 };
        let (stop, stopped) = oneshot::channel();
        let task = tokio::spawn(async move {
            scheduler
                .run(&mut journal, async move {
                    let _ = stopped.await;
                })
                .await;
            journal.events
        });
        tokio::task::yield_now().await;
        stop.send(()).expect("stop scheduler");
        assert!(task.await.expect("join scheduler") <= 1);
    });
}

#[test]
fn poison_segment_cursor_reaches_a_later_valid_candidate_after_backoff() {
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

fn no_shutdown() -> tokio::sync::watch::Receiver<bool> {
    let (sender, receiver) = tokio::sync::watch::channel(false);
    std::mem::forget(sender);
    receiver
}

fn scheduler(temporary: &TestDirectory, wake: SyncWake) -> SyncScheduler {
    SyncScheduler::new(
        temporary.path().join("captures"),
        stream(),
        -1,
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

#[derive(Default)]
struct FakeJournal {
    calls: Vec<Call>,
    remote: HashMap<String, HashMap<String, Vec<LocalFile>>>,
    list_outcomes: HashMap<String, VecDeque<Result<SegmentsEnvelope, SyncOperationError>>>,
    upload_outcomes: HashMap<String, VecDeque<Result<UploadResult, SyncOperationError>>>,
}

#[derive(Clone)]
enum Call {
    Upload(String),
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
    }

    fn uploads(&self) -> Vec<String> {
        self.calls
            .iter()
            .filter_map(|call| match call {
                Call::Upload(segment) => Some(segment.clone()),
                Call::Listing(_) => None,
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
        candidate: &'a SegmentCandidate,
        files: Vec<PathBuf>,
    ) -> Pin<Box<dyn Future<Output = Result<UploadResult, SyncOperationError>> + Send + 'a>> {
        Box::pin(async move {
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
            self.remote
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
            self.calls.push(Call::Listing(day.to_owned()));
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

fn listing_with_original_key(
    key: &str,
    original_key: &str,
    files: Vec<LocalFile>,
) -> SegmentsEnvelope {
    SegmentsEnvelope {
        total: 1,
        items: vec![SegmentItem {
            key: key.to_owned(),
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
            original_key: Some(original_key.to_owned()),
        }],
        protocol_version: 2,
    }
}

struct StatusJournal {
    events: usize,
}

impl SyncJournal for StatusJournal {
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
        Box::pin(async { unreachable!("empty status fixture has no upload") })
    }

    fn segments<'a>(
        &'a mut self,
        _day: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<SegmentsEnvelope, SyncOperationError>> + Send + 'a>>
    {
        Box::pin(async {
            Ok(SegmentsEnvelope {
                items: Vec::new(),
                total: 0,
                protocol_version: 2,
            })
        })
    }

    fn status_event<'a>(
        &'a mut self,
        _beacon: &'a StatusBeacon,
    ) -> Pin<Box<dyn Future<Output = Result<(), SyncOperationError>> + Send + 'a>> {
        Box::pin(async move {
            self.events += 1;
            Ok(())
        })
    }
}

struct GatedJournal {
    inner: FakeJournal,
    entered: Option<oneshot::Sender<()>>,
    release: Option<oneshot::Receiver<()>>,
}

impl SyncJournal for GatedJournal {
    fn observer_name(&self) -> Option<&str> {
        self.inner.observer_name()
    }

    fn ensure_registered<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<bool, SyncOperationError>> + Send + 'a>> {
        self.inner.ensure_registered()
    }

    fn upload<'a>(
        &'a mut self,
        candidate: &'a SegmentCandidate,
        files: Vec<PathBuf>,
    ) -> Pin<Box<dyn Future<Output = Result<UploadResult, SyncOperationError>> + Send + 'a>> {
        Box::pin(async move {
            if let Some(entered) = self.entered.take() {
                let _ = entered.send(());
            }
            if let Some(release) = self.release.take() {
                let _ = release.await;
            }
            self.inner.upload(candidate, files).await
        })
    }

    fn segments<'a>(
        &'a mut self,
        day: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<SegmentsEnvelope, SyncOperationError>> + Send + 'a>>
    {
        self.inner.segments(day)
    }

    fn status_event<'a>(
        &'a mut self,
        beacon: &'a StatusBeacon,
    ) -> Pin<Box<dyn Future<Output = Result<(), SyncOperationError>> + Send + 'a>> {
        self.inner.status_event(beacon)
    }
}

struct InterleavingJournal {
    inner: FakeJournal,
    uploads: usize,
    eighth: Option<oneshot::Sender<()>>,
    release: Option<oneshot::Receiver<()>>,
    interleaved: Arc<AtomicBool>,
    ninth_saw_interleave: Arc<AtomicBool>,
}

impl SyncJournal for InterleavingJournal {
    fn observer_name(&self) -> Option<&str> {
        self.inner.observer_name()
    }

    fn ensure_registered<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<bool, SyncOperationError>> + Send + 'a>> {
        self.inner.ensure_registered()
    }

    fn upload<'a>(
        &'a mut self,
        candidate: &'a SegmentCandidate,
        files: Vec<PathBuf>,
    ) -> Pin<Box<dyn Future<Output = Result<UploadResult, SyncOperationError>> + Send + 'a>> {
        Box::pin(async move {
            self.uploads += 1;
            if self.uploads == 8 {
                if let Some(eighth) = self.eighth.take() {
                    let _ = eighth.send(());
                }
                if let Some(release) = self.release.take() {
                    let _ = release.await;
                }
            }
            if self.uploads == 9 {
                self.ninth_saw_interleave
                    .store(self.interleaved.load(Ordering::SeqCst), Ordering::SeqCst);
            }
            self.inner.upload(candidate, files).await
        })
    }

    fn segments<'a>(
        &'a mut self,
        day: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<SegmentsEnvelope, SyncOperationError>> + Send + 'a>>
    {
        self.inner.segments(day)
    }

    fn status_event<'a>(
        &'a mut self,
        beacon: &'a StatusBeacon,
    ) -> Pin<Box<dyn Future<Output = Result<(), SyncOperationError>> + Send + 'a>> {
        self.inner.status_event(beacon)
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum BlockingStage {
    Registration,
    EmptyListing,
    PreUploadListing,
    Upload,
    PostUploadListing,
    StatusEvent,
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
            BlockingStage::EmptyListing | BlockingStage::PreUploadListing => {
                self.segment_calls == 1
            }
            BlockingStage::PostUploadListing => self.segment_calls == 2,
            _ => false,
        }
    }
}

impl SyncJournal for BlockingJournal {
    fn observer_name(&self) -> Option<&str> {
        (self.stage == BlockingStage::StatusEvent).then_some("observer")
    }

    fn ensure_registered<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<bool, SyncOperationError>> + Send + 'a>> {
        Box::pin(async move {
            if self.stage == BlockingStage::Registration {
                self.wait_forever().await;
            }
            Ok(false)
        })
    }

    fn upload<'a>(
        &'a mut self,
        candidate: &'a SegmentCandidate,
        _files: Vec<PathBuf>,
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
                protocol_version: 2,
            })
        })
    }

    fn status_event<'a>(
        &'a mut self,
        _beacon: &'a StatusBeacon,
    ) -> Pin<Box<dyn Future<Output = Result<(), SyncOperationError>> + Send + 'a>> {
        Box::pin(async move {
            if self.stage == BlockingStage::StatusEvent {
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
