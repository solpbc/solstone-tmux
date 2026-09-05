// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

mod support;

use std::fs;
use std::future::Future;
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use reqwest::{Method, StatusCode};
use solstone_tmux::clock::{Clock, SystemClock};
use solstone_tmux::config::DEFAULT_SOURCE;
use solstone_tmux::health::DiagnosticCode;
use solstone_tmux::instance_lock::InstanceLock;
use solstone_tmux::journal::{
    INGEST_MANIFEST_DAY_PATH, INGEST_MANIFEST_PATH, INGEST_SEGMENTS_PATH,
};
use solstone_tmux::model::CaptureResult;
use solstone_tmux::observer::{
    CaptureProvider, ObserverConfig, ObserverOperationError, SegmentLifecycle, ShutdownEvent,
    run_observer, shutdown_barrier,
};
use solstone_tmux::paths::ensure_private_directory;
use solstone_tmux::private_link::{
    OBSERVER_HEADER_NAME, PROTOCOL_VERSION, PROTOCOL_VERSION_HEADER_NAME, PrivateLinkBridge,
};
use solstone_tmux::segment::SegmentClose;
use solstone_tmux::sync::JournalSession;
use spl_core::frame::RECOMMENDED_CHUNK;
use support::TestDirectory;
use support::private_link_peer::PrivateLinkPeer;
use time::UtcOffset;
use tokio::sync::{Notify, oneshot};

#[test]
fn linked_device_bridge_rejects_caller_auth_and_mints_only_v3_protocol_header() {
    runtime().block_on(async {
        let peer = PrivateLinkPeer::start().await;
        assert!(peer.credential().endpoints.iter().all(|endpoint| {
            endpoint
                .host
                .parse::<IpAddr>()
                .is_ok_and(|address| address.is_loopback())
        }));
        let temporary = TestDirectory::new("bridge-v3");
        ensure_private_directory(temporary.path()).expect("private root");
        let lock = InstanceLock::acquire(temporary.path()).expect("acquire lock");
        let session = JournalSession::start(
            peer.credential(),
            temporary.path().to_path_buf(),
            temporary.path().to_path_buf(),
            lock.identity().clone(),
        )
        .await
        .expect("session");

        let rejected = session
            .journal()
            .request(Method::GET, "/caller-auth")
            .expect("request")
            .header("authorization", "Bearer caller")
            .header(OBSERVER_HEADER_NAME, "caller-observer")
            .header(PROTOCOL_VERSION_HEADER_NAME, "99")
            .send()
            .await
            .expect("bridge response");
        assert_eq!(rejected.status(), StatusCode::FORBIDDEN);
        assert!(peer.requests().is_empty());

        peer.enqueue_response(200, br#"{}"#.to_vec());
        let response = session
            .journal()
            .request(Method::GET, "/v3")
            .expect("request")
            .send()
            .await
            .expect("peer response");
        assert_eq!(response.status(), StatusCode::OK);
        let requests = peer.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].header(PROTOCOL_VERSION_HEADER_NAME),
            Some(PROTOCOL_VERSION)
        );
        assert!(requests[0].header("authorization").is_none());
        assert!(requests[0].header(OBSERVER_HEADER_NAME).is_none());
        session.shutdown().await.expect("shutdown");
        peer.shutdown().await;
    });
}

#[test]
fn relay_only_credential_starts_the_private_link_bridge() {
    runtime().block_on(async {
        let peer = PrivateLinkPeer::start().await;
        let mut credential = peer.credential();
        credential.endpoints.clear();
        credential.relay_origin = Some("https://relay.example.invalid".to_owned());
        credential.device_token = Some("relay-only-test-token".to_owned());

        let temporary = TestDirectory::new("bridge-relay-only");
        ensure_private_directory(temporary.path()).expect("private root");
        let lock = InstanceLock::acquire(temporary.path()).expect("acquire lock");
        let refresh = solstone_tmux::journal_version::VersionRefreshState::new(
            temporary.path().to_path_buf(),
            temporary.path().to_path_buf(),
            credential.instance_id.clone(),
            &credential.ca_fp_prefix,
            lock.identity().clone(),
        );

        let bridge = PrivateLinkBridge::start(credential, None, refresh)
            .await
            .expect("start relay-only bridge");
        bridge.shutdown().await;
        peer.shutdown().await;
    });
}

