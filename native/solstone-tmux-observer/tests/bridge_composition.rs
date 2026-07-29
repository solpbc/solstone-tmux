// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

mod support;

use std::collections::BTreeSet;
use std::fs;
use std::future::Future;
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use reqwest::header::{CONTENT_LENGTH, CONTENT_TYPE, TRANSFER_ENCODING};
use reqwest::{Method, StatusCode};
use serde_json::Value;
use sha2::{Digest, Sha256};
use solstone_tmux_observer::clock::{Clock, TestClock};
use solstone_tmux_observer::health::DiagnosticCode;
use solstone_tmux_observer::journal::{RegistrationDescriptor, UploadStatus, inventory_files};
use solstone_tmux_observer::model::{CaptureResult, PaneInfo, WindowInfo};
use solstone_tmux_observer::name::derive_component;
use solstone_tmux_observer::observer::{
    CaptureProvider, ObserverConfig, ObserverOperationError, SegmentManager, ShutdownEvent,
    run_observer, shutdown_barrier, stream_directory,
};
use solstone_tmux_observer::paths::ensure_private_directory;
use solstone_tmux_observer::private_link::{
    OBSERVER_FILENAME, OBSERVER_HEADER_NAME, PROTOCOL_VERSION_HEADER_NAME, load_observer,
};
use solstone_tmux_observer::segment::SegmentState;
use solstone_tmux_observer::serialize::serialize_frame;
use solstone_tmux_observer::sync::RegistrationOwner;
use solstone_tmux_observer::sync::SyncWake;
use spl_core::frame::RECOMMENDED_CHUNK;
use spl_core::mux::{INITIAL_WINDOW, UPLOAD_BODY_STAGE_CAPACITY};
use support::TestDirectory;
use support::private_link_peer::{PeerRequest, PrivateLinkPeer};
use time::{Date, Month, PrimitiveDateTime, Time, UtcOffset};
use tokio::sync::oneshot;

const TEST_OBSERVER_KEY: &str = "test-observer-key";
const UPLOAD_DAY: &str = "20260728";
const UPLOAD_SEGMENT: &str = "120000_300";
const FIRST_UPLOAD_FILE: &str = "tmux_main_screen.jsonl";
const SECOND_UPLOAD_FILE: &str = "tmux_work_screen.jsonl";

#[test]
fn bridge_registration_composes_on_the_production_runtime_shape() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .expect("build current-thread runtime");
    runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(5), async {
            let peer = PrivateLinkPeer::start().await;
            let credential = peer.credential();
            let credential_instance_id = credential.instance_id.clone();
            assert!(
                credential.endpoints.iter().all(|endpoint| {
                    endpoint
                        .host
                        .parse::<IpAddr>()
                        .is_ok_and(|address| address.is_loopback())
                }),
                "private-link peer endpoint is not loopback"
            );
            let temporary = TestDirectory::new("bridge-composition");
            ensure_private_directory(temporary.path()).expect("create private config root");
            peer.enqueue_response(200, registration_response("/app/observer/ingest"));

            let owner = RegistrationOwner::start(credential, temporary.path().to_path_buf())
                .await
                .expect("start registration owner");
            owner
                .ensure_registration(&descriptor())
                .await
                .expect("register observer");
            let persisted = load_observer(temporary.path(), &credential_instance_id)
                .expect("load registered observer")
                .expect("registered observer exists");
            assert!(
                persisted.credential_instance_id == credential_instance_id,
                "registered observer has the wrong credential binding"
            );

            let registration_requests = peer.requests();
            assert_eq!(registration_requests.len(), 1);
            let registration = &registration_requests[0];
            assert_eq!(registration.method(), "POST");
            assert_eq!(registration.path(), "/app/observer/register");
            assert_eq!(registration.header(PROTOCOL_VERSION_HEADER_NAME), Some("2"));
            assert!(registration.header(OBSERVER_HEADER_NAME).is_none());
            assert!(registration.header("authorization").is_none());
            assert_registration_body(registration.body());

            let rejected = owner
                .journal()
                .request(Method::GET, "/post-registration")
                .expect("build caller-auth request")
                .header(OBSERVER_HEADER_NAME, "caller-observer")
                .header(PROTOCOL_VERSION_HEADER_NAME, "99")
                .send()
                .await
                .expect("receive caller-auth rejection");
            assert_eq!(rejected.status(), StatusCode::FORBIDDEN);
            assert_eq!(
                peer.requests().len(),
                1,
                "caller-auth request reached the paired peer"
            );

            peer.enqueue_response(200, br#"{"status":"ok"}"#);
            let response = owner
                .journal()
                .request(Method::GET, "/post-registration")
                .expect("build registered request")
                .send()
                .await
                .expect("receive registered response");
            assert_eq!(response.status(), StatusCode::OK);
            let response_body = response.bytes().await.expect("read registered response");
            assert!(
                response_body.as_ref() == br#"{"status":"ok"}"#,
                "paired peer response did not round-trip"
            );

            let requests = peer.requests();
            assert_eq!(requests.len(), 2);
            let registered = &requests[1];
            assert!(
                registered.header(OBSERVER_HEADER_NAME) == Some(TEST_OBSERVER_KEY),
                "observer header was not minted by the opener"
            );
            assert!(
                registered.header("authorization") == Some("Bearer test-observer-key"),
                "authorization header was not minted by the opener"
            );
            assert_eq!(registered.header(PROTOCOL_VERSION_HEADER_NAME), Some("2"));
            assert!(
                registered.header(OBSERVER_HEADER_NAME) != Some("caller-observer"),
                "caller observer header reached the peer"
            );
            assert!(
                registered.header(PROTOCOL_VERSION_HEADER_NAME) != Some("99"),
                "caller protocol header reached the peer"
            );
            assert_eq!(
                peer.accepted_carriers(),
                1,
                "bridge did not reuse its persistent carrier"
            );

            owner.shutdown().await.expect("shutdown registration owner");
            peer.shutdown().await;
        })
        .await
        .expect("bridge composition timed out");
    });
}

