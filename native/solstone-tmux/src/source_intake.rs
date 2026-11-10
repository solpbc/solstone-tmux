// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! How much terminal capture this computer has taken in since this counter existed.
//!
//! The file is `source-intake.json` in the data root (`~/.local/share/solstone-tmux`
//! on Linux, `~/Library/Application Support/solstone-tmux` on macOS). Another program
//! on this computer can read it. It is not copied anywhere.
//!
//! ```text
//! {
//!   "version": 1,
//!   "terminal": {
//!     "seconds": 0,
//!     "started": "YYYY-MM-DD",
//!     "started_at": "RFC3339",
//!     "last": null,
//!     "seen": ["YYYYMMDD/HHMMSS_LEN"]
//!   }
//! }
//! ```
//!
//! `started` is the local date counting began. `started_at` is that moment. A
//! segment whose own day and time are earlier adds nothing, even if its directory
//! was renamed later. `seen` is the segments already counted. A file that is not
//! version 1, or whose total is not a whole number, is left untouched.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use time::format_description::well_known::Rfc3339;
use time::{Date, Month, OffsetDateTime, PrimitiveDateTime, Time, UtcOffset};

use crate::recovery::RecoveryAction;
use crate::segment::SegmentClose;
use crate::storage::atomic_write_bytes;

pub const FILENAME: &str = "source-intake.json";
const LOCK_NAME: &str = "source-intake.lock";

pub fn path(data_root: &Path) -> PathBuf {
    data_root.join(FILENAME)
}

pub fn ensure(data_root: &Path) {
    let _ = update(data_root, &ClockStamp::now(), |_| false);
}

pub fn record_close(data_root: &Path, close: &SegmentClose) {
    let SegmentClose::Finalized(finalized) = close else {
        return;
    };
    record_finalized(data_root, finalized);
}

pub fn record_finalized(data_root: &Path, finalized: &Path) {
    record_finalized_at(data_root, finalized, &ClockStamp::now());
}

pub fn record_recovery(data_root: &Path, action: RecoveryAction, candidate: &Path) {
    if matches!(
        action,
        RecoveryAction::Finalized | RecoveryAction::RepairThenFinalize
    ) {
        record_finalized(data_root, candidate);
    }
}

fn record_finalized_at(data_root: &Path, finalized: &Path, stamp: &ClockStamp) {
    if !directory_has_terminal_frames(finalized) {
        return;
    }
    let Some((day, segment)) = day_and_segment(finalized) else {
        return;
    };
    let Some(length) = length_suffix(&segment) else {
        return;
    };
    let identity = format!("{day}/{segment}");
    let _ = update(data_root, stamp, |file| {
        if seen_contains(file, &identity) || before_cutoff(file, &day, &segment) {
            return false;
        }
        let Some(total) = file["terminal"]["seconds"].as_u64() else {
            return false;
        };
        file["terminal"]["seconds"] = json!(total.saturating_add(length));
        file["terminal"]["last"] = json!(stamp.at.clone());
        push_seen(file, identity);
        true
    });
}

fn day_and_segment(finalized: &Path) -> Option<(String, String)> {
    let segment = finalized.file_name()?.to_str()?.to_owned();
    let day = finalized.ancestors().skip(1).find_map(|ancestor| {
        let name = ancestor.file_name()?.to_str()?;
        (name.len() == 8 && name.bytes().all(|byte| byte.is_ascii_digit())).then(|| name.to_owned())
    })?;
    Some((day, segment))
}

fn directory_has_terminal_frames(dir: &Path) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    entries.filter_map(Result::ok).any(|entry| {
        entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.ends_with("_screen.jsonl"))
    })
}

fn length_suffix(segment: &str) -> Option<u64> {
    let (stem, suffix) = segment.rsplit_once('_')?;
    if stem.len() != 6 || !stem.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    if suffix.is_empty() || suffix.len() > 6 || !suffix.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    suffix.parse().ok()
}

fn before_cutoff(file: &Value, day: &str, segment: &str) -> bool {
    let Some(started_at) = file["terminal"].get("started_at").and_then(Value::as_str) else {
        return true;
    };
    let Ok(cutoff) = OffsetDateTime::parse(started_at, &Rfc3339) else {
        return true;
    };
    let Some(stem) = segment
        .rsplit_once('_')
        .map(|(stem, _)| stem)
        .or(Some(segment))
    else {
        return true;
    };
    capture_instant(day, stem, cutoff.offset()).is_none_or(|capture| capture < cutoff)
}

