// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

mod support;

use std::fs;
use std::net::IpAddr;

use reqwest::{Method, StatusCode};
use solstone_tmux::health::DiagnosticCode;
use solstone_tmux::paths::ensure_private_directory;
use solstone_tmux::private_link::{
    OBSERVER_HEADER_NAME, PROTOCOL_VERSION, PROTOCOL_VERSION_HEADER_NAME,
};
use solstone_tmux::sync::JournalSession;
use support::TestDirectory;
use support::private_link_peer::PrivateLinkPeer;

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
        let session = JournalSession::start(peer.credential(), temporary.path().to_path_buf())
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
fn journal_response_body_limit_applies_to_success_and_error_responses() {
    runtime().block_on(async {
        let peer = PrivateLinkPeer::start().await;
        let temporary = TestDirectory::new("bridge-response-limit-v3");
        ensure_private_directory(temporary.path()).expect("private root");
        let session = JournalSession::start(peer.credential(), temporary.path().to_path_buf())
            .await
            .expect("session");
        let oversized = vec![b'x'; 4 * 1024 * 1024 + 1];
        peer.enqueue_response(200, oversized.clone());
        let error = session
            .journal()
            .ingest_manifest_day("20260815")
            .await
            .expect_err("oversized successful day manifest accepted");
        assert_eq!(error.diagnostic(), DiagnosticCode::JournalResponseTooLarge);
        peer.enqueue_response(403, oversized);
        let error = session
            .journal()
            .ingest_manifest()
            .await
            .expect_err("oversized error manifest accepted");
        assert_eq!(error.diagnostic(), DiagnosticCode::JournalResponseTooLarge);
        session.shutdown().await.expect("shutdown");
        peer.shutdown().await;
    });
}

#[test]
fn bridge_registration_composes_on_the_production_runtime_shape() {
    runtime().block_on(async {
        let peer = PrivateLinkPeer::start().await;
        let temporary = TestDirectory::new("bridge-session-composition-v3");
        ensure_private_directory(temporary.path()).expect("private root");
        let session = JournalSession::start(peer.credential(), temporary.path().to_path_buf())
            .await
            .expect("linked-device session");
        peer.enqueue_response(200, br#"{"days":{}}"#.to_vec());
        session.journal().ingest_manifest().await.expect("manifest");
        assert_eq!(peer.requests()[0].path(), "/app/devices/ingest/manifest");
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
        let session = JournalSession::start(peer.credential(), temporary.path().to_path_buf())
            .await
            .expect("session");
        for path in [
            "/app/devices/ingest/manifest/20260815?foreign",
            "/app/devices/ingest/segments/20260815#foreign",
        ] {
            assert!(session.journal().request(Method::GET, path).is_err());
        }
        assert!(
            session
                .journal()
                .ingest_manifest_day("20260815?foreign")
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
        let temporary = TestDirectory::new("bridge-multipart-backpressure-v3");
        ensure_private_directory(temporary.path()).expect("private root");
        let session = JournalSession::start(peer.credential(), temporary.path().to_path_buf())
            .await
            .expect("session");
        let capture = temporary.path().join("capture.jsonl");
        fs::write(&capture, vec![b'x'; 1024 * 1024]).expect("capture bytes");
        peer.enqueue_response(200, br#"{"status":"ok","segment":"143000_1"}"#.to_vec());
        session
            .journal()
            .ingest_upload("20260815", "143000_1", vec![capture.clone()])
            .await
            .expect("upload");
        assert_eq!(
            fs::metadata(capture).expect("capture remains").len(),
            1024 * 1024
        );
        assert!(session.journal().upload_stage_high_water_bytes() > 0);
        session.shutdown().await.expect("shutdown");
        peer.shutdown().await;
    });
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .expect("runtime")
}
