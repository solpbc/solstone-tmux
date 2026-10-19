// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

#[path = "../support/mod.rs"]
mod support;

use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use solstone_tmux::clock::{Zone, ZoneSource};
use solstone_tmux::command::{CommandInvocation, CommandOperation, DisplayField, TmuxOperation};
use solstone_tmux::foreground::SystemLookup;
use solstone_tmux::model::ClientInfo;
use solstone_tmux::name::derive_component;
use solstone_tmux::observer::{SegmentLifecycle, SegmentManager};
use solstone_tmux::segment::{AppendOutcome, SegmentState};
use solstone_tmux::sync::SyncWake;
use solstone_tmux::tmux::{TMUX_TIMEOUT, TmuxAdapter};
use support::{
    ExpectedInvocation, FixtureOutcome, FixtureRunner, RecordingWarnings, TestDirectory,
    golden_capture, nonzero, output,
};
use time::{Date, Month, OffsetDateTime, PrimitiveDateTime, Time, UtcOffset};

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
        "120000",
        PrimitiveDateTime::new(date, time).assume_utc(),
        Duration::ZERO,
        UtcOffset::UTC,
        None,
        Duration::from_secs(300),
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

#[tokio::test]
async fn all_false_poll_writes_no_activity_file() {
    let temporary = TestDirectory::new("all-false-no-activity");
    let wall = wall_now(100);
    let (adapter, runner) =
        context_adapter_with(&temporary, wall, false, output(b"ignored\n".to_vec()));
    let poll = adapter
        .poll_context(wall, Duration::from_secs(5))
        .await
        .expect("poll");
    let mut manager = segment_manager(&temporary, wall);
    manager
        .process_poll(
            &poll.captures,
            &poll.activity_jsonl,
            wall,
            Duration::from_secs(1),
            Duration::from_secs(300),
        )
        .expect("process poll");

    assert!(poll.activity_jsonl.is_empty());
    assert_eq!(manager.segment_mut().metadata().activity_durable_offset, 0);
    assert!(
        !manager
            .segment_mut()
            .incomplete_dir()
            .join("120000_desktop_activity.jsonl")
            .exists()
    );
    runner.assert_finished().expect("all fixture calls used");
}

#[tokio::test]
async fn unchanged_screen_writes_two_activity_rows_at_different_wall_times() {
    let temporary = TestDirectory::new("unchanged-screen-activity");
    let first_wall = wall_now(100);
    let mut manager = segment_manager(&temporary, first_wall);
    let mut session_name = String::new();
    for (wall, monotonic) in [(first_wall, 1), (wall_now(105), 2)] {
        let (adapter, runner) =
            context_adapter_with(&temporary, wall, true, output(b"same title\n".to_vec()));
        let poll = adapter
            .poll_context(wall, Duration::from_secs(5))
            .await
            .expect("poll");
        session_name.clone_from(&poll.captures[0].session);
        manager
            .process_poll(
                &poll.captures,
                &poll.activity_jsonl,
                wall,
                Duration::from_secs(monotonic),
                Duration::from_secs(300),
            )
            .expect("process poll");
        runner.assert_finished().expect("all fixture calls used");
    }
    let segment = manager.segment_mut();
    let session = segment
        .metadata()
        .sessions
        .get(&session_name)
        .expect("screen metadata");
    let screen = fs::read(segment.incomplete_dir().join(&session.filename)).expect("screen JSONL");
    let activity = fs::read(
        segment
            .incomplete_dir()
            .join("120000_desktop_activity.jsonl"),
    )
    .expect("activity JSONL");
    assert_eq!(screen.iter().filter(|byte| **byte == b'\n').count(), 1);
    let rows = activity
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice::<Value>(line).expect("activity JSON"))
        .collect::<Vec<_>>();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["window"], "active");
    assert_eq!(rows[1]["window"], "active");
    assert_eq!(rows[0]["ts"], 100_000);
    assert_eq!(rows[0]["end_ts"], 100_000);
    assert_eq!(rows[1]["ts"], 105_000);
    assert_eq!(rows[1]["end_ts"], 105_000);
    assert_eq!(segment.metadata().durable_frame_count, 1);
}

#[tokio::test]
async fn metadata_timeout_keeps_pane_text_and_records_availability() {
    assert_context_failure_is_nonfatal(FixtureOutcome::Timeout).await;
}

#[tokio::test]
async fn metadata_malformed_output_keeps_pane_text_and_records_availability() {
    assert_context_failure_is_nonfatal(output(b"malformed".to_vec())).await;
}

