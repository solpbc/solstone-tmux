// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use solstone_tmux::command::{
    CommandInvocation, CommandOperation, CommandRunner, TokioCommandRunner,
};
use std::time::Duration;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let output = TokioCommandRunner
        .run(CommandInvocation {
            operation: CommandOperation::HostFact,
            executable: "/usr/bin/locale".into(),
            args: vec!["-k".into(), "LC_NUMERIC".into()],
            timeout: Duration::from_secs(5),
        })
        .await
        .expect("locale subprocess must start");
    assert_eq!(output.status, 0);
    assert!(
        output.stderr.is_empty(),
        "locale subprocess reported an error"
    );
    println!(
        "{}",
        serde_json::json!({
            "parent_lc_all": std::env::var("LC_ALL").unwrap_or_default(),
            "child_lc_numeric": String::from_utf8(output.stdout).expect("locale output must be UTF-8"),
        })
    );
}
