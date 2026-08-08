// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

mod support;

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use solstone_tmux::clock::{Clock, TestClock};
use solstone_tmux::health::DiagnosticCode;
use solstone_tmux::model::CaptureResult;
use solstone_tmux::name::derive_component;
use solstone_tmux::observer::{
    CaptureProvider, LifecycleLock, ObserverConfig, ObserverExit, ObserverOperationError,
    SegmentManager, ShutdownEvent, ShutdownIndicator, SupervisionControl, run_observer,
    shutdown_barrier, stream_directory, supervise_observer,
};
use solstone_tmux::paths::ensure_private_directory;
use solstone_tmux::segment::SegmentState;
use solstone_tmux::sync::{RetentionFence, SyncActivity, SyncWake};
use support::{TestDirectory, golden_capture};
use time::{Date, Month, PrimitiveDateTime, Time, UtcOffset};

#[test]
fn early_successful_sync_exit_is_a_process_failure() {
    let exit = runtime().block_on(supervise_fixture(async { Ok(()) }));

    assert_eq!(exit.exit_code, 1);
    assert!(
        exit.failures
            .iter()
            .any(|failure| failure == "sync task exited unexpectedly")
    );
    assert_eq!(exit.shutdown_event, Some(ShutdownEvent::Injected));
}

#[test]
fn sync_panic_is_a_process_failure_and_finalizes_observer() {
    let exit = runtime().block_on(supervise_fixture(async {
        panic!("sync fixture panic");
    }));

    assert_eq!(exit.exit_code, 1);
    assert_eq!(exit.failures, ["sync task failed: panic"]);
    assert_eq!(exit.shutdown_event, Some(ShutdownEvent::Injected));
}

#[test]
fn authorized_shutdown_joins_sync_before_indicator_and_lock_cleanup() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let indicator = RecordingIndicator(Arc::clone(&log));
    let lock = RecordingLock(Arc::clone(&log));
    let (_activity, activity_receiver) = tokio::sync::watch::channel(SyncActivity::Idle);
    let (sync_stop, mut sync_shutdown) = tokio::sync::watch::channel(false);
    let (observer_stop, _observer_shutdown) =
        tokio::sync::watch::channel::<Option<ShutdownEvent>>(None);
    let sync_log = Arc::clone(&log);
    let sync = async move {
        wait_for_stop(&mut sync_shutdown).await;
        sync_log.lock().expect("log poisoned").push("sync");
        Ok(())
    };
    let observer = async {
        ObserverExit {
            exit_code: 0,
            shutdown_event: Some(ShutdownEvent::SigTerm),
            failures: Vec::new(),
        }
    };
    let (observer_barrier, supervisor_barrier) = shutdown_barrier();
    drop(observer_barrier);

    let exit = runtime().block_on(supervise_observer(
        observer,
        sync,
        Box::new(indicator),
        Box::new(lock),
        SupervisionControl {
            activity: activity_receiver,
            sync_stop,
            observer_stop,
            shutdown_barrier: supervisor_barrier,
            retention_fence: Arc::new(RetentionFence::new()),
        },
    ));

    assert_eq!(exit.exit_code, 0);
    assert_eq!(
        *log.lock().expect("log poisoned"),
        ["sync", "indicator", "lock"]
    );
}

#[tokio::test(start_paused = true)]
async fn uncooperative_sync_times_out_before_indicator_and_lock_cleanup() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let (_activity, activity_receiver) = tokio::sync::watch::channel(SyncActivity::Idle);
    let (sync_stop, _sync_shutdown) = tokio::sync::watch::channel(false);
    let (observer_stop, _observer_shutdown) =
        tokio::sync::watch::channel::<Option<ShutdownEvent>>(None);
    let (observer_barrier, supervisor_barrier) = shutdown_barrier();
    drop(observer_barrier);
    let observer = async {
        ObserverExit {
            exit_code: 0,
            shutdown_event: Some(ShutdownEvent::SigTerm),
            failures: Vec::new(),
        }
    };
    let task = tokio::spawn(supervise_observer(
        observer,
        std::future::pending::<Result<(), DiagnosticCode>>(),
        Box::new(RecordingIndicator(Arc::clone(&log))),
        Box::new(RecordingLock(Arc::clone(&log))),
        SupervisionControl {
            activity: activity_receiver,
            sync_stop,
            observer_stop,
            shutdown_barrier: supervisor_barrier,
            retention_fence: Arc::new(RetentionFence::new()),
        },
    ));

    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(15)).await;
    let exit = task.await.expect("join supervisor");

    assert_eq!(exit.exit_code, 1);
    assert_eq!(
        exit.failures,
        [DiagnosticCode::SyncTaskTimedOut.message().to_owned()]
    );
    assert_eq!(*log.lock().expect("log poisoned"), ["indicator", "lock"]);
}

