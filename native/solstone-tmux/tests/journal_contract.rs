// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

mod support;

use std::fs;

use serde_json::Value;
use solstone_tmux::health::DiagnosticCode;
use solstone_tmux::journal::{
    MAX_MULTIPART_PART_BYTES, UploadStatus, decode_manifest_day_response, decode_manifest_response,
    decode_segments_response, decode_upload_response,
};
use solstone_tmux::paths::ensure_private_directory;
use solstone_tmux::private_link::{
    MAX_REQUEST_BODY_BYTES, OBSERVER_HEADER_NAME, PROTOCOL_VERSION, PROTOCOL_VERSION_HEADER_NAME,
};
use solstone_tmux::sync::JournalSession;
use support::TestDirectory;
use support::private_link_peer::{PeerRequest, PrivateLinkPeer};

const DAY: &str = "20260815";
const SEGMENT: &str = "143000_1";

#[test]
fn v3_operations_use_projection_examples_and_exact_multipart_envelope() {
    runtime().block_on(async {
        let peer = PrivateLinkPeer::start().await;
        let temporary = TestDirectory::new("journal-contract-v3");
        ensure_private_directory(temporary.path()).expect("private root");
        let session = JournalSession::start(peer.credential(), temporary.path().to_path_buf())
            .await
            .expect("start journal session");
        let first = temporary.path().join("first.jsonl");
        let second = temporary.path().join("second.jsonl");
        fs::write(&first, b"first\n").expect("first file");
        fs::write(&second, b"second\n").expect("second file");

        peer.enqueue_response(200, projection_example("upload_normal"));
        session
            .journal()
            .ingest_upload(DAY, SEGMENT, vec![first, second])
            .await
            .expect("upload");
        peer.enqueue_response(200, projection_example("manifest"));
        session
            .journal()
            .ingest_manifest()
            .await
            .expect("root manifest");
        peer.enqueue_response(200, projection_example("manifest_day"));
        session
            .journal()
            .ingest_manifest_day(DAY)
            .await
            .expect("day manifest");
        peer.enqueue_response(200, projection_example("segments"));
        session
            .journal()
            .ingest_segments(DAY)
            .await
            .expect("segments");

        let requests = peer.requests();
        assert_eq!(requests.len(), 4);
        assert_exact_multipart(&requests[0], &["first.jsonl", "second.jsonl"]);
        assert_eq!(requests[1].path(), "/app/devices/ingest/manifest");
        assert_eq!(requests[2].path(), "/app/devices/ingest/manifest/20260815");
        assert_eq!(requests[3].path(), "/app/devices/ingest/segments/20260815");
        for request in &requests {
            assert_eq!(
                request.header(PROTOCOL_VERSION_HEADER_NAME),
                Some(PROTOCOL_VERSION)
            );
            assert!(request.header("authorization").is_none());
            assert!(request.header(OBSERVER_HEADER_NAME).is_none());
        }
        session.shutdown().await.expect("shutdown session");
        peer.shutdown().await;
    });
}

#[test]
fn malformed_v3_manifest_and_listing_payloads_are_rejected() {
    assert_contract_error(decode_manifest_response(
        br#"{"days":{"not-a-day":{"segments":1}}}"#,
    ));
    assert_contract_error(decode_manifest_day_response(
        br#"{"version":1,"day":"20260816","segments":{}}"#,
        DAY,
    ));
    assert_contract_error(decode_segments_response(
        br#"{"protocol_version":2,"total":0,"items":[]}"#,
    ));
    assert_contract_error(decode_segments_response(
        br#"{"protocol_version":3,"total":1,"items":[]}"#,
    ));
}

