// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

#[path = "../support/mod.rs"]
mod support;

use solstone_tmux::serialize::{
    serialize_availability_activity, serialize_frame, serialize_terminal_activity,
};
use support::{golden_capture, sha256};

#[test]
fn observer_main_golden_is_byte_exact() {
    let expected = include_bytes!("../data/golden/observer-envelope-main.jsonl");
    let actual = serialize_frame(&golden_capture("main"), 1, 0.25).expect("serialize frame");

    assert_eq!(actual.as_slice(), expected);
    assert_eq!(actual.len(), 924);
    assert_eq!(
        sha256(&actual),
        [
            0x92, 0xa1, 0x61, 0xba, 0x1e, 0xd8, 0x84, 0x9f, 0xad, 0x09, 0xc8, 0xe5, 0x41, 0x6e,
            0xec, 0x78, 0x67, 0x39, 0x64, 0xdc, 0x8a, 0x11, 0xe7, 0x64, 0xca, 0x23, 0xb4, 0xf0,
            0xda, 0xe9, 0xac, 0x65
        ]
    );
}

#[test]
fn observer_special_golden_preserves_raw_identity() {
    let expected = include_bytes!("../data/golden/observer-envelope-special.jsonl");
    let actual =
        serialize_frame(&golden_capture("my/session name"), 2, 12.5).expect("serialize frame");

    assert_eq!(actual.as_slice(), expected);
}

#[test]
fn ascii_escapes_unicode_and_ansi_with_trailing_lf() {
    let actual = serialize_frame(&golden_capture("main"), 1, 0.25).expect("serialize frame");
    let text = std::str::from_utf8(&actual).expect("JSON is ASCII UTF-8");

    assert!(text.contains(r"dev caf\u00e9"));
    assert!(text.contains(r"\u001b[31mRED\u001b[0m caf\u00e9\n"));
    assert!(!text.contains('é'));
    assert!(!actual.contains(&0x1b));
    assert!(actual.ends_with(b"\n"));
    assert!(!actual[..actual.len() - 1].contains(&b'\n'));
    assert!(text.starts_with(r#"{"frame_id": 1, "timestamp": 0.25, "requests": []"#));
}

#[test]
fn activity_rows_are_compact_and_omit_unspecified_fields() {
    let bytes = serialize_terminal_activity(1234, "main", "dev", Some("title"), None, None);
    assert_eq!(
    bytes,
        br#"{"v":1,"t":"desktop_activity","kind":"terminal","method":"tmux","ts":1234,"end_ts":1234,"session":"main","window":"dev","pane_title":"title"}"#
        .iter()
        .copied()
        .chain(*b"\n")
        .collect::<Vec<_>>()
);
    assert!(!bytes.windows(4).any(|window| window == b"null"));
}

#[test]
fn activity_availability_row_has_only_its_closed_key_set() {
    let bytes = serialize_availability_activity(1234, "unavailable");
    let value: serde_json::Value = serde_json::from_slice(&bytes).expect("availability JSON");
    let object = value.as_object().expect("object");
    assert_eq!(
        object
            .keys()
            .map(String::as_str)
            .collect::<std::collections::BTreeSet<_>>(),
        [
            "v",
            "t",
            "kind",
            "capability",
            "status",
            "method",
            "ts",
            "end_ts"
        ]
        .into_iter()
        .collect()
    );
    assert!(bytes.ends_with(b"\n"));
    assert!(!bytes[..bytes.len() - 1].contains(&b'\n'));
}
