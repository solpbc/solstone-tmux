// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

mod support;

use std::fs::{self, OpenOptions};
use std::future::Future;
use std::io::Write;
use std::os::unix::fs::{FileTypeExt, MetadataExt, symlink};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sha2::{Digest, Sha256};
use solstone_tmux::journal::inventory_files;
use solstone_tmux::observer::{
    LifecycleLock, ObserverExit, ObserverOperationError, ShutdownEvent, ShutdownIndicator,
    SupervisionControl, shutdown_barrier, supervise_observer,
};
use solstone_tmux::sync::{
    FileIdentity, RetentionFence, SegmentCandidate, SyncActivity, delete_confirmed_segment,
};
use support::TestDirectory;

const STREAM: &str = "host.tmux";
const SEGMENT: &str = "120000_300";
const FILE: &str = "tmux_main_screen.jsonl";

type DeleteHook = Arc<dyn Fn(usize) + Send + Sync>;

/// Runs the production removal path: the captures tree under `data_root`,
/// its sync ledger beside it, and no removal observer.
async fn delete_confirmed(
    data_root: &Path,
    candidate: &SegmentCandidate,
    expected_digests: &[(&str, &str)],
    expected_identities: &[FileIdentity],
    delete_hook: Option<DeleteHook>,
    fence: Arc<RetentionFence>,
) -> bool {
    delete_confirmed_segment(
        &data_root.join("captures"),
        &data_root.join("sync-ledger"),
        candidate,
        expected_digests,
        expected_identities,
        delete_hook,
        None,
        fence,
    )
    .await
}

/// Writes a stand-in acknowledgment where the removal path keeps it, so a test
/// can see whether a removal also cleared the ledger entry.
fn write_ledger_ack(data_root: &Path, candidate: &SegmentCandidate) -> PathBuf {
    let path = data_root
        .join("sync-ledger")
        .join(candidate.day())
        .join(candidate.stream())
        .join(candidate.segment())
        .join("ack.json");
    fs::create_dir_all(path.parent().expect("ledger parent")).expect("create ledger entry");
    fs::write(&path, b"{}").expect("write ledger acknowledgment");
    path
}

