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
use solstone_tmux::clock::{Clock, TestClock};
use solstone_tmux::config::{CONFIG_FILENAME, RuntimeConfig};
use solstone_tmux::health::{DiagnosticCode, HEALTH_FILENAME, HealthWriter};
use solstone_tmux::instance_lock::InstanceLock;
use solstone_tmux::journal::RegistrationDescriptor;
use solstone_tmux::migration::{MigrationOutcome, migrate_legacy_config};
use solstone_tmux::model::CaptureResult;
use solstone_tmux::observer::{
    CaptureProvider, ObserverConfig, ObserverOperationError, SegmentManager, ShutdownEvent,
    run_observer, shutdown_barrier, stream_directory,
};
use solstone_tmux::paths::{PlatformKind, ensure_private_directory};
use solstone_tmux::private_link::{
    CREDENTIALS_FILENAME, OBSERVER_FILENAME, ObserverState, load_observer, persist_credential,
    persist_observer,
};
use solstone_tmux::segment::SegmentState;
use solstone_tmux::sync::{RegistrationOwner, SyncActivity, SyncTask, SyncWake};
use support::private_link_peer::PrivateLinkPeer;
use support::{TestDirectory, golden_capture};
use time::{Date, Month, PrimitiveDateTime, Time, UtcOffset};
use tokio::sync::oneshot;

const LEGACY_KEY: &str = "LEGACYKEYCANARY-do-not-copy";
const RETURNED_KEY: &str = "journal-returned-observer-key";
const CANDIDATE_BYTES: &[u8] = b"existing cache remains\n";

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

#[tokio::test]
async fn returned_name_mismatch_is_not_persisted_and_capture_continues() {
    let fixture = BindingFixture::new("binding-returned-name");
    fixture.install_legacy(empty_stream_fixture());
    assert_eq!(fixture.migrate("host.example"), MigrationOutcome::Migrated);
    let peer = PrivateLinkPeer::start().await;
    peer.enqueue_response(
        200,
        registration_response("wrong-name", "wrong-name-observer-key"),
    );

    let evidence = run_binding_failure(
        &fixture,
        &peer,
        "host.example",
        DiagnosticCode::RegistrationNameMismatch,
    )
    .await;

    let requests = peer.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].path(), "/app/observer/register");
    assert!(!fixture.config_root.join(OBSERVER_FILENAME).exists());
    evidence.assert_capture_continued();
    assert_eq!(
        fs::read(&evidence.candidate).expect("retained candidate"),
        CANDIDATE_BYTES
    );
    assert_eq!(
        evidence.health["last_error_code"],
        "registration_name_mismatch"
    );
    peer.shutdown().await;
}

#[tokio::test]
async fn normal_prior_install_migrates_and_reuses_checked_registration() {
    let fixture = BindingFixture::new("binding-normal-prior-install");
    let legacy = real_legacy_fixture();
    assert!(String::from_utf8_lossy(&legacy).contains(LEGACY_KEY));
    fixture.install_legacy(legacy);
    let cache = fixture.create_candidate("extro.tmux");
    assert_eq!(fixture.migrate("extro.example"), MigrationOutcome::Migrated);
    let runtime =
        RuntimeConfig::load(&fixture.config_root, "extro.example").expect("load migrated settings");
    assert_eq!(runtime.stream.as_str(), "extro.tmux");

    let peer = PrivateLinkPeer::start().await;
    let credential = peer.credential();
    persist_credential(&fixture.config_root, &credential).expect("pairing state");
    peer.enqueue_response(200, registration_response("extro.tmux", RETURNED_KEY));
    let owner = RegistrationOwner::start(credential.clone(), fixture.config_root.clone())
        .await
        .expect("start registration owner");
    let descriptor = descriptor("extro.example");
    let (registered, contacted) = owner
        .ensure_registration(&descriptor, "extro.tmux")
        .await
        .expect("idempotent Journal registration");
    assert!(contacted);
    assert_eq!(registered.key, RETURNED_KEY);
    owner.shutdown().await.expect("first owner shutdown");

    let persisted = load_observer(&fixture.config_root, &credential.instance_id, "extro.tmux")
        .expect("load observer state")
        .expect("observer state present");
    assert_eq!(persisted.key, RETURNED_KEY);
    assert_eq!(persisted.credential_instance_id, credential.instance_id);
    assert_eq!(
        fs::read(&cache).expect("existing cache retained"),
        CANDIDATE_BYTES
    );

    let native = fs::read(fixture.config_root.join(CONFIG_FILENAME)).expect("native config");
    let credentials =
        fs::read(fixture.config_root.join(CREDENTIALS_FILENAME)).expect("credentials state");
    let observer = fs::read(fixture.config_root.join(OBSERVER_FILENAME)).expect("observer state");
    for bytes in [&native, &credentials, &observer] {
        assert!(
            !String::from_utf8_lossy(bytes).contains(LEGACY_KEY),
            "legacy key reached native or private state"
        );
    }
    assert!(!String::from_utf8_lossy(&native).contains("server_url"));
    assert_ne!(
        fixture.config_root.join(CONFIG_FILENAME),
        fixture.config_root.join(CREDENTIALS_FILENAME)
    );

    let second = RegistrationOwner::start(credential, fixture.config_root.clone())
        .await
        .expect("restart registration owner");
    let (reused, contacted) = second
        .ensure_registration(&descriptor, "extro.tmux")
        .await
        .expect("reuse registration");
    assert!(!contacted);
    assert!(reused == persisted, "cached registration changed");
    peer.enqueue_response(
        200,
        br#"{"protocol_version":2,"items":[],"total":0}"#.to_vec(),
    );
    second
        .journal()
        .ingest_segments("20260729")
        .await
        .expect("authenticated listing");
    let requests = peer.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].path(), "/app/observer/register");
    assert_eq!(
        requests[1].header("authorization"),
        Some("Bearer journal-returned-observer-key")
    );
    assert_eq!(
        requests[1].header("x-solstone-observer"),
        Some(RETURNED_KEY)
    );
    second.shutdown().await.expect("second owner shutdown");
    peer.shutdown().await;
}

