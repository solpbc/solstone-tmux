// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::ffi::OsString;
use std::fmt;

pub const USAGE_EXIT_CODE: i32 = 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CliCommand {
    Run,
    Setup,
    Status,
    InstallService,
    UninstallService,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CliError(String);

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for CliError {}

pub fn parse_args<I>(args: I) -> Result<CliCommand, CliError>
where
    I: IntoIterator<Item = OsString>,
{
    let mut args = args.into_iter();
    let _program = args.next();
    let command = match args.next() {
        None => CliCommand::Run,
        Some(value) if value == "run" => CliCommand::Run,
        Some(value) if value == "setup" => CliCommand::Setup,
        Some(value) if value == "status" => CliCommand::Status,
        Some(value) if value == "install-service" => CliCommand::InstallService,
        Some(value) if value == "uninstall-service" => CliCommand::UninstallService,
        Some(value) if value == "-h" || value == "--help" => {
            return Err(CliError(usage().to_owned()));
        }
        Some(value) => {
            return Err(CliError(format!(
                "unknown command '{}'\n{}",
                value.to_string_lossy(),
                usage()
            )));
        }
    };
    if let Some(value) = args.next() {
        return Err(CliError(format!(
            "unexpected argument '{}'\n{}",
            value.to_string_lossy(),
            usage()
        )));
    }
    Ok(command)
}

pub const fn usage() -> &'static str {
    "usage: solstone-tmux [run|setup|status|install-service|uninstall-service]"
}
