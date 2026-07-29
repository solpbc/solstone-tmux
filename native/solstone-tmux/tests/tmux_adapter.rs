// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

mod support;

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use solstone_tmux::command::{CommandInvocation, CommandOperation, CommandRunner, TmuxOperation};
use solstone_tmux::tmux::{TMUX_TIMEOUT, TmuxAdapter};
use support::{ExpectedInvocation, FixtureRunner, output};

const TMUX: &str = "/usr/bin/tmux";

#[tokio::test]
async fn list_clients_argv_timeout_and_no_socket_override() {
    let expected = invocation(
        TmuxOperation::ListClients,
        &["list-clients", "-F", "#{client_session} #{client_activity}"],
    );
    let runner = FixtureRunner::new([ExpectedInvocation {
        invocation: expected.clone(),
        outcome: output(b"main 100\n".to_vec()),
    }]);
    let adapter = TmuxAdapter::new(PathBuf::from(TMUX), runner.clone()).expect("adapter");

    let clients = adapter.list_clients().await.expect("clients");

    assert_eq!(clients[0].session, "main");
    assert_eq!(runner.calls(), vec![expected]);
    assert!(
        !runner.calls()[0]
            .args
            .iter()
            .any(|argument| argument == "-L" || argument == "-S")
    );
    runner.assert_finished().expect("all fixture calls used");
}

#[tokio::test]
async fn list_windows_argv_preserves_raw_session() {
    let runner = complete_session_runner("space session", b"pane\n".to_vec());
    let adapter = TmuxAdapter::new(PathBuf::from(TMUX), runner.clone()).expect("adapter");

    adapter
        .capture_session("space session")
        .await
        .expect("capture");

    assert_eq!(
        runner.calls()[0],
        invocation(
            TmuxOperation::ListWindows("space session".to_owned()),
            &[
                "list-windows",
                "-t",
                "space session",
                "-F",
                "#{window_active} #{window_id} #{window_index} #{window_name}",
            ],
        )
    );
    runner.assert_finished().expect("all fixture calls used");
}

#[tokio::test]
async fn list_panes_argv_is_exact() {
    let runner = complete_session_runner("main", b"pane\n".to_vec());
    let adapter = TmuxAdapter::new(PathBuf::from(TMUX), runner.clone()).expect("adapter");

    adapter.capture_session("main").await.expect("capture");

    assert_eq!(
        runner.calls()[1],
        invocation(
            TmuxOperation::ListPanes("@0".to_owned()),
            &[
                "list-panes",
                "-t",
                "@0",
                "-F",
                "#{pane_id} #{pane_index} #{pane_left} #{pane_top} #{pane_width} #{pane_height} #{pane_active}",
            ],
        )
    );
    runner.assert_finished().expect("all fixture calls used");
}

#[tokio::test]
async fn capture_pane_argv_is_exact() {
    let runner = complete_session_runner("main", b"pane\n".to_vec());
    let adapter = TmuxAdapter::new(PathBuf::from(TMUX), runner.clone()).expect("adapter");

    adapter.capture_session("main").await.expect("capture");

    assert_eq!(
        runner.calls()[2],
        invocation(
            TmuxOperation::CapturePane("%0".to_owned()),
            &["capture-pane", "-p", "-e", "-t", "%0"],
        )
    );
    runner.assert_finished().expect("all fixture calls used");
}

#[tokio::test]
async fn fixture_runner_rejects_unexpected_or_unused_invocations() {
    let expected = invocation(TmuxOperation::ListClients, &["list-clients"]);
    let unused = FixtureRunner::new([ExpectedInvocation {
        invocation: expected.clone(),
        outcome: output(Vec::new()),
    }]);
    assert!(unused.assert_finished().is_err());

    let unexpected = FixtureRunner::default();
    assert!(unexpected.run(expected).await.is_err());
    assert!(unexpected.assert_finished().is_err());
}

#[tokio::test]
async fn spaced_client_session_parses_from_right() {
    let runner = FixtureRunner::new([ExpectedInvocation {
        invocation: invocation(
            TmuxOperation::ListClients,
            &["list-clients", "-F", "#{client_session} #{client_activity}"],
        ),
        outcome: output(include_bytes!("data/tmux/list-clients-spaced.txt").to_vec()),
    }]);
    let adapter = TmuxAdapter::new(PathBuf::from(TMUX), runner).expect("adapter");

    let clients = adapter.list_clients().await.expect("clients");

    assert_eq!(clients[0].session, "space session");
    assert_eq!(clients[0].activity, 1_785_259_414);
    assert_eq!(clients[1].session, "plain");
}

