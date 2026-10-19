// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

#[path = "../support/mod.rs"]
mod support;

use std::ffi::OsString;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use serde_json::Value;
use solstone_tmux::command::{
    CommandInvocation, CommandOperation, CommandRunner, DisplayField, TmuxOperation,
};
use solstone_tmux::foreground::{ExeFailure, ForegroundLookup};
use solstone_tmux::tmux::{TMUX_TIMEOUT, TmuxAdapter, TmuxError, reports_no_server};
use support::{
    ExpectedInvocation, FixtureOutcome, FixtureRunner, RecordingWarnings, command_output, nonzero,
    output,
};

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
async fn no_tmux_server_means_no_clients_to_observe() {
    // Before any server starts the socket file is missing; after the last
    // session closes it is stale. tmux reports the two differently.
    for stderr in [
        b"error connecting to /tmp/tmux-1000/default (No such file or directory)\n".as_slice(),
        b"no server running on /tmp/tmux-1000/default\n".as_slice(),
    ] {
        let expected = invocation(
            TmuxOperation::ListClients,
            &["list-clients", "-F", "#{client_session} #{client_activity}"],
        );
        let runner = FixtureRunner::new([ExpectedInvocation {
            invocation: expected,
            outcome: command_output(b"", stderr, 1),
        }]);
        let adapter = TmuxAdapter::new(PathBuf::from(TMUX), runner.clone()).expect("adapter");

        let clients = adapter
            .list_active_clients(100, Duration::from_secs(5))
            .await
            .expect("no server is not a capture failure");

        assert!(clients.is_empty());
        runner.assert_finished().expect("all fixture calls used");
    }
}

#[tokio::test]
async fn other_list_clients_failures_still_surface() {
    let expected = invocation(
        TmuxOperation::ListClients,
        &["list-clients", "-F", "#{client_session} #{client_activity}"],
    );
    let runner = FixtureRunner::new([ExpectedInvocation {
        invocation: expected,
        outcome: command_output(
            b"",
            b"error connecting to /tmp/tmux-1000/default (Permission denied)\n",
            1,
        ),
    }]);
    let adapter = TmuxAdapter::new(PathBuf::from(TMUX), runner.clone()).expect("adapter");

    let error = adapter
        .list_clients()
        .await
        .expect_err("permission failure");

    assert!(matches!(error, TmuxError::Nonzero { .. }));
    runner.assert_finished().expect("all fixture calls used");
}

