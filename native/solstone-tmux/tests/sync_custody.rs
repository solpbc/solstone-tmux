// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

mod support;

use std::fs;

use serde_json::Value;
use solstone_tmux::journal::{
    LocalFile, SegmentFile, SegmentItem, SegmentsEnvelope, decode_segments_response,
};
use solstone_tmux::sync::fresh_listing_proves_custody;

#[test]
fn v3_projection_listing_requires_matching_name_digest_size_and_held_status() {
    let listing = projection_listing();
    let entry = &listing.items[0];
    let remote = &entry.files[0];
    let local = local_from_remote(remote);
    assert!(fresh_listing_proves_custody(
        &listing,
        &entry.key,
        &entry.key,
        std::slice::from_ref(&local),
    ));

    let wrong_size = LocalFile {
        size: local.size + 1,
        ..local.clone()
    };
    assert!(!fresh_listing_proves_custody(
        &listing,
        &entry.key,
        &entry.key,
        &[wrong_size],
    ));
    let wrong_digest = LocalFile {
        sha256: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_owned(),
        ..local
    };
    assert!(!fresh_listing_proves_custody(
        &listing,
        &entry.key,
        &entry.key,
        &[wrong_digest],
    ));
}

#[test]
fn malformed_unknown_status_never_becomes_custody_evidence() {
    let payload = serde_json::json!({
        "protocol_version": 3,
        "total": 1,
        "items": [{
            "key": "143000_1",
            "observed": true,
            "files": [{
                "name": "capture.jsonl",
                "size": 1,
                "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "status": "unknown"
            }]
        }]
    });
    assert!(decode_segments_response(&serde_json::to_vec(&payload).expect("bytes")).is_err());
}

#[test]
fn duplicate_remote_evidence_and_original_key_ambiguity_retain_the_segment() {
    let mut listing = projection_listing();
    let entry = listing.items[0].clone();
    let local = local_from_remote(&entry.files[0]);
    listing.items.push(SegmentItem {
        key: "143001_1".to_owned(),
        observed: true,
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
fn authority_submitted_name_omission_falls_back_to_remote_name() {
    let mut listing = projection_listing();
    let entry = &mut listing.items[0];
    entry.files[0].submitted_name = None;
    let local = LocalFile {
        name: entry.files[0].name.clone(),
        size: entry.files[0].size,
        sha256: entry.files[0].sha256.clone(),
    };
    let key = entry.key.clone();
    assert!(fresh_listing_proves_custody(&listing, &key, &key, &[local]));
}

#[test]
fn partial_and_ambiguous_file_evidence_retain_the_segment() {
    let mut listing = projection_listing();
    let key = listing.items[0].key.clone();
    let local = local_from_remote(&listing.items[0].files[0]);
    listing.items[0].files.clear();
    assert!(!fresh_listing_proves_custody(
        &listing,
        &key,
        &key,
        &[local]
    ));
}

#[test]
fn local_case_collision_custody_can_match_original_key() {
    let mut listing = projection_listing();
    let mut entry = listing.items.remove(0);
    let submitted = entry.key.clone();
    entry.key = "143001_1".to_owned();
    entry.original_key = Some(submitted.clone());
    let local = local_from_remote(&entry.files[0]);
    listing.items.push(entry);
    assert!(fresh_listing_proves_custody(
        &listing,
        &submitted,
        "143001_1",
        &[local]
    ));
}

#[test]
fn malformed_hash_or_duplicate_local_name_retain_the_segment() {
    let listing = projection_listing();
    let entry = &listing.items[0];
    let local = local_from_remote(&entry.files[0]);
    let bad_hash = LocalFile {
        sha256: "not-a-sha256".to_owned(),
        ..local.clone()
    };
    assert!(!fresh_listing_proves_custody(
        &listing,
        &entry.key,
        &entry.key,
        &[bad_hash]
    ));
    assert!(!fresh_listing_proves_custody(
        &listing,
        &entry.key,
        &entry.key,
        &[local.clone(), local],
    ));
}

fn projection_listing() -> SegmentsEnvelope {
    let projection: Value = serde_json::from_slice(
        &fs::read(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("vendor/observer-client-contract/projection.openapi.json"),
        )
        .expect("projection"),
    )
    .expect("projection JSON");
    let value = &projection["paths"]["/app/devices/ingest/segments/{day}"]["get"]["responses"]["200"]
        ["content"]["application/json"]["example"];
    decode_segments_response(&serde_json::to_vec(value).expect("listing bytes"))
        .expect("v3 listing")
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
