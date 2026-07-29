// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::ffi::OsString;
use std::process::{Command, Output};

use solstone_tmux::cli::{CliCommand, USAGE_EXIT_CODE, parse_args, usage};

const HELP: &[u8] =
    b"usage: solstone-tmux [run|setup|status|install-service|uninstall-service|--help|--version]\n";

#[test]
fn help_flags_write_exact_stdout_and_succeed() {
    for flag in ["-h", "--help"] {
        let output = run(&[flag]);
        assert_eq!(output.status.code(), Some(0));
        assert_eq!(output.stdout, HELP);
        assert!(output.stderr.is_empty());
    }
}

#[test]
fn version_flags_write_development_version_to_stdout_and_succeed() {
    let expected = format!(
        "solstone-tmux {} (source development)\n",
        env!("CARGO_PKG_VERSION")
    );
    for flag in ["-V", "--version"] {
        let output = run(&[flag]);
        assert_eq!(output.status.code(), Some(0));
        assert_eq!(output.stdout, expected.as_bytes());
        assert!(output.stderr.is_empty());
    }
}

#[test]
fn parser_preserves_five_commands_and_no_argument_default() {
    let cases = [
        ("run", CliCommand::Run),
        ("setup", CliCommand::Setup),
        ("status", CliCommand::Status),
        ("install-service", CliCommand::InstallService),
        ("uninstall-service", CliCommand::UninstallService),
    ];
    for (argument, expected) in cases {
        assert_eq!(parse(&[argument]).expect("parse command"), expected);
    }
    assert_eq!(parse(&[]).expect("parse default"), CliCommand::Run);
}

#[test]
fn parser_recognizes_only_flag_forms_for_help_and_version() {
    assert_eq!(parse(&["-h"]).expect("parse short help"), CliCommand::Help);
    assert_eq!(
        parse(&["--help"]).expect("parse long help"),
        CliCommand::Help
    );
    assert_eq!(
        parse(&["-V"]).expect("parse short version"),
        CliCommand::Version
    );
    assert_eq!(
        parse(&["--version"]).expect("parse long version"),
        CliCommand::Version
    );
    assert!(parse(&["help"]).is_err());
    assert!(parse(&["version"]).is_err());
}

#[test]
fn invalid_arguments_keep_stderr_and_exit_two() {
    let output = run(&["unknown"]);
    assert_eq!(output.status.code(), Some(USAGE_EXIT_CODE));
    assert!(output.stdout.is_empty());
    assert_eq!(
        output.stderr,
        format!("unknown command 'unknown'\n{}\n", usage()).as_bytes()
    );

    let output = run(&["status", "extra"]);
    assert_eq!(output.status.code(), Some(USAGE_EXIT_CODE));
    assert!(output.stdout.is_empty());
    assert_eq!(
        output.stderr,
        format!("unexpected argument 'extra'\n{}\n", usage()).as_bytes()
    );
}

fn parse(arguments: &[&str]) -> Result<CliCommand, solstone_tmux::cli::CliError> {
    parse_args(
        std::iter::once(OsString::from("solstone-tmux"))
            .chain(arguments.iter().map(|argument| OsString::from(*argument))),
    )
}

fn run(arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_solstone-tmux"))
        .args(arguments)
        .output()
        .expect("run solstone-tmux")
}