#[test]
fn no_server_detection_requires_a_failed_command() {
    let missing = solstone_tmux::command::CommandOutput {
        stdout: Vec::new(),
        stderr: b"no server running on /tmp/tmux-1000/default\n".to_vec(),
        status: 0,
    };
    assert!(!reports_no_server(&missing));
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
        outcome: output(include_bytes!("../data/tmux/list-clients-spaced.txt").to_vec()),
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
        outcome: output(include_bytes!("../data/tmux/list-clients-spaced.txt").to_vec()),
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
async fn multiple_clients_on_one_session_use_most_recent_activity() {
    let runner = FixtureRunner::new([ExpectedInvocation {
        invocation: invocation(
            TmuxOperation::ListClients,
            &["list-clients", "-F", "#{client_session} #{client_activity}"],
        ),
        outcome: output(b"main 100\nmain 105\nother 103\nmain 102\n".to_vec()),
    }]);
    let warnings = RecordingWarnings::default();
    let adapter =
        TmuxAdapter::with_warnings(PathBuf::from(TMUX), runner, warnings.clone()).expect("adapter");

    let clients = adapter
        .list_active_clients(106, Duration::from_secs(2))
        .await
        .expect("active clients");

    assert_eq!(clients.len(), 1);
    assert_eq!(clients[0].session, "main");
    assert_eq!(clients[0].activity, 105);
    assert!(warnings.messages().is_empty());
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
            outcome: output(include_bytes!("../data/tmux/list-windows-multi.txt").to_vec()),
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
            outcome: output(include_bytes!("../data/tmux/list-panes-split.txt").to_vec()),
        },
        ExpectedInvocation {
            invocation: invocation(
                TmuxOperation::CapturePane("%0".to_owned()),
                &["capture-pane", "-p", "-e", "-t", "%0"],
            ),
            outcome: output(include_bytes!("../data/tmux/capture-pane-ansi.bin").to_vec()),
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
    let bytes = include_bytes!("../data/tmux/capture-pane-ansi.bin");
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

fn successful_session(session: &str, pane: Vec<u8>) -> impl Iterator<Item = ExpectedInvocation> {
    [
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
]
.into_iter()
}

#[tokio::test]
async fn all_false_context_keeps_argv_unchanged() {
    let runner = FixtureRunner::new(
        [ExpectedInvocation {
            invocation: clients_invocation(),
            outcome: output(b"main 100\n".to_vec()),
        }]
        .into_iter()
        .chain(successful_session("main", b"screen\n".to_vec())),
    );
    let lookup_calls = Arc::new(Mutex::new(Vec::new()));
    let adapter = context_adapter(runner.clone(), false, false, false, lookup_calls.clone());

    let poll = adapter
        .poll_context(wall_now(100), Duration::from_secs(5))
        .await
        .expect("poll");

    assert!(poll.activity_jsonl.is_empty());
    assert!(lookup_calls.lock().expect("lookup calls").is_empty());
    assert_eq!(runner.calls().len(), 4);
    runner.assert_finished().expect("all fixture calls used");
}

#[tokio::test]
async fn pane_title_switch_uses_only_title_argv_and_json_key() {
    let runner = context_runner_with("main", DisplayField::PaneTitle, output(b"title\n".to_vec()));
    let lookup_calls = Arc::new(Mutex::new(Vec::new()));
    let adapter = context_adapter(runner.clone(), true, false, false, lookup_calls.clone());
    let poll = adapter
        .poll_context(wall_now(100), Duration::from_secs(5))
        .await
        .expect("poll");
    let row = only_row(&poll.activity_jsonl);
    assert_eq!(row["window"], "active");
    assert_ne!(row["window"], "@0");
    assert_eq!(row["pane_title"], "title");
    assert!(row.get("cwd").is_none());
    assert!(row.get("exe").is_none());
    assert!(lookup_calls.lock().expect("lookup calls").is_empty());
    assert_eq!(
        runner.calls().last().expect("display invocation").args,
        ["display-message", "-p", "-t", "%0", "#{pane_title}"]
            .into_iter()
            .map(OsString::from)
            .collect::<Vec<_>>()
    );
    runner.assert_finished().expect("all fixture calls used");
}

#[tokio::test]
async fn working_directory_switch_uses_only_cwd_argv_and_json_key() {
    let runner = context_runner_with(
        "main",
        DisplayField::PaneCurrentPath,
        output(b"/work\n".to_vec()),
    );
    let lookup_calls = Arc::new(Mutex::new(Vec::new()));
    let adapter = context_adapter(runner.clone(), false, true, false, lookup_calls.clone());
    let poll = adapter
        .poll_context(wall_now(100), Duration::from_secs(5))
        .await
        .expect("poll");
    let row = only_row(&poll.activity_jsonl);
    assert_eq!(row["window"], "active");
    assert_ne!(row["window"], "@0");
    assert_eq!(row["cwd"], "/work");
    assert!(row.get("pane_title").is_none());
    assert!(row.get("exe").is_none());
    assert!(lookup_calls.lock().expect("lookup calls").is_empty());
    assert_eq!(
        runner.calls().last().expect("display invocation").args[4],
        "#{pane_current_path}"
    );
    runner.assert_finished().expect("all fixture calls used");
}

#[tokio::test]
async fn executable_switch_queries_pid_and_uses_lookup_basename() {
    let runner = FixtureRunner::new(
        [ExpectedInvocation {
            invocation: clients_invocation(),
            outcome: output(b"main 100\n".to_vec()),
        }]
        .into_iter()
        .chain(successful_session("main", b"screen\n".to_vec()))
        .chain([ExpectedInvocation {
            invocation: display_invocation("%0", DisplayField::PanePid),
            outcome: output(b"321\n".to_vec()),
        }]),
    );
    let lookup_calls = Arc::new(Mutex::new(Vec::new()));
    let adapter = context_adapter(runner.clone(), false, false, true, lookup_calls.clone());
    let poll = adapter
        .poll_context(wall_now(100), Duration::from_secs(5))
        .await
        .expect("poll");
    let row = only_row(&poll.activity_jsonl);
    assert_eq!(row["window"], "active");
    assert_ne!(row["window"], "@0");
    assert_eq!(row["exe"], "sleep");
    assert_eq!(lookup_calls.lock().expect("lookup calls").as_slice(), [321]);
    assert_eq!(runner.calls()[4].args[4], "#{pane_pid}");
    assert_eq!(runner.calls().len(), 5);
    assert!(!runner.calls().iter().any(|call| {
        call.args.iter().any(|argument| argument == "#{pane_tty}")
    }));
    runner.assert_finished().expect("all fixture calls used");
}

#[tokio::test]
async fn executable_invalid_pid_warns_without_lookup() {
    let runner = FixtureRunner::new(
        [ExpectedInvocation {
            invocation: clients_invocation(),
            outcome: output(b"main 100\n".to_vec()),
        }]
        .into_iter()
        .chain(successful_session("main", b"screen\n".to_vec()))
        .chain([ExpectedInvocation {
            invocation: display_invocation("%0", DisplayField::PanePid),
            outcome: output(b"0\n".to_vec()),
        }]),
    );
    let lookup_calls = Arc::new(Mutex::new(Vec::new()));
    let warnings = RecordingWarnings::default();
    let adapter = TmuxAdapter::with_context(
        PathBuf::from(TMUX),
        runner.clone(),
        warnings.clone(),
        false,
        false,
        true,
        Arc::new(FakeLookup::new(
            lookup_calls.clone(),
            Ok("sleep".to_owned()),
        )),
    )
    .expect("adapter");
    let poll = adapter
        .poll_context(wall_now(100), Duration::from_secs(5))
        .await
        .expect("poll");
    assert_eq!(runner.calls()[4].args[4], "#{pane_pid}");
    assert_eq!(runner.calls().len(), 5);
    assert!(lookup_calls.lock().expect("lookup calls").is_empty());
    let row = only_row(&poll.activity_jsonl);
    assert_eq!(row["kind"], "availability");
    assert_eq!(row["status"], "unavailable");
    assert_eq!(
        warnings.messages(),
        ["executable was not recorded (unavailable)"]
    );
    runner.assert_finished().expect("all fixture calls used");
}

#[tokio::test]
async fn executable_unsupported_is_only_reported_as_unsupported() {
    let runner = FixtureRunner::new(
        [ExpectedInvocation {
            invocation: clients_invocation(),
            outcome: output(b"main 100\n".to_vec()),
        }]
        .into_iter()
        .chain(successful_session("main", b"screen\n".to_vec()))
        .chain([ExpectedInvocation {
            invocation: display_invocation("%0", DisplayField::PanePid),
            outcome: output(b"321\n".to_vec()),
        }]),
    );
    let warnings = RecordingWarnings::default();
    let adapter = TmuxAdapter::with_context(
        PathBuf::from(TMUX),
        runner.clone(),
        warnings.clone(),
        false,
        false,
        true,
        Arc::new(FakeLookup::new(
            Arc::new(Mutex::new(Vec::new())),
            Err(ExeFailure::Unsupported),
        )),
    )
    .expect("adapter");
    let poll = adapter
        .poll_context(wall_now(100), Duration::from_secs(5))
        .await
        .expect("poll");
    assert_eq!(only_row(&poll.activity_jsonl)["status"], "unsupported");
    assert_eq!(
        warnings.messages(),
        ["executable was not recorded (unsupported)"]
    );
    runner.assert_finished().expect("all fixture calls used");
}

#[tokio::test]
async fn stale_client_does_not_query_pane_context() {
    let runner = FixtureRunner::new([ExpectedInvocation {
        invocation: clients_invocation(),
        outcome: output(b"main 90\n".to_vec()),
    }]);
    let lookup_calls = Arc::new(Mutex::new(Vec::new()));
    let adapter = context_adapter(runner.clone(), true, true, true, lookup_calls.clone());
    let poll = adapter
        .poll_context(wall_now(100), Duration::from_secs(5))
        .await
        .expect("poll");
    assert!(poll.captures.is_empty());
    assert!(poll.activity_jsonl.is_empty());
    assert!(lookup_calls.lock().expect("lookup calls").is_empty());
    assert_eq!(runner.calls().len(), 1);
    runner.assert_finished().expect("all fixture calls used");
}

#[tokio::test]
async fn metadata_fixtures_37b_and_38_preserve_observed_title_and_cwd() {
    let runner = FixtureRunner::new(
        [ExpectedInvocation {
            invocation: clients_invocation(),
            outcome: output(b"main 100\nother 100\n".to_vec()),
        }]
        .into_iter()
        .chain(successful_session("main", b"one\n".to_vec()))
        .chain([
            ExpectedInvocation {
                invocation: display_invocation("%0", DisplayField::PaneTitle),
                outcome: output(
                    include_bytes!("../data/tmux-metadata/3.7b-pane_title.bin").to_vec(),
                ),
            },
            ExpectedInvocation {
                invocation: display_invocation("%0", DisplayField::PaneCurrentPath),
                outcome: output(
                    include_bytes!("../data/tmux-metadata/3.7b-pane_current_path.bin").to_vec(),
                ),
            },
        ])
        .chain(successful_session("other", b"two\n".to_vec()))
        .chain([
            ExpectedInvocation {
                invocation: display_invocation("%0", DisplayField::PaneTitle),
                outcome: output(
                    include_bytes!("../data/tmux-metadata/3.8-pane_title.bin").to_vec(),
                ),
            },
            ExpectedInvocation {
                invocation: display_invocation("%0", DisplayField::PaneCurrentPath),
                outcome: output(
                    include_bytes!("../data/tmux-metadata/3.8-pane_current_path.bin").to_vec(),
                ),
            },
        ]),
    );
    let adapter = TmuxAdapter::with_context(
        PathBuf::from(TMUX),
        runner.clone(),
        RecordingWarnings::default(),
        true,
        true,
        false,
        Arc::new(FakeLookup::new(
            Arc::new(Mutex::new(Vec::new())),
            Ok("unused".to_owned()),
        )),
    )
    .expect("adapter");
    let poll = adapter
        .poll_context(wall_now(100), Duration::from_secs(5))
        .await
        .expect("poll");
    let rows = poll
        .activity_jsonl
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice::<Value>(line).expect("activity row"))
        .collect::<Vec<_>>();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["session"], "main");
    assert_eq!(rows[1]["session"], "other");
    let expected_title = r#"synthetic title "quoted" \\ slash #{pane_current_path}"#;
    let expected_cwd = "/var/tmp/tmux-metadata-fixtures-hchhor97/path with space\ttab\nline";
    for row in rows {
        assert_eq!(row["window"], "active");
        assert_ne!(row["window"], "@0");
        assert_eq!(row["pane_title"], expected_title);
        assert_eq!(row["cwd"], expected_cwd);
        assert!(row.get("exe").is_none());
    }
    runner.assert_finished().expect("all fixture calls used");
}

