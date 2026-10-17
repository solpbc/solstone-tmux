// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::ffi::OsString;
use std::fmt;

pub use crate::pairing_answer::MarkOption;

pub const USAGE_EXIT_CODE: i32 = 2;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CliCommand {
    Run,
    Setup(MarkOption),
    Confirm(MarkOption),
    Status,
    About,
    InstallService,
    UninstallService,
    Help,
    Version,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CliError(crate::i18n::Message);

impl CliError {
    pub fn message(&self) -> &crate::i18n::Message {
        &self.0
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl std::error::Error for CliError {}

pub fn parse_args<I>(args: I) -> Result<CliCommand, CliError>
where
    I: IntoIterator<Item = OsString>,
{
    let mut args = args.into_iter();
    let _program = args.next();
    let next_arg = args.next();

    let (mut command, allows_mark) = match next_arg {
        None => (CliCommand::Run, false),
        Some(value) if value == "run" => (CliCommand::Run, false),
        Some(value) if value == "setup" => (CliCommand::Setup(MarkOption::Absent), true),
        Some(value) if value == "confirm" => (CliCommand::Confirm(MarkOption::Absent), true),
        Some(value) if value == "status" => (CliCommand::Status, false),
        Some(value) if value == "about" => (CliCommand::About, false),
        Some(value) if value == "install-service" => (CliCommand::InstallService, false),
        Some(value) if value == "uninstall-service" => (CliCommand::UninstallService, false),
        Some(value) if value == "-h" || value == "--help" => (CliCommand::Help, false),
        Some(value) if value == "-V" || value == "--version" => (CliCommand::Version, false),
        Some(value) => {
            return Err(CliError(crate::i18n::Message::new(
                crate::i18n::MessageId::CliCliParseArgsUnknownCommand,
                vec![
                    (
                        "arg0",
                        crate::i18n::ArgValue::Text((value.to_string_lossy()).to_string()),
                    ),
                    ("arg1", crate::i18n::ArgValue::Text((usage()).to_string())),
                ],
            )));
        }
    };

    if allows_mark {
        let mut mark_state = MarkOption::Absent;
        while let Some(arg) = args.next() {
            if arg == "--mark" {
                match mark_state {
                    MarkOption::Absent => match args.next() {
                        Some(val) if val == "--mark" => {
                            // Empty/missing value or immediately repeated --mark
                            mark_state = MarkOption::Repeated;
                        }
                        Some(val) => {
                            mark_state = MarkOption::Value(val.to_string_lossy().into_owned());
                        }
                        None => {
                            mark_state = MarkOption::MissingValue;
                        }
                    },
                    MarkOption::Value(_) | MarkOption::MissingValue | MarkOption::Repeated => {
                        let _ = args.next(); // consume next if any
                        mark_state = MarkOption::Repeated;
                    }
                }
            } else {
                return Err(CliError(crate::i18n::Message::new(
                    crate::i18n::MessageId::CliCliParseArgsUnexpectedArgument,
                    vec![
                        (
                            "arg0",
                            crate::i18n::ArgValue::Text((arg.to_string_lossy()).to_string()),
                        ),
                        ("arg1", crate::i18n::ArgValue::Text((usage()).to_string())),
                    ],
                )));
            }
        }
        match command {
            CliCommand::Setup(_) => command = CliCommand::Setup(mark_state),
            CliCommand::Confirm(_) => command = CliCommand::Confirm(mark_state),
            _ => {}
        }
    } else if let Some(value) = args.next() {
        return Err(CliError(crate::i18n::Message::new(
            crate::i18n::MessageId::CliCliParseArgsUnexpectedArgument,
            vec![
                (
                    "arg0",
                    crate::i18n::ArgValue::Text((value.to_string_lossy()).to_string()),
                ),
                ("arg1", crate::i18n::ArgValue::Text((usage()).to_string())),
            ],
        )));
    }

    Ok(command)
}

pub fn usage() -> String {
    crate::i18n::render(
        crate::i18n::MessageId::CliCliUsageUsageSolstoneTmuxRunSetupConfirmStatusAbout,
        &[(
            "arg0",
            crate::i18n::ArgValue::Text((crate::pairing_answer::MARK_HELP).to_string()),
        )],
    )
}

pub fn version() -> String {
    let source = option_env!("SOLSTONE_TMUX_SOURCE_COMMIT").unwrap_or("development");
    crate::i18n::render(
        crate::i18n::MessageId::CliCliVersionSolstoneTmuxSource,
        &[
            (
                "arg0",
                crate::i18n::ArgValue::Text((env!("CARGO_PKG_VERSION")).to_string()),
            ),
            ("source", crate::i18n::ArgValue::Text((source).to_string())),
        ],
    )
}
