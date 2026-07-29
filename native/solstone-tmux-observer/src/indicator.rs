// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::ffi::OsString;
use std::fmt;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

use crate::command::{CommandInvocation, CommandOperation, CommandRunner, TmuxOperation};
use crate::tmux::TMUX_TIMEOUT;

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
            if output.status != 0 {
                return if name == SOLSTONE_OPTION {
                    Ok(OptionValue::Absent)
                } else {
                    Err(IndicatorError::Nonzero {
                        name: name.to_owned(),
                        status: output.status,
                    })
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
            } else {
                Err(IndicatorError::Nonzero {
                    name: name.to_owned(),
                    status: output.status,
                })
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
            } else {
                Err(IndicatorError::Nonzero {
                    name: name.to_owned(),
                    status: output.status,
                })
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
        let original_status_left = io.read(STATUS_LEFT).await?;
        let original_solstone = io.read(SOLSTONE_OPTION).await?;
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
        self.io.write(SOLSTONE_OPTION, &value).await?;
        self.written_solstone = value;
        Ok(true)
    }

    pub async fn restore(&mut self) -> Result<(), IndicatorRestoreError> {
        let mut failures = Vec::new();
        match self.io.read(STATUS_LEFT).await {
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
                Ok(_) => {}
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
    Nonzero { name: String, status: i32 },
    Protocol(String),
}

impl fmt::Display for IndicatorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Command(error) => error.fmt(formatter),
            Self::Nonzero { name, status } => {
                write!(formatter, "tmux option command for {name} exited {status}")
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