fn capture_instant(day: &str, hms: &str, offset: UtcOffset) -> Option<OffsetDateTime> {
    if day.len() != 8 || hms.len() != 6 {
        return None;
    }
    let year: i32 = day[0..4].parse().ok()?;
    let month = Month::try_from(day[4..6].parse::<u8>().ok()?).ok()?;
    let day_n: u8 = day[6..8].parse().ok()?;
    let hour: u8 = hms[0..2].parse().ok()?;
    let minute: u8 = hms[2..4].parse().ok()?;
    let second: u8 = hms[4..6].parse().ok()?;
    let date = Date::from_calendar_date(year, month, day_n).ok()?;
    let time = Time::from_hms(hour, minute, second).ok()?;
    Some(PrimitiveDateTime::new(date, time).assume_offset(offset))
}

fn seen_contains(file: &Value, identity: &str) -> bool {
    file["terminal"]
        .get("seen")
        .and_then(Value::as_array)
        .is_some_and(|seen| seen.iter().any(|item| item.as_str() == Some(identity)))
}

fn push_seen(file: &mut Value, identity: String) {
    if seen_contains(file, &identity) {
        return;
    }
    if !file["terminal"]["seen"].is_array() {
        file["terminal"]["seen"] = json!([]);
    }
    file["terminal"]["seen"]
        .as_array_mut()
        .expect("seen was just made an array")
        .push(json!(identity));
}

fn update(
    data_root: &Path,
    stamp: &ClockStamp,
    change: impl FnOnce(&mut Value) -> bool,
) -> io::Result<()> {
    let Some(_lock) = lock_root(data_root) else {
        return Err(io::Error::other("could not lock source intake"));
    };
    let file_path = path(data_root);
    let existing = match fs::read(&file_path) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    let mut file = match existing {
        None => fresh_file(stamp),
        Some(ref bytes) => match parse_file(bytes) {
            Some(file) => file,
            None => return Ok(()),
        },
    };
    if !file.get("terminal").is_some_and(Value::is_object) {
        file["terminal"] = terminal_object(stamp);
    }
    if file["terminal"]
        .get("seconds")
        .and_then(Value::as_u64)
        .is_none()
    {
        file["terminal"]["seconds"] = json!(0);
    }
    if file["terminal"]
        .get("started")
        .and_then(Value::as_str)
        .is_none()
    {
        file["terminal"]["started"] = json!(stamp.started.clone());
    }
    if file["terminal"]
        .get("started_at")
        .and_then(Value::as_str)
        .is_none()
    {
        file["terminal"]["started_at"] = json!(stamp.at.clone());
    }
    if file["terminal"].get("last").is_none() {
        file["terminal"]["last"] = Value::Null;
    }
    if !file["terminal"]["seen"].is_array() {
        file["terminal"]["seen"] = json!([]);
    }
    let changed = change(&mut file);
    if !changed && existing.is_some() {
        return Ok(());
    }
    let bytes =
        serde_json::to_vec_pretty(&file).map_err(|error| io::Error::other(error.to_string()))?;
    atomic_write_bytes(&file_path, data_root, &bytes)
        .map_err(|error| io::Error::other(error.to_string()))
}

fn lock_root(data_root: &Path) -> Option<File> {
    fs::create_dir_all(data_root).ok()?;
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(data_root.join(LOCK_NAME))
        .ok()?;
    file.write_all(b"").ok()?;
    rustix::fs::flock(&file, rustix::fs::FlockOperation::LockExclusive).ok()?;
    Some(file)
}

fn parse_file(bytes: &[u8]) -> Option<Value> {
    let value: Value = serde_json::from_slice(bytes).ok()?;
    if value.get("version").and_then(Value::as_u64) != Some(1) {
        return None;
    }
    if let Some(seconds) = value
        .get("terminal")
        .and_then(|terminal| terminal.get("seconds"))
        && !(seconds.as_u64().is_some() && seconds.is_u64())
    {
        return None;
    }
    if let Some(started_at) = value
        .get("terminal")
        .and_then(|terminal| terminal.get("started_at"))
        && !started_at
            .as_str()
            .is_some_and(|text| OffsetDateTime::parse(text, &Rfc3339).is_ok())
    {
        return None;
    }
    if let Some(seen) = value
        .get("terminal")
        .and_then(|terminal| terminal.get("seen"))
    {
        let Value::Array(items) = seen else {
            return None;
        };
        if items.iter().any(|item| item.as_str().is_none()) {
            return None;
        }
    }
    Some(value)
}

