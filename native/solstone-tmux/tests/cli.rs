// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::ffi::OsString;
use std::process::{Command, Output};

use solstone_tmux::cli::{CliCommand, MarkOption, USAGE_EXIT_CODE, parse_args, usage};

const HELP: &[u8] = b"usage: solstone-tmux [run|setup|confirm|status|about|install-service|uninstall-service|--help|--version]\n       solstone-tmux setup [--mark <words>]\n       solstone-tmux confirm [--mark <words>]\n--mark  the two words of the mark your journal's network app shows. needed when there's no terminal to ask you on.\nexit 0 paired or already confirmed; exit 1 not paired, nothing changed, or no terminal; exit 2 usage; exit 5 held. status still uses exit 3 and 4.\n";

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
fn about_prints_two_copyable_lines_without_starting_the_observer() {
    let root = std::env::temp_dir().join(format!(
        "tmux-about-cli-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let output = Command::new(env!("CARGO_BIN_EXE_solstone-tmux"))
        .arg("about")
        .env_clear()
        .env("HOME", &root)
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_DATA_HOME", root.join("data"))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0));
    let text = String::from_utf8(output.stdout).unwrap();
    let lines = text.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 2);
    assert!(lines[0].starts_with(&format!("tmux app {} · ", env!("CARGO_PKG_VERSION"))));
    assert_eq!(lines[1], "journal unknown");
    assert!(text.ends_with('\n'));
    assert!(!text.contains("development"));
    assert!(output.stderr.is_empty());
    assert!(
        !root.exists(),
        "About must not create observer or credential state"
    );
}

#[test]
fn parser_preserves_five_commands_and_no_argument_default() {
    let cases = [
        ("run", CliCommand::Run),
        ("setup", CliCommand::Setup(MarkOption::Absent)),
        ("confirm", CliCommand::Confirm(MarkOption::Absent)),
        ("status", CliCommand::Status),
        ("about", CliCommand::About),
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

#[test]
fn parser_parses_mark_options_and_rejects_positional_link() {
    assert_eq!(
        parse(&["setup", "--mark", "bramble quokka"]).expect("setup mark"),
        CliCommand::Setup(MarkOption::Value("bramble quokka".to_owned()))
    );
    assert_eq!(
        parse(&["confirm", "--mark", "bramble quokka"]).expect("confirm mark"),
        CliCommand::Confirm(MarkOption::Value("bramble quokka".to_owned()))
    );
    assert_eq!(
        parse(&["setup", "--mark"]).expect("setup missing mark"),
        CliCommand::Setup(MarkOption::MissingValue)
    );
    assert_eq!(
        parse(&["setup", "--mark", "foo", "--mark", "bar"]).expect("setup repeated mark"),
        CliCommand::Setup(MarkOption::Repeated)
    );
    assert!(parse(&["setup", "https://link"]).is_err());
    assert!(parse(&["run", "--mark", "foo"]).is_err());
    assert!(parse(&["status", "--mark", "foo"]).is_err());
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
