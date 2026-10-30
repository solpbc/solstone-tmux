// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Lines solstone-tmux writes to stderr. While `run` is the process, each
//! line starts with the local time, so a line in the service log can be
//! matched to the moment it was written.

use std::fmt::Display;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};

static TIMESTAMPS: AtomicBool = AtomicBool::new(false);

/// Starts every later line with the local time. `run` calls this first.
pub fn enable_timestamps() {
    TIMESTAMPS.store(true, Ordering::Relaxed);
}

pub fn line(message: impl Display) {
    let text = if TIMESTAMPS.load(Ordering::Relaxed) {
        timestamped_line(&jiff::Zoned::now(), message)
    } else {
        format!("solstone-tmux: {message}")
    };
    let _ = writeln!(std::io::stderr().lock(), "{text}");
}

pub fn warn(message: impl Display) {
    line(format_args!("warning: {message}"));
}

pub fn timestamped_line(now: &jiff::Zoned, message: impl Display) -> String {
    format!(
        "{} solstone-tmux: {message}",
        now.strftime("%Y-%m-%dT%H:%M:%S%:z")
    )
}