#[test]
fn activity_borrow_is_released_before_indicator_update_awaits() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_time()
        .build()
        .expect("test runtime");
    let (activity_sender, activity_receiver) = tokio::sync::watch::channel(SyncActivity::Idle);
    activity_sender.send_replace(SyncActivity::Working);
    let (indicator_entered_sender, indicator_entered_receiver) = mpsc::channel();
    let (indicator_release_sender, indicator_release_receiver) = tokio::sync::oneshot::channel();
    let (observer_finish_sender, observer_finish_receiver) = tokio::sync::oneshot::channel();
    let sender_thread = std::thread::spawn(move || {
        indicator_entered_receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("indicator update started");
        activity_sender.send_replace(SyncActivity::Idle);
        let _ = indicator_release_sender.send(());
        let _ = observer_finish_sender.send(());
    });
    let indicator = BlockingActivityIndicator {
        entered: Some(indicator_entered_sender),
        release: Some(indicator_release_receiver),
    };
    let (sync_stop, mut sync_shutdown) = tokio::sync::watch::channel(false);
    let sync = async move {
        wait_for_stop(&mut sync_shutdown).await;
        Ok(())
    };
    let observer = async move {
        let _ = observer_finish_receiver.await;
        ObserverExit {
            exit_code: 0,
            shutdown_event: Some(ShutdownEvent::SigTerm),
            failures: Vec::new(),
        }
    };
    let (observer_stop, _observer_shutdown) =
        tokio::sync::watch::channel::<Option<ShutdownEvent>>(None);
    let (observer_barrier, supervisor_barrier) = shutdown_barrier();
    drop(observer_barrier);

    let result = runtime.block_on(async {
        tokio::time::timeout(
            Duration::from_secs(5),
            supervise_observer(
                observer,
                sync,
                Box::new(indicator),
                Box::new(RecordingLock::default()),
                SupervisionControl {
                    activity: activity_receiver,
                    sync_stop,
                    observer_stop,
                    shutdown_barrier: supervisor_barrier,
                    retention_fence: Arc::new(RetentionFence::new()),
                },
            ),
        )
        .await
    });
    sender_thread.join().expect("activity sender thread");

    assert_eq!(result.expect("supervision must not deadlock").exit_code, 0);
}

#[test]
fn shutdown_keeps_the_final_segment_for_a_later_scan() {
    let temporary = TestDirectory::new("supervision-final-segment");
    let data_root = temporary.path().join("data");
    ensure_private_directory(&data_root).expect("data root");
    let lock =
        solstone_tmux::instance_lock::InstanceLock::acquire(&data_root).expect("instance lock");
    let clock = test_clock();
    let stream = derive_component("host.tmux").expect("stream");
    let stream_dir = stream_directory(&data_root, &stream, clock.wall_now(), clock.local_offset())
        .expect("stream directory");
    let mut segment = SegmentState::create(
        &stream_dir,
        clock.wall_now(),
        Duration::ZERO,
        clock.local_offset(),
    )
    .expect("segment");
    segment
        .append_capture(&golden_capture("main"), 0.25, Duration::from_secs(1))
        .expect("append capture");
    clock.set_monotonic(Duration::from_secs(5));
    let finalized = stream_dir.join("120000_005");
    let wake = SyncWake::default();
    let manager = SegmentManager::new(
        segment,
        data_root.clone(),
        stream,
        clock.local_offset(),
        wake.clone(),
    );
    let (observer_barrier, supervisor_barrier) = shutdown_barrier();
    let observer = run_observer(
        Arc::new(NoCaptures),
        Box::new(manager),
        Arc::new(clock) as Arc<dyn Clock>,
        Box::pin(async { ShutdownEvent::Injected }),
        observer_barrier,
        ObserverConfig {
            capture_interval: Duration::from_secs(5),
            segment_interval: Duration::from_secs(300),
        },
    );
    let (_activity, activity_receiver) = tokio::sync::watch::channel(SyncActivity::Idle);
    let (sync_stop, mut sync_shutdown) = tokio::sync::watch::channel(false);
    let (observer_stop, _observer_shutdown) =
        tokio::sync::watch::channel::<Option<ShutdownEvent>>(None);
    let sync_target = finalized.clone();
    let sync = async move {
        loop {
            tokio::select! {
                () = wake.wait() => {
                    let file = sync_target.join("tmux_main_screen.jsonl");
                    if file.is_file() {
                        std::fs::remove_file(file).expect("remove notified fixture file");
                        std::fs::remove_dir(&sync_target)
                            .expect("remove notified fixture segment");
                    }
                }
                changed = sync_shutdown.changed() => {
                    if changed.is_err() || *sync_shutdown.borrow_and_update() {
                        break Ok(());
                    }
                }
            }
        }
    };

    let exit = runtime().block_on(supervise_observer(
        observer,
        sync,
        Box::new(NoopIndicator),
        Box::new(lock),
        SupervisionControl {
            activity: activity_receiver,
            sync_stop,
            observer_stop,
            shutdown_barrier: supervisor_barrier,
            retention_fence: Arc::new(RetentionFence::new()),
        },
    ));

    assert_eq!(exit.exit_code, 0);
    assert!(finalized.is_dir());
    assert!(finalized.join("tmux_main_screen.jsonl").is_file());
    let reacquired = solstone_tmux::instance_lock::InstanceLock::acquire_existing(&data_root)
        .expect("inspect released lock")
        .expect("existing lock");
    drop(reacquired);
}

