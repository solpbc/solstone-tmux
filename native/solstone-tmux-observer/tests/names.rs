// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::ffi::OsString;
use std::sync::atomic::{AtomicUsize, Ordering};

use solstone_tmux_observer::name::{NameError, derive_component};
use solstone_tmux_observer::paths::{Environment, PathError, PlatformKind, resolve_stream_paths};

#[test]
fn main_keeps_canonical_filename() {
    let name = derive_component("main").expect("canonical name");
    assert_eq!(name.as_str(), "main");
    assert_eq!(name.session_filename(), "tmux_main_screen.jsonl");
}

#[test]
fn slash_space_and_underscore_do_not_alias() {
    let special = derive_component("my/session name").expect("special name");
    let canonical = derive_component("my_session_name").expect("canonical name");
    assert_eq!(
        special.as_str(),
        "my_session_name~6d792f73657373696f6e206e616d65"
    );
    assert_eq!(canonical.as_str(), "my_session_name");
    assert_ne!(special, canonical);
}

#[test]
fn ascii_case_variants_do_not_alias() {
    let upper = derive_component("Main").expect("upper name");
    let lower = derive_component("main").expect("lower name");
    assert_eq!(upper.as_str(), "Main~4d61696e");
    assert_ne!(upper.as_str().to_ascii_lowercase(), lower.as_str());
}

#[test]
fn nfc_and_nfd_do_not_alias() {
    let nfc = derive_component("café").expect("NFC name");
    let nfd = derive_component("cafe\u{301}").expect("NFD name");
    assert_eq!(nfc.as_str(), "caf_~636166c3a9");
    assert_eq!(nfd.as_str(), "cafe_~63616665cc81");
    assert_ne!(nfc, nfd);
}

#[test]
fn reserved_suffix_marker_is_encoded() {
    let name = derive_component("main~6d61696e").expect("noncanonical name");
    assert_eq!(name.as_str(), "main_6d61696e~6d61696e7e3664363136393665");
    assert_ne!(name.as_str(), "main~6d61696e");
}

#[test]
fn absolute_and_parent_paths_are_rejected_before_open() {
    assert_eq!(derive_component("/main"), Err(NameError::Absolute));
    assert_eq!(derive_component("../main"), Err(NameError::Traversal));
    assert_eq!(derive_component("main/../other"), Err(NameError::Traversal));
    assert_eq!(derive_component("."), Err(NameError::Traversal));
}

#[test]
fn stream_and_session_use_same_rule() {
    let direct = derive_component("my/session name").expect("session name");
    let environment = CountingEnvironment::new(Some("/owner"));
    let (stream, _) = resolve_stream_paths(PlatformKind::Linux, &environment, "my/session name")
        .expect("stream paths");
    assert_eq!(stream, direct);
}

#[test]
fn overlong_stream_name_fails_startup_before_any_directory() {
    let environment = CountingEnvironment::new(Some("/owner"));
    let error = resolve_stream_paths(PlatformKind::Linux, &environment, &"a".repeat(201))
        .expect_err("overlong stream must fail");
    assert!(matches!(
        error,
        PathError::Name(NameError::TooLong {
            actual: 201,
            limit: 200
        })
    ));
    assert_eq!(environment.reads.load(Ordering::Relaxed), 0);
}

struct CountingEnvironment {
    home: Option<OsString>,
    reads: AtomicUsize,
}

impl CountingEnvironment {
    fn new(home: Option<&str>) -> Self {
        Self {
            home: home.map(OsString::from),
            reads: AtomicUsize::new(0),
        }
    }
}

impl Environment for CountingEnvironment {
    fn var_os(&self, key: &str) -> Option<OsString> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        (key == "HOME").then(|| self.home.clone()).flatten()
    }
}