#[test]
fn registration_rejects_unconfined_ingest_locations() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .expect("build current-thread runtime");
    runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(5), async {
            let peer = PrivateLinkPeer::start().await;
            let temporary = TestDirectory::new("ingest-confinement");
            ensure_private_directory(temporary.path()).expect("create private config root");
            let owner = RegistrationOwner::start(peer.credential(), temporary.path().to_path_buf())
                .await
                .expect("start registration owner");

            for ingest_url in [
                "http://127.0.0.1/app/observer/ingest",
                "//foreign.example/app/observer/ingest",
                "/app/observer/ingest?mode=foreign",
                "/app/observer/ingest#foreign",
            ] {
                peer.enqueue_response(200, registration_response(ingest_url));
                let result = owner.ensure_registration(&descriptor()).await;
                assert!(
                    matches!(
                        result,
                        Err(error)
                            if error.diagnostic() == DiagnosticCode::JournalContractInvalid
                    ),
                    "unconfined ingest location was accepted"
                );
                assert!(
                    !temporary.path().join(OBSERVER_FILENAME).exists(),
                    "rejected registration was persisted"
                );
            }

            assert_eq!(peer.requests().len(), 4);
            assert_eq!(peer.accepted_carriers(), 1);
            owner.shutdown().await.expect("shutdown registration owner");
            peer.shutdown().await;
        })
        .await
        .expect("ingest confinement timed out");
    });
}

