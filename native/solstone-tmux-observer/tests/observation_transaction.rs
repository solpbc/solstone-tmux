// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

mod support;

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use solstone_tmux_observer::command::{CommandInvocation, TmuxOperation};
use solstone_tmux_observer::model::ClientInfo;
use solstone_tmux_observer::segment::{AppendOutcome, SegmentState};
use solstone_tmux_observer::tmux::{TMUX_TIMEOUT, TmuxAdapter};
use support::{
    ExpectedInvocation, FixtureOutcome, FixtureRunner, RecordingWarnings, TestDirectory,
    golden_capture, nonzero, output,
};
use time::{Date, Month, PrimitiveDateTime, Time, UtcOffset};

const TMUX: &str = "/usr/bin/tmux";

#[tokio::test]
async fn malformed_client_row_is_dropped_and_other_sessions_still_append() {
    let runner = FixtureRunner::new(
        [ExpectedInvocation {
            invocation: invocation(
                TmuxOperation::ListClients,
                &["list-clients", "-F", "#{client_session} #{client_activity}"],
            ),
            outcome: output(b"offending-row\ngood 100\n".to_vec()),
        }]
        .into_iter()
        .chain(successful_session("good", b"complete\n".to_vec())),
    );
    let warnings = RecordingWarnings::default();
    let adapter =
        TmuxAdapter::with_warnings(PathBuf::from(TMUX), runner, warnings.clone()).expect("adapter");

    let clients = adapter.list_clients().await.expect("client discovery");
    let captures = adapter.capture_sessions(&clients).await;

    assert_eq!(captures.len(), 1);
    assert_eq!(captures[0].session, "good");
    assert!(
        warnings
            .messages()
            .iter()
            .any(|message| message.contains("offending-row"))
    );
}

#[tokio::test]
async fn malformed_window_row_fails_whole_transaction() {
    let runner = FixtureRunner::new(
        [ExpectedInvocation {
            invocation: invocation(
                TmuxOperation::ListWindows("broken".to_owned()),
                &[
                    "list-windows",
                    "-t",
                    "broken",
                    "-F",
                    "#{window_active} #{window_id} #{window_index} #{window_name}",
                ],
            ),
            outcome: output(b"1 missing-fields\n".to_vec()),
        }]
        .into_iter()
        .chain(successful_session("good", b"complete\n".to_vec())),
    );
    let warnings = RecordingWarnings::default();
    let adapter =
        TmuxAdapter::with_warnings(PathBuf::from(TMUX), runner, warnings).expect("adapter");

    let captures = adapter
        .capture_sessions(&[client("broken"), client("good")])
        .await;

    assert_eq!(captures.len(), 1);
    assert_eq!(captures[0].session, "good");
}

#[tokio::test]
async fn window_failure_emits_nothing_and_other_session_appends() {
    let runner = FixtureRunner::new(
        [ExpectedInvocation {
            invocation: invocation(
                TmuxOperation::ListWindows("broken".to_owned()),
                &[
                    "list-windows",
                    "-t",
                    "broken",
                    "-F",
                    "#{window_active} #{window_id} #{window_index} #{window_name}",
                ],
            ),
            outcome: nonzero(1),
        }]
        .into_iter()
        .chain(successful_session("good", b"complete\n".to_vec())),
    );
    assert_only_good_capture(runner).await;
}

#[tokio::test]
async fn pane_failure_emits_nothing_and_other_session_appends() {
    let runner = FixtureRunner::new(
        [
            ExpectedInvocation {
                invocation: windows_invocation("broken"),
                outcome: output(b"1 @bad 0 active\n".to_vec()),
            },
            ExpectedInvocation {
                invocation: panes_invocation("@bad"),
                outcome: nonzero(1),
            },
        ]
        .into_iter()
        .chain(successful_session("good", b"complete\n".to_vec())),
    );
    assert_only_good_capture(runner).await;
}

#[tokio::test]
async fn malformed_pane_row_fails_whole_transaction() {
    let runner = FixtureRunner::new(
        [
            ExpectedInvocation {
                invocation: windows_invocation("broken"),
                outcome: output(b"1 @bad 0 active\n".to_vec()),
            },
            ExpectedInvocation {
                invocation: panes_invocation("@bad"),
                outcome: output(b"%0 missing fields\n".to_vec()),
            },
        ]
        .into_iter()
        .chain(successful_session("good", b"complete\n".to_vec())),
    );
    assert_only_good_capture(runner).await;
}

#[tokio::test]
async fn one_capture_failure_discards_whole_session() {
    let runner = FixtureRunner::new(
        [
            ExpectedInvocation {
                invocation: windows_invocation("broken"),
                outcome: output(b"1 @bad 0 active\n".to_vec()),
            },
            ExpectedInvocation {
                invocation: panes_invocation("@bad"),
                outcome: output(b"%0 0 0 0 40 24 1\n%1 1 40 0 40 24 0\n".to_vec()),
            },
            ExpectedInvocation {
                invocation: capture_invocation("%0"),
                outcome: output(b"first pane\n".to_vec()),
            },
            ExpectedInvocation {
                invocation: capture_invocation("%1"),
                outcome: nonzero(1),
            },
        ]
        .into_iter()
        .chain(successful_session("good", b"complete\n".to_vec())),
    );
    assert_only_good_capture(runner).await;
}