async fn supervise_fixture(
    sync: impl Future<Output = Result<(), DiagnosticCode>> + Send + 'static,
) -> ObserverExit {
    let (_activity, activity_receiver) = tokio::sync::watch::channel(SyncActivity::Idle);
    let (sync_stop, _sync_shutdown) = tokio::sync::watch::channel(false);
    let (observer_stop, mut observer_shutdown) =
        tokio::sync::watch::channel::<Option<ShutdownEvent>>(None);
    let observer = async move {
        loop {
            if let Some(event) = *observer_shutdown.borrow_and_update() {
                break ObserverExit {
                    exit_code: 0,
                    shutdown_event: Some(event),
                    failures: Vec::new(),
                };
            }
            if observer_shutdown.changed().await.is_err() {
                break ObserverExit {
                    exit_code: 1,
                    shutdown_event: None,
                    failures: vec!["observer stop channel closed".to_owned()],
                };
            }
        }
    };
    let (observer_barrier, supervisor_barrier) = shutdown_barrier();
    drop(observer_barrier);
    supervise_observer(
        observer,
        sync,
        Box::new(NoopIndicator),
        Box::new(RecordingLock::default()),
        SupervisionControl {
            activity: activity_receiver,
            sync_stop,
            observer_stop,
            shutdown_barrier: supervisor_barrier,
            retention_fence: Arc::new(RetentionFence::new()),
        },
    )
    .await
}

async fn wait_for_stop(receiver: &mut tokio::sync::watch::Receiver<bool>) {
    while !*receiver.borrow_and_update() {
        if receiver.changed().await.is_err() {
            break;
        }
    }
}

struct NoCaptures;

impl CaptureProvider for NoCaptures {
    fn poll<'a>(
        &'a self,
        _wall_unix_seconds: i64,
        _capture_interval: Duration,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<CaptureResult>, ObserverOperationError>> + Send + 'a>>
    {
        Box::pin(async { Ok(Vec::new()) })
    }
}

struct NoopIndicator;

impl ShutdownIndicator for NoopIndicator {
    fn restore<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<(), ObserverOperationError>> + Send + 'a>> {
        Box::pin(async { Ok(()) })
    }
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

struct BlockingActivityIndicator {
    entered: Option<mpsc::Sender<()>>,
    release: Option<tokio::sync::oneshot::Receiver<()>>,
}

impl ShutdownIndicator for BlockingActivityIndicator {
    fn set_activity<'a>(
        &'a mut self,
        activity: SyncActivity,
    ) -> Pin<Box<dyn Future<Output = Result<(), ObserverOperationError>> + Send + 'a>> {
        Box::pin(async move {
            if activity == SyncActivity::Working {
                if let Some(entered) = self.entered.take() {
                    entered
                        .send(())
                        .map_err(|_| ObserverOperationError("indicator probe failed".to_owned()))?;
                }
                if let Some(release) = self.release.take() {
                    let _ = release.await;
                }
            }
            Ok(())
        })
    }

    fn restore<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<(), ObserverOperationError>> + Send + 'a>> {
        Box::pin(async { Ok(()) })
    }
}

#[derive(Default)]
struct RecordingLock(Arc<Mutex<Vec<&'static str>>>);

impl LifecycleLock for RecordingLock {}

impl Drop for RecordingLock {
    fn drop(&mut self) {
        self.0.lock().expect("log poisoned").push("lock");
    }
}

fn test_clock() -> TestClock {
    let date = Date::from_calendar_date(2026, Month::July, 29).expect("date");
    let time = Time::from_hms(12, 0, 0).expect("time");
    TestClock::new(
        PrimitiveDateTime::new(date, time).assume_utc(),
        Duration::ZERO,
        UtcOffset::UTC,
    )
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("test runtime")
}