#[test]
fn journal_response_body_limit_applies_to_success_and_error_responses() {
    runtime().block_on(async {
        let peer = PrivateLinkPeer::start().await;
        let temporary = TestDirectory::new("bridge-response-limit-v3");
        ensure_private_directory(temporary.path()).expect("private root");
        let lock = InstanceLock::acquire(temporary.path()).expect("acquire lock");
        let session = JournalSession::start(
            peer.credential(),
            temporary.path().to_path_buf(),
            temporary.path().to_path_buf(),
            lock.identity().clone(),
        )
        .await
        .expect("session");
        let oversized = vec![b'x'; 4 * 1024 * 1024 + 1];
        peer.enqueue_response(200, oversized.clone());
        let error = session
            .journal()
            .ingest_manifest_day("20260815", DEFAULT_SOURCE)
            .await
            .expect_err("oversized successful day manifest accepted");
        assert_eq!(error.diagnostic(), DiagnosticCode::JournalResponseTooLarge);
        peer.enqueue_response(403, oversized);
        let error = session
            .journal()
            .ingest_manifest(DEFAULT_SOURCE)
            .await
            .expect_err("oversized error manifest accepted");
        assert_eq!(error.diagnostic(), DiagnosticCode::JournalResponseTooLarge);
        session.shutdown().await.expect("shutdown");
        peer.shutdown().await;
    });
}

