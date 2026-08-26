// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

mod support;

use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use solstone_tmux::clock::{Clock, TestClock};
use solstone_tmux::config::{CONFIG_FILENAME, RuntimeConfig};
use solstone_tmux::health::{DiagnosticCode, HEALTH_FILENAME, HealthWriter};
use solstone_tmux::instance_lock::InstanceLock;
use solstone_tmux::journal::{
    INGEST_MANIFEST_DAY_PATH, INGEST_MANIFEST_PATH, INGEST_PATH, INGEST_SEGMENTS_PATH,
};
use solstone_tmux::migration::{MigrationOutcome, migrate_legacy_config};
use solstone_tmux::model::CaptureResult;
use solstone_tmux::observer::{
    CaptureProvider, ObserverConfig, ObserverOperationError, SegmentManager, ShutdownEvent,
    run_observer, shutdown_barrier, stream_directory,
};
use solstone_tmux::paths::{PlatformKind, ensure_private_directory};
use solstone_tmux::private_link::{
    OBSERVER_HEADER_NAME, PROTOCOL_VERSION, PROTOCOL_VERSION_HEADER_NAME, persist_credential,
};
use solstone_tmux::segment::SegmentState;
use solstone_tmux::sync::{JournalSession, SyncActivity, SyncScheduler, SyncTask, SyncWake};
use support::private_link_peer::PrivateLinkPeer;
use support::{TestDirectory, golden_capture};
use time::{Date, Month, PrimitiveDateTime, Time, UtcOffset};
use tokio::sync::{oneshot, watch};

const CANDIDATE_BYTES: &[u8] = b"existing cache remains\n";
const LINKED_DEVICE_DAY: &str = "20260729";
const LINKED_DEVICE_STREAM: &str = "host.tmux";
const LINKED_DEVICE_SEGMENT: &str = "120000_300";
const LINKED_DEVICE_FILE: &str = "tmux_linked_device_screen.jsonl";
const LINKED_DEVICE_BYTES: &[u8] = b"linked-device candidate\n";

#[tokio::test]
async fn migrated_custom_stream_refuses_before_network_while_capture_continues() {
    let fixture = BindingFixture::new("binding-custom-stream");
    fixture.install_legacy(real_legacy_fixture());
    assert_eq!(
        fixture.migrate("different-host.example"),
        MigrationOutcome::Migrated
    );
    let peer = PrivateLinkPeer::start().await;

    let evidence = run_binding_failure(
        &fixture,
        &peer,
        "different-host.example",
        DiagnosticCode::ConfiguredStreamMismatch,
    )
    .await;

    assert_eq!(peer.accepted_carriers(), 0);
    assert!(peer.requests().is_empty());
    evidence.assert_capture_continued();
    assert_eq!(
        fs::read(&evidence.candidate).expect("retained candidate"),
        CANDIDATE_BYTES
    );
    assert_eq!(
        evidence.health["last_error_code"],
        "configured_stream_mismatch"
    );
    peer.shutdown().await;
}

#[test]
fn linked_device_sweep_diagnostics_are_actionable() {
    assert_eq!(
        DiagnosticCode::ConfiguredStreamMismatch.as_str(),
        "configured_stream_mismatch"
    );
    assert_eq!(
        DiagnosticCode::ConfiguredStreamMismatch.message(),
        "set stream to the hostname-derived tmux name and restart"
    );
    assert_eq!(DiagnosticCode::JournalRejected.as_str(), "journal_rejected");
    assert_eq!(
        DiagnosticCode::JournalRejected.message(),
        "journal request was rejected"
    );
}

