// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use crate::command::{
    CommandError, CommandInvocation, CommandOperation, CommandOutput, CommandRunner, TmuxOperation,
};
use crate::model::{CaptureResult, ClientInfo, PaneInfo, WindowInfo};
use crate::name::derive_component;

pub const TMUX_TIMEOUT: Duration = Duration::from_secs(5);

pub trait WarningSink: Send + Sync {
    fn warn(&self, message: &str);
}

#[derive(Clone, Copy, Debug, Default)]
pub struct StderrWarnings;

impl WarningSink for StderrWarnings {
    fn warn(&self, message: &str) {
        eprintln!("solstone-tmux: warning: {message}");
    }
}

#[derive(Debug)]
pub enum TmuxError {
    ExecutableMustBeAbsolute(PathBuf),
    Command(CommandError),
    Nonzero {
        operation: TmuxOperation,
        status: i32,
        stderr: String,
    },
    InvalidUtf8 {
        operation: TmuxOperation,
        source: std::string::FromUtf8Error,
    },
    Malformed {
        operation: TmuxOperation,
        detail: String,
    },
}

impl fmt::Display for TmuxError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ExecutableMustBeAbsolute(path) => {
                write!(
                    formatter,
                    "tmux executable must be absolute: {}",
                    path.display()
                )
            }
            Self::Command(error) => error.fmt(formatter),
            Self::Nonzero {
                operation,
                status,
                stderr,
            } => write!(
                formatter,
                "{operation:?} exited with status {status}: {}",
                stderr.trim_end()
            ),
            Self::InvalidUtf8 { operation, source } => {
                write!(
                    formatter,
                    "{operation:?} returned non-UTF-8 output: {source}"
                )
            }
            Self::Malformed { operation, detail } => {
                write!(formatter, "malformed {operation:?} output: {detail}")
            }
        }
    }
}

impl std::error::Error for TmuxError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Command(error) => Some(error),
            Self::InvalidUtf8 { source, .. } => Some(source),
            _ => None,
        }
    }
}

pub struct TmuxAdapter<R, W = StderrWarnings> {
    runner: R,
    warnings: W,
    executable: PathBuf,
}

impl<R> TmuxAdapter<R, StderrWarnings>
where
    R: CommandRunner,
{
    pub fn new(executable: PathBuf, runner: R) -> Result<Self, TmuxError> {
        Self::with_warnings(executable, runner, StderrWarnings)
    }
}