#[test]
fn linked_device_session_composes_on_the_production_runtime_shape() {
    runtime().block_on(async {
        let peer = PrivateLinkPeer::start().await;
        let temporary = TestDirectory::new("bridge-session-composition-v3");
        ensure_private_directory(temporary.path()).expect("private root");
        let lock = InstanceLock::acquire(temporary.path()).expect("acquire lock");
        let session = JournalSession::start(
            peer.credential(),
            temporary.path().to_path_buf(),
            temporary.path().to_path_buf(),
            lock.identity().clone(),
        )
        .await
        .expect("linked-device session");
        peer.enqueue_response(200, br#"{"days":{}}"#.to_vec());
        session
            .journal()
            .ingest_manifest(DEFAULT_SOURCE)
            .await
            .expect("manifest");
        assert_eq!(
            peer.requests()[0].path_without_query(),
            INGEST_MANIFEST_PATH
        );
        assert_eq!(
            peer.requests()[0].query_param("source"),
            Some(DEFAULT_SOURCE)
        );
        session.shutdown().await.expect("shutdown");
        peer.shutdown().await;
    });
}

#[test]
fn v3_routes_refuse_unconfined_day_values() {
    runtime().block_on(async {
        let peer = PrivateLinkPeer::start().await;
        let temporary = TestDirectory::new("bridge-route-confinement-v3");
        ensure_private_directory(temporary.path()).expect("private root");
        let lock = InstanceLock::acquire(temporary.path()).expect("acquire lock");
        let session = JournalSession::start(
            peer.credential(),
            temporary.path().to_path_buf(),
            temporary.path().to_path_buf(),
            lock.identity().clone(),
        )
        .await
        .expect("session");
        for path in [
            &format!(
                "{}?foreign",
                INGEST_MANIFEST_DAY_PATH.replace("{day}", "20260815")
            ),
            &format!(
                "{}#foreign",
                INGEST_SEGMENTS_PATH.replace("{day}", "20260815")
            ),
        ] {
            assert!(session.journal().request(Method::GET, path).is_err());
        }
        assert!(
            session
                .journal()
                .ingest_manifest_day("20260815?foreign", DEFAULT_SOURCE)
                .await
                .is_err()
        );
        assert!(peer.requests().is_empty());
        session.shutdown().await.expect("shutdown");
        peer.shutdown().await;
    });
}

#[test]
fn slow_large_multipart_preserves_capture_on_the_production_runtime() {
    runtime().block_on(async {
        let peer = PrivateLinkPeer::start().await;
        peer.withhold_upload_credit();
        let temporary = TestDirectory::new("bridge-multipart-backpressure-v3");
        ensure_private_directory(temporary.path()).expect("private root");
        let lock = InstanceLock::acquire(temporary.path()).expect("acquire lock");
        let session = JournalSession::start(
            peer.credential(),
            temporary.path().to_path_buf(),
            temporary.path().to_path_buf(),
            lock.identity().clone(),
        )
        .await
        .expect("session");
        let capture = temporary.path().join("capture.jsonl");
        fs::write(&capture, vec![b'x'; 1024 * 1024]).expect("capture bytes");
        peer.enqueue_response(200, br#"{"status":"ok","segment":"143000_1"}"#.to_vec());
        let capture_polls = Arc::new(AtomicUsize::new(0));
        let capture_polled = Arc::new(Notify::new());
        let (observer_stop, observer_shutdown) = oneshot::channel();
        let (observer_barrier, supervisor_barrier) = shutdown_barrier();
        drop(supervisor_barrier);
        let observer = tokio::spawn(run_observer(
            Arc::new(CountingCapture {
                polls: Arc::clone(&capture_polls),
                polled: Arc::clone(&capture_polled),
            }),
            Box::new(CountingSegment),
            Arc::new(SystemClock::new(UtcOffset::UTC)) as Arc<dyn Clock>,
            Box::pin(async move {
                let _ = observer_shutdown.await;
                ShutdownEvent::Injected
            }),
            observer_barrier,
            ObserverConfig {
                capture_interval: Duration::from_millis(1),
                segment_interval: Duration::from_secs(300),
            },
        ));
        let mut upload = Box::pin(
            session
            .journal()
            .ingest_upload("20260815", "143000_1", vec![capture.clone()], DEFAULT_SOURCE),
        );
        tokio::time::timeout(Duration::from_secs(5), async {
            tokio::select! {
                biased;
                result = &mut upload => panic!("upload completed before peer credit was granted: {result:?}"),
                () = peer.wait_for_upload_stall() => {}
            }
        })
        .await
        .expect("upload did not reach the credit boundary");
        let polls_before = capture_polls.load(Ordering::SeqCst);
        tokio::time::timeout(Duration::from_secs(5), async {
            tokio::select! {
                biased;
                result = &mut upload => panic!("upload completed before capture continued: {result:?}"),
                () = wait_for_capture_poll_after(&capture_polls, &capture_polled, polls_before) => {}
            }
        })
        .await
        .expect("capture did not continue while upload was stalled");
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                peer.grant_upload_credit(
                    u32::try_from(RECOMMENDED_CHUNK).expect("frame credit fits u32"),
                );
                tokio::select! {
                    biased;
                    result = &mut upload => return result,
                    () = tokio::task::yield_now() => {}
                }
            }
        })
        .await
        .expect("upload did not finish after peer credit")
        .expect("upload after credit");
        drop(upload);
        assert_eq!(
            fs::metadata(capture).expect("capture remains").len(),
            1024 * 1024
        );
        assert!(session.journal().upload_stage_high_water_bytes() > 0);
        observer_stop.send(()).expect("stop observer");
        assert_eq!(observer.await.expect("join observer").exit_code, 0);
        session.shutdown().await.expect("shutdown");
        peer.shutdown().await;
    });
}

struct CountingCapture {
    polls: Arc<AtomicUsize>,
    polled: Arc<Notify>,
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
            self.polled.notify_one();
            Ok(Vec::new())
        })
    }
}

async fn wait_for_capture_poll_after(polls: &AtomicUsize, polled: &Notify, before: usize) {
    loop {
        if polls.load(Ordering::SeqCst) > before {
            return;
        }
        polled.notified().await;
    }
}

struct CountingSegment;

impl SegmentLifecycle for CountingSegment {
    fn process_poll(
        &mut self,
        _captures: &[CaptureResult],
        _wall_now: time::OffsetDateTime,
        _monotonic_now: Duration,
        _segment_interval: Duration,
    ) -> Result<(), ObserverOperationError> {
        Ok(())
    }

    fn shutdown(
        &mut self,
        _monotonic_now: Duration,
    ) -> Result<SegmentClose, ObserverOperationError> {
        Ok(SegmentClose::RemovedEmpty)
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .expect("runtime")
}