#[tokio::test]
async fn metadata_failures_warn_without_payload() {
    for outcome in [
        FixtureOutcome::Timeout,
        nonzero(1),
        output(b"unterminated".to_vec()),
    ] {
        let runner = context_runner_with("main", DisplayField::PaneTitle, outcome);
        let warnings = RecordingWarnings::default();
        let adapter = TmuxAdapter::with_context(
            PathBuf::from(TMUX),
            runner.clone(),
            warnings.clone(),
            true,
            false,
            false,
            Arc::new(FakeLookup::new(
                Arc::new(Mutex::new(Vec::new())),
                Ok("unused".to_owned()),
            )),
        )
        .expect("adapter");
        let poll = adapter
            .poll_context(wall_now(100), Duration::from_secs(5))
            .await
            .expect("metadata failure is nonfatal");
        assert_eq!(poll.captures[0].panes[0].content, "screen\n");
        assert!(
            std::str::from_utf8(&poll.activity_jsonl)
                .unwrap()
                .contains("availability")
        );
        assert_eq!(
            warnings.messages(),
            ["pane title was not recorded (unavailable)"]
        );
        runner.assert_finished().expect("all fixture calls used");
    }
}

#[tokio::test]
async fn successful_sibling_field_is_kept_with_one_availability_row() {
    let runner = FixtureRunner::new(
        [ExpectedInvocation {
            invocation: clients_invocation(),
            outcome: output(b"main 100\n".to_vec()),
        }]
        .into_iter()
        .chain(successful_session("main", b"screen\n".to_vec()))
        .chain([
            ExpectedInvocation {
                invocation: display_invocation("%0", DisplayField::PaneTitle),
                outcome: output(b"title\n".to_vec()),
            },
            ExpectedInvocation {
                invocation: display_invocation("%0", DisplayField::PaneCurrentPath),
                outcome: FixtureOutcome::Timeout,
            },
        ]),
    );
    let warnings = RecordingWarnings::default();
    let adapter = TmuxAdapter::with_context(
        PathBuf::from(TMUX),
        runner.clone(),
        warnings.clone(),
        true,
        true,
        false,
        Arc::new(FakeLookup::new(
            Arc::new(Mutex::new(Vec::new())),
            Ok("unused".to_owned()),
        )),
    )
    .expect("adapter");
    let poll = adapter
        .poll_context(wall_now(100), Duration::from_secs(5))
        .await
        .expect("poll");
    let rows = poll
        .activity_jsonl
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice::<Value>(line).expect("activity row"))
        .collect::<Vec<_>>();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["kind"], "terminal");
    assert_eq!(rows[0]["window"], "active");
    assert_ne!(rows[0]["window"], "@0");
    assert_eq!(rows[0]["pane_title"], "title");
    assert!(rows[0].get("cwd").is_none());
    assert_eq!(rows[1]["kind"], "availability");
    assert_eq!(rows[1]["status"], "unavailable");
    assert_eq!(
        warnings.messages(),
        ["working directory was not recorded (unavailable)"]
    );
    runner.assert_finished().expect("all fixture calls used");
}

