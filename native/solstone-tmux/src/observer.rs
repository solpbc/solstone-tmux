// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use time::{OffsetDateTime, UtcOffset};
use tokio::sync::{oneshot, watch};
use tokio::task::{JoinError, JoinSet};

use crate::clock::{Clock, local_date_and_time};
use crate::command::CommandRunner;
use crate::health::DiagnosticCode;
use crate::indicator::{IndicatorIo, IndicatorOwnership};
use crate::instance_lock::InstanceLock;
use crate::model::CaptureResult;
use crate::name::DerivedName;
use crate::segment::{SegmentClose, SegmentError, SegmentState};
use crate::sync::{SyncActivity, SyncWake};
use crate::tmux::{TmuxAdapter, WarningSink};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShutdownEvent {
    SigInt,
    SigTerm,
    Injected,
}

#[derive(Clone, Copy, Debug)]
pub struct ObserverConfig {
    pub capture_interval: Duration,
    pub segment_interval: Duration,
}

pub trait CaptureProvider: Send + Sync {
    fn poll<'a>(
        &'a self,
        wall_unix_seconds: i64,
        capture_interval: Duration,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<CaptureResult>, ObserverOperationError>> + Send + 'a>>;
}

impl<R, W> CaptureProvider for TmuxAdapter<R, W>
where
    R: CommandRunner + 'static,
    W: WarningSink + 'static,
{
    fn poll<'a>(
        &'a self,
        wall_unix_seconds: i64,
        capture_interval: Duration,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<CaptureResult>, ObserverOperationError>> + Send + 'a>>
    {
        Box::pin(async move {
            let clients = self
                .list_active_clients(wall_unix_seconds, capture_interval)
                .await
                .map_err(|error| ObserverOperationError(error.to_string()))?;
            Ok(self.capture_sessions(&clients).await)
        })
    }
}

pub trait SegmentLifecycle: Send {
    fn process_poll(
        &mut self,
        captures: &[CaptureResult],
        wall_now: OffsetDateTime,
        monotonic_now: Duration,
        segment_interval: Duration,
    ) -> Result<(), ObserverOperationError>;

    fn shutdown(&mut self, monotonic_now: Duration)
    -> Result<SegmentClose, ObserverOperationError>;
}

pub struct SegmentManager {
    segment: SegmentState,
    data_root: PathBuf,
    stream: DerivedName,
    local_offset: UtcOffset,
    sync_wake: SyncWake,
}

impl SegmentManager {
    pub fn new(
        segment: SegmentState,
        data_root: PathBuf,
        stream: DerivedName,
        local_offset: UtcOffset,
        sync_wake: SyncWake,
    ) -> Self {
        Self {
            segment,
            data_root,
            stream,
            local_offset,
            sync_wake,
        }
    }
}

impl SegmentLifecycle for SegmentManager {
    fn process_poll(
        &mut self,
        captures: &[CaptureResult],
        wall_now: OffsetDateTime,
        monotonic_now: Duration,
        segment_interval: Duration,
    ) -> Result<(), ObserverOperationError> {
        if self.segment.rotation_due(monotonic_now, segment_interval) {
            let close = self
                .segment
                .finalize(monotonic_now)
                .map_err(operation_error)?;
            self.sync_wake.segment_closed(&close);
            let stream_dir =
                stream_directory(&self.data_root, &self.stream, wall_now, self.local_offset)?;
            self.segment =
                SegmentState::create(&stream_dir, wall_now, monotonic_now, self.local_offset)
                    .map_err(operation_error)?;
        }
        let timestamp = self.segment.frame_timestamp(wall_now);
        for capture in captures {
            if let Err(error) = self
                .segment
                .append_capture(capture, timestamp, monotonic_now)
            {
                if error.is_recoverable_append() {
                    eprintln!(
                        "solstone-tmux: warning: durable append for session {:?} rolled back and will retry: {error}",
                        capture.session
                    );
                    continue;
                }
                return Err(operation_error(error));
            }
        }
        Ok(())
    }

    fn shutdown(
        &mut self,
        monotonic_now: Duration,
    ) -> Result<SegmentClose, ObserverOperationError> {
        let close = self
            .segment
            .finalize(monotonic_now)
            .map_err(operation_error)?;
        Ok(close)
    }
}