impl<R, W> TmuxAdapter<R, W>
where
    R: CommandRunner,
    W: WarningSink,
{
    pub fn with_warnings(executable: PathBuf, runner: R, warnings: W) -> Result<Self, TmuxError> {
        if !executable.is_absolute() {
            return Err(TmuxError::ExecutableMustBeAbsolute(executable));
        }
        Ok(Self {
            runner,
            warnings,
            executable,
        })
    }

    pub async fn list_clients(&self) -> Result<Vec<ClientInfo>, TmuxError> {
        let operation = TmuxOperation::ListClients;
        let output = self
            .run(
                operation.clone(),
                [
                    OsString::from("list-clients"),
                    OsString::from("-F"),
                    OsString::from("#{client_session} #{client_activity}"),
                ],
            )
            .await?;
        let stdout = String::from_utf8(output.stdout).map_err(|source| TmuxError::InvalidUtf8 {
            operation: operation.clone(),
            source,
        })?;

        let mut clients = Vec::new();
        let mut seen = HashMap::new();
        for row in stdout.lines() {
            if row.is_empty() {
                self.warn_client_row(row, "empty row");
                continue;
            }
            let Some((session, activity)) = row.rsplit_once(' ') else {
                self.warn_client_row(row, "missing activity separator");
                continue;
            };
            if session.is_empty() {
                self.warn_client_row(row, "empty session identity");
                continue;
            }
            let Ok(activity) = activity.parse::<i64>() else {
                self.warn_client_row(row, "activity is not an integer");
                continue;
            };
            if let Some(previous) = seen.get(session) {
                if *previous != activity {
                    self.warn_client_row(row, "duplicate session has contradictory activity");
                }
                continue;
            }
            seen.insert(session.to_owned(), activity);
            clients.push(ClientInfo {
                session: session.to_owned(),
                activity,
            });
        }
        Ok(clients)
    }

    pub async fn list_active_clients(
        &self,
        now_unix_seconds: i64,
        poll_interval: Duration,
    ) -> Result<Vec<ClientInfo>, TmuxError> {
        let maximum_age = i128::from(poll_interval.as_secs());
        Ok(self
            .list_clients()
            .await?
            .into_iter()
            .filter(|client| {
                i128::from(now_unix_seconds) - i128::from(client.activity) <= maximum_age
            })
            .collect())
    }

    pub async fn capture_session(&self, session: &str) -> Result<CaptureResult, TmuxError> {
        let windows = self.list_windows(session).await?;
        let active_window = windows
            .iter()
            .find(|window| window.active)
            .expect("validated window set has one active window")
            .clone();
        let mut panes = self.list_panes(&active_window.id).await?;
        for pane in &mut panes {
            pane.content = self.capture_pane(&pane.id).await?;
        }
        Ok(CaptureResult {
            session: session.to_owned(),
            window: active_window,
            windows,
            panes,
        })
    }

    pub async fn capture_sessions(&self, clients: &[ClientInfo]) -> Vec<CaptureResult> {
        let mut captures = Vec::new();
        for client in clients {
            if let Err(error) = derive_component(&client.session) {
                self.warnings.warn(&format!(
                    "session {:?} was skipped before observation: {error}",
                    client.session
                ));
                continue;
            }
            match self.capture_session(&client.session).await {
                Ok(capture) => captures.push(capture),
                Err(error) => self.warnings.warn(&format!(
                    "session {:?} observation failed: {error}",
                    client.session
                )),
            }
        }
        captures
    }

    async fn list_windows(&self, session: &str) -> Result<Vec<WindowInfo>, TmuxError> {
        let operation = TmuxOperation::ListWindows(session.to_owned());
        let output = self
            .run(
                operation.clone(),
                [
                    OsString::from("list-windows"),
                    OsString::from("-t"),
                    OsString::from(session),
                    OsString::from("-F"),
                    OsString::from("#{window_active} #{window_id} #{window_index} #{window_name}"),
                ],
            )
            .await?;
        let stdout = String::from_utf8(output.stdout).map_err(|source| TmuxError::InvalidUtf8 {
            operation: operation.clone(),
            source,
        })?;
        parse_windows(&stdout, operation)
    }

    async fn list_panes(&self, window_id: &str) -> Result<Vec<PaneInfo>, TmuxError> {
        let operation = TmuxOperation::ListPanes(window_id.to_owned());
        let output = self
            .run(
                operation.clone(),
                [
                    OsString::from("list-panes"),
                    OsString::from("-t"),
                    OsString::from(window_id),
                    OsString::from("-F"),
                    OsString::from(
                        "#{pane_id} #{pane_index} #{pane_left} #{pane_top} #{pane_width} #{pane_height} #{pane_active}",
                    ),
                ],
            )
            .await?;
        let stdout = String::from_utf8(output.stdout).map_err(|source| TmuxError::InvalidUtf8 {
            operation: operation.clone(),
            source,
        })?;
        parse_panes(&stdout, operation)
    }

    async fn capture_pane(&self, pane_id: &str) -> Result<String, TmuxError> {
        let operation = TmuxOperation::CapturePane(pane_id.to_owned());
        let output = self
            .run(
                operation.clone(),
                [
                    OsString::from("capture-pane"),
                    OsString::from("-p"),
                    OsString::from("-e"),
                    OsString::from("-t"),
                    OsString::from(pane_id),
                ],
            )
            .await?;
        String::from_utf8(output.stdout)
            .map_err(|source| TmuxError::InvalidUtf8 { operation, source })
    }

    async fn run<const N: usize>(
        &self,
        operation: TmuxOperation,
        args: [OsString; N],
    ) -> Result<CommandOutput, TmuxError> {
        let output = self
            .runner
            .run(CommandInvocation {
                operation: CommandOperation::Tmux(operation.clone()),
                executable: self.executable.clone(),
                args: args.into(),
                timeout: TMUX_TIMEOUT,
            })
            .await
            .map_err(TmuxError::Command)?;
        if output.status != 0 {
            return Err(TmuxError::Nonzero {
                operation,
                status: output.status,
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }
        Ok(output)
    }

    fn warn_client_row(&self, row: &str, reason: &str) {
        self.warnings.warn(&format!(
            "dropping malformed list-clients row {row:?}: {reason}"
        ));
    }
}

fn parse_windows(stdout: &str, operation: TmuxOperation) -> Result<Vec<WindowInfo>, TmuxError> {
    if stdout.is_empty() {
        return malformed(operation, "empty successful output");
    }
    let mut windows = Vec::new();
    let mut ids = HashSet::new();
    let mut indexes = HashSet::new();
    for row in stdout.lines() {
        let fields = row.splitn(4, ' ').collect::<Vec<_>>();
        if fields.len() != 4 || fields[1].is_empty() || fields[3].is_empty() {
            return malformed(operation, format!("invalid row {row:?}"));
        }
        let active = parse_active(fields[0])
            .ok_or_else(|| malformed_error(operation.clone(), format!("invalid row {row:?}")))?;
        let index = fields[2]
            .parse::<u32>()
            .map_err(|_| malformed_error(operation.clone(), format!("invalid row {row:?}")))?;
        if !ids.insert(fields[1].to_owned()) || !indexes.insert(index) {
            return malformed(operation, format!("duplicate ID or index in row {row:?}"));
        }
        windows.push(WindowInfo {
            id: fields[1].to_owned(),
            index,
            name: fields[3].to_owned(),
            active,
        });
    }
    if windows.is_empty() {
        return malformed(operation, "empty successful output");
    }
    if windows.iter().filter(|window| window.active).count() != 1 {
        return malformed(operation, "expected exactly one active window");
    }
    Ok(windows)
}

fn parse_panes(stdout: &str, operation: TmuxOperation) -> Result<Vec<PaneInfo>, TmuxError> {
    if stdout.is_empty() {
        return malformed(operation, "empty successful output");
    }
    let mut panes = Vec::new();
    let mut ids = HashSet::new();
    let mut indexes = HashSet::new();
    for row in stdout.lines() {
        let fields = row.split(' ').collect::<Vec<_>>();
        if fields.len() != 7 || fields[0].is_empty() {
            return malformed(operation, format!("invalid row {row:?}"));
        }
        let index = parse_u32(fields[1], &operation, row)?;
        let left = parse_u32(fields[2], &operation, row)?;
        let top = parse_u32(fields[3], &operation, row)?;
        let width = parse_u32(fields[4], &operation, row)?;
        let height = parse_u32(fields[5], &operation, row)?;
        if width == 0 || height == 0 {
            return malformed(operation, format!("nonpositive pane size in row {row:?}"));
        }
        let active = parse_active(fields[6])
            .ok_or_else(|| malformed_error(operation.clone(), format!("invalid row {row:?}")))?;
        if !ids.insert(fields[0].to_owned()) || !indexes.insert(index) {
            return malformed(operation, format!("duplicate ID or index in row {row:?}"));
        }
        panes.push(PaneInfo {
            id: fields[0].to_owned(),
            index,
            left,
            top,
            width,
            height,
            active,
            content: String::new(),
        });
    }
    if panes.is_empty() {
        return malformed(operation, "empty successful output");
    }
    if panes.iter().filter(|pane| pane.active).count() != 1 {
        return malformed(operation, "expected exactly one active pane");
    }
    Ok(panes)
}

fn parse_u32(value: &str, operation: &TmuxOperation, row: &str) -> Result<u32, TmuxError> {
    value
        .parse::<u32>()
        .map_err(|_| malformed_error(operation.clone(), format!("invalid row {row:?}")))
}

fn parse_active(value: &str) -> Option<bool> {
    match value {
        "0" => Some(false),
        "1" => Some(true),
        _ => None,
    }
}

fn malformed<T>(operation: TmuxOperation, detail: impl Into<String>) -> Result<T, TmuxError> {
    Err(malformed_error(operation, detail))
}

fn malformed_error(operation: TmuxOperation, detail: impl Into<String>) -> TmuxError {
    TmuxError::Malformed {
        operation,
        detail: detail.into(),
    }
}