#[test]
fn slow_large_multipart_preserves_capture_on_the_production_runtime() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .expect("build current-thread runtime");
    runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(8), async {
            let peer = PrivateLinkPeer::start().await;
            let temporary = TestDirectory::new("bridge-large-upload");
            ensure_private_directory(temporary.path()).expect("create private config root");
            peer.enqueue_response(200, registration_response("/app/observer/ingest"));
            let owner = RegistrationOwner::start(
                peer.credential(),
                temporary.path().to_path_buf(),
            )
            .await
            .expect("start registration owner");
            let (observer, _) = owner
                .ensure_registration(&descriptor())
                .await
                .expect("register observer");

            let first_bytes = serialized_large_session("main");
            let second_bytes = serialized_large_session("work");
            assert_eq!(
                first_bytes.iter().filter(|byte| **byte == b'\n').count(),
                60
            );
            assert_eq!(
                second_bytes.iter().filter(|byte| **byte == b'\n').count(),
                60
            );
            let first_path = temporary.path().join(FIRST_UPLOAD_FILE);
            let second_path = temporary.path().join(SECOND_UPLOAD_FILE);
            fs::write(&first_path, &first_bytes).expect("write first upload fixture");
            fs::write(&second_path, &second_bytes).expect("write second upload fixture");
            let first_on_disk = fs::read(&first_path).expect("read first upload fixture");
            let second_on_disk = fs::read(&second_path).expect("read second upload fixture");
            assert_exact_bytes(&first_on_disk, &first_bytes, "first serialized fixture");
            assert_exact_bytes(&second_on_disk, &second_bytes, "second serialized fixture");
            let total_file_bytes = first_on_disk.len() + second_on_disk.len();
            assert!(
                total_file_bytes > INITIAL_WINDOW,
                "serializer fixture did not exceed the mux window"
            );
            let inventory = inventory_files(vec![first_path.clone(), second_path.clone()])
                .await
                .expect("inventory upload fixture");
            assert_eq!(inventory.len(), 2);
            assert_eq!(inventory[0].name, FIRST_UPLOAD_FILE);
            assert_eq!(inventory[0].size, first_on_disk.len() as u64);
            assert_eq!(inventory[1].name, SECOND_UPLOAD_FILE);
            assert_eq!(inventory[1].size, second_on_disk.len() as u64);
            let first_sha256 = inventory[0].sha256.clone();
            let second_sha256 = inventory[1].sha256.clone();
            assert_eq!(first_sha256, sha256_hex(&first_on_disk));
            assert_eq!(second_sha256, sha256_hex(&second_on_disk));

            peer.enqueue_response(200, br#"{"status":"ok","segment":"120000_300"}"#);
            peer.withhold_upload_credit();
            let journal = owner.journal().clone();
            let ingest_path = observer.ingest_url.clone();
            let mut upload = tokio::spawn(async move {
                journal
                    .ingest_upload(
                        &ingest_path,
                        UPLOAD_DAY,
                        UPLOAD_SEGMENT,
                        vec![first_path, second_path],
                    )
                    .await
            });

            wait_until("initial upload window", || {
                peer.received_upload_bytes() >= INITIAL_WINDOW
            })
            .await;
            assert!(
                !upload.is_finished(),
                "upload completed while peer credit was withheld"
            );

            let clock = upload_clock();
            let data_root = temporary.path().join("runtime-data");
            let stream = derive_component("runtime.tmux").expect("derive runtime stream");
            let stream_dir =
                stream_directory(&data_root, &stream, clock.wall_now(), clock.local_offset())
                    .expect("resolve runtime stream");
            let segment = SegmentState::create(
                &stream_dir,
                clock.wall_now(),
                Duration::ZERO,
                clock.local_offset(),
            )
            .expect("create runtime segment");
            let finalized = stream_dir.join("120000_005");
            let capture_polls = Arc::new(AtomicUsize::new(0));
            let (stop_observer, stopped_observer) = oneshot::channel();
            let (observer_shutdown_barrier, supervisor_shutdown_barrier) = shutdown_barrier();
            drop(supervisor_shutdown_barrier);
            let observer_task = tokio::spawn(run_observer(
                Arc::new(CountingCapture {
                    polls: Arc::clone(&capture_polls),
                    capture: runtime_capture(),
                }),
                Box::new(SegmentManager::new(
                    segment,
                    data_root,
                    stream,
                    clock.local_offset(),
                    SyncWake::default(),
                )),
                Arc::clone(&clock) as Arc<dyn Clock>,
                Box::pin(async move {
                    let _ = stopped_observer.await;
                    ShutdownEvent::Injected
                }),
                observer_shutdown_barrier,
                ObserverConfig {
                    capture_interval: Duration::from_millis(10),
                    segment_interval: Duration::from_secs(5),
                },
            ));

            wait_until("first capture poll", || {
                capture_polls.load(Ordering::SeqCst) >= 1
            })
            .await;
            clock.set_wall(clock.wall_now() + time::Duration::seconds(5));
            clock.set_monotonic(Duration::from_secs(5));
            wait_until("segment rotation", || finalized.is_dir()).await;
            assert!(
                capture_polls.load(Ordering::SeqCst) >= 2,
                "capture did not continue during the stalled upload"
            );
            assert!(
                finalized.join("tmux_runtime_screen.jsonl").is_file(),
                "rotation did not finalize the captured segment"
            );
            assert!(
                !upload.is_finished(),
                "upload completed before paced credit was granted"
            );

            for _ in 0..16 {
                if upload.is_finished() {
                    break;
                }
                peer.grant_upload_credit(RECOMMENDED_CHUNK as u32);
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
            let upload_result = tokio::time::timeout(Duration::from_secs(2), &mut upload)
                .await
                .expect("paced upload did not complete")
                .expect("join paced upload")
                .expect("paced upload succeeded");
            assert_eq!(upload_result.status, UploadStatus::Ok);

            let peak = owner.journal().upload_stage_high_water_bytes();
            assert!(peak > 0, "upload stage was not measured");
            assert!(
                peak <= UPLOAD_BODY_STAGE_CAPACITY,
                "upload stage exceeded its fixed bound"
            );
            assert!(peak < first_on_disk.len());
            assert!(peak < second_on_disk.len());
            assert!(peak < total_file_bytes);

            let requests = peer.requests();
            assert_eq!(requests.len(), 2);
            let upload_request = &requests[1];
            let parts = assert_large_multipart(upload_request, &first_on_disk, &second_on_disk);
            assert_eq!(sha256_hex(parts[2].body), first_sha256);
            assert_eq!(sha256_hex(parts[3].body), second_sha256);
            println!(
                "AC5 measurements: first_file_bytes={} second_file_bytes={} total_file_bytes={} multipart_bytes={} peak_stage_bytes={} first_sha256={} second_sha256={}",
                first_on_disk.len(),
                second_on_disk.len(),
                total_file_bytes,
                upload_request.body().len(),
                peak,
                first_sha256,
                second_sha256
            );

            let _ = stop_observer.send(());
            let observer_exit = observer_task.await.expect("join runtime observer");
            assert_eq!(observer_exit.exit_code, 0);
            assert_eq!(peer.accepted_carriers(), 1);
            owner.shutdown().await.expect("shutdown registration owner");
            peer.shutdown().await;
        })
        .await
        .expect("large multipart composition timed out");
    });
}