#[tokio::test]
async fn fresh_client_is_selected_and_stale_client_is_ignored() {
    let runner = FixtureRunner::new([ExpectedInvocation {
        invocation: invocation(
            TmuxOperation::ListClients,
            &["list-clients", "-F", "#{client_session} #{client_activity}"],
        ),
        outcome: output(include_bytes!("data/tmux/list-clients-spaced.txt").to_vec()),
    }]);
    let adapter = TmuxAdapter::new(PathBuf::from(TMUX), runner).expect("adapter");

    let clients = adapter
        .list_active_clients(1_785_259_417, Duration::from_secs(1))
        .await
        .expect("active clients");

    assert_eq!(clients.len(), 1);
    assert_eq!(clients[0].session, "plain");
}

#[tokio::test]
async fn active_window_uses_all_split_panes() {
    let runner = FixtureRunner::new([
        ExpectedInvocation {
            invocation: invocation(
                TmuxOperation::ListWindows("space session".to_owned()),
                &[
                    "list-windows",
                    "-t",
                    "space session",
                    "-F",
                    "#{window_active} #{window_id} #{window_index} #{window_name}",
                ],
            ),
            outcome: output(include_bytes!("data/tmux/list-windows-multi.txt").to_vec()),
        },
        ExpectedInvocation {
            invocation: invocation(
                TmuxOperation::ListPanes("@0".to_owned()),
                &[
                    "list-panes",
                    "-t",
                    "@0",
                    "-F",
                    "#{pane_id} #{pane_index} #{pane_left} #{pane_top} #{pane_width} #{pane_height} #{pane_active}",
                ],
            ),
            outcome: output(include_bytes!("data/tmux/list-panes-split.txt").to_vec()),
        },
        ExpectedInvocation {
            invocation: invocation(
                TmuxOperation::CapturePane("%0".to_owned()),
                &["capture-pane", "-p", "-e", "-t", "%0"],
            ),
            outcome: output(include_bytes!("data/tmux/capture-pane-ansi.bin").to_vec()),
        },
        ExpectedInvocation {
            invocation: invocation(
                TmuxOperation::CapturePane("%1".to_owned()),
                &["capture-pane", "-p", "-e", "-t", "%1"],
            ),
            outcome: output(b"second pane\n".to_vec()),
        },
    ]);
    let adapter = TmuxAdapter::new(PathBuf::from(TMUX), runner.clone()).expect("adapter");

    let capture = adapter
        .capture_session("space session")
        .await
        .expect("capture");

    assert_eq!(capture.window.id, "@0");
    assert_eq!(capture.windows.len(), 2);
    assert_eq!(capture.panes.len(), 2);
    runner.assert_finished().expect("all fixture calls used");
}

#[tokio::test]
async fn ansi_fixture_bytes_survive_capture() {
    let bytes = include_bytes!("data/tmux/capture-pane-ansi.bin");
    let runner = complete_session_runner("main", bytes.to_vec());
    let adapter = TmuxAdapter::new(PathBuf::from(TMUX), runner).expect("adapter");

    let capture = adapter.capture_session("main").await.expect("capture");

    assert_eq!(capture.panes[0].content.as_bytes(), bytes);
    assert!(
        capture.panes[0]
            .content
            .as_bytes()
            .windows(2)
            .any(|pair| pair == b"\x1b[")
    );
}

fn complete_session_runner(session: &str, pane: Vec<u8>) -> FixtureRunner {
    FixtureRunner::new([
        ExpectedInvocation {
            invocation: invocation(
                TmuxOperation::ListWindows(session.to_owned()),
                &[
                    "list-windows",
                    "-t",
                    session,
                    "-F",
                    "#{window_active} #{window_id} #{window_index} #{window_name}",
                ],
            ),
            outcome: output(b"1 @0 0 active\n".to_vec()),
        },
        ExpectedInvocation {
            invocation: invocation(
                TmuxOperation::ListPanes("@0".to_owned()),
                &[
                    "list-panes",
                    "-t",
                    "@0",
                    "-F",
                    "#{pane_id} #{pane_index} #{pane_left} #{pane_top} #{pane_width} #{pane_height} #{pane_active}",
                ],
            ),
            outcome: output(b"%0 0 0 0 80 24 1\n".to_vec()),
        },
        ExpectedInvocation {
            invocation: invocation(
                TmuxOperation::CapturePane("%0".to_owned()),
                &["capture-pane", "-p", "-e", "-t", "%0"],
            ),
            outcome: output(pane),
        },
    ])
}

fn invocation(operation: TmuxOperation, args: &[&str]) -> CommandInvocation {
    CommandInvocation {
        operation: CommandOperation::Tmux(operation),
        executable: PathBuf::from(TMUX),
        args: args.iter().map(OsString::from).collect(),
        timeout: TMUX_TIMEOUT,
    }
}