#[test]
fn linked_device_sweep_uses_exactly_the_four_v3_operations_without_legacy_headers() {
    linked_device_runtime().block_on(async {
        let peer = PrivateLinkPeer::start().await;
        let temporary = TestDirectory::new("linked-device-four-v3-operations");
        ensure_private_directory(temporary.path()).expect("private root");
        let candidate = create_linked_device_candidate(&temporary);
        let mut session = JournalSession::start(peer.credential(), temporary.path().to_path_buf())
            .await
            .expect("linked-device session");
        enqueue_v3_success_chain(&peer);
        let mut scheduler = linked_device_scheduler(&temporary, -1);

        let summary = scheduler
            .run_sweep(&mut session, linked_device_no_shutdown())
            .await;

        assert_eq!(summary.attempted, 1);
        assert_eq!(summary.custodied, 1, "{summary:?}");
        assert_eq!(
            peer.requests()
                .iter()
                .map(|request| (request.method().to_owned(), request.path().to_owned()))
                .collect::<Vec<_>>(),
            vec![
                ("POST".to_owned(), INGEST_PATH.to_owned()),
                ("GET".to_owned(), INGEST_MANIFEST_PATH.to_owned()),
                (
                    "GET".to_owned(),
                    INGEST_MANIFEST_DAY_PATH.replace("{day}", LINKED_DEVICE_DAY),
                ),
                (
                    "GET".to_owned(),
                    INGEST_SEGMENTS_PATH.replace("{day}", LINKED_DEVICE_DAY),
                ),
            ],
            "the real mTLS peer must observe no registration or extra liveness request",
        );
        for request in peer.requests() {
            assert_eq!(
                request.header(PROTOCOL_VERSION_HEADER_NAME),
                Some(PROTOCOL_VERSION)
            );
            assert_legacy_header_is_absent(&request, "authorization");
            assert_legacy_header_is_absent(&request, OBSERVER_HEADER_NAME);
        }
        assert_eq!(
            fs::read(candidate).expect("candidate bytes"),
            LINKED_DEVICE_BYTES
        );
        session
            .shutdown()
            .await
            .expect("shutdown linked-device session");
        peer.shutdown().await;
    });
}

#[test]
fn linked_device_403_and_426_retain_every_candidate_for_each_operation_class() {
    linked_device_runtime().block_on(async {
        for (status, reason_code) in [
            (403, "linked_device_required"),
            (426, "protocol_version_legacy"),
            (426, "protocol_version_future"),
        ] {
            for operation in ["upload", "manifest", "manifest_day", "segments"] {
                let peer = PrivateLinkPeer::start().await;
                let temporary = TestDirectory::new(&format!(
                    "linked-device-{status}-{reason_code}-{operation}"
                ));
                ensure_private_directory(temporary.path()).expect("private root");
                let candidate = create_linked_device_candidate(&temporary);
                let mut session = JournalSession::start(
                    peer.credential(),
                    temporary.path().to_path_buf(),
                )
                .await
                .expect("linked-device session");
                enqueue_v3_rejection(&peer, operation, status, reason_code);
                let mut scheduler = linked_device_scheduler(&temporary, 0);

                let summary = scheduler
                    .run_sweep(&mut session, linked_device_no_shutdown())
                    .await;

                assert_eq!(summary.attempted, 1, "{operation} {reason_code}");
                assert_eq!(summary.custodied, 0, "{operation} {reason_code}");
                assert_eq!(
                    summary.diagnostic,
                    Some(DiagnosticCode::JournalRejected),
                    "{operation} {reason_code}: {summary:?}",
                );
                assert_eq!(
                    peer.requests()
                        .iter()
                        .map(|request| request.path().to_owned())
                        .collect::<Vec<_>>(),
                    v3_paths_through(operation),
                    "a refused {operation} must not populate reconciliation state or mark the day fresh",
                );
                assert!(candidate.parent().expect("segment directory").is_dir());
                assert_eq!(
                    fs::read(&candidate).expect("retained candidate bytes"),
                    LINKED_DEVICE_BYTES,
                    "a refused {operation} must not clean up local data",
                );
                session.shutdown().await.expect("shutdown linked-device session");
                peer.shutdown().await;
            }
        }
    });
}

