// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

mod support;

use std::fs;

use serde_json::Value;
use solstone_tmux::health::DiagnosticCode;
use solstone_tmux::journal::{
    INGEST_MANIFEST_DAY_PATH, INGEST_MANIFEST_PATH, INGEST_PATH, INGEST_SEGMENTS_PATH,
    MAX_MULTIPART_PART_BYTES, UploadStatus, decode_manifest_day_response, decode_manifest_response,
    decode_segments_response, decode_upload_response,
};
use solstone_tmux::paths::ensure_private_directory;
use solstone_tmux::private_link::{
    MAX_REQUEST_BODY_BYTES, OBSERVER_HEADER_NAME, PROTOCOL_VERSION, PROTOCOL_VERSION_HEADER_NAME,
    PROTOCOL_VERSION_NUMBER,
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
        assert_eq!(requests[1].path(), INGEST_MANIFEST_PATH);
        assert_eq!(
            requests[2].path(),
            INGEST_MANIFEST_DAY_PATH.replace("{day}", DAY)
        );
        assert_eq!(
            requests[3].path(),
            INGEST_SEGMENTS_PATH.replace("{day}", DAY)
        );
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
    let legacy_version = PROTOCOL_VERSION_NUMBER.saturating_sub(1);
    assert_contract_error(decode_segments_response(
        &serde_json::to_vec(&serde_json::json!({
            "protocol_version": legacy_version,
            "total": 0,
            "items": [],
        }))
        .expect("legacy payload"),
    ));
    assert_contract_error(decode_segments_response(
        &serde_json::to_vec(&serde_json::json!({
            "protocol_version": PROTOCOL_VERSION_NUMBER,
            "total": 1,
            "items": [],
        }))
        .expect("mismatched total payload"),
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

#[test]
fn exact_multipart_checker_rejects_an_extra_file_part() {
    let boundary = "test-boundary";
    let envelope = serde_json::json!({
        "day": DAY,
        "segment": SEGMENT,
        "files": [{ "submitted": "first.jsonl" }],
    })
    .to_string();
    let body = [
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"envelope\"\r\nContent-Type: application/json\r\n\r\n{envelope}\r\n"
        ),
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"files\"; filename=\"first.jsonl\"\r\nContent-Type: application/octet-stream\r\n\r\nfirst\r\n"
        ),
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"files\"; filename=\"extra.jsonl\"\r\nContent-Type: application/octet-stream\r\n\r\nextra\r\n--{boundary}--\r\n"
        ),
    ]
    .concat();

    assert!(
        assert_exact_multipart_body(
            &format!("multipart/form-data; boundary={boundary}"),
            body.as_bytes(),
            &["first.jsonl"],
        )
        .is_err(),
        "an extra file part was accepted"
    );
}

fn assert_exact_multipart(request: &PeerRequest, submitted: &[&str]) {
    assert_eq!(request.method(), "POST");
    assert_eq!(request.path(), INGEST_PATH);
    let content_type = request
        .header("content-type")
        .expect("multipart content type");
    assert_exact_multipart_body(content_type, request.body(), submitted)
        .unwrap_or_else(|error| panic!("invalid multipart body: {error}"));
}

struct MultipartPart<'a> {
    name: String,
    filename: Option<String>,
    content_type: String,
    body: &'a [u8],
}

fn assert_exact_multipart_body(
    content_type: &str,
    body: &[u8],
    submitted: &[&str],
) -> Result<(), String> {
    let parts = parse_multipart_parts(content_type, body)?;
    if parts.len() != submitted.len() + 1 {
        return Err(format!(
            "expected {} multipart parts, got {}",
            submitted.len() + 1,
            parts.len()
        ));
    }

    let envelope_part = &parts[0];
    if envelope_part.name != "envelope"
        || envelope_part.filename.is_some()
        || !envelope_part
            .content_type
            .eq_ignore_ascii_case("application/json")
    {
        return Err("first multipart part is not the JSON envelope".to_owned());
    }
    let envelope: Value = serde_json::from_slice(envelope_part.body)
        .map_err(|_| "envelope is not JSON".to_owned())?;
    let envelope_object = envelope
        .as_object()
        .ok_or_else(|| "envelope is not an object".to_owned())?;
    if envelope_object.len() != 3
        || envelope.get("day") != Some(&Value::String(DAY.to_owned()))
        || envelope.get("segment") != Some(&Value::String(SEGMENT.to_owned()))
    {
        return Err("envelope fields differ from the v3 contract".to_owned());
    }
    let envelope_files = envelope
        .get("files")
        .and_then(Value::as_array)
        .ok_or_else(|| "envelope files are missing".to_owned())?;
    let envelope_submitted = envelope_files
        .iter()
        .map(|file| {
            let object = file
                .as_object()
                .ok_or_else(|| "envelope file is not an object".to_owned())?;
            if object.len() != 1 {
                return Err("envelope file has unexpected fields".to_owned());
            }
            object
                .get("submitted")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| "envelope file is missing submitted".to_owned())
        })
        .collect::<Result<Vec<_>, _>>()?;
    let expected = submitted
        .iter()
        .map(|name| (*name).to_owned())
        .collect::<Vec<_>>();
    if envelope_submitted != expected {
        return Err("envelope submitted names differ from expected files".to_owned());
    }

    for (part, expected_name) in parts[1..].iter().zip(&envelope_submitted) {
        if part.name != "files"
            || part.filename.as_deref() != Some(expected_name)
            || !part
                .content_type
                .eq_ignore_ascii_case("application/octet-stream")
        {
            return Err("file part differs from its envelope entry".to_owned());
        }
    }
    Ok(())
}

