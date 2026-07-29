// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

mod support;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use support::TestDirectory;

const SOURCE_COMMIT: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const VERSION: &str = "1.0.0";
const TAG: &str = "v1.0.0";
const TARGET: &str = "aarch64-apple-darwin";
const APPLICATION_IDENTITY: &str = "Developer ID Application: Test Fixture";
const INSTALLER_IDENTITY: &str = "Developer ID Installer: Test Fixture";
const NOTARY_PROFILE: &str = "test-notary-profile";

#[test]
fn operator_flow_has_one_exact_ordered_command_path() {
    let fixture = OperatorFixture::new();
    let run = fixture.run(0, "sequence");
    assert_success(&run.output);
    assert!(run.output.stdout.ends_with(b"/candidate\n"));

    let names = run
        .workflow_commands
        .iter()
        .map(|command| command_name(command))
        .collect::<Vec<_>>();
    assert_eq!(names, expected_command_names());

    let commands = run.workflow_commands.join("\n");
    assert!(commands.contains("MACOSX_DEPLOYMENT_TARGET=14.0"));
    assert!(commands.contains("codesign --force --sign Developer\\ ID\\ Application"));
    assert!(commands.contains("--options runtime --timestamp"));
    assert!(commands.contains("productsign --sign Developer\\ ID\\ Installer"));
    assert!(commands.contains("xcrun notarytool submit"));
    assert!(commands.contains("xcrun stapler staple"));
    assert!(commands.contains("pkgbuild --root"));
    assert!(commands.contains("--install-location /"));
    assert!(commands.contains("/usr/local/bin/solstone-tmux install-service"));
    assert!(commands.contains("launchctl print gui/501/com.solstone.tmux"));
    assert!(commands.contains("/usr/local/bin/solstone-tmux uninstall-service"));
    assert!(!commands.contains(" gh "));
    assert!(!commands.contains(" publish"));
    assert!(!commands.contains(" release"));
}

#[test]
fn every_operator_command_failure_stops_before_later_work() {
    let fixture = OperatorFixture::new();
    let successful = fixture.run(0, "failure-baseline");
    assert_success(&successful.output);
    let command_count = successful.workflow_commands.len();
    assert_eq!(command_count, expected_command_names().len());

    for failure_position in 1..=command_count {
        let run = fixture.run(failure_position, &format!("failure-{failure_position}"));
        assert!(
            !run.output.status.success(),
            "command position {failure_position} unexpectedly succeeded"
        );
        assert_eq!(
            run.workflow_commands.len(),
            failure_position,
            "position {failure_position} ran a later workflow command"
        );
        assert_eq!(
            run.workflow_commands,
            successful.workflow_commands[..failure_position],
            "position {failure_position} diverged before the injected failure"
        );
        assert!(
            run.cleanup_commands
                .iter()
                .all(|command| command_name(command) != "mv"),
            "position {failure_position} finalized a candidate during cleanup"
        );
    }
}