#[tokio::test]
async fn stale_cached_observer_name_forces_reregistration() {
    let fixture = BindingFixture::new("binding-stale-cached-name");
    let peer = PrivateLinkPeer::start().await;
    let credential = peer.credential();
    persist_observer(
        &fixture.config_root,
        &ObserverState {
            credential_instance_id: credential.instance_id.clone(),
            key: "stale-key".to_owned(),
            prefix: "stale-prefix".to_owned(),
            name: "stale-name".to_owned(),
            ingest_url: "/app/observer/ingest".to_owned(),
            protocol_version: 2,
        },
    )
    .expect("persist stale observer");
    peer.enqueue_response(200, registration_response("host.tmux", RETURNED_KEY));

    let owner = RegistrationOwner::start(credential.clone(), fixture.config_root.clone())
        .await
        .expect("start registration owner");
    let (observer, contacted) = owner
        .ensure_registration(&descriptor("host.example"), "host.tmux")
        .await
        .expect("stale name re-registers");

    assert!(contacted);
    assert_eq!(observer.name, "host.tmux");
    assert_eq!(observer.key, RETURNED_KEY);
    assert_eq!(peer.requests().len(), 1);
    assert_eq!(peer.requests()[0].path(), "/app/observer/register");
    assert!(
        load_observer(&fixture.config_root, &credential.instance_id, "host.tmux")
            .expect("load refreshed observer")
            .expect("refreshed observer")
            == observer,
        "refreshed observer state differs"
    );
    owner.shutdown().await.expect("owner shutdown");
    peer.shutdown().await;
}

#[test]
fn registration_binding_diagnostics_are_actionable() {
    assert_eq!(
        DiagnosticCode::ConfiguredStreamMismatch.as_str(),
        "configured_stream_mismatch"
    );
    assert_eq!(
        DiagnosticCode::ConfiguredStreamMismatch.message(),
        "set stream to the hostname-derived tmux name and restart"
    );
    assert_eq!(
        DiagnosticCode::RegistrationNameMismatch.as_str(),
        "registration_name_mismatch"
    );
    assert_eq!(
        DiagnosticCode::RegistrationNameMismatch.message(),
        "update the paired journal and retry registration"
    );
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
            platform: PlatformKind::Linux,
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

fn empty_stream_fixture() -> Vec<u8> {
    fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/legacy/config-empty-stream.json"),
    )
    .expect("empty-stream legacy fixture")
}

fn registration_response(name: &str, key: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "key": key,
        "prefix": "checked-prefix",
        "name": name,
        "ingest_url": "/app/observer/ingest",
        "protocol_version": 2
    }))
    .expect("registration response")
}

fn descriptor(hostname: &str) -> RegistrationDescriptor {
    RegistrationDescriptor {
        platform: "linux".to_owned(),
        hostname: hostname.to_owned(),
    }
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