#[test]
fn multipart_limits_reject_before_the_peer_and_admit_newly_supported_parts() {
    runtime().block_on(async {
        let peer = PrivateLinkPeer::start().await;
        let temporary = TestDirectory::new("journal-limits-v3");
        ensure_private_directory(temporary.path()).expect("private root");
        let session = JournalSession::start(peer.credential(), temporary.path().to_path_buf())
            .await
            .expect("start session");

        let admitted = temporary.path().join("admitted.jsonl");
        fs::File::create(&admitted)
            .expect("create admitted")
            .set_len(19_200_078)
            .expect("size admitted");
        peer.enqueue_response(200, projection_example("upload_normal"));
        assert!(
            session
                .journal()
                .ingest_upload(DAY, SEGMENT, vec![admitted])
                .await
                .is_ok()
        );

        let exact_part = temporary.path().join("exact-64.jsonl");
        fs::File::create(&exact_part)
            .expect("create exact part")
            .set_len(MAX_MULTIPART_PART_BYTES)
            .expect("size exact part");
        peer.enqueue_response(200, projection_example("upload_normal"));
        assert!(
            session
                .journal()
                .ingest_upload(DAY, SEGMENT, vec![exact_part])
                .await
                .is_ok()
        );

        let oversized = temporary.path().join("oversized.jsonl");
        fs::File::create(&oversized)
            .expect("create oversized")
            .set_len(MAX_MULTIPART_PART_BYTES + 1)
            .expect("size oversized");
        let error = session
            .journal()
            .ingest_upload(DAY, SEGMENT, vec![oversized])
            .await
            .expect_err("oversized part accepted");
        assert_eq!(error.diagnostic(), DiagnosticCode::RequestTooLarge);

        let first = temporary.path().join("first-64.jsonl");
        let second = temporary.path().join("second-64.jsonl");
        for path in [&first, &second] {
            fs::File::create(path)
                .expect("create boundary part")
                .set_len(MAX_MULTIPART_PART_BYTES)
                .expect("size boundary part");
        }
        let error = session
            .journal()
            .ingest_upload(DAY, SEGMENT, vec![first, second])
            .await
            .expect_err("over-body multipart accepted");
        assert_eq!(error.diagnostic(), DiagnosticCode::RequestTooLarge);
        assert_eq!(peer.requests().len(), 2, "rejected bodies reached peer");
        assert!(MAX_REQUEST_BODY_BYTES > MAX_MULTIPART_PART_BYTES as usize);
        session.shutdown().await.expect("shutdown session");
        peer.shutdown().await;
    });
}

#[test]
fn declared_upload_ok_and_collision_fixtures_require_selected_segment_keys() {
    for status in ["ok", "collision"] {
        assert_eq!(declared_upload_status(status), status);
        let mut payload = projection_upload("normal");
        payload["status"] = Value::String(status.to_owned());
        let result = decode_upload_response(&serde_json::to_vec(&payload).expect("payload"))
            .expect("selected segment response");
        assert_eq!(
            result.status,
            if status == "ok" {
                UploadStatus::Ok
            } else {
                UploadStatus::Collision
            }
        );
        assert_eq!(result.authoritative_key.as_deref(), Some("143000_1"));
    }
}

#[test]
fn declared_upload_duplicate_fixture_requires_existing_segment_key() {
    assert_eq!(declared_upload_status("duplicate"), "duplicate");
    let result = decode_upload_response(
        &serde_json::to_vec(&projection_upload("duplicate")).expect("payload"),
    )
    .expect("duplicate response");
    assert_eq!(result.status, UploadStatus::Duplicate);
    assert_eq!(result.authoritative_key.as_deref(), Some("143000_1"));
}

#[test]
fn declared_upload_conflict_and_failed_fixtures_fail_closed() {
    for (status, expected) in [
        ("conflict", UploadStatus::Conflict),
        ("failed", UploadStatus::Failed),
    ] {
        let payload = declared_upload_payload(status);
        let result = decode_upload_response(&serde_json::to_vec(&payload).expect("payload"))
            .expect("closed status response");
        assert_eq!(result.status, expected);
        assert_eq!(result.authoritative_key, None);
    }
}

