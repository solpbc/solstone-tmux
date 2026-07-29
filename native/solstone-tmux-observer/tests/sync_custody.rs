// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

mod support;

use solstone_tmux_observer::journal::{
    ListingFileStatus, LocalFile, SegmentFile, SegmentItem, SegmentsEnvelope,
    decode_segments_response,
};
use solstone_tmux_observer::sync::fresh_listing_proves_custody;
use support::observer_wire_fixture;

#[test]
fn authority_custody_statuses_accept_only_held_files() {
    let listing = fixture_listing("recorded.segments.custody_statuses");
    let entry = &listing.items[0];

    assert!(fresh_listing_proves_custody(
        &listing,
        &entry.key,
        &entry.key,
        &[local_from_remote(&entry.files[0])],
    ));
    assert!(fresh_listing_proves_custody(
        &listing,
        &entry.key,
        &entry.key,
        &[local_from_remote(&entry.files[2])],
    ));
    assert!(!fresh_listing_proves_custody(
        &listing,
        &entry.key,
        &entry.key,
        &[local_from_remote(&entry.files[1])],
    ));
}

#[test]
fn authority_submitted_name_omission_falls_back_to_remote_name() {
    let listing = fixture_listing("recorded.segments.submitted_name_omitted");
    let entry = &listing.items[0];
    assert!(entry.files[0].submitted_name.is_none());
    assert!(fresh_listing_proves_custody(
        &listing,
        &entry.key,
        &entry.key,
        &[local_from_remote(&entry.files[0])],
    ));
}

#[test]
fn authority_unknown_status_cannot_prove_custody() {
    let fixture =
        observer_wire_fixture("declared.observer.ingestSegments.custody_unknown_rejected");
    let bytes = serde_json::to_vec(&fixture.payload).expect("serialize custody fixture");
    assert!(
        decode_segments_response(&bytes).is_err(),
        "unknown custody status was accepted"
    );
}

#[test]
fn partial_and_ambiguous_file_evidence_retain_the_segment() {
    let listing = fixture_listing("recorded.segments.custody_statuses");
    let entry = &listing.items[0];
    let held = local_from_remote(&entry.files[0]);
    let partial = LocalFile {
        name: "unlisted.jsonl".to_owned(),
        size: 1,
        sha256: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
    };
    assert!(!fresh_listing_proves_custody(
        &listing,
        &entry.key,
        &entry.key,
        &[held.clone(), partial],
    ));

    let mut ambiguous = listing.clone();
    let duplicate = ambiguous.items[0].files[0].clone();
    ambiguous.items[0].files.push(duplicate);
    assert!(!fresh_listing_proves_custody(
        &ambiguous,
        &entry.key,
        &entry.key,
        &[held],
    ));
}

#[test]
fn ambiguous_listing_entries_retain_the_segment() {
    let mut listing = fixture_listing("recorded.segments.custody_statuses");
    let entry = listing.items[0].clone();
    let local = local_from_remote(&entry.files[0]);
    listing.items.push(SegmentItem {
        key: "120000_301".to_owned(),
        observed: false,
        files: entry.files.clone(),
        original_key: Some(entry.key.clone()),
    });
    listing.total = listing.items.len();
    assert!(!fresh_listing_proves_custody(
        &listing,
        &entry.key,
        &entry.key,
        &[local],
    ));
}

#[test]
fn local_case_collision_custody_can_match_original_key() {
    let hash = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let listing = SegmentsEnvelope {
        items: vec![SegmentItem {
            key: "120000_301".to_owned(),
            observed: false,
            files: vec![SegmentFile {
                name: "stored-screen.jsonl".to_owned(),
                size: 7,
                sha256: hash.to_owned(),
                status: ListingFileStatus::Present,
                submitted_name: Some("screen.jsonl".to_owned()),
            }],
            original_key: Some("120000_300".to_owned()),
        }],
        total: 1,
        protocol_version: 2,
    };
    let local = LocalFile {
        name: "screen.jsonl".to_owned(),
        size: 7,
        sha256: hash.to_owned(),
    };
    assert!(fresh_listing_proves_custody(
        &listing,
        "120000_300",
        "120000_301",
        &[local],
    ));
}

#[test]
fn malformed_hash_or_duplicate_local_name_retain_the_segment() {
    let listing = fixture_listing("recorded.segments.custody_statuses");
    let entry = &listing.items[0];
    let mut malformed = local_from_remote(&entry.files[0]);
    malformed.sha256.make_ascii_uppercase();
    assert!(!fresh_listing_proves_custody(
        &listing,
        &entry.key,
        &entry.key,
        &[malformed],
    ));

    let held = local_from_remote(&entry.files[0]);
    assert!(!fresh_listing_proves_custody(
        &listing,
        &entry.key,
        &entry.key,
        &[held.clone(), held],
    ));
}

fn fixture_listing(id: &str) -> SegmentsEnvelope {
    let fixture = observer_wire_fixture(id);
    let bytes = serde_json::to_vec(&fixture.payload).expect("serialize listing fixture");
    decode_segments_response(&bytes).expect("decode listing fixture")
}

fn local_from_remote(remote: &SegmentFile) -> LocalFile {
    LocalFile {
        name: remote
            .submitted_name
            .clone()
            .unwrap_or_else(|| remote.name.clone()),
        size: remote.size,
        sha256: remote.sha256.clone(),
    }
}
