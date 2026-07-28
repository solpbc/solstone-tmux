// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::ffi::OsString;
use std::fmt;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::time::Duration;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommandOperation {
    Tmux(TmuxOperation),
    Service(ServiceOperation),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TmuxOperation {
    ListClients,
    ListWindows(String),
    ListPanes(String),
    CapturePane(String),
    ShowOption(String),
    SetOption(String),
    UnsetOption(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServiceOperation {
    SystemdStop,
    SystemdDaemonReload,
    SystemdEnableNow,
    SystemdIsActive,
    SystemdDisableNow,
    LaunchdBootout,
    LaunchdEnable,
    LaunchdBootstrap,
    LaunchdPrint,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandInvocation {
    pub operation: CommandOperation,
    pub executable: PathBuf,
    pub args: Vec<OsString>,
    pub timeout: Duration,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub status: i32,
}

#[derive(Debug)]
pub enum CommandError {
    Spawn(std::io::Error),
    Timeout {
        operation: CommandOperation,
        duration: Duration,
    },
}

impl fmt::Display for CommandError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spawn(error) => write!(formatter, "could not start command: {error}"),
            Self::Timeout {
                operation,
                duration,
            } => write!(
                formatter,
                "{operation:?} exceeded its {} second timeout",
                duration.as_secs()
            ),
        }
    }
}

impl std::error::Error for CommandError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Spawn(error) => Some(error),
            Self::Timeout { .. } => None,
        }
    }
}

pub trait CommandRunner: Send + Sync {
    fn run<'a>(
        &'a self,
        invocation: CommandInvocation,
    ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, CommandError>> + Send + 'a>>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct TokioCommandRunner;

impl CommandRunner for TokioCommandRunner {
    fn run<'a>(
        &'a self,
        invocation: CommandInvocation,
    ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, CommandError>> + Send + 'a>> {
        Box::pin(async move {
            let mut command = tokio::process::Command::new(&invocation.executable);
            command
                .args(&invocation.args)
                .kill_on_drop(true)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
            let operation = invocation.operation;
            let duration = invocation.timeout;
            let output = tokio::time::timeout(duration, command.output())
                .await
                .map_err(|_| CommandError::Timeout {
                    operation,
                    duration,
                })?
                .map_err(CommandError::Spawn)?;
            Ok(CommandOutput {
                stdout: output.stdout,
                stderr: output.stderr,
                status: output.status.code().unwrap_or(-1),
            })
        })
    }
}
