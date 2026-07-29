// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

mod support;

use std::future::{Future, pending, ready};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use solstone_tmux_observer::clock::{Clock, TestClock};
use solstone_tmux_observer::health::DiagnosticCode;
use solstone_tmux_observer::model::CaptureResult;
use solstone_tmux_observer::name::{DerivedName, derive_component};
use solstone_tmux_observer::observer::{
    CaptureProvider, LifecycleLock, ObserverConfig, ObserverExit, ObserverOperationError,
    SegmentLifecycle, SegmentManager, ShutdownEvent, ShutdownIndicator, run_observer,
    stream_directory, supervise_observer,
};
use solstone_tmux_observer::segment::{SegmentClose, SegmentState};
use solstone_tmux_observer::sync::{SyncActivity, SyncWake};
use support::{TestDirectory, golden_capture};
use time::{Date, Month, OffsetDateTime, PrimitiveDateTime, Time, UtcOffset};

#[test]
fn injected_shutdown_uses_production_path() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let exit = run_test_observer(
        Box::new(RecordingSegment::new(Arc::clone(&log), false)),
        Box::new(RecordingIndicator::new(Arc::clone(&log), false)),
        Box::new(RecordingLock::new(Arc::clone(&log))),
        Box::pin(ready(ShutdownEvent::Injected)),
        Arc::new(NoCaptures),
    );

    assert_eq!(exit.exit_code, 0);
    assert_eq!(exit.shutdown_event, Some(ShutdownEvent::Injected));
    assert_eq!(
        *log.lock().expect("log poisoned"),
        ["segment", "indicator", "lock"]
    );
}

#[test]
fn signal_future_maps_to_same_shutdown_event() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let exit = run_test_observer(
        Box::new(RecordingSegment::new(Arc::clone(&log), false)),
        Box::new(RecordingIndicator::new(Arc::clone(&log), false)),
        Box::new(RecordingLock::new(Arc::clone(&log))),
        Box::pin(ready(ShutdownEvent::SigTerm)),
        Arc::new(NoCaptures),
    );

    assert_eq!(exit.exit_code, 0);
    assert_eq!(exit.shutdown_event, Some(ShutdownEvent::SigTerm));
    assert_eq!(
        *log.lock().expect("log poisoned"),
        ["segment", "indicator", "lock"]
    );
}

#[test]
fn nonempty_segment_finalizes_exactly_once() {
    let (temporary, segment, clock, data_root, stream) = actual_segment("lifecycle-finalize", true);
    let segment_stream = segment.stream_dir().to_owned();
    let manager = SegmentManager::new(
        segment,
        data_root,
        stream,
        clock.local_offset(),
        SyncWake::default(),
    );

    let exit = run_with_clock(
        Box::new(manager),
        Box::new(RecordingIndicator::default()),
        Box::new(RecordingLock::default()),
        Box::pin(ready(ShutdownEvent::Injected)),
        Arc::new(NoCaptures),
        Arc::new(clock),
    );

    assert_eq!(exit.exit_code, 0);
    let finalized = std::fs::read_dir(&segment_stream)
        .expect("stream entries")
        .filter_map(Result::ok)
        .filter(|entry| {
            entry.path().is_dir() && !entry.file_name().to_string_lossy().ends_with(".incomplete")
        })
        .count();
    assert_eq!(finalized, 1);
    drop(temporary);
}

#[test]
fn confirmed_empty_segment_is_removed() {
    let (temporary, segment, clock, data_root, stream) = actual_segment("lifecycle-empty", false);
    let source = segment.incomplete_dir().to_owned();
    let metadata = segment.metadata_path().to_owned();
    let manager = SegmentManager::new(
        segment,
        data_root,
        stream,
        clock.local_offset(),
        SyncWake::default(),
    );

    let exit = run_with_clock(
        Box::new(manager),
        Box::new(RecordingIndicator::default()),
        Box::new(RecordingLock::default()),
        Box::pin(ready(ShutdownEvent::Injected)),
        Arc::new(NoCaptures),
        Arc::new(clock),
    );

    assert_eq!(exit.exit_code, 0);
    assert!(!source.exists());
    assert!(!metadata.exists());
    drop(temporary);
}

#[test]
fn indicator_restores_before_lock_release() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let exit = run_test_observer(
        Box::new(RecordingSegment::new(Arc::clone(&log), false)),
        Box::new(RecordingIndicator::new(Arc::clone(&log), false)),
        Box::new(RecordingLock::new(Arc::clone(&log))),
        Box::pin(ready(ShutdownEvent::Injected)),
        Arc::new(NoCaptures),
    );

    assert_eq!(exit.exit_code, 0);
    let log = log.lock().expect("log poisoned");
    let indicator = log
        .iter()
        .position(|entry| *entry == "indicator")
        .expect("indicator");
    let lock = log.iter().position(|entry| *entry == "lock").expect("lock");
    assert!(indicator < lock);
}