fn expected_command_names() -> Vec<&'static str> {
    let mut names = vec!["sh"; 42];
    names.extend([
        "rust-targets.sh",
        "git",
        "git",
        "git",
        "cargo",
        "jq",
        "cargo-deny",
        "rustup",
        "grep",
        "security",
        "grep",
        "security",
        "grep",
        "xcrun",
        "mktemp",
        "mkdir",
        "test",
        "id",
        "launchctl",
        "tmux",
        "tmux",
        "bash",
        "cargo",
        "cargo",
        "cargo",
        "cargo",
        "cargo",
        "git",
        "date",
        "env",
        "lipo",
        "otool",
        "sed",
        "solstone-tmux",
        "grep",
        "shasum",
        "sed",
        "install",
        "codesign",
        "codesign",
        "spctl",
        "shasum",
        "sed",
        "mkdir",
        "install",
        "chmod",
        "touch",
        "tar",
        "gzip",
        "chmod",
        "mkdir",
        "install",
        "touch",
        "pkgbuild",
        "productsign",
        "xcrun",
        "xcrun",
        "pkgutil",
        "spctl",
        "xcrun",
        "pkgutil",
        "find",
        "grep",
        "pkgutil",
        "sed",
        "grep",
        "find",
        "find",
        "cmp",
        "stat",
        "tar",
        "mkdir",
        "tar",
        "cmp",
        "codesign",
        "grep",
        "sudo",
        "cmp",
        "solstone-tmux",
        "grep",
        "launchctl",
        "launchctl",
        "tmux",
        "sh",
        "solstone-tmux",
        "launchctl",
        "grep",
        "sh",
        "solstone-tmux",
        "launchctl",
        "kill",
        "tmux",
        "launchctl",
        "launchctl",
        "sudo",
        "sudo",
        "rustc",
        "shasum",
        "sed",
        "jq",
        "shasum",
        "sed",
        "jq",
        "jq",
        "jq",
        "chmod",
        "mv",
    ]);
    names
}

