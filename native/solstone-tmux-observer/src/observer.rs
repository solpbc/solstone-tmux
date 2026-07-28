// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use time::{OffsetDateTime, UtcOffset};
use tokio::task::{JoinError, JoinSet};

use crate::clock::Clock;
use crate::command::CommandRunner;
use crate::indicator::{IndicatorIo, IndicatorOwnership};
use crate::instance_lock::{InstanceLock, InstanceLockError};
use crate::model::CaptureResult;
use crate::paths::ensure_private_directory;
use crate::segment::{SegmentClose, SegmentError, SegmentState};
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
    stream_dir: PathBuf,
    local_offset: UtcOffset,
}

impl SegmentManager {
    pub fn new(segment: SegmentState, local_offset: UtcOffset) -> Self {
        let stream_dir = segment.stream_dir().to_owned();
        Self {
            segment,
            stream_dir,
            local_offset,
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
            self.segment
                .finalize(monotonic_now)
                .map_err(operation_error)?;
            self.segment =
                SegmentState::create(&self.stream_dir, wall_now, monotonic_now, self.local_offset)
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
                        "solstone-tmux-observer: warning: durable append for session {:?} rolled back and will retry: {error}",
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
        self.segment
            .finalize(monotonic_now)
            .map_err(operation_error)
    }
}

fn operation_error(error: SegmentError) -> ObserverOperationError {
    ObserverOperationError(error.to_string())
}

pub trait ShutdownIndicator: Send {
    fn restore<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<(), ObserverOperationError>> + Send + 'a>>;
}

impl<I: IndicatorIo> ShutdownIndicator for IndicatorOwnership<I> {
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

pub async fn acquire_run_lock(data_root: &Path) -> Result<InstanceLock, ObserverOperationError> {
    let data_root = data_root.to_owned();
    let handle = tokio::task::spawn_blocking(move || {
        ensure_private_directory(&data_root)
            .map_err(|error| ObserverOperationError(error.to_string()))?;
        InstanceLock::acquire(&data_root).map_err(lock_error)
    });
    handle
        .await
        .map_err(|error| ObserverOperationError(join_error_message(error)))?
}

fn lock_error(error: InstanceLockError) -> ObserverOperationError {
    ObserverOperationError(error.to_string())
}

pub async fn run_observer(
    provider: Arc<dyn CaptureProvider>,
    mut segment: Box<dyn SegmentLifecycle>,
    mut indicator: Box<dyn ShutdownIndicator>,
    instance_lock: Box<dyn LifecycleLock>,
    clock: Arc<dyn Clock>,
    mut shutdown: Pin<Box<dyn Future<Output = ShutdownEvent> + Send>>,
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
                return finish_without_segment(indicator, instance_lock, failures, None).await;
            }
        }
    };

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
    if let Err(error) = indicator.restore().await {
        failures.push(format!("indicator shutdown failed: {error}"));
    }
    drop(indicator);
    drop(instance_lock);

    ObserverExit {
        exit_code: i32::from(!failures.is_empty()),
        shutdown_event,
        failures,
    }
}

async fn finish_without_segment(
    mut indicator: Box<dyn ShutdownIndicator>,
    instance_lock: Box<dyn LifecycleLock>,
    mut failures: Vec<String>,
    shutdown_event: Option<ShutdownEvent>,
) -> ObserverExit {
    if let Err(error) = indicator.restore().await {
        failures.push(format!("indicator shutdown failed: {error}"));
    }
    drop(indicator);
    drop(instance_lock);
    ObserverExit {
        exit_code: 1,
        shutdown_event,
        failures,
    }
}

pub async fn supervise_observer<F>(observer: F) -> ObserverExit
where
    F: Future<Output = ObserverExit> + Send + 'static,
{
    let mut tasks = JoinSet::new();
    tasks.spawn(observer);
    match tasks.join_next().await {
        Some(Ok(exit)) => exit,
        Some(Err(error)) => ObserverExit {
            exit_code: 1,
            shutdown_event: None,
            failures: vec![format!(
                "observer task failed: {}",
                join_error_message(error)
            )],
        },
        None => ObserverExit {
            exit_code: 1,
            shutdown_event: None,
            failures: vec!["observer task exited without a result".to_owned()],
        },
    }
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