fn fresh_file(stamp: &ClockStamp) -> Value {
    json!({
        "version": 1,
        "terminal": terminal_object(stamp),
    })
}

fn terminal_object(stamp: &ClockStamp) -> Value {
    json!({
        "seconds": 0,
        "started": stamp.started,
        "started_at": stamp.at,
        "last": Value::Null,
        "seen": [],
    })
}

struct ClockStamp {
    started: String,
    at: String,
}

impl ClockStamp {
    fn now() -> Self {
        // `time` 0.3 here has no `now_local`. The same zone the capture clock
        // uses turns the UTC instant into the local calendar day.
        let utc = OffsetDateTime::now_utc();
        let offset = crate::clock::resolve_system_zone()
            .map(|zone| zone.offset_at(utc))
            .unwrap_or(UtcOffset::UTC);
        Self::from_offset(utc.to_offset(offset))
    }

    fn from_offset(now: OffsetDateTime) -> Self {
        Self {
            started: format!(
                "{:04}-{:02}-{:02}",
                now.year(),
                u8::from(now.month()),
                now.day()
            ),
            at: now.format(&Rfc3339).unwrap_or_else(|_| now.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::{TestClock, Zone, ZoneSource};
    use crate::model::{CaptureResult, PaneInfo, WindowInfo};
    use crate::name::derive_component;
    use crate::observer::{SegmentLifecycle, SegmentManager};
    use crate::sync::SyncWake;
    use crate::tmux::StderrWarnings;
    use std::sync::Arc;
    use std::time::Duration;

    struct FixedZone(UtcOffset);

    impl ZoneSource for FixedZone {
        fn read(&mut self) -> Result<Zone, String> {
            Ok(Zone::fixed(self.0))
        }
    }

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "solstone-intake-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn offset() -> UtcOffset {
        UtcOffset::from_hms(-6, 0, 0).unwrap()
    }

    fn wall() -> OffsetDateTime {
        OffsetDateTime::parse("2026-11-09T16:00:00-06:00", &Rfc3339).unwrap()
    }

    fn capture() -> CaptureResult {
        CaptureResult {
            session: "s1".into(),
            window: WindowInfo {
                id: "w1".into(),
                index: 0,
                name: "shell".into(),
                active: true,
            },
            windows: vec![],
            panes: vec![PaneInfo {
                id: "p1".into(),
                index: 0,
                left: 0,
                top: 0,
                width: 80,
                height: 24,
                active: true,
                cursor_x: 0,
                cursor_y: 0,
                cursor_visible: true,
                alternate_on: false,
                pane_in_mode: false,
                content: "hello".into(),
            }],
            output_only: false,
        }
    }

    fn seed(root: &Path) {
        fs::create_dir_all(root).unwrap();
        fs::write(
            path(root),
            br#"{"version":1,"terminal":{"seconds":0,"started":"2026-11-09","started_at":"2026-11-09T00:00:00-06:00","last":null,"seen":[]}}"#,
        )
        .unwrap();
    }

    #[test]
    fn shutdown_counts_a_segment_with_frames_once_without_the_close_wake() {
        let temp = Scratch::new();
        let root = temp.path().join("data");
        fs::create_dir_all(&root).unwrap();
        seed(&root);
        let clock = TestClock::new(wall(), Duration::from_secs(0), offset());
        let wake = SyncWake::default();
        let mut manager = SegmentManager::start(
            root.clone(),
            derive_component("host").unwrap(),
            &clock,
            Duration::from_secs(300),
            wake.clone(),
            Box::new(FixedZone(offset())),
            Arc::new(StderrWarnings),
        )
        .unwrap();
        manager
            .segment_mut()
            .append_capture(&capture(), 0.0, Duration::from_secs(1))
            .unwrap();
        let close = SegmentLifecycle::shutdown(&mut manager, Duration::from_secs(5)).unwrap();
        let SegmentClose::Finalized(finalized) = &close else {
            panic!("segment with frames was not finalized");
        };
        assert!(finalized.join("tmux_s1_screen.jsonl").is_file());
        record_close(&root, &close);
        let file: Value = serde_json::from_slice(&fs::read(path(&root)).unwrap()).unwrap();
        assert_eq!(file["terminal"]["seconds"], 5);
        assert_eq!(file["terminal"]["seen"].as_array().unwrap().len(), 1);
        assert!(file["terminal"]["last"].is_string());

        let mut again = SegmentManager::start(
            root.clone(),
            derive_component("host").unwrap(),
            &clock,
            Duration::from_secs(300),
            SyncWake::default(),
            Box::new(FixedZone(offset())),
            Arc::new(StderrWarnings),
        )
        .unwrap();
        let _ = SegmentLifecycle::shutdown(&mut again, Duration::from_secs(1));
        record_recovery(&root, RecoveryAction::Finalized, finalized);
        let file: Value = serde_json::from_slice(&fs::read(path(&root)).unwrap()).unwrap();
        assert_eq!(file["terminal"]["seconds"], 5);
    }

    #[test]
    fn a_leftover_before_the_cutoff_adds_nothing_even_though_it_was_just_renamed() {
        let temp = Scratch::new();
        let root = temp.path().join("data");
        seed(&root);
        let old = root
            .join("captures")
            .join("20261108")
            .join("host")
            .join("150000_004");
        fs::create_dir_all(&old).unwrap();
        fs::write(old.join("tmux_s1_screen.jsonl"), b"frame").unwrap();
        record_recovery(&root, RecoveryAction::RepairThenFinalize, &old);
        let file: Value = serde_json::from_slice(&fs::read(path(&root)).unwrap()).unwrap();
        assert_eq!(file["terminal"]["seconds"], 0);
        assert_eq!(file["terminal"]["seen"], json!([]));
        let kept = root
            .join("captures")
            .join("20261109")
            .join("host")
            .join("180000_008");
        fs::create_dir_all(&kept).unwrap();
        fs::write(kept.join("tmux_s1_screen.jsonl"), b"frame").unwrap();
        record_recovery(&root, RecoveryAction::Finalized, &kept);
        record_recovery(&root, RecoveryAction::Finalized, &kept);
        let file: Value = serde_json::from_slice(&fs::read(path(&root)).unwrap()).unwrap();
        assert_eq!(file["terminal"]["seconds"], 8);
        assert_eq!(file["terminal"]["seen"], json!(["20261109/180000_008"]));
    }

    #[test]
    fn activity_only_a_nameless_length_and_a_bad_file_add_nothing() {
        let temp = Scratch::new();
        let root = temp.path().join("data");
        seed(&root);
        let activity = root
            .join("captures")
            .join("20261109")
            .join("host")
            .join("180000_003");
        fs::create_dir_all(&activity).unwrap();
        fs::write(activity.join("180000_desktop_activity.jsonl"), b"row").unwrap();
        record_finalized(&root, &activity);
        let bare = root
            .join("captures")
            .join("20261109")
            .join("host")
            .join("180000");
        fs::create_dir_all(&bare).unwrap();
        fs::write(bare.join("tmux_s1_screen.jsonl"), b"frame").unwrap();
        record_finalized(&root, &bare);
        let file: Value = serde_json::from_slice(&fs::read(path(&root)).unwrap()).unwrap();
        assert_eq!(file["terminal"]["seconds"], 0);
        assert_eq!(file["terminal"]["seen"], json!([]));

        let bad = br#"{"version":1,"terminal":{"seconds":1.5}}"#;
        fs::write(path(&root), bad).unwrap();
        record_finalized(&root, &activity);
        assert_eq!(fs::read(path(&root)).unwrap(), bad);
    }

    #[test]
    fn the_local_date_is_not_taken_from_utc() {
        let temp = Scratch::new();
        let root = temp.path().join("data");
        let _ = update(
            &root,
            &ClockStamp::from_offset(
                OffsetDateTime::parse("2020-01-01T20:30:00-06:00", &Rfc3339).unwrap(),
            ),
            |_| false,
        );
        let file: Value = serde_json::from_slice(&fs::read(path(&root)).unwrap()).unwrap();
        assert_eq!(file["terminal"]["started"], "2020-01-01");
    }
}