fn command_name(command: &str) -> &str {
    command
        .split(' ')
        .next()
        .expect("logged command name")
        .rsplit('/')
        .next()
        .expect("command basename")
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "operator script failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

struct OperatorRun {
    output: Output,
    workflow_commands: Vec<String>,
    cleanup_commands: Vec<String>,
}

struct OperatorFixture {
    root: TestDirectory,
    dispatcher: PathBuf,
    script: PathBuf,
}

impl OperatorFixture {
    fn new() -> Self {
        let root = TestDirectory::new("macos-operator");
        let dispatcher = root.path().join("fake-operator");
        fs::write(&dispatcher, fake_dispatcher()).expect("write fake operator dispatcher");
        fs::set_permissions(&dispatcher, fs::Permissions::from_mode(0o700))
            .expect("chmod fake operator dispatcher");
        let script =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packaging/macos/build-candidate.sh");
        Self {
            root,
            dispatcher,
            script,
        }
    }

    fn run(&self, fail_at: usize, label: &str) -> OperatorRun {
        let run_root = self.root.path().join(label);
        let output_parent = run_root.join("output");
        fs::create_dir_all(&output_parent).expect("create output parent");
        let log = run_root.join("commands.log");
        let count = run_root.join("count");
        let state = run_root.join("service-state");
        let output_path = output_parent.join("candidate");
        let output = Command::new(&self.script)
            .args([
                TARGET,
                SOURCE_COMMIT,
                VERSION,
                TAG,
                APPLICATION_IDENTITY,
                INSTALLER_IDENTITY,
                NOTARY_PROFILE,
            ])
            .arg(&output_path)
            .env("SOLSTONE_TMUX_SCRATCH_HOST", "1")
            .env("SOLSTONE_TMUX_OPERATOR_DISPATCHER", &self.dispatcher)
            .env("FAKE_OPERATOR_LOG", &log)
            .env("FAKE_OPERATOR_COUNT", &count)
            .env("FAKE_OPERATOR_FAIL_AT", fail_at.to_string())
            .env("FAKE_OPERATOR_STATE", &state)
            .env("FAKE_SOURCE_COMMIT", SOURCE_COMMIT)
            .env("FAKE_VERSION", VERSION)
            .env("FAKE_TARGET", TARGET)
            .env("FAKE_APPLICATION_IDENTITY", APPLICATION_IDENTITY)
            .env("FAKE_INSTALLER_IDENTITY", INSTALLER_IDENTITY)
            .output()
            .expect("run macOS operator script");
        let entries = fs::read_to_string(&log)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let mut workflow_commands = Vec::new();
        let mut cleanup_commands = Vec::new();
        for entry in entries {
            let (kind, command) = entry.split_once('\t').expect("typed dispatcher log entry");
            let command = normalize_command(command, &run_root);
            if kind == "cleanup" {
                cleanup_commands.push(command);
            } else {
                workflow_commands.push(command);
            }
        }
        OperatorRun {
            output,
            workflow_commands,
            cleanup_commands,
        }
    }
}

fn normalize_command(command: &str, run_root: &Path) -> String {
    let mut normalized = command.replace(run_root.to_str().expect("UTF-8 test run root"), "$RUN");
    while let Some(start) = normalized.find(".solstone-tmux-macos.") {
        let suffix_start = start + ".solstone-tmux-macos.".len();
        let suffix_len = normalized[suffix_start..]
            .find(['/', ' '])
            .unwrap_or(normalized.len() - suffix_start);
        normalized.replace_range(start..suffix_start + suffix_len, "$SCRATCH");
    }
    normalized
}

fn fake_dispatcher() -> &'static str {
    r#"#!/usr/bin/env bash
set -euo pipefail

cleanup="${SOLSTONE_TMUX_OPERATOR_CLEANUP:-0}"
if [[ "$cleanup" == "1" ]]; then
    kind="cleanup"
else
    kind="workflow"
    count=0
    if [[ -f "$FAKE_OPERATOR_COUNT" ]]; then
        count="$(<"$FAKE_OPERATOR_COUNT")"
    fi
    count=$((count + 1))
    printf '%s\n' "$count" >"$FAKE_OPERATOR_COUNT"
fi
{
    printf '%s\t' "$kind"
    printf '%q ' "$@"
    printf '\n'
} >>"$FAKE_OPERATOR_LOG"
if [[ "$cleanup" != "1" &&
    "${FAKE_OPERATOR_FAIL_AT:-0}" != "0" &&
    "$count" == "$FAKE_OPERATOR_FAIL_AT" ]]; then
    exit 97
fi

command_name="$1"
shift
case "$command_name" in
    sh)
        if [[ "${1:-}" == "-c" && "${2:-}" == command\ -v* ]]; then
            exit 0
        fi
        if [[ "${1:-}" == "-c" && "${2:-}" == *"nohup script"* ]]; then
            printf '%s\n' 4242 >"${*: -1}"
            exit 0
        fi
        if [[ "${1:-}" == "-c" && "${2:-}" == *"for ignored"* ]]; then
            exit 0
        fi
        ;;
    */scripts/rust-targets.sh)
        printf '%s\n' "$FAKE_TARGET"
        ;;
    git)
        case " $* " in
            *" status "*) ;;
            *" rev-parse "*) printf '%s\n' "$FAKE_SOURCE_COMMIT" ;;
            *" show "*) printf '%s\n' 1700000000 ;;
        esac
        ;;
    cargo)
        if [[ "${1:-}" == "metadata" ]]; then
            printf '{"packages":[{"name":"solstone-tmux","version":"%s"}]}\n' "$FAKE_VERSION"
        fi
        ;;
    jq)
        if [[ " $* " == *" .packages[] "* ]]; then
            printf '%s\n' "$FAKE_VERSION"
        elif [[ " $* " == *" -s "* ]]; then
            printf '[]\n'
        else
            printf '{}\n'
        fi
        ;;
    cargo-deny)
        if [[ "${1:-}" == "--version" ]]; then
            echo "cargo-deny 0.20.2"
        fi
        ;;
    rustup)
        printf '%s\n' "$FAKE_TARGET"
        ;;
    security)
        if [[ " $* " == *" codesigning "* ]]; then
            printf '%s\n' "$FAKE_APPLICATION_IDENTITY"
        else
            printf '%s\n' "$FAKE_INSTALLER_IDENTITY"
        fi
        ;;
    grep)
        if [[ " $* " == *" -q . "* ]]; then
            exit 1
        elif [[ " $* " == *" -Fxc "* ]]; then
            echo 1
        fi
        ;;
    xcrun | bash | env | codesign | spctl | cmp | kill) ;;
    mktemp)
        /usr/bin/mktemp "$@"
        ;;
    mkdir)
        /bin/mkdir "$@"
        ;;
    test)
        exit 1
        ;;
    id)
        echo 501
        ;;
    launchctl)
        case "${1:-}" in
            print)
                if [[ -f "$FAKE_OPERATOR_STATE" ]]; then
                    echo "    pid = 4321"
                else
                    exit 113
                fi
                ;;
            setenv | unsetenv) ;;
        esac
        ;;
    tmux) ;;
    lipo)
        echo arm64
        ;;
    otool)
        echo "      minos 14.0"
        ;;
    sed)
        /bin/sed "$@"
        ;;
    */solstone-tmux)
        case "${1:-}" in
            --version)
                printf 'solstone-tmux %s (source %s)\n' "$FAKE_VERSION" "$FAKE_SOURCE_COMMIT"
                ;;
            install-service)
                : >"$FAKE_OPERATOR_STATE"
                ;;
            uninstall-service)
                rm -f "$FAKE_OPERATOR_STATE"
                ;;
        esac
        ;;
    shasum)
        if [[ "${*: -1}" == *"/target/"* ]]; then
            digest="aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        elif [[ "${*: -1}" == *"/stage/solstone-tmux" ]]; then
            digest="bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
        else
            digest="cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
        fi
        printf '%s  %s\n' "$digest" "${*: -1}"
        ;;
    install)
        destination="${*: -1}"
        /bin/mkdir -p "${destination%/*}"
        printf 'fixture executable\n' >"$destination"
        /bin/chmod 0755 "$destination"
        ;;
    date)
        echo 202311142213.20
        ;;
    chmod)
        /bin/chmod "$@"
        ;;
    touch)
        /usr/bin/touch "$@"
        ;;
    tar)
        if [[ " $* " == *" -cf "* ]]; then
            previous=""
            for argument in "$@"; do
                if [[ "$previous" == "-cf" ]]; then
                    : >"$argument"
                    break
                fi
                previous="$argument"
            done
        elif [[ "${1:-}" == "-tzf" ]]; then
            printf 'INSTALL.md\nsolstone-tmux\n'
        elif [[ "${1:-}" == "-xzf" ]]; then
            destination=""
            previous=""
            for argument in "$@"; do
                if [[ "$previous" == "-C" ]]; then
                    destination="$argument"
                    break
                fi
                previous="$argument"
            done
            /bin/mkdir -p "$destination"
            printf 'fixture executable\n' >"$destination/solstone-tmux"
            /bin/chmod 0755 "$destination/solstone-tmux"
        fi
        ;;
    gzip)
        printf 'fixture gzip bytes\n'
        ;;
    pkgbuild | productsign)
        destination="${*: -1}"
        /bin/mkdir -p "${destination%/*}"
        printf 'fixture package bytes\n' >"$destination"
        ;;
    pkgutil)
        case "${1:-}" in
            --expand-full)
                destination="${*: -1}"
                /bin/mkdir -p "$destination/Payload/usr/local/bin"
                printf 'fixture executable\n' >"$destination/Payload/usr/local/bin/solstone-tmux"
                /bin/chmod 0755 "$destination/Payload/usr/local/bin/solstone-tmux"
                ;;
            --payload-files)
                echo "usr/local/bin/solstone-tmux"
                ;;
        esac
        ;;
    find)
        if [[ "$*" == *"-type f"* && "$*" == *"-path"* ]]; then
            printf '%s\n' "$1/Payload/usr/local/bin/solstone-tmux"
        fi
        ;;
    stat)
        echo 755
        ;;
    sudo) ;;
    rustc)
        printf 'rustc 1.97.1 (fixture)\nhost: fixture\n'
        ;;
    mv)
        /bin/mv "$@"
        ;;
    rm)
        /bin/rm "$@"
        ;;
esac
"#
}