fn clients_invocation() -> CommandInvocation {
    invocation(
        TmuxOperation::ListClients,
        &["list-clients", "-F", "#{client_session} #{client_activity}"],
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

fn context_runner_with(
    session: &str,
    field: DisplayField,
    outcome: support::FixtureOutcome,
) -> FixtureRunner {
    FixtureRunner::new(
        [ExpectedInvocation {
            invocation: clients_invocation(),
            outcome: output(format!("{session} 100\n").into_bytes()),
        }]
        .into_iter()
        .chain(successful_session(session, b"screen\n".to_vec()))
        .chain([ExpectedInvocation {
            invocation: display_invocation("%0", field),
            outcome,
        }]),
    )
}

fn context_adapter(
    runner: FixtureRunner,
    pane_title: bool,
    working_directory: bool,
    executable_enabled: bool,
    lookup_calls: Arc<Mutex<Vec<u32>>>,
) -> TmuxAdapter<FixtureRunner, RecordingWarnings> {
    TmuxAdapter::with_context(
        PathBuf::from(TMUX),
        runner,
        RecordingWarnings::default(),
        pane_title,
        working_directory,
        executable_enabled,
        Arc::new(FakeLookup::new(lookup_calls, Ok("sleep".to_owned()))),
    )
    .expect("adapter")
}

fn only_row(bytes: &[u8]) -> Value {
    serde_json::from_slice(&bytes[..bytes.len() - 1]).expect("one JSON row")
}

fn wall_now(seconds: i64) -> time::OffsetDateTime {
    time::OffsetDateTime::from_unix_timestamp(seconds).expect("wall time")
}

struct FakeLookup {
    pids: Arc<Mutex<Vec<u32>>>,
    result: Result<String, ExeFailure>,
}

impl FakeLookup {
    fn new(pids: Arc<Mutex<Vec<u32>>>, result: Result<String, ExeFailure>) -> Self {
        Self { pids, result }
    }
}

impl ForegroundLookup for FakeLookup {
    fn lookup<'a>(
        &'a self,
        pane_pid: u32,
    ) -> Pin<Box<dyn Future<Output = Result<String, ExeFailure>> + Send + 'a>> {
        Box::pin(async move {
            self.pids
                .lock()
                .expect("lookup calls")
                .push(pane_pid);
            self.result.clone()
        })
    }
}

fn invocation(operation: TmuxOperation, args: &[&str]) -> CommandInvocation {
    CommandInvocation {
        operation: CommandOperation::Tmux(operation),
        executable: PathBuf::from(TMUX),
        args: args.iter().map(OsString::from).collect(),
        timeout: TMUX_TIMEOUT,
    }
}