pub fn stream_directory(
    data_root: &Path,
    stream: &DerivedName,
    wall_now: OffsetDateTime,
    local_offset: UtcOffset,
) -> Result<PathBuf, ObserverOperationError> {
    let (date, _) = local_date_and_time(wall_now, local_offset);
    stream
        .join_checked(&data_root.join("captures").join(date))
        .map_err(|error| ObserverOperationError(error.to_string()))
}

fn operation_error(error: SegmentError) -> ObserverOperationError {
    ObserverOperationError(error.to_string())
}

pub trait ShutdownIndicator: Send {
    fn set_activity<'a>(
        &'a mut self,
        _activity: SyncActivity,
    ) -> Pin<Box<dyn Future<Output = Result<(), ObserverOperationError>> + Send + 'a>> {
        Box::pin(async { Ok(()) })
    }

    fn restore<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<(), ObserverOperationError>> + Send + 'a>>;
}

impl<I: IndicatorIo> ShutdownIndicator for IndicatorOwnership<I> {
    fn set_activity<'a>(
        &'a mut self,
        activity: SyncActivity,
    ) -> Pin<Box<dyn Future<Output = Result<(), ObserverOperationError>> + Send + 'a>> {
        Box::pin(async move {
            let value = match activity {
                SyncActivity::Idle => crate::indicator::OBSERVING_VALUE,
                SyncActivity::Working => crate::indicator::SYNCING_VALUE,
            };
            self.update_solstone(value.to_owned())
                .await
                .map(|_| ())
                .map_err(|_| {
                    ObserverOperationError(
                        DiagnosticCode::IndicatorUpdateFailed.message().to_owned(),
                    )
                })
        })
    }

    fn restore<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<(), ObserverOperationError>> + Send + 'a>> {
        Box::pin(async move {
            IndicatorOwnership::restore(self)
                .await
                .map_err(|error| ObserverOperationError(error.to_string()))
        })
    }
}

pub trait LifecycleLock: Send {}

impl LifecycleLock for InstanceLock {}

pub struct ObserverShutdownBarrier {
    request: Option<oneshot::Sender<()>>,
    release: oneshot::Receiver<()>,
}

pub struct SupervisorShutdownBarrier {
    request: oneshot::Receiver<()>,
    release: Option<oneshot::Sender<()>>,
}

pub fn shutdown_barrier() -> (ObserverShutdownBarrier, SupervisorShutdownBarrier) {
    let (request, requested) = oneshot::channel();
    let (release, released) = oneshot::channel();
    (
        ObserverShutdownBarrier {
            request: Some(request),
            release: released,
        },
        SupervisorShutdownBarrier {
            request: requested,
            release: Some(release),
        },
    )
}

pub struct SupervisionControl {
    pub activity: watch::Receiver<SyncActivity>,
    pub sync_stop: watch::Sender<bool>,
    pub observer_stop: watch::Sender<Option<ShutdownEvent>>,
    pub shutdown_barrier: SupervisorShutdownBarrier,
}