#[test]
fn finalize_failure_exits_nonzero_and_keeps_source() {
    let (temporary, segment, clock, data_root, stream) = actual_segment("lifecycle-failure", true);
    let source = segment.incomplete_dir().to_owned();
    let collision = segment.stream_dir().join("120000_005");
    std::fs::create_dir(&collision).expect("create finalization collision");
    let manager = SegmentManager::new(
        segment,
        data_root,
        stream,
        clock.local_offset(),
        SyncWake::default(),
    );

    let exit = run_with_clock(
        Box::new(manager),
        Box::new(RecordingIndicator::default()),
        Box::new(RecordingLock::default()),
        Box::pin(ready(ShutdownEvent::Injected)),
        Arc::new(NoCaptures),
        Arc::new(clock),
    );

    assert_eq!(exit.exit_code, 1);
    assert!(source.is_dir());
    assert!(
        exit.failures
            .iter()
            .any(|failure| failure.contains("already exists"))
    );
    drop(temporary);
}

#[test]
fn unexpected_task_exit_surfaces_cause() {
    let exit = run_test_observer(
        Box::new(RecordingSegment::default()),
        Box::new(RecordingIndicator::default()),
        Box::new(RecordingLock::default()),
        Box::pin(pending()),
        Arc::new(FailingCapture),
    );

    assert_eq!(exit.exit_code, 1);
    assert_eq!(exit.shutdown_event, None);
    assert!(
        exit.failures
            .iter()
            .any(|failure| failure.contains("fixture capture exited"))
    );
}

#[test]
fn panic_string_payload_surfaces_and_exits_nonzero() {
    let exit = supervise_test(panic_with_string());
    assert_eq!(exit.exit_code, 1);
    assert!(
        exit.failures
            .iter()
            .any(|failure| failure.contains("panic: lifecycle boom"))
    );
}

#[test]
fn nonstring_panic_is_reported() {
    let exit = supervise_test(panic_without_string());
    assert_eq!(exit.exit_code, 1);
    assert!(
        exit.failures
            .iter()
            .any(|failure| failure.contains("non-string panic payload"))
    );
}

async fn panic_with_string() -> ObserverExit {
    panic!("lifecycle boom")
}

async fn panic_without_string() -> ObserverExit {
    std::panic::panic_any(17_u8)
}

fn run_test_observer(
    segment: Box<dyn SegmentLifecycle>,
    indicator: Box<dyn ShutdownIndicator>,
    instance_lock: Box<dyn LifecycleLock>,
    shutdown: Pin<Box<dyn Future<Output = ShutdownEvent> + Send>>,
    provider: Arc<dyn CaptureProvider>,
) -> ObserverExit {
    run_with_clock(
        segment,
        indicator,
        instance_lock,
        shutdown,
        provider,
        Arc::new(test_clock()),
    )
}

fn run_with_clock(
    segment: Box<dyn SegmentLifecycle>,
    indicator: Box<dyn ShutdownIndicator>,
    instance_lock: Box<dyn LifecycleLock>,
    shutdown: Pin<Box<dyn Future<Output = ShutdownEvent> + Send>>,
    provider: Arc<dyn CaptureProvider>,
    clock: Arc<dyn Clock>,
) -> ObserverExit {
    runtime().block_on(async move {
        let observer = run_observer(
            provider,
            segment,
            clock,
            shutdown,
            ObserverConfig {
                capture_interval: Duration::from_millis(10),
                segment_interval: Duration::from_secs(300),
            },
        );
        supervise_test_future(observer, indicator, instance_lock).await
    })
}

fn supervise_test(observer: impl Future<Output = ObserverExit> + Send + 'static) -> ObserverExit {
    runtime().block_on(supervise_test_future(
        observer,
        Box::new(RecordingIndicator::default()),
        Box::new(RecordingLock::default()),
    ))
}

