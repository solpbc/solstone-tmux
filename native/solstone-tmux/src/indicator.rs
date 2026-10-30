// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::ffi::OsString;
use std::fmt;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::time::Duration;

use crate::command::{
    CommandInvocation, CommandOperation, CommandOutput, CommandRunner, TmuxOperation,
};
use crate::tmux::{TMUX_TIMEOUT, reports_no_server};

pub const STATUS_LEFT: &str = "status-left";
pub const SOLSTONE_OPTION: &str = "@solstone";
pub const OBSERVING_VALUE: &str = "observing";
pub const SYNCING_VALUE: &str = "syncing";
const INDICATOR_PREFIX: &str = "#{?@solstone,#{?#{==:#{@solstone},syncing},#[fg=yellow]☼#[default],#[fg=colour245]☼#[default]},}";

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OptionValue {
    Absent,
    Present(String),
}

pub trait IndicatorIo: Send + Sync {
    /// Identifies the running tmux server, so a replaced server is noticed.
    fn server_identity<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<String, IndicatorError>> + Send + 'a>>;

    fn read<'a>(
        &'a self,
        name: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<OptionValue, IndicatorError>> + Send + 'a>>;

    fn write<'a>(
        &'a self,
        name: &'a str,
        value: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), IndicatorError>> + Send + 'a>>;

    fn clear<'a>(
        &'a self,
        name: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), IndicatorError>> + Send + 'a>>;
}

#[derive(Clone)]
pub struct CommandIndicatorIo<R> {
    runner: R,
    tmux: PathBuf,
}

impl<R: CommandRunner> CommandIndicatorIo<R> {
    pub fn new(runner: R, tmux: PathBuf) -> Result<Self, IndicatorError> {
        if !tmux.is_absolute() {
            return Err(IndicatorError::Protocol(
                "tmux executable must be absolute".to_owned(),
            ));
        }
        Ok(Self { runner, tmux })
    }
}

impl<R: CommandRunner> IndicatorIo for CommandIndicatorIo<R> {
    fn server_identity<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<String, IndicatorError>> + Send + 'a>> {
        Box::pin(async move {
            let output = self
                .runner
                .run(CommandInvocation {
                    operation: CommandOperation::Tmux(TmuxOperation::ServerIdentity),
                    executable: self.tmux.clone(),
                    args: ["display-message", "-p", "#{pid} #{start_time}"]
                        .into_iter()
                        .map(OsString::from)
                        .collect(),
                    timeout: TMUX_TIMEOUT,
                })
                .await
                .map_err(IndicatorError::Command)?;
            if reports_no_server(&output) {
                return Err(IndicatorError::NoServer);
            }
            if output.status != 0 {
                return Err(IndicatorError::nonzero("server identity", &output));
            }
            let stdout = String::from_utf8(output.stdout)
                .map_err(|error| IndicatorError::Protocol(error.to_string()))?;
            Ok(stdout.trim_end().to_owned())
        })
    }

    fn read<'a>(
        &'a self,
        name: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<OptionValue, IndicatorError>> + Send + 'a>> {
        Box::pin(async move {
            let output = self
                .runner
                .run(CommandInvocation {
                    operation: CommandOperation::Tmux(TmuxOperation::ShowOption(name.to_owned())),
                    executable: self.tmux.clone(),
                    args: ["show-options", "-gv", name]
                        .into_iter()
                        .map(OsString::from)
                        .collect(),
                    timeout: TMUX_TIMEOUT,
                })
                .await
                .map_err(IndicatorError::Command)?;
            if reports_no_server(&output) {
                return Err(IndicatorError::NoServer);
            }
            if output.status != 0 {
                return if name == SOLSTONE_OPTION {
                    Ok(OptionValue::Absent)
                } else {
                    Err(IndicatorError::nonzero(name, &output))
                };
            }
            let stdout = String::from_utf8(output.stdout)
                .map_err(|error| IndicatorError::Protocol(error.to_string()))?;
            let value = stdout.strip_suffix('\n').unwrap_or(&stdout);
            Ok(OptionValue::Present(value.to_owned()))
        })
    }

    fn write<'a>(
        &'a self,
        name: &'a str,
        value: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), IndicatorError>> + Send + 'a>> {
        Box::pin(async move {
            let output = self
                .runner
                .run(CommandInvocation {
                    operation: CommandOperation::Tmux(TmuxOperation::SetOption(name.to_owned())),
                    executable: self.tmux.clone(),
                    args: ["set-option", "-g", name, value]
                        .into_iter()
                        .map(OsString::from)
                        .collect(),
                    timeout: TMUX_TIMEOUT,
                })
                .await
                .map_err(IndicatorError::Command)?;
            if output.status == 0 {
                Ok(())
            } else if reports_no_server(&output) {
                Err(IndicatorError::NoServer)
            } else {
                Err(IndicatorError::nonzero(name, &output))
            }
        })
    }

    fn clear<'a>(
        &'a self,
        name: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), IndicatorError>> + Send + 'a>> {
        Box::pin(async move {
            let output = self
                .runner
                .run(CommandInvocation {
                    operation: CommandOperation::Tmux(TmuxOperation::UnsetOption(name.to_owned())),
                    executable: self.tmux.clone(),
                    args: ["set-option", "-gu", name]
                        .into_iter()
                        .map(OsString::from)
                        .collect(),
                    timeout: TMUX_TIMEOUT,
                })
                .await
                .map_err(IndicatorError::Command)?;
            if output.status == 0 {
                Ok(())
            } else if reports_no_server(&output) {
                Err(IndicatorError::NoServer)
            } else {
                Err(IndicatorError::nonzero(name, &output))
            }
        })
    }
}

