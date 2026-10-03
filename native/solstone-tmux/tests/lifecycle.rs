// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

mod support;

use std::future::{Future, pending, ready};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use solstone_tmux::clock::{Clock, TestClock, Zone, ZoneSource};
use solstone_tmux::health::DiagnosticCode;
use solstone_tmux::instance_lock::InstanceLock;
use solstone_tmux::model::CaptureResult;
use solstone_tmux::name::{DerivedName, derive_component};
use solstone_tmux::observer::{
    CaptureProvider, LifecycleLock, ObserverConfig, ObserverExit, ObserverOperationError,
    SegmentLifecycle, SegmentManager, ShutdownEvent, ShutdownIndicator, SupervisionControl,
    run_observer, shutdown_barrier, stream_directory, supervise_observer,
};
use solstone_tmux::segment::{SegmentClose, SegmentState};
use solstone_tmux::sync::{SyncActivity, SyncWake};
use solstone_tmux::tmux::StderrWarnings;
use support::{TestDirectory, golden_capture};
use time::{Date, Month, OffsetDateTime, PrimitiveDateTime, Time, UtcOffset};

struct ClockZoneSource(Arc<TestClock>);

impl ZoneSource for ClockZoneSource {
    fn read(&mut self) -> Result<Zone, String> {
        Ok(Zone::fixed(self.0.offset_at(self.0.wall_now())))
    }
}

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
        SyncWake::default(),
        Box::new(ClockZoneSource(Arc::clone(&clock))),
        Arc::new(StderrWarnings),
        None,
        false,
    );

    let exit = run_with_clock(
        Box::new(manager),
        Box::new(RecordingIndicator::default()),
        Box::new(RecordingLock::default()),
        Box::pin(ready(ShutdownEvent::Injected)),
        Arc::new(NoCaptures),
        Arc::clone(&clock) as Arc<dyn Clock>,
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
        SyncWake::default(),
        Box::new(ClockZoneSource(Arc::clone(&clock))),
        Arc::new(StderrWarnings),
        None,
        false,
    );

    let exit = run_with_clock(
        Box::new(manager),
        Box::new(RecordingIndicator::default()),
        Box::new(RecordingLock::default()),
        Box::pin(ready(ShutdownEvent::Injected)),
        Arc::new(NoCaptures),
        Arc::clone(&clock) as Arc<dyn Clock>,
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
        SyncWake::default(),
        Box::new(ClockZoneSource(Arc::clone(&clock))),
        Arc::new(StderrWarnings),
        None,
        false,
    );

    let exit = run_with_clock(
        Box::new(manager),
        Box::new(RecordingIndicator::default()),
        Box::new(RecordingLock::default()),
        Box::pin(ready(ShutdownEvent::Injected)),
        Arc::new(NoCaptures),
        Arc::clone(&clock) as Arc<dyn Clock>,
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
        let (observer_shutdown_barrier, supervisor_shutdown_barrier) = shutdown_barrier();
        let observer = run_observer(
            provider,
            segment,
            clock,
            shutdown,
            observer_shutdown_barrier,
            ObserverConfig {
                capture_interval: Duration::from_millis(10),
                segment_interval: Duration::from_secs(300),
            },
        );
        supervise_test_future(
            observer,
            indicator,
            instance_lock,
            supervisor_shutdown_barrier,
        )
        .await
    })
}

fn supervise_test(observer: impl Future<Output = ObserverExit> + Send + 'static) -> ObserverExit {
    let (observer_shutdown_barrier, supervisor_shutdown_barrier) = shutdown_barrier();
    drop(observer_shutdown_barrier);
    runtime().block_on(supervise_test_future(
        observer,
        Box::new(RecordingIndicator::default()),
        Box::new(RecordingLock::default()),
        supervisor_shutdown_barrier,
    ))
}