async fn assert_context_failure_is_nonfatal(outcome: FixtureOutcome) {
    let temporary = TestDirectory::new("metadata-failure-nonfatal");
    let wall = wall_now(100);
    let warnings = RecordingWarnings::default();
    let runner = FixtureRunner::new(
        [ExpectedInvocation {
            invocation: invocation(
                TmuxOperation::ListClients,
                &["list-clients", "-F", "#{client_session} #{client_activity}"],
            ),
            outcome: output(b"main 100\n".to_vec()),
        }]
        .into_iter()
        .chain(successful_session("main", b"pane text\n".to_vec()))
        .chain([ExpectedInvocation {
            invocation: display_invocation("%0", DisplayField::PaneTitle),
            outcome,
        }]),
    );
    let adapter = TmuxAdapter::with_context(
        PathBuf::from(TMUX),
        runner.clone(),
        warnings.clone(),
        true,
        false,
        false,
        Arc::new(SystemLookup),
    )
    .expect("adapter");
    let poll = adapter
        .poll_context(wall, Duration::from_secs(5))
        .await
        .expect("poll survives metadata failure");
    assert_eq!(poll.captures[0].panes[0].content, "pane text\n");
    let rows = poll
        .activity_jsonl
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice::<Value>(line).expect("activity JSON"))
        .collect::<Vec<_>>();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["kind"], "availability");
    assert_eq!(rows[0]["status"], "unavailable");
    let mut manager = segment_manager(&temporary, wall);
    manager
        .process_poll(
            &poll.captures,
            &poll.activity_jsonl,
            wall,
            Duration::from_secs(1),
            Duration::from_secs(300),
        )
        .expect("persist poll");
    let (incomplete_dir, filename) = {
        let segment = manager.segment_mut();
        let filename = segment
            .metadata()
            .sessions
            .get("main")
            .expect("screen metadata")
            .filename
            .clone();
        (segment.incomplete_dir().to_owned(), filename)
    };
    let screen = fs::read(incomplete_dir.join(filename)).expect("screen file");
    assert!(std::str::from_utf8(&screen).unwrap().contains("pane text"));
    assert_eq!(
        warnings.messages(),
        ["pane title was not recorded (unavailable)"]
    );
    runner.assert_finished().expect("all fixture calls used");
}

fn context_adapter_with(
    _temporary: &TestDirectory,
    wall: OffsetDateTime,
    pane_title: bool,
    title_outcome: support::FixtureOutcome,
) -> (TmuxAdapter<FixtureRunner, RecordingWarnings>, FixtureRunner) {
    let mut expected = vec![ExpectedInvocation {
        invocation: invocation(
            TmuxOperation::ListClients,
            &["list-clients", "-F", "#{client_session} #{client_activity}"],
        ),
        outcome: output(format!("main {}\n", wall.unix_timestamp()).into_bytes()),
    }];
    expected.extend(successful_session("main", b"pane text\n".to_vec()));
    if pane_title {
        expected.push(ExpectedInvocation {
            invocation: display_invocation("%0", DisplayField::PaneTitle),
            outcome: title_outcome,
        });
    }
    let runner = FixtureRunner::new(expected);
    let adapter = TmuxAdapter::with_context(
        PathBuf::from(TMUX),
        runner.clone(),
        RecordingWarnings::default(),
        pane_title,
        false,
        false,
        Arc::new(SystemLookup),
    )
    .expect("adapter");
    (adapter, runner)
}

fn segment_manager(temporary: &TestDirectory, wall: OffsetDateTime) -> SegmentManager {
    let segment = SegmentState::create(
        &temporary.path().join("stream"),
        "120000",
        wall,
        Duration::ZERO,
        UtcOffset::UTC,
        None,
        Duration::from_secs(300),
    )
    .expect("segment");
    SegmentManager::new(
        segment,
        temporary.path().to_owned(),
        derive_component("host.tmux").expect("stream"),
        SyncWake::default(),
        Box::new(UtcZone),
        Arc::new(RecordingWarnings::default()),
        None,
        false,
    )
}

fn display_invocation(pane: &str, field: DisplayField) -> CommandInvocation {
    invocation(
        TmuxOperation::DisplayMessage {
            target: pane.to_owned(),
            field,
        },
        &["display-message", "-p", "-t", pane, field.format()],
    )
}

fn wall_now(seconds: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(seconds).expect("wall time")
}

struct UtcZone;

impl ZoneSource for UtcZone {
    fn read(&mut self) -> Result<Zone, String> {
        Ok(Zone::utc())
    }
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
        operation: CommandOperation::Tmux(operation),
        executable: PathBuf::from(TMUX),
        args: args.iter().map(OsString::from).collect(),
        timeout: TMUX_TIMEOUT,
    }
}
