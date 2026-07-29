// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

mod support;

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use solstone_tmux_observer::clock::{Clock, TestClock};
use solstone_tmux_observer::health::DiagnosticCode;
use solstone_tmux_observer::model::CaptureResult;
use solstone_tmux_observer::name::derive_component;
use solstone_tmux_observer::observer::{
    CaptureProvider, LifecycleLock, ObserverConfig, ObserverExit, ObserverOperationError,
    SegmentManager, ShutdownEvent, ShutdownIndicator, run_observer, stream_directory,
    supervise_observer,
};
use solstone_tmux_observer::paths::ensure_private_directory;
use solstone_tmux_observer::segment::SegmentState;
use solstone_tmux_observer::sync::{SyncActivity, SyncWake};
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

    let exit = runtime().block_on(supervise_observer(
        observer,
        sync,
        Box::new(indicator),
        Box::new(lock),
        activity_receiver,
        sync_stop,
        observer_stop,
    ));

    assert_eq!(exit.exit_code, 0);
    assert_eq!(
        *log.lock().expect("log poisoned"),
        ["sync", "indicator", "lock"]
    );
}

#[test]
fn shutdown_keeps_the_final_segment_for_a_later_scan() {
    let temporary = TestDirectory::new("supervision-final-segment");
    let data_root = temporary.path().join("data");
    ensure_private_directory(&data_root).expect("data root");
    let lock = solstone_tmux_observer::instance_lock::InstanceLock::acquire(&data_root)
        .expect("instance lock");
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
    let manager = SegmentManager::new(
        segment,
        data_root.clone(),
        stream,
        clock.local_offset(),
        SyncWake::default(),
    );
    let observer = run_observer(
        Arc::new(NoCaptures),
        Box::new(manager),
        Arc::new(clock) as Arc<dyn Clock>,
        Box::pin(async { ShutdownEvent::Injected }),
        ObserverConfig {
            capture_interval: Duration::from_secs(5),
            segment_interval: Duration::from_secs(300),
        },
    );
    let (_activity, activity_receiver) = tokio::sync::watch::channel(SyncActivity::Idle);
    let (sync_stop, mut sync_shutdown) = tokio::sync::watch::channel(false);
    let (observer_stop, _observer_shutdown) =
        tokio::sync::watch::channel::<Option<ShutdownEvent>>(None);
    let sync = async move {
        wait_for_stop(&mut sync_shutdown).await;
        Ok(())
    };

    let exit = runtime().block_on(supervise_observer(
        observer,
        sync,
        Box::new(NoopIndicator),
        Box::new(lock),
        activity_receiver,
        sync_stop,
        observer_stop,
    ));

    assert_eq!(exit.exit_code, 0);
    let finalized = stream_dir.join("120000_005");
    assert!(finalized.is_dir());
    assert!(finalized.join("tmux_main_screen.jsonl").is_file());
    let reacquired =
        solstone_tmux_observer::instance_lock::InstanceLock::acquire_existing(&data_root)
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
    supervise_observer(
        observer,
        sync,
        Box::new(NoopIndicator),
        Box::new(RecordingLock::default()),
        activity_receiver,
        sync_stop,
        observer_stop,
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