async fn supervise_test_future(
    observer: impl Future<Output = ObserverExit> + Send + 'static,
    indicator: Box<dyn ShutdownIndicator>,
    instance_lock: Box<dyn LifecycleLock>,
    shutdown_barrier: solstone_tmux::observer::SupervisorShutdownBarrier,
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
        SupervisionControl {
            activity: activity_receiver,
            sync_stop,
            observer_stop,
            shutdown_barrier,
            retention_fence: Arc::new(solstone_tmux::sync::RetentionFence::new()),
        },
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
) -> (
    TestDirectory,
    SegmentState,
    Arc<TestClock>,
    PathBuf,
    DerivedName,
) {
    let temporary = TestDirectory::new(label);
    let clock = Arc::new(test_clock());
    let data_root = temporary.path().join("data");
    let stream = derive_component("test.tmux").expect("stream name");
    let offset = clock.offset_at(clock.wall_now());
    let stream_dir =
        stream_directory(&data_root, &stream, clock.wall_now(), offset).expect("stream path");
    let mut segment = SegmentState::create(
        &stream_dir,
        "120000",
        clock.wall_now(),
        Duration::ZERO,
        offset,
        None,
        Duration::from_secs(300),
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

#[test]
fn rotation_uses_the_offset_at_each_segment_start() {
    let date = Date::from_calendar_date(2026, Month::July, 15).expect("date");
    let time = Time::from_hms(18, 0, 0).expect("time");
    let t = PrimitiveDateTime::new(date, time).assume_utc();
    let before_offset = UtcOffset::from_hms(-6, 0, 0).expect("offset");
    let after_offset = UtcOffset::from_hms(-7, 0, 0).expect("offset");

    let clock = Arc::new(TestClock::with_offset_step(
        t - time::Duration::seconds(10),
        Duration::ZERO,
        before_offset,
        t,
        after_offset,
    ));
    let temporary = TestDirectory::new("rotation-offset-step");
    let data_root = temporary.path().join("data");
    let stream = derive_component("test.tmux").expect("stream");
    let segment_interval = Duration::from_secs(2);
    let mut manager = SegmentManager::start(
        data_root.clone(),
        stream,
        clock.as_ref(),
        segment_interval,
        SyncWake::default(),
        Box::new(ClockZoneSource(Arc::clone(&clock))),
        Arc::new(StderrWarnings),
    )
    .expect("start manager");

    let wall_t_minus_1 = t - time::Duration::seconds(1);
    clock.set_wall(wall_t_minus_1);
    clock.set_monotonic(Duration::from_secs(2));
    manager
        .process_poll(
            &[golden_capture("main")],
            wall_t_minus_1,
            Duration::from_secs(2),
            segment_interval,
        )
        .expect("poll at t-1s");

    let wall_t_plus_1 = t + time::Duration::seconds(1);
    clock.set_wall(wall_t_plus_1);
    clock.set_monotonic(Duration::from_secs(4));
    manager
        .process_poll(
            &[golden_capture("main")],
            wall_t_plus_1,
            Duration::from_secs(4),
            segment_interval,
        )
        .expect("poll at t+1s");

    let finalized_t_minus_1 = data_root
        .join("captures")
        .join("20260715")
        .join("test.tmux")
        .join("115959_002");
    assert!(
        finalized_t_minus_1.is_dir(),
        "finalized segment must exist at {:?}",
        finalized_t_minus_1
    );

    let incomplete_t_plus_1 = data_root
        .join("captures")
        .join("20260715")
        .join("test.tmux")
        .join("110001.incomplete");
    assert!(
        incomplete_t_plus_1.is_dir(),
        "incomplete segment must exist at {:?}",
        incomplete_t_plus_1
    );

    let meta_path = data_root
        .join("captures")
        .join("20260715")
        .join("test.tmux")
        .join("110001.incomplete.meta");
    let meta_bytes = std::fs::read(&meta_path).expect("read incomplete meta");
    let meta: solstone_tmux::storage::SegmentMetadata =
        serde_json::from_slice(&meta_bytes).expect("parse metadata");
    assert_eq!(meta.local_offset_seconds, after_offset.whole_seconds());
}

#[test]
fn forward_offset_step_opens_the_new_local_date() {
    let date = Date::from_calendar_date(2026, Month::January, 15).expect("date");
    let time = Time::from_hms(7, 0, 0).expect("time");
    let t = PrimitiveDateTime::new(date, time).assume_utc();
    let before_offset = UtcOffset::from_hms(-7, 0, 0).expect("offset");
    let after_offset = UtcOffset::from_hms(-6, 0, 0).expect("offset");

    let initial_wall = t - time::Duration::seconds(1);
    let clock = Arc::new(TestClock::with_offset_step(
        initial_wall,
        Duration::ZERO,
        before_offset,
        t,
        after_offset,
    ));
    let temporary = TestDirectory::new("forward-offset-step");
    let data_root = temporary.path().join("data");
    let stream = derive_component("test.tmux").expect("stream");
    let interval = Duration::from_secs(2);
    let mut manager = SegmentManager::start(
        data_root.clone(),
        stream,
        clock.as_ref(),
        interval,
        SyncWake::default(),
        Box::new(ClockZoneSource(Arc::clone(&clock))),
        Arc::new(StderrWarnings),
    )
    .expect("start manager");

    let poll_wall = t + time::Duration::seconds(1);
    clock.set_wall(poll_wall);
    clock.set_monotonic(interval);
    manager
        .process_poll(&[], poll_wall, interval, interval)
        .expect("process poll");

    let new_incomplete = data_root
        .join("captures")
        .join("20260115")
        .join("test.tmux")
        .join("010001.incomplete");
    assert!(new_incomplete.is_dir());

    let old_stream_dir = data_root
        .join("captures")
        .join("20260114")
        .join("test.tmux");
    if old_stream_dir.exists() {
        let count = std::fs::read_dir(&old_stream_dir)
            .expect("read old stream dir")
            .count();
        assert_eq!(count, 0);
    }
}

#[test]
fn backward_offset_step_across_midnight_opens_the_earlier_date() {
    let date = Date::from_calendar_date(2026, Month::January, 15).expect("date");
    let time = Time::from_hms(7, 0, 0).expect("time");
    let t = PrimitiveDateTime::new(date, time).assume_utc();
    let before_offset = UtcOffset::from_hms(-7, 0, 0).expect("offset");
    let after_offset = UtcOffset::from_hms(-8, 0, 0).expect("offset");

    let initial_wall = t - time::Duration::seconds(5);
    let clock = Arc::new(TestClock::with_offset_step(
        initial_wall,
        Duration::ZERO,
        before_offset,
        t,
        after_offset,
    ));
    let temporary = TestDirectory::new("backward-offset-step");
    let data_root = temporary.path().join("data");
    let stream = derive_component("test.tmux").expect("stream");
    let interval = Duration::from_secs(2);
    let mut manager = SegmentManager::start(
        data_root.clone(),
        stream,
        clock.as_ref(),
        interval,
        SyncWake::default(),
        Box::new(ClockZoneSource(Arc::clone(&clock))),
        Arc::new(StderrWarnings),
    )
    .expect("start manager");

    clock.set_wall(t);
    clock.set_monotonic(interval);
    manager
        .process_poll(&[], t, interval, interval)
        .expect("process poll");

    let new_incomplete = data_root
        .join("captures")
        .join("20260114")
        .join("test.tmux")
        .join("230000.incomplete");
    assert!(new_incomplete.is_dir());
}

#[test]
fn fallback_hour_reuses_a_stem_and_records_each_start_offset() {
    let date = Date::from_calendar_date(2026, Month::November, 1).expect("date");
    let time = Time::from_hms(8, 0, 0).expect("time");
    let t = PrimitiveDateTime::new(date, time).assume_utc();
    let before_offset = UtcOffset::from_hms(-6, 0, 0).expect("offset -6");
    let after_offset = UtcOffset::from_hms(-7, 0, 0).expect("offset -7");

    let start_wall = PrimitiveDateTime::new(
        Date::from_calendar_date(2026, Month::November, 1).expect("date"),
        Time::from_hms(7, 0, 0).expect("time"),
    )
    .assume_utc();
    let clock = Arc::new(TestClock::with_offset_step(
        start_wall,
        Duration::ZERO,
        before_offset,
        t,
        after_offset,
    ));
    let temporary = TestDirectory::new("fallback-hour");
    let data_root = temporary.path().join("data");
    let stream = derive_component("test.tmux").expect("stream");
    let interval = Duration::from_secs(300);
    let mut manager = SegmentManager::start(
        data_root.clone(),
        stream.clone(),
        clock.as_ref(),
        interval,
        SyncWake::default(),
        Box::new(ClockZoneSource(Arc::clone(&clock))),
        Arc::new(StderrWarnings),
    )
    .expect("start manager");

    // Initial poll at monotonic 0
    manager
        .process_poll(
            &[golden_capture("main")],
            start_wall,
            Duration::ZERO,
            interval,
        )
        .expect("initial poll");

    let initial_offset = clock.offset_at(start_wall);
    let (initial_date, initial_stem) =
        solstone_tmux::clock::local_date_and_time(start_wall, initial_offset);
    let initial_meta_path = data_root
        .join("captures")
        .join(&initial_date)
        .join("test.tmux")
        .join(format!("{initial_stem}.incomplete.meta"));
    let initial_meta_bytes = std::fs::read(&initial_meta_path).expect("read initial metadata");
    let initial_meta: solstone_tmux::storage::SegmentMetadata =
        serde_json::from_slice(&initial_meta_bytes).expect("parse initial metadata");
    assert_eq!(
        initial_meta.local_offset_seconds,
        clock.offset_at(start_wall).whole_seconds()
    );
    assert_eq!(
        initial_meta.incomplete_dir,
        format!("{initial_stem}.incomplete")
    );

    let mut utc_minus_6_stems = vec![initial_stem];
    let mut utc_minus_7_stems = Vec::new();

    let mut current_wall = start_wall;
    let mut current_monotonic = Duration::ZERO;

    for _step in 1..=12 {
        current_wall += time::Duration::seconds(300);
        current_monotonic += interval;
        clock.set_wall(current_wall);
        clock.set_monotonic(current_monotonic);

        let prev_wall = current_wall - time::Duration::seconds(300);
        let prev_offset = clock.offset_at(prev_wall);

        manager
            .process_poll(
                &[golden_capture("main")],
                current_wall,
                current_monotonic,
                interval,
            )
            .expect("rotation poll");

        // The previous segment finalized: if it was UTC-6, simulate confirm-and-delete
        if prev_offset == before_offset {
            let (prev_date, prev_time) =
                solstone_tmux::clock::local_date_and_time(prev_wall, prev_offset);
            let prev_finalized = data_root
                .join("captures")
                .join(&prev_date)
                .join("test.tmux")
                .join(format!("{prev_time}_300"));
            if prev_finalized.is_dir() {
                std::fs::remove_dir_all(&prev_finalized).expect("remove finalized");
            }
            let zone_file = solstone_tmux::storage::capture_time_path(
                &data_root
                    .join("captures")
                    .join(&prev_date)
                    .join("test.tmux"),
                &format!("{prev_time}_300"),
            );
            if zone_file.exists() {
                std::fs::remove_file(&zone_file).expect("remove zone file");
            }
        }

        // Check the newly opened segment's metadata
        let expected_offset = clock.offset_at(current_wall);
        let (expected_date, expected_stem) =
            solstone_tmux::clock::local_date_and_time(current_wall, expected_offset);
        if expected_offset == before_offset {
            utc_minus_6_stems.push(expected_stem.clone());
        } else {
            utc_minus_7_stems.push(expected_stem.clone());
        }

        let meta_path = data_root
            .join("captures")
            .join(&expected_date)
            .join("test.tmux")
            .join(format!("{expected_stem}.incomplete.meta"));
        let meta_bytes = std::fs::read(&meta_path).expect("read metadata");
        let meta: solstone_tmux::storage::SegmentMetadata =
            serde_json::from_slice(&meta_bytes).expect("parse metadata");

        let start_wall_dt = OffsetDateTime::from_unix_timestamp_nanos(meta.start_wall_unix_nanos)
            .expect("valid timestamp");
        assert_eq!(
            meta.local_offset_seconds,
            clock.offset_at(start_wall_dt).whole_seconds()
        );
        assert_eq!(meta.incomplete_dir, format!("{expected_stem}.incomplete"));
        assert_eq!(expected_date, "20261101");
    }

    assert!(
        utc_minus_7_stems
            .iter()
            .any(|stem| utc_minus_6_stems.contains(stem)),
        "at least one UTC-7 stem must equal a UTC-6 stem"
    );
}

struct BerlinZoneSource(Zone);

impl BerlinZoneSource {
    fn new() -> Self {
        let bytes = std::fs::read(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/europe-berlin.tzif"),
        )
        .expect("read berlin tzif");
        Self(Zone::from_tzif("Europe/Berlin", &bytes).expect("berlin zone"))
    }
}

impl ZoneSource for BerlinZoneSource {
    fn read(&mut self) -> Result<Zone, String> {
        Ok(self.0.clone())
    }
}

#[test]
fn repeated_berlin_hour_stores_the_next_free_second() {
    let t0 = PrimitiveDateTime::new(
        Date::from_calendar_date(2026, Month::October, 25).expect("date"),
        Time::from_hms(0, 30, 0).expect("time"),
    )
    .assume_utc();
    let clock = Arc::new(TestClock::new(
        t0,
        Duration::ZERO,
        UtcOffset::from_hms(2, 0, 0).expect("offset +2"),
    ));
    let temporary = TestDirectory::new("berlin-repeated-hour");
    let data_root = temporary.path().join("data");
    let stream = derive_component("test.tmux").expect("stream");
    let interval = Duration::from_secs(300);

    let mut manager = SegmentManager::start(
        data_root.clone(),
        stream.clone(),
        clock.as_ref(),
        interval,
        SyncWake::default(),
        Box::new(BerlinZoneSource::new()),
        Arc::new(StderrWarnings),
    )
    .expect("start manager");

    manager
        .process_poll(&[golden_capture("main")], t0, Duration::ZERO, interval)
        .expect("poll at t0");

    let t1 = PrimitiveDateTime::new(
        Date::from_calendar_date(2026, Month::October, 25).expect("date"),
        Time::from_hms(1, 30, 0).expect("time"),
    )
    .assume_utc();
    clock.set_wall(t1);
    clock.set_monotonic(interval);

    manager
        .process_poll(&[golden_capture("main")], t1, interval, interval)
        .expect("poll at t1");

    let day_stream_dir = data_root
        .join("captures")
        .join("20261025")
        .join("test.tmux");

    // First segment finalized as 023000_300 with .zone.json offset +7200
    let first_finalized = day_stream_dir.join("023000_300");
    assert!(
        first_finalized.is_dir(),
        "first segment must be finalized as 023000_300"
    );
    let first_zone = solstone_tmux::storage::load_capture_time(
        &solstone_tmux::storage::capture_time_path(&day_stream_dir, "023000_300"),
    );
    assert_eq!(
        first_zone,
        solstone_tmux::storage::CaptureTimeLoad::Present(solstone_tmux::storage::CaptureTime {
            tz: Some("Europe/Berlin".to_owned()),
            utc_offset_seconds: 7200,
        })
    );

    // New segment is 023001.incomplete
    let second_incomplete = day_stream_dir.join("023001.incomplete");
    assert!(
        second_incomplete.is_dir(),
        "second segment must be 023001.incomplete"
    );

    let second_meta_bytes =
        std::fs::read(day_stream_dir.join("023001.incomplete.meta")).expect("read second meta");
    let second_meta: solstone_tmux::storage::SegmentMetadata =
        serde_json::from_slice(&second_meta_bytes).expect("parse second metadata");
    assert_eq!(second_meta.start_wall_unix_nanos, t1.unix_timestamp_nanos());
    assert_eq!(second_meta.local_offset_seconds, 3600);
    assert_eq!(second_meta.tz, Some("Europe/Berlin".to_owned()));

    let jsonl_bytes = std::fs::read(second_incomplete.join("tmux_main_screen.jsonl"))
        .expect("read second incomplete jsonl");
    let frame_val: serde_json::Value =
        serde_json::from_slice(&jsonl_bytes).expect("parse frame json");
    let ts = frame_val["timestamp"].as_f64().expect("timestamp f64");
    assert!(ts >= 0.0);
}

#[test]
fn startup_collision_uses_the_next_free_second() {
    let t = PrimitiveDateTime::new(
        Date::from_calendar_date(2026, Month::October, 25).expect("date"),
        Time::from_hms(1, 30, 0).expect("time"),
    )
    .assume_utc();
    let clock = Arc::new(TestClock::new(
        t,
        Duration::ZERO,
        UtcOffset::from_hms(1, 0, 0).expect("offset +1"),
    ));
    let temporary = TestDirectory::new("startup-collision");
    let data_root = temporary.path().join("data");
    let stream = derive_component("test.tmux").expect("stream");

    let day_stream_dir = data_root
        .join("captures")
        .join("20261025")
        .join("test.tmux");
    std::fs::create_dir_all(day_stream_dir.join("023000_300")).expect("plant directory");

    let mut manager = SegmentManager::start(
        data_root.clone(),
        stream,
        clock.as_ref(),
        Duration::from_secs(300),
        SyncWake::default(),
        Box::new(BerlinZoneSource::new()),
        Arc::new(StderrWarnings),
    )
    .expect("start manager");

    assert_eq!(
        manager
            .segment_mut()
            .incomplete_dir()
            .file_name()
            .unwrap()
            .to_str()
            .unwrap(),
        "023001.incomplete"
    );
    assert!(day_stream_dir.join("023001.incomplete").is_dir());
}

#[test]
fn taken_names_advance_by_civil_seconds_across_midnight() {
    let t_berlin = PrimitiveDateTime::new(
        Date::from_calendar_date(2026, Month::October, 25).expect("date"),
        Time::from_hms(1, 30, 0).expect("time"),
    )
    .assume_utc();
    let clock = Arc::new(TestClock::new(
        t_berlin,
        Duration::ZERO,
        UtcOffset::from_hms(1, 0, 0).expect("offset +1"),
    ));
    let temporary = TestDirectory::new("advance-across-midnight");
    let data_root = temporary.path().join("data");
    let stream = derive_component("test.tmux").expect("stream");
    let day_stream_dir = data_root
        .join("captures")
        .join("20261025")
        .join("test.tmux");

    // Case 1: 023000 and 023001 taken -> 023002
    std::fs::create_dir_all(day_stream_dir.join("023000_300")).expect("plant 023000");
    std::fs::create_dir_all(day_stream_dir.join("023001_300")).expect("plant 023001");

    let mut manager1 = SegmentManager::start(
        data_root.clone(),
        stream.clone(),
        clock.as_ref(),
        Duration::from_secs(300),
        SyncWake::default(),
        Box::new(BerlinZoneSource::new()),
        Arc::new(StderrWarnings),
    )
    .expect("start manager 1");
    assert_eq!(
        manager1
            .segment_mut()
            .incomplete_dir()
            .file_name()
            .unwrap()
            .to_str()
            .unwrap(),
        "023002.incomplete"
    );
    let meta1 = manager1.segment_mut().metadata();
    assert_eq!(meta1.start_wall_unix_nanos, t_berlin.unix_timestamp_nanos());
    assert_eq!(meta1.local_offset_seconds, 3600);
    assert_eq!(meta1.tz, Some("Europe/Berlin".to_owned()));

    // Case 2: Natural 235959 taken on day D -> finalized 000000_{len} in day D+1
    let kolkata_bytes = std::fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/asia-kolkata.tzif"),
    )
    .expect("read kolkata tzif");
    let kolkata_zone = Zone::from_tzif("Asia/Kolkata", &kolkata_bytes).expect("kolkata zone");
    struct KolkataZoneSource(Zone);
    impl ZoneSource for KolkataZoneSource {
        fn read(&mut self) -> Result<Zone, String> {
            Ok(self.0.clone())
        }
    }

    let t_kolkata = PrimitiveDateTime::new(
        Date::from_calendar_date(2026, Month::September, 29).expect("date"),
        Time::from_hms(18, 29, 59).expect("time"),
    )
    .assume_utc();
    let clock_kolkata = Arc::new(TestClock::new(
        t_kolkata,
        Duration::ZERO,
        UtcOffset::from_hms(5, 30, 0).expect("offset +5:30"),
    ));
    let day_stream_29 = data_root
        .join("captures")
        .join("20260929")
        .join("test.tmux");
    std::fs::create_dir_all(day_stream_29.join("235959_300")).expect("plant 235959");

    let mut manager2 = SegmentManager::start(
        data_root.clone(),
        stream.clone(),
        clock_kolkata.as_ref(),
        Duration::from_secs(300),
        SyncWake::default(),
        Box::new(KolkataZoneSource(kolkata_zone.clone())),
        Arc::new(StderrWarnings),
    )
    .expect("start manager 2");
    assert_eq!(
        manager2
            .segment_mut()
            .incomplete_dir()
            .file_name()
            .unwrap()
            .to_str()
            .unwrap(),
        "000000.incomplete"
    );
    let next_day_stream = data_root
        .join("captures")
        .join("20260930")
        .join("test.tmux");
    assert!(next_day_stream.join("000000.incomplete").is_dir());
    manager2
        .process_poll(
            &[golden_capture("main")],
            t_kolkata,
            Duration::ZERO,
            Duration::from_secs(5),
        )
        .expect("poll");
    let meta2 = manager2.segment_mut().metadata();
    assert_eq!(
        meta2.start_wall_unix_nanos,
        t_kolkata.unix_timestamp_nanos()
    );
    assert_eq!(meta2.tz, Some("Asia/Kolkata".to_owned()));
    assert_eq!(meta2.local_offset_seconds, 19800);

    let close = manager2
        .segment_mut()
        .finalize(Duration::from_secs(5))
        .expect("finalize");
    match close {
        SegmentClose::Finalized(path) => {
            assert_eq!(path.file_name().unwrap().to_str().unwrap(), "000000_005");
            assert!(path.starts_with(&next_day_stream));
        }
        _ => panic!("expected finalized"),
    }

    // Case 3: A free stem keeps the natural HHMMSS
    let t_free = PrimitiveDateTime::new(
        Date::from_calendar_date(2026, Month::September, 29).expect("date"),
        Time::from_hms(12, 0, 0).expect("time"),
    )
    .assume_utc();
    let clock_free = Arc::new(TestClock::new(
        t_free,
        Duration::ZERO,
        UtcOffset::from_hms(5, 30, 0).expect("offset"),
    ));
    let mut manager3 = SegmentManager::start(
        data_root.clone(),
        stream,
        clock_free.as_ref(),
        Duration::from_secs(300),
        SyncWake::default(),
        Box::new(KolkataZoneSource(kolkata_zone)),
        Arc::new(StderrWarnings),
    )
    .expect("start manager 3");
    assert_eq!(
        manager3
            .segment_mut()
            .incomplete_dir()
            .file_name()
            .unwrap()
            .to_str()
            .unwrap(),
        "173000.incomplete"
    );
}

#[test]
fn existing_entries_stay_byte_identical_across_recovery_and_rotation() {
    let t0 = PrimitiveDateTime::new(
        Date::from_calendar_date(2026, Month::October, 25).expect("date"),
        Time::from_hms(0, 30, 0).expect("time"),
    )
    .assume_utc();
    let clock = Arc::new(TestClock::new(
        t0,
        Duration::ZERO,
        UtcOffset::from_hms(2, 0, 0).expect("offset +2"),
    ));
    let temporary = TestDirectory::new("byte-identical-entries");
    let data_root = temporary.path().join("data");
    let stream = derive_component("test.tmux").expect("stream");
    let interval = Duration::from_secs(300);

    let mut manager = SegmentManager::start(
        data_root.clone(),
        stream.clone(),
        clock.as_ref(),
        interval,
        SyncWake::default(),
        Box::new(BerlinZoneSource::new()),
        Arc::new(StderrWarnings),
    )
    .expect("start manager");

    manager
        .process_poll(&[golden_capture("main")], t0, Duration::ZERO, interval)
        .expect("poll t0");

    let day_stream_dir = data_root
        .join("captures")
        .join("20261025")
        .join("test.tmux");

    let close = manager
        .segment_mut()
        .finalize(Duration::from_secs(300))
        .expect("finalize first");
    let finalized_path = match close {
        SegmentClose::Finalized(path) => path,
        _ => panic!("expected finalized"),
    };
    let zone_path = solstone_tmux::storage::capture_time_path(&day_stream_dir, "023000_300");

    let ack_dir = data_root
        .join("sync-ledger")
        .join("20261025")
        .join("test.tmux")
        .join("023000_300");
    std::fs::create_dir_all(&ack_dir).expect("create ack dir");
    let ack_path = ack_dir.join("ack.json");
    std::fs::write(&ack_path, b"{\"receipt\":\"valid\"}").expect("write ack");

    // Build real writer output for stranded segment 040000
    let mut stranded = SegmentState::create(
        &day_stream_dir,
        "040000",
        t0,
        Duration::ZERO,
        UtcOffset::from_hms(2, 0, 0).expect("offset +2"),
        Some("Europe/Berlin"),
        Duration::from_secs(300),
    )
    .expect("create stranded");
    stranded
        .append_capture(&golden_capture("main"), 0.25, Duration::from_secs(300))
        .expect("append stranded");
    let stranded_incomplete = day_stream_dir.join("040000.incomplete");
    let stranded_jsonl_path = stranded_incomplete.join("tmux_main_screen.jsonl");
    let snap_stranded_jsonl = std::fs::read(&stranded_jsonl_path).expect("read stranded jsonl");
    let stranded_meta = day_stream_dir.join("040000.incomplete.meta");
    let stranded_finalized = day_stream_dir.join(stranded.metadata().finalized_dir.clone());
    std::fs::create_dir_all(&stranded_finalized).expect("create stranded finalized collision");
    drop(stranded);

    let jsonl_path = finalized_path.join("tmux_main_screen.jsonl");
    let snap_jsonl = std::fs::read(&jsonl_path).expect("read jsonl");
    let snap_zone = std::fs::read(&zone_path).expect("read zone");
    let snap_ack = std::fs::read(&ack_path).expect("read ack");
    let snap_stranded_meta = std::fs::read(&stranded_meta).expect("read stranded meta");

    let lock = InstanceLock::acquire(&data_root).expect("lock");
    let records = solstone_tmux::recovery::recover_capture_streams(
        &lock,
        &data_root,
        Duration::from_secs(300),
    )
    .expect("recover");
    assert!(records.iter().any(
        |r| r.action == solstone_tmux::recovery::RecoveryAction::Failed
            && r.detail.contains("finalized target collision")
    ));

    let t1 = PrimitiveDateTime::new(
        Date::from_calendar_date(2026, Month::October, 25).expect("date"),
        Time::from_hms(1, 30, 0).expect("time"),
    )
    .assume_utc();
    clock.set_wall(t1);
    clock.set_monotonic(interval * 2);

    let mut manager2 = SegmentManager::start(
        data_root.clone(),
        stream,
        clock.as_ref(),
        interval,
        SyncWake::default(),
        Box::new(BerlinZoneSource::new()),
        Arc::new(StderrWarnings),
    )
    .expect("start manager 2");
    manager2
        .process_poll(&[golden_capture("main")], t1, interval * 2, interval)
        .expect("poll t1");

    assert_eq!(std::fs::read(&jsonl_path).expect("read jsonl"), snap_jsonl);
    assert_eq!(std::fs::read(&zone_path).expect("read zone"), snap_zone);
    assert_eq!(std::fs::read(&ack_path).expect("read ack"), snap_ack);
    assert_eq!(
        std::fs::read(&stranded_meta).expect("read stranded meta"),
        snap_stranded_meta
    );
    assert_eq!(
        std::fs::read(&stranded_jsonl_path).expect("read stranded jsonl"),
        snap_stranded_jsonl
    );

    assert!(!day_stream_dir.join("040000.failed").exists());
    assert!(!day_stream_dir.join("040000.failed.meta").exists());
}

#[test]
fn bump_into_next_day_that_cannot_be_created_falls_back_to_the_natural_name() {
    let kolkata_bytes = std::fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/asia-kolkata.tzif"),
    )
    .expect("read kolkata tzif");
    let kolkata_zone = Zone::from_tzif("Asia/Kolkata", &kolkata_bytes).expect("kolkata zone");
    struct KolkataZoneSource(Zone);
    impl ZoneSource for KolkataZoneSource {
        fn read(&mut self) -> Result<Zone, String> {
            Ok(self.0.clone())
        }
    }

    let t_kolkata = PrimitiveDateTime::new(
        Date::from_calendar_date(2026, Month::September, 29).expect("date"),
        Time::from_hms(18, 29, 59).expect("time"),
    )
    .assume_utc();
    let clock = Arc::new(TestClock::new(
        t_kolkata,
        Duration::ZERO,
        UtcOffset::from_hms(5, 30, 0).expect("offset +5:30"),
    ));
    let temporary = TestDirectory::new("bump-next-day-uncreatable");
    let data_root = temporary.path().join("data");
    let stream = derive_component("test.tmux").expect("stream");
    let day_stream_29 = data_root
        .join("captures")
        .join("20260929")
        .join("test.tmux");
    std::fs::create_dir_all(day_stream_29.join("235959_300")).expect("plant 235959");
    // A regular file where the next day's directory would go makes creating it fail.
    std::fs::write(
        data_root.join("captures").join("20260930"),
        b"not a directory",
    )
    .expect("plant blocking file");

    let mut manager = SegmentManager::start(
        data_root.clone(),
        stream,
        clock.as_ref(),
        Duration::from_secs(300),
        SyncWake::default(),
        Box::new(KolkataZoneSource(kolkata_zone)),
        Arc::new(StderrWarnings),
    )
    .expect("an uncreatable next-day directory must not stop capture");
    let incomplete = manager.segment_mut().incomplete_dir().to_path_buf();
    assert_eq!(incomplete, day_stream_29.join("235959.incomplete"));
    assert!(incomplete.is_dir());
    assert!(day_stream_29.join("235959_300").is_dir());
}