async fn run_binding_failure(
    fixture: &BindingFixture,
    peer: &PrivateLinkPeer,
    hostname: &str,
    diagnostic: DiagnosticCode,
) -> FailureEvidence {
    let config =
        RuntimeConfig::load(&fixture.config_root, hostname).expect("load runtime settings");
    persist_credential(&fixture.config_root, &peer.credential())
        .expect("persist paired credential");
    let candidate = fixture.create_candidate(config.stream.as_str());
    let clock = Arc::new(test_clock());
    let lock = InstanceLock::acquire(&fixture.data_root).expect("instance lock");
    let stream_dir = stream_directory(
        &fixture.data_root,
        &config.stream,
        clock.wall_now(),
        clock.local_offset(),
    )
    .expect("active stream directory");
    let segment = SegmentState::create(
        &stream_dir,
        clock.wall_now(),
        Duration::ZERO,
        clock.local_offset(),
    )
    .expect("active segment");
    let polls = Arc::new(AtomicUsize::new(0));
    let (observer_stop, observer_stopped) = oneshot::channel();
    let (observer_barrier, supervisor_barrier) = shutdown_barrier();
    drop(supervisor_barrier);
    let observer = tokio::spawn(run_observer(
        Arc::new(CountingCapture(Arc::clone(&polls))),
        Box::new(SegmentManager::new(
            segment,
            fixture.data_root.clone(),
            config.stream.clone(),
            clock.local_offset(),
            SyncWake::default(),
        )),
        Arc::clone(&clock) as Arc<dyn Clock>,
        Box::pin(async move {
            let _ = observer_stopped.await;
            ShutdownEvent::Injected
        }),
        observer_barrier,
        ObserverConfig {
            capture_interval: Duration::from_millis(10),
            segment_interval: Duration::from_secs(300),
        },
    ));

    let (sync_stop, sync_shutdown) = tokio::sync::watch::channel(false);
    let (activity, _activity_receiver) = tokio::sync::watch::channel(SyncActivity::Idle);
    let sync = tokio::spawn(
        SyncTask {
            config_root: fixture.config_root.clone(),
            data_root: fixture.data_root.clone(),
            config,
            hostname: hostname.to_owned(),
            clock: Arc::clone(&clock) as Arc<dyn Clock>,
            wake: SyncWake::default(),
            activity,
            health: HealthWriter::new(fixture.data_root.clone(), &lock),
            retention_fence: Arc::new(solstone_tmux::sync::RetentionFence::new()),
        }
        .run(sync_shutdown),
    );

    let health = wait_for_diagnostic(&fixture.data_root, diagnostic).await;
    wait_until("multiple capture polls", || {
        polls.load(Ordering::SeqCst) >= 2
    })
    .await;
    let _ = observer_stop.send(());
    sync_stop.send_replace(true);
    let observer_exit = observer.await.expect("join observer");
    assert_eq!(observer_exit.exit_code, 0);
    sync.await
        .expect("join sync task")
        .expect("sync task shutdown");
    drop(lock);

    FailureEvidence {
        candidate,
        polls: polls.load(Ordering::SeqCst),
        local_files: jsonl_files(&fixture.data_root.join("captures")),
        health,
    }
}

struct FailureEvidence {
    candidate: PathBuf,
    polls: usize,
    local_files: Vec<PathBuf>,
    health: Value,
}

impl FailureEvidence {
    fn assert_capture_continued(&self) {
        assert!(self.polls >= 2, "capture polls: {}", self.polls);
        assert!(
            self.local_files.len() >= 2,
            "durable files after capture: {:?}",
            self.local_files
        );
    }
}

struct BindingFixture {
    _temporary: TestDirectory,
    data_root: PathBuf,
    config_root: PathBuf,
}

impl BindingFixture {
    fn new(label: &str) -> Self {
        let temporary = TestDirectory::new(label);
        let data_root = temporary.path().join("data");
        let config_root = temporary.path().join("config");
        ensure_private_directory(&data_root).expect("data root");
        ensure_private_directory(&config_root).expect("config root");
        Self {
            _temporary: temporary,
            data_root,
            config_root,
        }
    }

    fn install_legacy(&self, bytes: Vec<u8>) {
        let path = self.data_root.join("config").join(CONFIG_FILENAME);
        fs::create_dir_all(path.parent().expect("legacy parent")).expect("legacy directory");
        fs::write(path, bytes).expect("legacy settings");
    }

    fn migrate(&self, hostname: &str) -> MigrationOutcome {
        migrate_legacy_config(
            PlatformKind::Linux,
            &self.data_root,
            &self.config_root,
            hostname,
        )
        .expect("migrate settings")
    }

    fn create_candidate(&self, stream: &str) -> PathBuf {
        let path = self
            .data_root
            .join("captures/20260729")
            .join(stream)
            .join("110000_300")
            .join("tmux_existing_screen.jsonl");
        fs::create_dir_all(path.parent().expect("candidate parent")).expect("candidate directory");
        fs::write(&path, CANDIDATE_BYTES).expect("candidate bytes");
        path
    }
}

struct CountingCapture(Arc<AtomicUsize>);

impl CaptureProvider for CountingCapture {
    fn poll<'a>(
        &'a self,
        _wall_unix_seconds: i64,
        _capture_interval: Duration,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<CaptureResult>, ObserverOperationError>> + Send + 'a>>
    {
        Box::pin(async move {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(vec![golden_capture("main")])
        })
    }
}