pub async fn run_observer(
    provider: Arc<dyn CaptureProvider>,
    mut segment: Box<dyn SegmentLifecycle>,
    clock: Arc<dyn Clock>,
    mut shutdown: Pin<Box<dyn Future<Output = ShutdownEvent> + Send>>,
    mut shutdown_barrier: ObserverShutdownBarrier,
    config: ObserverConfig,
) -> ObserverExit {
    let mut interval = tokio::time::interval(config.capture_interval);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut failures = Vec::new();
    let shutdown_event = loop {
        tokio::select! {
            biased;
            event = &mut shutdown => break Some(event),
            _ = interval.tick() => {}
        }

        let wall_now = clock.wall_now();
        let monotonic_now = clock.monotonic_now();
        let mut captures = JoinSet::new();
        let provider = Arc::clone(&provider);
        captures.spawn(async move {
            provider
                .poll(wall_now.unix_timestamp(), config.capture_interval)
                .await
        });
        let capture_result = tokio::select! {
            biased;
            event = &mut shutdown => {
                captures.abort_all();
                while captures.join_next().await.is_some() {}
                break Some(event);
            }
            result = captures.join_next() => result,
        };
        let captures = match capture_result {
            Some(Ok(Ok(captures))) => captures,
            Some(Ok(Err(error))) => {
                failures.push(format!("capture task exited: {error}"));
                break None;
            }
            Some(Err(error)) => {
                failures.push(format!(
                    "capture task failed: {}",
                    join_error_message(error)
                ));
                break None;
            }
            None => {
                failures.push("capture task exited without a result".to_owned());
                break None;
            }
        };

        let blocking = tokio::task::spawn_blocking(move || {
            let result =
                segment.process_poll(&captures, wall_now, monotonic_now, config.segment_interval);
            (segment, result)
        });
        match blocking.await {
            Ok((returned, Ok(()))) => segment = returned,
            Ok((returned, Err(error))) => {
                segment = returned;
                failures.push(format!("segment poll failed: {error}"));
                break None;
            }
            Err(error) => {
                failures.push(format!(
                    "blocking segment task failed: {}",
                    join_error_message(error)
                ));
                return ObserverExit {
                    exit_code: 1,
                    shutdown_event: None,
                    failures,
                };
            }
        }
    };

    if let Some(request) = shutdown_barrier.request.take() {
        let _ = request.send(());
    }
    let _ = shutdown_barrier.release.await;
    let monotonic_now = clock.monotonic_now();
    let blocking = tokio::task::spawn_blocking(move || {
        let result = segment.shutdown(monotonic_now);
        (segment, result)
    });
    match blocking.await {
        Ok((_returned, Ok(_))) => {}
        Ok((_returned, Err(error))) => failures.push(format!("segment shutdown failed: {error}")),
        Err(error) => failures.push(format!(
            "blocking segment shutdown failed: {}",
            join_error_message(error)
        )),
    }
    ObserverExit {
        exit_code: i32::from(!failures.is_empty()),
        shutdown_event,
        failures,
    }
}

pub async fn supervise_observer<O, S>(
    observer: O,
    sync: S,
    mut indicator: Box<dyn ShutdownIndicator>,
    instance_lock: Box<dyn LifecycleLock>,
    control: SupervisionControl,
) -> ObserverExit
where
    O: Future<Output = ObserverExit> + Send + 'static,
    S: Future<Output = Result<(), DiagnosticCode>> + Send + 'static,
{
    let SupervisionControl {
        mut activity,
        sync_stop,
        observer_stop,
        mut shutdown_barrier,
    } = control;
    let mut observer_task = tokio::spawn(observer);
    let mut sync_task = tokio::spawn(sync);
    let mut activity_open = true;
    let mut shutdown_request_open = true;
    let trigger = loop {
        tokio::select! {
            result = &mut observer_task => {
                break SupervisionTrigger::Observer(result);
            }
            result = &mut sync_task => {
                break SupervisionTrigger::Sync(result);
            }
            requested = &mut shutdown_barrier.request, if shutdown_request_open => {
                if requested.is_ok() {
                    break SupervisionTrigger::ObserverShutdown;
                } else {
                    shutdown_request_open = false;
                }
            }
            changed = activity.changed(), if activity_open => {
                if changed.is_err() {
                    activity_open = false;
                } else if indicator.set_activity(*activity.borrow_and_update()).await.is_err() {
                    break SupervisionTrigger::IndicatorFailed;
                }
            }
        }
    };

    let mut exit = match trigger {
        SupervisionTrigger::Observer(result) => {
            sync_stop.send_replace(true);
            let sync_result = sync_task.await;
            release_observer(&mut shutdown_barrier);
            let mut exit = observer_join_result(result);
            append_authorized_sync_failure(&mut exit, sync_result);
            exit
        }
        SupervisionTrigger::ObserverShutdown => {
            sync_stop.send_replace(true);
            let sync_result = sync_task.await;
            release_observer(&mut shutdown_barrier);
            let mut exit = observer_join_result(observer_task.await);
            append_authorized_sync_failure(&mut exit, sync_result);
            exit
        }
        SupervisionTrigger::Sync(result) => {
            observer_stop.send_replace(Some(ShutdownEvent::Injected));
            release_observer(&mut shutdown_barrier);
            let mut exit = ObserverExit {
                exit_code: 1,
                shutdown_event: None,
                failures: vec![unexpected_sync_failure(result)],
            };
            merge_observer_result(&mut exit, observer_task.await);
            exit
        }
        SupervisionTrigger::IndicatorFailed => {
            sync_stop.send_replace(true);
            observer_stop.send_replace(Some(ShutdownEvent::Injected));
            let sync_result = sync_task.await;
            release_observer(&mut shutdown_barrier);
            let mut exit = ObserverExit {
                exit_code: 1,
                shutdown_event: None,
                failures: vec![DiagnosticCode::IndicatorUpdateFailed.message().to_owned()],
            };
            append_authorized_sync_failure(&mut exit, sync_result);
            merge_observer_result(&mut exit, observer_task.await);
            exit
        }
    };
    if !exit.failures.is_empty() {
        exit.exit_code = 1;
    }
    finish_supervision(exit, indicator, instance_lock).await
}