fn descriptor() -> RegistrationDescriptor {
    RegistrationDescriptor {
        platform: "linux".to_owned(),
        hostname: "test-host".to_owned(),
    }
}

fn registration_response(ingest_url: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "key": TEST_OBSERVER_KEY,
        "prefix": "test-prefix",
        "name": "test-name",
        "ingest_url": ingest_url,
        "protocol_version": 2
    }))
    .expect("serialize registration response")
}

fn assert_registration_body(body: &[u8]) {
    let value = serde_json::from_slice::<Value>(body).expect("parse registration request");
    let object = value.as_object().expect("registration body is an object");
    let fields = object.keys().map(String::as_str).collect::<BTreeSet<_>>();
    assert_eq!(
        fields,
        BTreeSet::from(["hostname", "platform", "stream_type", "version"])
    );
    assert_eq!(object["platform"], "linux");
    assert_eq!(object["hostname"], "test-host");
    assert_eq!(object["stream_type"], "tmux");
    assert_eq!(object["version"], env!("CARGO_PKG_VERSION"));
}

fn serialized_large_session(session: &str) -> Vec<u8> {
    let mut file = Vec::new();
    for frame in 0..60 {
        let capture = large_capture(session, frame);
        file.extend_from_slice(
            &serialize_frame(&capture, frame + 1, (frame * 5) as f64)
                .expect("serialize large capture"),
        );
    }
    file
}

fn large_capture(session: &str, frame: u64) -> CaptureResult {
    let window = WindowInfo {
        id: "@1".to_owned(),
        index: 0,
        name: "main".to_owned(),
        active: true,
    };
    let mut content = String::new();
    for row in 0..60 {
        let marker = format!("{row:02} ");
        content.push_str(&marker);
        content.extend(std::iter::repeat_n('x', 200 - marker.len()));
        content.push('\n');
    }
    content.replace_range(0..3, &format!("{:02} ", frame % 100));
    CaptureResult {
        session: session.to_owned(),
        window: window.clone(),
        windows: vec![window],
        panes: vec![PaneInfo {
            id: "%1".to_owned(),
            index: 0,
            left: 0,
            top: 0,
            width: 200,
            height: 60,
            active: true,
            content,
        }],
    }
}

fn runtime_capture() -> CaptureResult {
    let window = WindowInfo {
        id: "@1".to_owned(),
        index: 0,
        name: "runtime".to_owned(),
        active: true,
    };
    CaptureResult {
        session: "runtime".to_owned(),
        window: window.clone(),
        windows: vec![window],
        panes: vec![PaneInfo {
            id: "%1".to_owned(),
            index: 0,
            left: 0,
            top: 0,
            width: 80,
            height: 24,
            active: true,
            content: "runtime fixture\n".to_owned(),
        }],
    }
}

fn upload_clock() -> Arc<TestClock> {
    let date = Date::from_calendar_date(2026, Month::July, 28).expect("test date");
    let time = Time::from_hms(12, 0, 0).expect("test time");
    Arc::new(TestClock::new(
        PrimitiveDateTime::new(date, time).assume_utc(),
        Duration::ZERO,
        UtcOffset::UTC,
    ))
}