async fn supervise_test_future(
    observer: impl Future<Output = ObserverExit> + Send + 'static,
    indicator: Box<dyn ShutdownIndicator>,
    instance_lock: Box<dyn LifecycleLock>,
) -> ObserverExit {
    let (_activity, activity_receiver) = tokio::sync::watch::channel(SyncActivity::Idle);
    let (sync_stop, mut sync_shutdown) = tokio::sync::watch::channel(false);
    let (observer_stop, _observer_shutdown) =
        tokio::sync::watch::channel::<Option<ShutdownEvent>>(None);
    let sync = async move {
        while !*sync_shutdown.borrow_and_update() {
            if sync_shutdown.changed().await.is_err() {
                break;
            }
        }
        Ok::<(), DiagnosticCode>(())
    };
    supervise_observer(
        observer,
        sync,
        indicator,
        instance_lock,
        activity_receiver,
        sync_stop,
        observer_stop,
    )
    .await
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("test runtime")
}

fn actual_segment(
    label: &str,
    nonempty: bool,
) -> (TestDirectory, SegmentState, TestClock, PathBuf, DerivedName) {
    let temporary = TestDirectory::new(label);
    let clock = test_clock();
    let data_root = temporary.path().join("data");
    let stream = derive_component("test.tmux").expect("stream name");
    let stream_dir = stream_directory(&data_root, &stream, clock.wall_now(), clock.local_offset())
        .expect("stream path");
    let mut segment = SegmentState::create(
        &stream_dir,
        clock.wall_now(),
        Duration::ZERO,
        clock.local_offset(),
    )
    .expect("segment");
    if nonempty {
        segment
            .append_capture(&golden_capture("main"), 0.25, Duration::from_secs(1))
            .expect("append");
    }
    clock.set_monotonic(Duration::from_secs(5));
    (temporary, segment, clock, data_root, stream)
}

fn test_clock() -> TestClock {
    let date = Date::from_calendar_date(2026, Month::July, 28).expect("date");
    let time = Time::from_hms(12, 0, 0).expect("time");
    TestClock::new(
        PrimitiveDateTime::new(date, time).assume_utc(),
        Duration::ZERO,
        UtcOffset::UTC,
    )
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

struct FailingCapture;

impl CaptureProvider for FailingCapture {
    fn poll<'a>(
        &'a self,
        _wall_unix_seconds: i64,
        _capture_interval: Duration,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<CaptureResult>, ObserverOperationError>> + Send + 'a>>
    {
        Box::pin(async { Err(ObserverOperationError("fixture capture exited".to_owned())) })
    }
}

#[derive(Default)]
struct RecordingSegment {
    log: Arc<Mutex<Vec<&'static str>>>,
    fail: bool,
    shutdowns: Arc<AtomicUsize>,
}

impl RecordingSegment {
    fn new(log: Arc<Mutex<Vec<&'static str>>>, fail: bool) -> Self {
        Self {
            log,
            fail,
            shutdowns: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl SegmentLifecycle for RecordingSegment {
    fn process_poll(
        &mut self,
        _captures: &[CaptureResult],
        _wall_now: OffsetDateTime,
        _monotonic_now: Duration,
        _segment_interval: Duration,
    ) -> Result<(), ObserverOperationError> {
        Ok(())
    }

    fn shutdown(
        &mut self,
        _monotonic_now: Duration,
    ) -> Result<SegmentClose, ObserverOperationError> {
        self.shutdowns.fetch_add(1, Ordering::Relaxed);
        self.log.lock().expect("log poisoned").push("segment");
        if self.fail {
            Err(ObserverOperationError(
                "fixture finalize failure".to_owned(),
            ))
        } else {
            Ok(SegmentClose::Finalized(PathBuf::from("fixture")))
        }
    }
}

#[derive(Default)]
struct RecordingIndicator {
    log: Arc<Mutex<Vec<&'static str>>>,
    fail: bool,
}

impl RecordingIndicator {
    fn new(log: Arc<Mutex<Vec<&'static str>>>, fail: bool) -> Self {
        Self { log, fail }
    }
}

impl ShutdownIndicator for RecordingIndicator {
    fn restore<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<(), ObserverOperationError>> + Send + 'a>> {
        Box::pin(async move {
            self.log.lock().expect("log poisoned").push("indicator");
            if self.fail {
                Err(ObserverOperationError(
                    "fixture indicator failure".to_owned(),
                ))
            } else {
                Ok(())
            }
        })
    }
}

#[derive(Default)]
struct RecordingLock {
    log: Arc<Mutex<Vec<&'static str>>>,
}

impl RecordingLock {
    fn new(log: Arc<Mutex<Vec<&'static str>>>) -> Self {
        Self { log }
    }
}

impl LifecycleLock for RecordingLock {}

impl Drop for RecordingLock {
    fn drop(&mut self) {
        self.log.lock().expect("log poisoned").push("lock");
    }
}