#[tokio::test]
async fn timeout_discards_whole_session() {
    let runner = FixtureRunner::new(
        [ExpectedInvocation {
            invocation: windows_invocation("broken"),
            outcome: FixtureOutcome::Timeout,
        }]
        .into_iter()
        .chain(successful_session("good", b"complete\n".to_vec())),
    );
    assert_only_good_capture(runner).await;
}

#[tokio::test]
async fn successful_empty_pane_is_a_complete_frame() {
    let runner = FixtureRunner::new(successful_session("empty", Vec::new()));
    let adapter = TmuxAdapter::new(PathBuf::from(TMUX), runner).expect("adapter");

    let capture = adapter.capture_session("empty").await.expect("capture");

    assert_eq!(capture.panes.len(), 1);
    assert_eq!(capture.panes[0].content, "");
}

#[tokio::test]
async fn overlong_session_identity_skips_only_that_session() {
    let runner = FixtureRunner::new(successful_session("good", b"complete\n".to_vec()));
    let warnings = RecordingWarnings::default();
    let adapter = TmuxAdapter::with_warnings(PathBuf::from(TMUX), runner.clone(), warnings.clone())
        .expect("adapter");

    let captures = adapter
        .capture_sessions(&[
            ClientInfo {
                session: "a".repeat(201),
                activity: 100,
            },
            client("good"),
        ])
        .await;

    assert_eq!(captures.len(), 1);
    assert_eq!(captures[0].session, "good");
    assert_eq!(runner.calls().len(), 3);
    assert!(
        warnings
            .messages()
            .iter()
            .any(|message| message.contains("201 bytes"))
    );
}

#[tokio::test]
async fn failed_transaction_does_not_advance_id_or_digest() {
    let runner = FixtureRunner::new([ExpectedInvocation {
        invocation: windows_invocation("broken"),
        outcome: output(b"1 malformed\n".to_vec()),
    }]);
    let adapter = TmuxAdapter::new(PathBuf::from(TMUX), runner).expect("adapter");
    let captures = adapter.capture_sessions(&[client("broken")]).await;
    assert!(captures.is_empty());

    let temporary = TestDirectory::new("failed-transaction-state");
    let date = Date::from_calendar_date(2026, Month::July, 28).expect("date");
    let time = Time::from_hms(12, 0, 0).expect("time");
    let mut segment = SegmentState::create(
        &temporary.path().join("stream"),
        PrimitiveDateTime::new(date, time).assume_utc(),
        Duration::ZERO,
        UtcOffset::UTC,
    )
    .expect("segment");
    for capture in &captures {
        segment
            .append_capture(capture, 0.25, Duration::from_secs(1))
            .expect("append complete capture");
    }
    assert_eq!(segment.metadata().last_durable_frame_id, 0);
    assert_eq!(segment.metadata().durable_frame_count, 0);

    assert_eq!(
        segment
            .append_capture(&golden_capture("broken"), 0.5, Duration::from_secs(2),)
            .expect("retry complete capture"),
        AppendOutcome::Appended { frame_id: 1 }
    );
}

async fn assert_only_good_capture(runner: FixtureRunner) {
    let warnings = RecordingWarnings::default();
    let adapter =
        TmuxAdapter::with_warnings(PathBuf::from(TMUX), runner.clone(), warnings).expect("adapter");
    let captures = adapter
        .capture_sessions(&[client("broken"), client("good")])
        .await;
    assert_eq!(captures.len(), 1);
    assert_eq!(captures[0].session, "good");
    runner.assert_finished().expect("all fixture calls used");
}

fn successful_session(session: &str, content: Vec<u8>) -> impl Iterator<Item = ExpectedInvocation> {
    [
        ExpectedInvocation {
            invocation: windows_invocation(session),
            outcome: output(b"1 @0 0 active\n".to_vec()),
        },
        ExpectedInvocation {
            invocation: panes_invocation("@0"),
            outcome: output(b"%0 0 0 0 80 24 1\n".to_vec()),
        },
        ExpectedInvocation {
            invocation: capture_invocation("%0"),
            outcome: output(content),
        },
    ]
    .into_iter()
}

fn client(session: &str) -> ClientInfo {
    ClientInfo {
        session: session.to_owned(),
        activity: 100,
    }
}

fn windows_invocation(session: &str) -> CommandInvocation {
    invocation(
        TmuxOperation::ListWindows(session.to_owned()),
        &[
            "list-windows",
            "-t",
            session,
            "-F",
            "#{window_active} #{window_id} #{window_index} #{window_name}",
        ],
    )
}

fn panes_invocation(window: &str) -> CommandInvocation {
    invocation(
        TmuxOperation::ListPanes(window.to_owned()),
        &[
            "list-panes",
            "-t",
            window,
            "-F",
            "#{pane_id} #{pane_index} #{pane_left} #{pane_top} #{pane_width} #{pane_height} #{pane_active}",
        ],
    )
}

fn capture_invocation(pane: &str) -> CommandInvocation {
    invocation(
        TmuxOperation::CapturePane(pane.to_owned()),
        &["capture-pane", "-p", "-e", "-t", pane],
    )
}

fn invocation(operation: TmuxOperation, args: &[&str]) -> CommandInvocation {
    CommandInvocation {
        operation,
        executable: PathBuf::from(TMUX),
        args: args.iter().map(OsString::from).collect(),
        timeout: TMUX_TIMEOUT,
    }
}