fn real_legacy_fixture() -> Vec<u8> {
    fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/data/legacy")
            .join(CONFIG_FILENAME),
    )
    .expect("real legacy fixture")
}

fn create_linked_device_candidate(temporary: &TestDirectory) -> PathBuf {
    let path = temporary
        .path()
        .join("captures")
        .join(LINKED_DEVICE_DAY)
        .join(LINKED_DEVICE_STREAM)
        .join(LINKED_DEVICE_SEGMENT)
        .join(LINKED_DEVICE_FILE);
    fs::create_dir_all(path.parent().expect("candidate parent")).expect("candidate directory");
    fs::write(&path, LINKED_DEVICE_BYTES).expect("candidate bytes");
    path
}

fn linked_device_scheduler(temporary: &TestDirectory, retention_days: i64) -> SyncScheduler {
    SyncScheduler::new(
        temporary.path().join("captures"),
        solstone_tmux::name::derive_component(LINKED_DEVICE_STREAM).expect("derived stream"),
        retention_days,
        Arc::new(test_clock()),
        SyncWake::default(),
    )
}

fn linked_device_no_shutdown() -> watch::Receiver<bool> {
    let (sender, receiver) = watch::channel(false);
    std::mem::forget(sender);
    receiver
}

fn enqueue_v3_success_chain(peer: &PrivateLinkPeer) {
    let digest = linked_device_digest();
    let mut upload = v3_projection_example("upload");
    upload["segment"] = Value::String(LINKED_DEVICE_SEGMENT.to_owned());
    peer.enqueue_response(200, serde_json::to_vec(&upload).expect("upload response"));

    let mut manifest = v3_projection_example("manifest");
    manifest["days"] = json!({ LINKED_DEVICE_DAY: { "segments": 1 } });
    peer.enqueue_response(
        200,
        serde_json::to_vec(&manifest).expect("manifest response"),
    );

    let mut manifest_day = v3_projection_example("manifest_day");
    manifest_day["day"] = Value::String(LINKED_DEVICE_DAY.to_owned());
    manifest_day["segments"] = json!({
        LINKED_DEVICE_SEGMENT: {
            "files": [linked_device_remote_file(&digest)]
        }
    });
    peer.enqueue_response(
        200,
        serde_json::to_vec(&manifest_day).expect("day manifest response"),
    );

    let mut segments = v3_projection_example("segments");
    segments["items"] = json!([{
        "key": LINKED_DEVICE_SEGMENT,
        "observed": true,
        "files": [linked_device_remote_file(&digest)]
    }]);
    segments["total"] = json!(1);
    peer.enqueue_response(
        200,
        serde_json::to_vec(&segments).expect("segments response"),
    );
}

fn enqueue_v3_rejection(peer: &PrivateLinkPeer, operation: &str, status: u16, reason_code: &str) {
    match operation {
        "upload" => peer.enqueue_response(status, rejection_response(reason_code)),
        "manifest" => {
            enqueue_v3_upload(peer);
            peer.enqueue_response(status, rejection_response(reason_code));
        }
        "manifest_day" => {
            enqueue_v3_upload(peer);
            enqueue_v3_manifest(peer);
            peer.enqueue_response(status, rejection_response(reason_code));
        }
        "segments" => {
            enqueue_v3_upload(peer);
            enqueue_v3_manifest(peer);
            enqueue_v3_manifest_day(peer);
            peer.enqueue_response(status, rejection_response(reason_code));
        }
        _ => panic!("unknown v3 operation: {operation}"),
    }
}

fn enqueue_v3_upload(peer: &PrivateLinkPeer) {
    let mut upload = v3_projection_example("upload");
    upload["segment"] = Value::String(LINKED_DEVICE_SEGMENT.to_owned());
    peer.enqueue_response(200, serde_json::to_vec(&upload).expect("upload response"));
}

fn enqueue_v3_manifest(peer: &PrivateLinkPeer) {
    let mut manifest = v3_projection_example("manifest");
    manifest["days"] = json!({ LINKED_DEVICE_DAY: { "segments": 1 } });
    peer.enqueue_response(
        200,
        serde_json::to_vec(&manifest).expect("manifest response"),
    );
}

fn enqueue_v3_manifest_day(peer: &PrivateLinkPeer) {
    let digest = linked_device_digest();
    let mut manifest_day = v3_projection_example("manifest_day");
    manifest_day["day"] = Value::String(LINKED_DEVICE_DAY.to_owned());
    manifest_day["segments"] = json!({
        LINKED_DEVICE_SEGMENT: {
            "files": [linked_device_remote_file(&digest)]
        }
    });
    peer.enqueue_response(
        200,
        serde_json::to_vec(&manifest_day).expect("day manifest response"),
    );
}