#[test]
fn unknown_upload_status_is_rejected() {
    assert_contract_error(decode_upload_response(br#"{"status":"unknown"}"#));
}

fn assert_exact_multipart(request: &PeerRequest, submitted: &[&str]) {
    assert_eq!(request.method(), "POST");
    assert_eq!(request.path(), "/app/devices/ingest");
    let body = String::from_utf8_lossy(request.body());
    assert!(body.contains("name=\"envelope\""));
    assert!(body.contains("Content-Type: application/json"));
    assert!(!body.contains("name=\"day\""));
    assert!(!body.contains("name=\"segment\""));
    let envelope_start = body.find("{\"day\"").expect("envelope JSON");
    let envelope_end = body[envelope_start..].find("\r\n").expect("envelope end") + envelope_start;
    let envelope: Value =
        serde_json::from_str(&body[envelope_start..envelope_end]).expect("parse envelope");
    assert_eq!(envelope["day"], DAY);
    assert_eq!(envelope["segment"], SEGMENT);
    assert!(envelope.get("stream").is_none());
    assert!(envelope.get("observer").is_none());
    let names = envelope["files"]
        .as_array()
        .expect("envelope files")
        .iter()
        .map(|file| file["submitted"].as_str().expect("submitted"))
        .collect::<Vec<_>>();
    assert_eq!(names, submitted);
    for name in submitted {
        assert!(body.contains(&format!("name=\"files\"; filename=\"{name}\"")));
    }
}

fn projection_example(name: &str) -> Vec<u8> {
    let projection: Value = serde_json::from_slice(
        &fs::read(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("vendor/observer-client-contract/projection.openapi.json"),
        )
        .expect("read projection"),
    )
    .expect("parse projection");
    let value = match name {
        "upload_normal" => {
            &projection["paths"]["/app/devices/ingest"]["post"]["responses"]["200"]["content"]["application/json"]
                ["examples"]["normal"]["value"]
        }
        "manifest" => {
            &projection["paths"]["/app/devices/ingest/manifest"]["get"]["responses"]["200"]["content"]
                ["application/json"]["example"]
        }
        "manifest_day" => {
            &projection["paths"]["/app/devices/ingest/manifest/{day}"]["get"]["responses"]["200"]["content"]
                ["application/json"]["example"]
        }
        "segments" => {
            &projection["paths"]["/app/devices/ingest/segments/{day}"]["get"]["responses"]["200"]["content"]
                ["application/json"]["example"]
        }
        _ => panic!("unknown projection example"),
    };
    serde_json::to_vec(value).expect("serialize projection example")
}

fn projection_upload(name: &str) -> Value {
    let projection: Value = serde_json::from_slice(
        &fs::read(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("vendor/observer-client-contract/projection.openapi.json"),
        )
        .expect("read projection"),
    )
    .expect("parse projection");
    projection["paths"]["/app/devices/ingest"]["post"]["responses"]["200"]["content"]
        ["application/json"]["examples"][name]["value"]
        .clone()
}

fn declared_upload_payload(status: &str) -> Value {
    let fixtures: Value = serde_json::from_slice(
        &fs::read(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("vendor/observer-client-contract/fixtures/wire-behavior.json"),
        )
        .expect("read fixtures"),
    )
    .expect("parse fixtures");
    fixtures["fixtures"]
        .as_array()
        .expect("fixture array")
        .iter()
        .find(|fixture| fixture["id"] == format!("declared.observer.ingestUpload.status.{status}"))
        .expect("declared status fixture")["payload"]
        .clone()
}

fn declared_upload_status(status: &str) -> String {
    declared_upload_payload(status)["status"]
        .as_str()
        .expect("fixture status")
        .to_owned()
}

fn assert_contract_error<T>(result: Result<T, solstone_tmux::journal::JournalError>) {
    let error = match result {
        Ok(_) => panic!("invalid response accepted"),
        Err(error) => error,
    };
    assert_eq!(error.diagnostic(), DiagnosticCode::JournalContractInvalid);
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .expect("runtime")
}