fn parse_multipart_parts<'a>(
    content_type: &str,
    body: &'a [u8],
) -> Result<Vec<MultipartPart<'a>>, String> {
    let boundary = content_type
        .split(';')
        .map(str::trim)
        .find_map(|parameter| parameter.strip_prefix("boundary="))
        .map(|boundary| boundary.trim_matches('"'))
        .filter(|boundary| !boundary.is_empty())
        .ok_or_else(|| "multipart boundary is missing".to_owned())?;
    let opening = format!("--{boundary}\r\n");
    let separator = format!("\r\n--{boundary}");
    let mut remainder = body
        .strip_prefix(opening.as_bytes())
        .ok_or_else(|| "multipart body has no opening boundary".to_owned())?;
    let mut parts = Vec::new();
    loop {
        let separator_offset = find_bytes(remainder, separator.as_bytes())
            .ok_or_else(|| "multipart part has no closing boundary".to_owned())?;
        parts.push(parse_multipart_part(&remainder[..separator_offset])?);
        remainder = &remainder[separator_offset + separator.len()..];
        if remainder == b"--\r\n" {
            return Ok(parts);
        }
        remainder = remainder
            .strip_prefix(b"\r\n")
            .ok_or_else(|| "multipart boundary is malformed".to_owned())?;
    }
}

fn parse_multipart_part(bytes: &[u8]) -> Result<MultipartPart<'_>, String> {
    let headers_end = find_bytes(bytes, b"\r\n\r\n")
        .ok_or_else(|| "multipart part has no header separator".to_owned())?;
    let headers = std::str::from_utf8(&bytes[..headers_end])
        .map_err(|_| "multipart headers are not UTF-8".to_owned())?;
    let disposition = headers
        .split("\r\n")
        .find_map(|header| header.strip_prefix("Content-Disposition: "))
        .ok_or_else(|| "multipart part has no content disposition".to_owned())?;
    let content_type = headers
        .split("\r\n")
        .find_map(|header| header.strip_prefix("Content-Type: "))
        .ok_or_else(|| "multipart part has no content type".to_owned())?
        .to_owned();
    if !disposition.starts_with("form-data") {
        return Err("multipart disposition is not form-data".to_owned());
    }
    let name = multipart_disposition_parameter(disposition, "name")
        .ok_or_else(|| "multipart part has no name".to_owned())?;
    Ok(MultipartPart {
        name,
        filename: multipart_disposition_parameter(disposition, "filename"),
        content_type,
        body: &bytes[headers_end + b"\r\n\r\n".len()..],
    })
}

fn multipart_disposition_parameter(disposition: &str, parameter: &str) -> Option<String> {
    disposition.split(';').skip(1).find_map(|attribute| {
        let (name, value) = attribute.trim().split_once('=')?;
        (name == parameter).then(|| value.trim_matches('"').to_owned())
    })
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    (!needle.is_empty())
        .then(|| {
            haystack
                .windows(needle.len())
                .position(|window| window == needle)
        })
        .flatten()
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
            &projection["paths"][INGEST_PATH]["post"]["responses"]["200"]["content"]["application/json"]
                ["examples"]["normal"]["value"]
        }
        "manifest" => {
            &projection["paths"][INGEST_MANIFEST_PATH]["get"]["responses"]["200"]["content"]["application/json"]
                ["example"]
        }
        "manifest_day" => {
            &projection["paths"][INGEST_MANIFEST_DAY_PATH]["get"]["responses"]["200"]["content"]["application/json"]
                ["example"]
        }
        "segments" => {
            &projection["paths"][INGEST_SEGMENTS_PATH]["get"]["responses"]["200"]["content"]["application/json"]
                ["example"]
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
    projection["paths"][INGEST_PATH]["post"]["responses"]["200"]["content"]
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