enum SupervisionTrigger {
    Observer(Result<ObserverExit, JoinError>),
    Sync(Result<Result<(), DiagnosticCode>, JoinError>),
    ObserverShutdown,
    IndicatorFailed,
}

fn release_observer(barrier: &mut SupervisorShutdownBarrier) {
    if let Some(release) = barrier.release.take() {
        let _ = release.send(());
    }
}

fn observer_join_result(result: Result<ObserverExit, JoinError>) -> ObserverExit {
    match result {
        Ok(exit) => exit,
        Err(error) => ObserverExit {
            exit_code: 1,
            shutdown_event: None,
            failures: vec![format!(
                "observer task failed: {}",
                join_error_message(error)
            )],
        },
    }
}

fn merge_observer_result(exit: &mut ObserverExit, result: Result<ObserverExit, JoinError>) {
    let observer_exit = observer_join_result(result);
    exit.failures.extend(observer_exit.failures);
    exit.shutdown_event = observer_exit.shutdown_event;
}

fn append_authorized_sync_failure(
    exit: &mut ObserverExit,
    result: Result<Result<(), DiagnosticCode>, JoinError>,
) {
    match result {
        Ok(Ok(())) => {}
        Ok(Err(code)) => exit
            .failures
            .push(format!("sync task failed: {}", code.message())),
        Err(error) if error.is_panic() => exit
            .failures
            .push(DiagnosticCode::SyncTaskPanicked.message().to_owned()),
        Err(_) => exit
            .failures
            .push(DiagnosticCode::SyncTaskCancelled.message().to_owned()),
    }
}

fn unexpected_sync_failure(result: Result<Result<(), DiagnosticCode>, JoinError>) -> String {
    match result {
        Ok(Ok(())) => DiagnosticCode::SyncTaskExited.message().to_owned(),
        Ok(Err(code)) => format!("sync task failed: {}", code.message()),
        Err(error) if error.is_panic() => DiagnosticCode::SyncTaskPanicked.message().to_owned(),
        Err(_) => DiagnosticCode::SyncTaskCancelled.message().to_owned(),
    }
}

async fn finish_supervision(
    mut exit: ObserverExit,
    mut indicator: Box<dyn ShutdownIndicator>,
    instance_lock: Box<dyn LifecycleLock>,
) -> ObserverExit {
    if let Err(error) = indicator.restore().await {
        exit.failures
            .push(format!("indicator shutdown failed: {error}"));
        exit.exit_code = 1;
    }
    drop(indicator);
    drop(instance_lock);
    exit
}

pub fn production_shutdown_future()
-> Result<Pin<Box<dyn Future<Output = ShutdownEvent> + Send>>, ObserverOperationError> {
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
        .map_err(|error| {
            ObserverOperationError(format!("could not install SIGINT handler: {error}"))
        })?;
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(|error| {
            ObserverOperationError(format!("could not install SIGTERM handler: {error}"))
        })?;
    Ok(Box::pin(async move {
        tokio::select! {
            _ = sigint.recv() => ShutdownEvent::SigInt,
            _ = sigterm.recv() => ShutdownEvent::SigTerm,
        }
    }))
}

fn join_error_message(error: JoinError) -> String {
    if error.is_panic() {
        let payload = error.into_panic();
        if let Some(message) = payload.downcast_ref::<&str>() {
            return format!("panic: {message}");
        }
        if let Some(message) = payload.downcast_ref::<String>() {
            return format!("panic: {message}");
        }
        return "panic: non-string panic payload".to_owned();
    }
    if error.is_cancelled() {
        "task was cancelled".to_owned()
    } else {
        error.to_string()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObserverOperationError(pub String);

impl fmt::Display for ObserverOperationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ObserverOperationError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObserverExit {
    pub exit_code: i32,
    pub shutdown_event: Option<ShutdownEvent>,
    pub failures: Vec<String>,
}