#[test]
fn traversal_candidate_cannot_escape_its_stream() {
    run(async {
        let temporary = TestDirectory::new("retention-traversal");
        let outside = create_segment(
            temporary.path(),
            "captures/20260701",
            "outside",
            STREAM,
            SEGMENT,
        );
        let candidate = SegmentCandidate::new("20260701", STREAM, "../outside");
        let before = snapshot_segment_entries(&outside);
        let (digests, identities) = expected_for(&outside).await;
        let digests_ref: Vec<(&str, &str)> = digests
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();

        let removed = delete_confirmed(
            temporary.path(),
            &candidate,
            &digests_ref,
            &identities,
            None,
            Arc::new(RetentionFence::new()),
        )
        .await;

        assert!(!removed);
        assert_segment_entries_unchanged(&outside, &before);
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
        let symlink_path = symlink_segment.join(FILE);
        symlink(&referent, &symlink_path).expect("create segment symlink");
        let symlink_target = fs::read_link(&symlink_path).expect("read symlink target");
        fs::write(
            symlink_segment.join("tmux_aux_screen.jsonl"),
            b"symlink sidecar\n",
        )
        .expect("write symlink sidecar");

        let socket_segment = segment_dir(&captures, "20260701", STREAM, "120500_300");
        fs::create_dir_all(&socket_segment).expect("create socket segment");
        let socket_path = socket_segment.join(FILE);
        let socket_source = temporary.path().join("s");
        let _listener = UnixListener::bind(&socket_source).expect("bind fixture socket");
        fs::rename(socket_source, &socket_path).expect("move socket into segment");
        fs::write(
            socket_segment.join("tmux_aux_screen.jsonl"),
            b"socket sidecar\n",
        )
        .expect("write socket sidecar");
        let symlink_before = snapshot_segment_entries(&symlink_segment);
        let socket_before = snapshot_segment_entries(&socket_segment);

        for (segment_name, _segment) in [
            ("120000_300", &symlink_segment),
            ("120500_300", &socket_segment),
        ] {
            let candidate = SegmentCandidate::new("20260701", STREAM, segment_name);
            let ack = write_ledger_ack(temporary.path(), &candidate);
            assert!(
                !delete_confirmed(
                    temporary.path(),
                    &candidate,
                    &[],
                    &[],
                    None,
                    Arc::new(RetentionFence::new()),
                )
                .await
            );
            assert!(ack.is_file(), "a retained segment keeps its acknowledgment");
        }
        assert_segment_entries_unchanged(&symlink_segment, &symlink_before);
        assert_segment_entries_unchanged(&socket_segment, &socket_before);
        assert!(
            fs::symlink_metadata(&symlink_path)
                .expect("inspect retained symlink")
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            fs::read_link(&symlink_path).expect("read retained symlink target"),
            symlink_target
        );
        assert!(
            fs::symlink_metadata(&socket_path)
                .expect("inspect retained socket")
                .file_type()
                .is_socket()
        );
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
        let (digests, identities) = expected_for(&finalized).await;
        let digests_ref: Vec<(&str, &str)> = digests
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();

        let ack = write_ledger_ack(temporary.path(), &candidate);

        assert!(
            delete_confirmed(
                temporary.path(),
                &candidate,
                &digests_ref,
                &identities,
                None,
                Arc::new(RetentionFence::new()),
            )
            .await
        );
        assert!(!finalized.exists());
        assert!(!ack.exists(), "a removed segment takes its acknowledgment");
        assert!(
            !ack.parent().expect("ledger entry").exists(),
            "a removed segment takes its ledger entry"
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
        let segment = create_segment(temporary.path(), "captures", "20260701", STREAM, SEGMENT);
        let candidate = SegmentCandidate::new("20260701", STREAM, SEGMENT);
        let (digests, identities) = expected_for(&segment).await;
        let digests_ref: Vec<(&str, &str)> = digests
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let target = segment.join(FILE);
        let hook_target = target.clone();
        let hook = Arc::new(move |index| {
            if index == 0 {
                let incoming = hook_target.with_file_name(".incoming");
                fs::write(&incoming, b"replacement\n").expect("write replacement");
                fs::rename(incoming, &hook_target).expect("install replacement");
            }
        });

        let ack = write_ledger_ack(temporary.path(), &candidate);

        let outcome = delete_confirmed(
            temporary.path(),
            &candidate,
            &digests_ref,
            &identities,
            Some(hook),
            Arc::new(RetentionFence::new()),
        )
        .await;

        assert!(!outcome);
        assert!(ack.is_file(), "a retained segment keeps its acknowledgment");
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
        let segment = create_segment(temporary.path(), "captures", "20260701", STREAM, SEGMENT);
        let auxiliary = segment.join("tmux_aux_screen.jsonl");
        fs::write(&auxiliary, b"auxiliary fixture\n").expect("write auxiliary fixture");
        let candidate = SegmentCandidate::new("20260701", STREAM, SEGMENT);
        let (digests, identities) = expected_for(&segment).await;
        let digests_ref: Vec<(&str, &str)> = digests
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let main = segment.join(FILE);
        let hook_main = main.clone();
        let hook = Arc::new(move |index| {
            if index == 1 {
                let incoming = hook_main.with_file_name(".incoming");
                fs::write(&incoming, b"replacement\n").expect("write replacement");
                fs::rename(incoming, &hook_main).expect("install replacement");
            }
        });

        let ack = write_ledger_ack(temporary.path(), &candidate);

        let outcome = delete_confirmed(
            temporary.path(),
            &candidate,
            &digests_ref,
            &identities,
            Some(hook),
            Arc::new(RetentionFence::new()),
        )
        .await;

        assert!(!outcome);
        assert!(ack.is_file(), "a retained segment keeps its acknowledgment");
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
        let segment = create_segment(temporary.path(), "captures", "20260701", STREAM, SEGMENT);
        let candidate = SegmentCandidate::new("20260701", STREAM, SEGMENT);
        let (digests, identities) = expected_for(&segment).await;
        let digests_ref: Vec<(&str, &str)> = digests
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
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

        let ack = write_ledger_ack(temporary.path(), &candidate);

        let outcome = delete_confirmed(
            temporary.path(),
            &candidate,
            &digests_ref,
            &identities,
            Some(hook),
            Arc::new(RetentionFence::new()),
        )
        .await;

        assert!(!outcome);
        assert!(ack.is_file(), "a retained segment keeps its acknowledgment");
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
        let segment = create_segment(temporary.path(), "captures", "20260701", STREAM, SEGMENT);
        fs::write(
            segment.join("tmux_aux_screen.jsonl"),
            b"auxiliary fixture\n",
        )
        .expect("write auxiliary fixture");
        let candidate = SegmentCandidate::new("20260701", STREAM, SEGMENT);
        let (digests, identities) = expected_for(&segment).await;
        let digests_ref: Vec<(&str, &str)> = digests
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
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

        let ack = write_ledger_ack(temporary.path(), &candidate);

        let outcome = delete_confirmed(
            temporary.path(),
            &candidate,
            &digests_ref,
            &identities,
            Some(hook),
            Arc::new(RetentionFence::new()),
        )
        .await;

        assert!(!outcome);
        assert!(ack.is_file(), "a retained segment keeps its acknowledgment");
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
    let data_root = temporary.path().to_path_buf();
    let segment = create_segment(temporary.path(), "captures", "20260701", STREAM, SEGMENT);
    let candidate = SegmentCandidate::new("20260701", STREAM, SEGMENT);
    let (digests, identities) = expected_for(&segment).await;
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
        let digests_ref: Vec<(&str, &str)> = digests
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let _ = delete_confirmed(
            &data_root,
            &candidate,
            &digests_ref,
            &identities,
            Some(hook),
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

fn create_segment(root: &Path, captures: &str, day: &str, stream: &str, segment: &str) -> PathBuf {
    let path = root.join(captures).join(day).join(stream).join(segment);
    fs::create_dir_all(&path).expect("create segment");
    fs::write(path.join(FILE), b"capture fixture\n").expect("write segment file");
    path
}

fn segment_dir(captures: &Path, day: &str, stream: &str, segment: &str) -> PathBuf {
    captures.join(day).join(stream).join(segment)
}

#[derive(Debug, Eq, PartialEq)]
enum SegmentEntry {
    Directory,
    File(Vec<u8>),
    Symlink(PathBuf),
    Socket,
    BlockDevice,
    CharacterDevice,
    Fifo,
}

fn snapshot_segment_entries(segment: &Path) -> std::collections::BTreeMap<PathBuf, SegmentEntry> {
    let mut entries = std::collections::BTreeMap::new();
    collect_segment_entries(segment, segment, &mut entries);
    entries
}

fn collect_segment_entries(
    segment: &Path,
    directory: &Path,
    entries: &mut std::collections::BTreeMap<PathBuf, SegmentEntry>,
) {
    for entry in fs::read_dir(directory).expect("read segment directory") {
        let entry = entry.expect("read segment entry");
        let path = entry.path();
        let relative = path
            .strip_prefix(segment)
            .expect("entry below segment")
            .to_owned();
        let file_type = fs::symlink_metadata(&path)
            .expect("inspect segment entry")
            .file_type();
        let snapshot = if file_type.is_dir() {
            collect_segment_entries(segment, &path, entries);
            SegmentEntry::Directory
        } else if file_type.is_file() {
            SegmentEntry::File(fs::read(&path).expect("read segment file"))
        } else if file_type.is_symlink() {
            SegmentEntry::Symlink(fs::read_link(&path).expect("read segment symlink"))
        } else if file_type.is_socket() {
            SegmentEntry::Socket
        } else if file_type.is_block_device() {
            SegmentEntry::BlockDevice
        } else if file_type.is_char_device() {
            SegmentEntry::CharacterDevice
        } else if file_type.is_fifo() {
            SegmentEntry::Fifo
        } else {
            panic!("unsupported segment entry type");
        };
        assert!(
            entries.insert(relative, snapshot).is_none(),
            "duplicate segment entry"
        );
    }
}

fn assert_segment_entries_unchanged(
    segment: &Path,
    before: &std::collections::BTreeMap<PathBuf, SegmentEntry>,
) {
    assert_eq!(
        snapshot_segment_entries(segment),
        *before,
        "segment entries changed"
    );
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

async fn expected_for(segment: &Path) -> (Vec<(String, String)>, Vec<FileIdentity>) {
    let mut paths = fs::read_dir(segment)
        .expect("read fixture segment")
        .map(|entry| entry.expect("read fixture entry").path())
        .collect::<Vec<_>>();
    paths.sort();
    let local = inventory_files(paths.clone(), None)
        .await
        .expect("inventory fixture");
    let digests: Vec<(String, String)> = local
        .into_iter()
        .map(|file| (file.name, file.sha256))
        .collect();
    let identities = paths
        .iter()
        .map(|path| {
            let file = solstone_tmux::storage::open_regular_readonly(path).expect("open fixture");
            let meta = file.metadata().expect("meta");
            FileIdentity {
                name: path.file_name().unwrap().to_str().unwrap().to_owned(),
                device: meta.dev(),
                inode: meta.ino(),
                size: meta.len(),
                mtime: meta.mtime(),
                mtime_nsec: meta.mtime_nsec(),
                ctime: meta.ctime(),
                ctime_nsec: meta.ctime_nsec(),
            }
        })
        .collect();
    (digests, identities)
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