pub struct IndicatorOwnership<I> {
    io: I,
    original_status_left: OptionValue,
    original_solstone: OptionValue,
    written_status_left: String,
    written_solstone: String,
    solstone_owned: bool,
}

impl<I: IndicatorIo> IndicatorOwnership<I> {
    pub async fn install_default(io: I) -> Result<Self, IndicatorError> {
        let original_status_left = normalize_status_left(io.read(STATUS_LEFT).await?);
        let original_solstone = normalize_solstone(io.read(SOLSTONE_OPTION).await?);
        let status_left = match &original_status_left {
            OptionValue::Absent => INDICATOR_PREFIX.to_owned(),
            OptionValue::Present(value) => format!("{INDICATOR_PREFIX}{value}"),
        };
        Self::install_with_originals(
            io,
            original_status_left,
            original_solstone,
            status_left,
            OBSERVING_VALUE.to_owned(),
        )
        .await
    }

    pub async fn install(
        io: I,
        written_status_left: String,
        written_solstone: String,
    ) -> Result<Self, IndicatorError> {
        let original_status_left = io.read(STATUS_LEFT).await?;
        let original_solstone = io.read(SOLSTONE_OPTION).await?;
        Self::install_with_originals(
            io,
            original_status_left,
            original_solstone,
            written_status_left,
            written_solstone,
        )
        .await
    }

    async fn install_with_originals(
        io: I,
        original_status_left: OptionValue,
        original_solstone: OptionValue,
        written_status_left: String,
        written_solstone: String,
    ) -> Result<Self, IndicatorError> {
        io.write(STATUS_LEFT, &written_status_left).await?;
        if let Err(error) = io.write(SOLSTONE_OPTION, &written_solstone).await {
            if matches!(error, IndicatorError::NoServer) {
                return Err(error);
            }
            if let Err(rollback) = restore_value(&io, STATUS_LEFT, &original_status_left).await {
                return Err(IndicatorError::Protocol(format!(
                    "{error}; status-left rollback also failed: {rollback}"
                )));
            }
            return Err(error);
        }
        Ok(Self {
            io,
            original_status_left,
            original_solstone,
            written_status_left,
            written_solstone,
            solstone_owned: true,
        })
    }

    pub async fn update_solstone(&mut self, value: String) -> Result<bool, IndicatorError> {
        if !self.solstone_owned {
            return Ok(false);
        }
        if self.io.read(SOLSTONE_OPTION).await?
            != OptionValue::Present(self.written_solstone.clone())
        {
            self.solstone_owned = false;
            return Ok(false);
        }
        if value == self.written_solstone {
            return Ok(true);
        }
        self.io.write(SOLSTONE_OPTION, &value).await?;
        self.written_solstone = value;
        Ok(true)
    }

    pub async fn restore(&mut self) -> Result<(), IndicatorRestoreError> {
        let mut failures = Vec::new();
        match self.io.read(STATUS_LEFT).await {
            // The server exited, and its options went with it.
            Err(IndicatorError::NoServer) => return Ok(()),
            Ok(current) if current == OptionValue::Present(self.written_status_left.clone()) => {
                if let Err(error) =
                    restore_value(&self.io, STATUS_LEFT, &self.original_status_left).await
                {
                    failures.push(format!("{STATUS_LEFT}: {error}"));
                }
            }
            Ok(_) => {}
            Err(error) => failures.push(format!("{STATUS_LEFT} read: {error}")),
        }
        if self.solstone_owned {
            match self.io.read(SOLSTONE_OPTION).await {
                Ok(current) if current == OptionValue::Present(self.written_solstone.clone()) => {
                    if let Err(error) =
                        restore_value(&self.io, SOLSTONE_OPTION, &self.original_solstone).await
                    {
                        failures.push(format!("{SOLSTONE_OPTION}: {error}"));
                    }
                }
                Ok(_) | Err(IndicatorError::NoServer) => {}
                Err(error) => failures.push(format!("{SOLSTONE_OPTION} read: {error}")),
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(IndicatorRestoreError(failures))
        }
    }
}

/// The indicator across the tmux server's lifecycle. With no server running
/// there is nothing to mark, so installation waits for one; a server that
/// exits takes its options with it, and the next server gets the indicator.
pub struct ServerIndicator<I> {
    io: I,
    installed: Option<(String, IndicatorOwnership<I>)>,
    working: bool,
    refresh_interval: Duration,
}

impl<I: IndicatorIo + Clone> ServerIndicator<I> {
    /// Installs now if a tmux server is running, and otherwise waits for one.
    /// Errors other than a missing server are returned.
    pub async fn start(io: I, refresh_interval: Duration) -> Result<Self, IndicatorError> {
        let mut indicator = Self {
            io,
            installed: None,
            working: false,
            refresh_interval,
        };
        indicator.refresh().await?;
        Ok(indicator)
    }

