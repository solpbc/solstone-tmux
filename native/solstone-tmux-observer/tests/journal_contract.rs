// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

mod support;

use std::collections::BTreeSet;
use std::fs;
use std::time::Duration;

use reqwest::header::{CONTENT_LENGTH, CONTENT_TYPE, TRANSFER_ENCODING};
use serde_json::{Map, Value, json};
use solstone_tmux_observer::health::DiagnosticCode;
use solstone_tmux_observer::journal::{
    JournalReasonCode, JournalStatusClass, ListingFileStatus, RegistrationDescriptor, UploadStatus,
    classify_error_response, decode_event_response, decode_registration_response,
    decode_segments_response, decode_upload_response,
};
use solstone_tmux_observer::paths::ensure_private_directory;
use solstone_tmux_observer::private_link::MAX_REQUEST_BODY_BYTES;
use solstone_tmux_observer::sync::RegistrationOwner;
use support::private_link_peer::{PeerRequest, PrivateLinkPeer};
use support::{TestDirectory, observer_wire_fixture};

#[test]
fn authority_fixtures_define_registration_and_closed_success_vocabularies() {
    let request =
        fixture_payload("example.observer.register.request.body.application-json.default");
    let required_request_fields = request
        .as_object()
        .expect("registration request fixture is an object")
        .keys()
        .filter(|field| field.as_str() != "label")
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    assert_eq!(
        required_request_fields,
        BTreeSet::from(["hostname", "platform", "stream_type", "version"])
    );

    let response = fixture_bytes("example.observer.register.response.200.application-json.default");
    let registration =
        decode_registration_response(&response, "credential-instance").expect("decode fixture");
    assert_eq!(registration.credential_instance_id, "credential-instance");
    assert!(!registration.key.is_empty());
    assert!(!registration.prefix.is_empty());
    assert!(!registration.name.is_empty());
    assert_eq!(registration.ingest_url, "/app/observer/ingest");
    assert_eq!(registration.protocol_version, 2);

    for (id, status, authoritative_key) in [
        (
            "recorded.ingestUpload.ok",
            UploadStatus::Ok,
            Some("120000_300"),
        ),
        (
            "recorded.ingestUpload.duplicate",
            UploadStatus::Duplicate,
            Some("120000_300"),
        ),
        (
            "recorded.ingestUpload.collision",
            UploadStatus::Collision,
            Some("120000_301"),
        ),
        (
            "recorded.ingestUpload.conflict",
            UploadStatus::Conflict,
            None,
        ),
        ("recorded.ingestUpload.failed", UploadStatus::Failed, None),
    ] {
        let result = decode_upload_response(&fixture_bytes(id)).expect("decode upload fixture");
        assert_eq!(result.status, status);
        assert_eq!(result.authoritative_key.as_deref(), authoritative_key);
    }
    assert_contract_error(decode_upload_response(&fixture_bytes(
        "declared.observer.ingestUpload.status_unknown_rejected",
    )));

    decode_event_response(&fixture_bytes(
        "example.observer.ingestEvent.response.200.application-json.default",
    ))
    .expect("decode event fixture");
}

#[test]
fn authority_fixtures_require_the_v2_listing_envelope_and_closed_file_statuses() {
    for id in [
        "example.observer.ingestSegments.response.200.application-json.v2",
        "recorded.segments.v2.envelope",
        "recorded.segments.custody_statuses",
        "recorded.segments.submitted_name_omitted",
    ] {
        decode_segments_response(&fixture_bytes(id)).expect("decode v2 listing fixture");
    }
    for id in [
        "example.observer.ingestSegments.response.200.application-json.legacy",
        "recorded.segments.legacy.absent_header",
        "recorded.segments.legacy.unparseable_header",
        "declared.observer.ingestSegments.envelope_total_mismatch",
        "declared.observer.ingestSegments.custody_unknown_rejected",
    ] {
        assert_contract_error(decode_segments_response(&fixture_bytes(id)));
    }

    let custody = decode_segments_response(&fixture_bytes("recorded.segments.custody_statuses"))
        .expect("decode custody status fixture");
    let statuses = custody.items[0]
        .files
        .iter()
        .map(|file| file.status)
        .collect::<BTreeSet<_>>();
    assert_eq!(
        statuses,
        BTreeSet::from([
            ListingFileStatus::Present,
            ListingFileStatus::Missing,
            ListingFileStatus::Processed,
        ])
    );

    let omitted =
        decode_segments_response(&fixture_bytes("recorded.segments.submitted_name_omitted"))
            .expect("decode submitted-name fixture");
    assert!(omitted.items[0].files[0].submitted_name.is_none());
}