fn linked_device_remote_file(digest: &str) -> Value {
    json!({
        "name": LINKED_DEVICE_FILE,
        "size": LINKED_DEVICE_BYTES.len(),
        "sha256": digest,
        "status": "present"
    })
}

fn linked_device_digest() -> String {
    format!("{:x}", Sha256::digest(LINKED_DEVICE_BYTES))
}

fn rejection_response(reason_code: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "error": "linked device rejected",
        "reason_code": reason_code,
        "detail": "linked-device identity or protocol version is not accepted"
    }))
    .expect("rejection response")
}

fn v3_projection_example(name: &str) -> Value {
    let projection: Value = serde_json::from_slice(
        &fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("vendor/observer-client-contract/projection.openapi.json"),
        )
        .expect("read projection"),
    )
    .expect("parse projection");
    match name {
        "upload" => projection["paths"][INGEST_PATH]["post"]["responses"]["200"]
            ["content"]["application/json"]["examples"]["normal"]["value"]
            .clone(),
        "manifest" => projection["paths"][INGEST_MANIFEST_PATH]["get"]
            ["responses"]["200"]["content"]["application/json"]["example"]
            .clone(),
        "manifest_day" => projection["paths"][INGEST_MANIFEST_DAY_PATH]["get"]
            ["responses"]["200"]["content"]["application/json"]["example"]
            .clone(),
        "segments" => projection["paths"][INGEST_SEGMENTS_PATH]["get"]
            ["responses"]["200"]["content"]["application/json"]["example"]
            .clone(),
        _ => panic!("unknown projection example: {name}"),
    }
}

fn v3_paths_through(operation: &str) -> Vec<String> {
    match operation {
        "upload" => vec![INGEST_PATH.to_owned()],
        "manifest" => vec![INGEST_PATH.to_owned(), INGEST_MANIFEST_PATH.to_owned()],
        "manifest_day" => vec![
            INGEST_PATH.to_owned(),
            INGEST_MANIFEST_PATH.to_owned(),
            INGEST_MANIFEST_DAY_PATH.replace("{day}", LINKED_DEVICE_DAY),
        ],
        "segments" => vec![
            INGEST_PATH.to_owned(),
            INGEST_MANIFEST_PATH.to_owned(),
            INGEST_MANIFEST_DAY_PATH.replace("{day}", LINKED_DEVICE_DAY),
            INGEST_SEGMENTS_PATH.replace("{day}", LINKED_DEVICE_DAY),
        ],
        _ => panic!("unknown v3 operation: {operation}"),
    }
}

fn assert_legacy_header_is_absent(request: &support::private_link_peer::PeerRequest, name: &str) {
    assert!(request.header(name).is_none(), "unexpected {name} header");
    assert!(
        request.header(&name.to_ascii_uppercase()).is_none(),
        "unexpected {name} header in another casing",
    );
}

fn linked_device_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .expect("runtime")
}

fn test_clock() -> TestClock {
    let date = Date::from_calendar_date(2026, Month::July, 29).expect("test date");
    let time = Time::from_hms(12, 0, 0).expect("test time");
    TestClock::new(
        PrimitiveDateTime::new(date, time).assume_utc(),
        Duration::ZERO,
        UtcOffset::UTC,
    )
}

async fn wait_for_diagnostic(data_root: &Path, diagnostic: DiagnosticCode) -> Value {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(bytes) = fs::read(data_root.join(HEALTH_FILENAME))
                && let Ok(snapshot) = serde_json::from_slice::<Value>(&bytes)
                && snapshot["last_error_code"] == diagnostic.as_str()
            {
                return snapshot;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("diagnostic health timeout")
}

async fn wait_until(context: &str, predicate: impl Fn() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !predicate() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{context} timed out"));
}

fn jsonl_files(root: &Path) -> Vec<PathBuf> {
    fn visit(path: &Path, files: &mut Vec<PathBuf>) {
        let Ok(entries) = fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                visit(&path, files);
            } else if path.extension().and_then(|value| value.to_str()) == Some("jsonl") {
                files.push(path);
            }
        }
    }

    let mut files = Vec::new();
    visit(root, &mut files);
    files.sort();
    files
}