async fn wait_until(context: &str, mut predicate: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while !predicate() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {context}"));
}

struct CountingCapture {
    polls: Arc<AtomicUsize>,
    capture: CaptureResult,
}

impl CaptureProvider for CountingCapture {
    fn poll<'a>(
        &'a self,
        _wall_unix_seconds: i64,
        _capture_interval: Duration,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<CaptureResult>, ObserverOperationError>> + Send + 'a>>
    {
        Box::pin(async move {
            self.polls.fetch_add(1, Ordering::SeqCst);
            Ok(vec![self.capture.clone()])
        })
    }
}

struct MultipartPart<'a> {
    name: String,
    filename: Option<String>,
    content_type: Option<String>,
    body: &'a [u8],
}

fn assert_large_multipart<'a>(
    request: &'a PeerRequest,
    first: &[u8],
    second: &[u8],
) -> Vec<MultipartPart<'a>> {
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
    let parts = parse_multipart(request.body(), boundary);
    assert_eq!(parts.len(), 4);
    assert_part(&parts[0], "day", None, None, UPLOAD_DAY.as_bytes());
    assert_part(&parts[1], "segment", None, None, UPLOAD_SEGMENT.as_bytes());
    assert_part(
        &parts[2],
        "files",
        Some(FIRST_UPLOAD_FILE),
        Some("application/octet-stream"),
        first,
    );
    assert_part(
        &parts[3],
        "files",
        Some(SECOND_UPLOAD_FILE),
        Some("application/octet-stream"),
        second,
    );
    parts
}

fn parse_multipart<'a>(body: &'a [u8], boundary: &str) -> Vec<MultipartPart<'a>> {
    let marker = format!("--{boundary}").into_bytes();
    let next_marker = format!("\r\n--{boundary}").into_bytes();
    let mut cursor = 0;
    let mut parts = Vec::new();
    loop {
        assert!(body[cursor..].starts_with(&marker));
        cursor += marker.len();
        if body[cursor..].starts_with(b"--\r\n") {
            cursor += 4;
            assert_eq!(cursor, body.len());
            return parts;
        }
        assert!(body[cursor..].starts_with(b"\r\n"));
        cursor += 2;
        let header_length =
            find_bytes(&body[cursor..], b"\r\n\r\n").expect("multipart header terminator");
        let headers =
            std::str::from_utf8(&body[cursor..cursor + header_length]).expect("multipart headers");
        cursor += header_length + 4;
        let body_length =
            find_bytes(&body[cursor..], &next_marker).expect("multipart body terminator");
        let part_body = &body[cursor..cursor + body_length];
        cursor += body_length + 2;

        let mut name = None;
        let mut filename = None;
        let mut content_type = None;
        for header in headers.split("\r\n") {
            let (header_name, value) = header.split_once(':').expect("multipart header");
            let value = value.trim();
            if header_name.eq_ignore_ascii_case("content-disposition") {
                for parameter in value.split("; ") {
                    if let Some(value) = parameter.strip_prefix("name=\"") {
                        name = Some(value.strip_suffix('"').expect("multipart name").to_owned());
                    } else if let Some(value) = parameter.strip_prefix("filename=\"") {
                        filename = Some(
                            value
                                .strip_suffix('"')
                                .expect("multipart filename")
                                .to_owned(),
                        );
                    }
                }
            } else if header_name.eq_ignore_ascii_case("content-type") {
                content_type = Some(value.to_owned());
            }
        }
        parts.push(MultipartPart {
            name: name.expect("multipart field name"),
            filename,
            content_type,
            body: part_body,
        });
    }
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn assert_part(
    part: &MultipartPart<'_>,
    name: &str,
    filename: Option<&str>,
    content_type: Option<&str>,
    body: &[u8],
) {
    assert_eq!(part.name, name);
    assert_eq!(part.filename.as_deref(), filename);
    assert_eq!(part.content_type.as_deref(), content_type);
    assert_exact_bytes(part.body, body, "multipart part");
}

fn assert_exact_bytes(actual: &[u8], expected: &[u8], context: &str) {
    assert_eq!(actual.len(), expected.len(), "{context} length mismatch");
    let actual_sha256 = sha256_hex(actual);
    let expected_sha256 = sha256_hex(expected);
    assert!(
        actual == expected,
        "{context} digest mismatch: actual={actual_sha256}, expected={expected_sha256}"
    );
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