#[test]
fn local_case_malformed_status_specific_success_payloads_are_rejected() {
    for payload in [
        json!({"status": "ok"}),
        json!({"status": "duplicate"}),
        json!({"status": "collision"}),
        json!({"status": "ok", "segment": "../escape"}),
    ] {
        let bytes = serde_json::to_vec(&payload).expect("serialize local malformed case");
        assert_contract_error(decode_upload_response(&bytes));
    }
}

#[test]
fn local_case_registration_ingest_location_stays_on_the_bridge() {
    let fixture =
        fixture_payload("example.observer.register.response.200.application-json.default");
    for ingest_url in [
        "http://127.0.0.1/app/observer/ingest",
        "//foreign.example/app/observer/ingest",
        "/app/observer/ingest?mode=foreign",
        "/app/observer/ingest#foreign",
    ] {
        let mut payload = fixture.clone();
        payload["ingest_url"] = Value::String(ingest_url.to_owned());
        let bytes = serde_json::to_vec(&payload).expect("serialize local confinement case");
        assert_contract_error(decode_registration_response(&bytes, "credential-instance"));
    }
}

#[test]
fn journal_operations_use_authority_fixtures_and_exact_streaming_multipart() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .expect("build current-thread runtime");
    runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(5), async {
            let peer = PrivateLinkPeer::start().await;
            let temporary = TestDirectory::new("journal-contract");
            ensure_private_directory(temporary.path()).expect("create private config root");
            peer.enqueue_response(
                200,
                fixture_bytes("example.observer.register.response.200.application-json.default"),
            );
            let owner = RegistrationOwner::start(peer.credential(), temporary.path().to_path_buf())
                .await
                .expect("start registration owner");
            let observer = owner
                .register_once(&RegistrationDescriptor {
                    platform: "linux".to_owned(),
                    hostname: "archon".to_owned(),
                })
                .await
                .expect("register from authority fixture");

            let multipart = fixture_payload(
                "example.observer.ingestUpload.request.body.multipart-form-data.default",
            );
            let day = multipart["day"].as_str().expect("fixture day");
            let segment = multipart["segment"].as_str().expect("fixture segment");
            let screen = b"screen fixture\n";
            let audio = b"audio fixture\n";
            let screen_path = temporary.path().join("screen.png");
            let audio_path = temporary.path().join("audio.flac");
            fs::write(&screen_path, screen).expect("write screen fixture");
            fs::write(&audio_path, audio).expect("write audio fixture");
            peer.enqueue_response(
                200,
                fixture_bytes("example.observer.ingestUpload.response.200.application-json.normal"),
            );
            let result = owner
                .journal()
                .ingest_upload(
                    &observer.ingest_url,
                    day,
                    segment,
                    vec![screen_path, audio_path],
                )
                .await
                .expect("upload fixture files");
            assert_eq!(result.status, UploadStatus::Ok);

            peer.enqueue_response(
                200,
                fixture_bytes("example.observer.ingestSegments.response.200.application-json.v2"),
            );
            let listing = owner
                .journal()
                .ingest_segments(day)
                .await
                .expect("list fixture segments");
            assert_eq!(listing.protocol_version, 2);

            let event = fixture_payload(
                "example.observer.ingestEvent.request.body.application-json.default",
            );
            let tract = event["tract"].as_str().expect("fixture tract");
            let event_name = event["event"].as_str().expect("fixture event");
            let mut fields = event.as_object().expect("event fixture object").clone();
            fields.remove("tract");
            fields.remove("event");
            peer.enqueue_response(
                200,
                fixture_bytes("example.observer.ingestEvent.response.200.application-json.default"),
            );
            owner
                .journal()
                .ingest_event(tract, event_name, fields)
                .await
                .expect("send fixture event");

            let requests = peer.requests();
            assert_eq!(requests.len(), 4);
            assert_registration_request(&requests[0]);
            assert_exact_multipart(&requests[1], day, segment, screen, audio);
            assert_eq!(requests[2].path(), "/app/observer/ingest/segments/20260618");
            assert_event_request(&requests[3], &event);
            assert_eq!(peer.accepted_carriers(), 1);

            owner.shutdown().await;
            peer.shutdown().await;
        })
        .await
        .expect("journal contract operations timed out");
    });
}

#[test]
fn over_limit_multipart_fails_before_an_http_request() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .expect("build current-thread runtime");
    runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(5), async {
            let peer = PrivateLinkPeer::start().await;
            let temporary = TestDirectory::new("journal-upload-limit");
            ensure_private_directory(temporary.path()).expect("create private config root");
            peer.enqueue_response(
                200,
                fixture_bytes("example.observer.register.response.200.application-json.default"),
            );
            let owner = RegistrationOwner::start(peer.credential(), temporary.path().to_path_buf())
                .await
                .expect("start registration owner");
            let observer = owner
                .register_once(&RegistrationDescriptor {
                    platform: "linux".to_owned(),
                    hostname: "archon".to_owned(),
                })
                .await
                .expect("register from authority fixture");

            let path = temporary.path().join("oversized.jsonl");
            fs::File::create(&path)
                .expect("create oversized file")
                .set_len(MAX_REQUEST_BODY_BYTES as u64)
                .expect("size oversized file");
            let error = owner
                .journal()
                .ingest_upload(&observer.ingest_url, "20260618", "143022_300", vec![path])
                .await
                .expect_err("oversized upload was accepted");
            assert_eq!(error.diagnostic(), DiagnosticCode::RequestTooLarge);
            assert_eq!(
                peer.requests().len(),
                1,
                "oversized upload reached the paired peer"
            );

            owner.shutdown().await;
            peer.shutdown().await;
        })
        .await
        .expect("multipart limit case timed out");
    });
}