    pub fn is_installed(&self) -> bool {
        self.installed.is_some()
    }

    pub fn refresh_interval(&self) -> Duration {
        self.refresh_interval
    }

    /// Installs on a server that does not have the indicator yet, and forgets
    /// a server that has exited or been replaced.
    pub async fn refresh(&mut self) -> Result<(), IndicatorError> {
        let server = match self.io.server_identity().await {
            Ok(server) => server,
            Err(IndicatorError::NoServer) => {
                self.installed = None;
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        if self
            .installed
            .as_ref()
            .is_some_and(|(owned, _)| *owned == server)
        {
            return Ok(());
        }
        self.installed = None;
        let mut ownership = match IndicatorOwnership::install_default(self.io.clone()).await {
            Ok(ownership) => ownership,
            Err(IndicatorError::NoServer) => return Ok(()),
            Err(error) => return Err(error),
        };
        if self.working {
            match ownership.update_solstone(SYNCING_VALUE.to_owned()).await {
                Ok(_) => {}
                Err(IndicatorError::NoServer) => return Ok(()),
                Err(error) => return Err(error),
            }
        }
        self.installed = Some((server, ownership));
        Ok(())
    }

    pub async fn set_working(&mut self, working: bool) -> Result<(), IndicatorError> {
        self.working = working;
        let Some((_, ownership)) = self.installed.as_mut() else {
            return Ok(());
        };
        let value = if working {
            SYNCING_VALUE
        } else {
            OBSERVING_VALUE
        };
        match ownership.update_solstone(value.to_owned()).await {
            Ok(_) => Ok(()),
            Err(IndicatorError::NoServer) => {
                self.installed = None;
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    pub async fn restore(&mut self) -> Result<(), IndicatorRestoreError> {
        match self.installed.take() {
            Some((_, mut ownership)) => ownership.restore().await,
            None => Ok(()),
        }
    }
}

fn normalize_status_left(original: OptionValue) -> OptionValue {
    match original {
        OptionValue::Present(value) => {
            let mut base = value.as_str();
            while let Some(stripped) = base.strip_prefix(INDICATOR_PREFIX) {
                base = stripped;
            }
            OptionValue::Present(base.to_owned())
        }
        OptionValue::Absent => OptionValue::Absent,
    }
}

fn normalize_solstone(original: OptionValue) -> OptionValue {
    match original {
        OptionValue::Present(value)
            if value.is_empty() || value == OBSERVING_VALUE || value == SYNCING_VALUE =>
        {
            OptionValue::Absent
        }
        original => original,
    }
}

async fn restore_value(
    io: &dyn IndicatorIo,
    name: &str,
    original: &OptionValue,
) -> Result<(), IndicatorError> {
    match original {
        OptionValue::Absent => io.clear(name).await,
        OptionValue::Present(value) => io.write(name, value).await,
    }
}

#[derive(Debug)]
pub enum IndicatorError {
    Command(crate::command::CommandError),
    NoServer,
    Nonzero {
        name: String,
        status: i32,
        stderr: String,
    },
    Protocol(String),
}

impl IndicatorError {
    fn nonzero(name: &str, output: &CommandOutput) -> Self {
        Self::Nonzero {
            name: name.to_owned(),
            status: output.status,
            stderr: String::from_utf8_lossy(&output.stderr)
                .trim_end()
                .to_owned(),
        }
    }
}

impl fmt::Display for IndicatorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Command(error) => error.fmt(formatter),
            Self::NoServer => formatter.write_str("no tmux server is running"),
            Self::Nonzero {
                name,
                status,
                stderr,
            } => {
                write!(formatter, "tmux option command for {name} exited {status}")?;
                if stderr.is_empty() {
                    Ok(())
                } else {
                    write!(formatter, ": {stderr}")
                }
            }
            Self::Protocol(error) => formatter.write_str(error),
        }
    }
}

impl std::error::Error for IndicatorError {}

#[derive(Debug)]
pub struct IndicatorRestoreError(pub Vec<String>);

impl fmt::Display for IndicatorRestoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "indicator restoration failed: {}",
            self.0.join("; ")
        )
    }
}

impl std::error::Error for IndicatorRestoreError {}
