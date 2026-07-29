// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

mod support;

use solstone_tmux::serialize::serialize_frame;
use support::{golden_capture, sha256};

#[test]
fn python_main_golden_is_byte_exact() {
    let expected = include_bytes!("data/golden/python-envelope-main.jsonl");
    let actual = serialize_frame(&golden_capture("main"), 1, 0.25).expect("serialize frame");

    assert_eq!(actual.as_slice(), expected);
    assert_eq!(actual.len(), 724);
    assert_eq!(
        sha256(&actual),
        [
            0x9a, 0xfd, 0x47, 0xd8, 0xd6, 0xf4, 0x40, 0x16, 0x2d, 0xd8, 0xe3, 0x03, 0x59, 0xd8,
            0x89, 0x12, 0xa1, 0xf9, 0xcd, 0x64, 0x33, 0x91, 0x17, 0x3d, 0x43, 0xeb, 0x6d, 0x6c,
            0x80, 0x4e, 0x31, 0x87,
        ]
    );
}

#[test]
fn python_special_golden_preserves_raw_identity() {
    let expected = include_bytes!("data/golden/python-envelope-special.jsonl");
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