#[test]
fn authority_error_fixture_is_reduced_to_allowlisted_diagnostics() {
    let fixture = observer_wire_fixture("recorded.ingestUpload.conflict");
    let status = fixture.provenance["status"]
        .as_u64()
        .expect("fixture status") as u16;
    let body = serde_json::to_vec(&fixture.payload).expect("serialize error fixture");
    let error = classify_error_response(status, &body);
    assert_eq!(error.diagnostic(), DiagnosticCode::JournalRejected);
    assert_eq!(error.status_class(), Some(JournalStatusClass::Client));
    assert_eq!(
        error.reason_code(),
        Some(JournalReasonCode::IngestSidecarConflict)
    );
}

fn fixture_payload(id: &str) -> Value {
    let fixture = observer_wire_fixture(id);
    assert!(!fixture.kind.is_empty(), "authority fixture has a kind");
    assert!(
        fixture.schema_validation.is_object(),
        "authority fixture has schema validation"
    );
    fixture.payload
}

fn fixture_bytes(id: &str) -> Vec<u8> {
    serde_json::to_vec(&fixture_payload(id)).expect("serialize authority fixture")
}

fn assert_contract_error<T>(result: Result<T, solstone_tmux_observer::journal::JournalError>) {
    let error = match result {
        Ok(_) => panic!("invalid contract data was accepted"),
        Err(error) => error,
    };
    assert_eq!(error.diagnostic(), DiagnosticCode::JournalContractInvalid);
}

fn assert_registration_request(request: &PeerRequest) {
    assert_eq!(request.method(), "POST");
    assert_eq!(request.path(), "/app/observer/register");
    let body = serde_json::from_slice::<Value>(request.body()).expect("parse registration request");
    let fields = body
        .as_object()
        .expect("registration request object")
        .keys()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let fixture =
        fixture_payload("example.observer.register.request.body.application-json.default");
    let required = fixture
        .as_object()
        .expect("registration fixture object")
        .keys()
        .filter(|field| field.as_str() != "label")
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    assert_eq!(fields, required);
    assert_eq!(body["platform"], "linux");
    assert_eq!(body["hostname"], "archon");
    assert_eq!(body["stream_type"], "tmux");
    assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
}

fn assert_exact_multipart(
    request: &PeerRequest,
    day: &str,
    segment: &str,
    screen: &[u8],
    audio: &[u8],
) {
    assert_eq!(request.method(), "POST");
    assert_eq!(request.path(), "/app/observer/ingest");
    assert!(request.header(TRANSFER_ENCODING.as_str()).is_none());
    let content_length = request
        .header(CONTENT_LENGTH.as_str())
        .expect("multipart content length")
        .parse::<usize>()
        .expect("parse multipart content length");
    assert_eq!(content_length, request.body().len());
    let content_type = request
        .header(CONTENT_TYPE.as_str())
        .expect("multipart content type");
    let boundary = content_type
        .strip_prefix("multipart/form-data; boundary=")
        .expect("multipart boundary");
    assert!(!boundary.is_empty());

    let mut expected = Vec::new();
    push_text_part(&mut expected, boundary, "day", day);
    push_text_part(&mut expected, boundary, "segment", segment);
    push_file_part(&mut expected, boundary, "screen.png", screen);
    push_file_part(&mut expected, boundary, "audio.flac", audio);
    expected.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    assert_eq!(request.body(), expected);
}

fn push_text_part(body: &mut Vec<u8>, boundary: &str, name: &str, value: &str) {
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
        )
        .as_bytes(),
    );
}

fn push_file_part(body: &mut Vec<u8>, boundary: &str, filename: &str, bytes: &[u8]) {
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"files\"; filename=\"{filename}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(bytes);
    body.extend_from_slice(b"\r\n");
}

fn assert_event_request(request: &PeerRequest, fixture: &Value) {
    assert_eq!(request.method(), "POST");
    assert_eq!(request.path(), "/app/observer/ingest/event");
    let body =
        serde_json::from_slice::<Map<String, Value>>(request.body()).expect("parse event request");
    assert_eq!(body, *fixture.as_object().expect("event fixture object"));
}
