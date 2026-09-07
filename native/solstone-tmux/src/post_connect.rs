// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sha2::{Digest, Sha256};
use tokio::sync::Notify;

use crate::client_metadata::run_metadata_job;
use crate::clock::{Clock, SystemClock};
use crate::config::system_hostname;
use crate::journal::{JournalClient, OPTIONAL_JOB_TIMEOUT};
use crate::journal_version::VersionRefreshState;
use crate::paths::PlatformKind;
use crate::private_link::PrivateLinkOpener;
use crate::relay_access::run_relay_access_job;
use crate::sync::CredentialStore;

pub fn compute_pairing_generation(client_cert_pem: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(client_cert_pem.as_bytes());
    hasher.finalize().into()
}

/// One lane may run its first pass and one coalesced pass. A burst is shared
/// so a carrier opened by one optional job cannot manufacture work in another.
#[derive(Default)]
struct LaneBurst {
    pass_count: u8,
    in_flight: bool,
    follow_up_requested: bool,
    attempt_id: u64,
}

#[derive(Default)]
struct BurstState {
    active: bool,
    burst_id: u64,
    metadata: LaneBurst,
    access: LaneBurst,
    // A description arriving after pass two is retained as a marker. The next
    // real trigger samples it from the hostname source.
    deferred_metadata: bool,
}

pub struct PostConnectCoordinator {
    pairing_generation: [u8; 32],
    alive: AtomicBool,
    attempt_counter: AtomicU64,
    burst: Mutex<BurstState>,
    journal_client: Arc<JournalClient>,
    store: Arc<CredentialStore>,
    opener: Arc<PrivateLinkOpener>,
    version_refresh: VersionRefreshState,
    hostname_source: Arc<dyn Fn() -> Option<String> + Send + Sync>,
    platform: PlatformKind,
    clock: Arc<dyn Clock>,
    timeout: Duration,
    quiesced: Notify,
}

impl PostConnectCoordinator {
    pub fn new(
        pairing_generation: [u8; 32],
        journal_client: Arc<JournalClient>,
        store: Arc<CredentialStore>,
        opener: Arc<PrivateLinkOpener>,
        version_refresh: VersionRefreshState,
    ) -> Arc<Self> {
        let platform = if cfg!(target_os = "macos") {
            PlatformKind::Macos
        } else {
            PlatformKind::Linux
        };
        Self::new_with_options(
            pairing_generation,
            journal_client,
            store,
            opener,
            version_refresh,
            Arc::new(|| system_hostname().ok()),
            platform,
            Arc::new(SystemClock::new(time::UtcOffset::UTC)),
            OPTIONAL_JOB_TIMEOUT,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_with_options(
        pairing_generation: [u8; 32],
        journal_client: Arc<JournalClient>,
        store: Arc<CredentialStore>,
        opener: Arc<PrivateLinkOpener>,
        version_refresh: VersionRefreshState,
        hostname_source: Arc<dyn Fn() -> Option<String> + Send + Sync>,
        platform: PlatformKind,
        clock: Arc<dyn Clock>,
        timeout: Duration,
    ) -> Arc<Self> {
        Arc::new(Self {
            pairing_generation,
            alive: AtomicBool::new(true),
            attempt_counter: AtomicU64::new(0),
            burst: Mutex::new(BurstState::default()),
            journal_client,
            store,
            opener,
            version_refresh,
            hostname_source,
            platform,
            clock,
            timeout,
            quiesced: Notify::new(),
        })
    }

    pub fn pairing_generation(&self) -> [u8; 32] {
        self.pairing_generation
    }

    pub fn shutdown(&self) {
        self.alive.store(false, Ordering::SeqCst);
        self.opener.retire();
        self.store.invalidate();
        self.version_refresh.invalidate();
        let mut burst = self.burst.lock().unwrap_or_else(|e| e.into_inner());
        burst.metadata.follow_up_requested = false;
        burst.access.follow_up_requested = false;
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    pub async fn wait_for_quiescence(&self, timeout: Duration) {
        tokio::time::timeout(timeout, async {
            loop {
                let notified = self.quiesced.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                let quiescent = {
                    let burst = self.burst.lock().unwrap_or_else(|e| e.into_inner());
                    !burst.active
                };
                if quiescent {
                    return;
                }
                notified.await;
            }
        })
        .await
        .expect("post-connect burst did not quiesce");
    }

    /// A bootstrap/attach/manual event is external. During a burst it only
    /// asks each lane for its single legal follow-up; after quiescence it starts
    /// a fresh two-lane burst.
    pub fn trigger_external(self: &Arc<Self>) {
        if !self.is_alive() {
            return;
        }
        let (metadata, access) = {
            let mut burst = self.burst.lock().unwrap_or_else(|e| e.into_inner());
            if !burst.active {
                burst.active = true;
                burst.burst_id = burst.burst_id.wrapping_add(1);
                burst.deferred_metadata = false;
                burst.metadata = LaneBurst::default();
                burst.access = LaneBurst::default();
                (
                    Self::start_lane(&mut burst.metadata, &self.attempt_counter),
                    Self::start_lane(&mut burst.access, &self.attempt_counter),
                )
            } else {
                (
                    Self::request_follow_up(&mut burst.metadata, &self.attempt_counter),
                    Self::request_follow_up(&mut burst.access, &self.attempt_counter),
                )
            }
        };
        if let Some(attempt) = metadata {
            self.spawn_metadata_job(attempt);
        }
        if let Some(attempt) = access {
            self.spawn_relay_access_job(attempt);
        }
    }

    pub fn trigger_post_bootstrap(self: &Arc<Self>) {
        self.trigger_external();
    }

    /// A bridge dial is job-induced while a burst is active. Once quiescent it
    /// is an external reconnect and starts exactly one new burst.
    pub(crate) fn burst_is_active(&self) -> bool {
        self.burst.lock().unwrap_or_else(|e| e.into_inner()).active
    }

    pub(crate) fn note_successful_dial(self: &Arc<Self>) {
        let quiescent = {
            let burst = self.burst.lock().unwrap_or_else(|e| e.into_inner());
            !burst.active
        };
        if quiescent {
            self.trigger_external();
        }
    }

    fn start_lane(lane: &mut LaneBurst, counter: &AtomicU64) -> Option<u64> {
        if lane.in_flight || lane.pass_count >= 2 {
            return None;
        }
        lane.pass_count += 1;
        lane.in_flight = true;
        lane.attempt_id = counter.fetch_add(1, Ordering::SeqCst) + 1;
        Some(lane.attempt_id)
    }

    fn request_follow_up(lane: &mut LaneBurst, counter: &AtomicU64) -> Option<u64> {
        if lane.pass_count >= 2 {
            return None;
        }
        if lane.in_flight {
            lane.follow_up_requested = true;
            return None;
        }
        Self::start_lane(lane, counter)
    }

    fn spawn_metadata_job(self: &Arc<Self>, attempt_id: u64) {
        let coordinator = Arc::clone(self);
        tokio::spawn(async move {
            let client = Arc::clone(&coordinator.journal_client);
            let refresh = coordinator.version_refresh.clone();
            let hostname_source = Arc::clone(&coordinator.hostname_source);
            // The complete lane, including publication waiting, shares one deadline.
            // A started blocking publication retains its own serialized lifetime.
            let result = run_metadata_job(
                &client,
                &coordinator.store,
                &refresh,
                move || hostname_source(),
                coordinator.platform,
                coordinator.timeout,
            )
            .await;
            coordinator.on_metadata_complete(attempt_id, result.is_ok());
        });
    }

    fn spawn_relay_access_job(self: &Arc<Self>, attempt_id: u64) {
        let coordinator = Arc::clone(self);
        tokio::spawn(async move {
            // The lane wait is bounded; queued blocking publication stays owned.
            let result = run_relay_access_job(
                &coordinator.journal_client,
                &coordinator.store,
                &coordinator.opener,
                attempt_id,
                coordinator.clock.as_ref(),
                coordinator.timeout,
            )
            .await;
            coordinator.on_access_complete(attempt_id, result.is_ok());
        });
    }

    fn on_metadata_complete(self: &Arc<Self>, attempt_id: u64, _within_timeout: bool) {
        let (next, quiesced) = {
            let mut burst = self.burst.lock().unwrap_or_else(|e| e.into_inner());
            let deferred = {
                let lane = &mut burst.metadata;
                if lane.attempt_id != attempt_id {
                    return;
                }
                lane.in_flight = false;
                let deferred = lane.follow_up_requested && lane.pass_count >= 2;
                let next = if lane.follow_up_requested && lane.pass_count < 2 && self.is_alive() {
                    lane.follow_up_requested = false;
                    Self::start_lane(lane, &self.attempt_counter)
                } else {
                    lane.follow_up_requested = false;
                    None
                };
                (next, deferred)
            };
            let (next, deferred) = deferred;
            if deferred {
                burst.deferred_metadata = true;
            }
            let quiesced = Self::finish_if_quiescent(&mut burst);
            (next, quiesced)
        };
        if quiesced {
            self.quiesced.notify_waiters();
        }
        if let Some(attempt) = next {
            self.spawn_metadata_job(attempt);
        }
    }

    fn on_access_complete(self: &Arc<Self>, attempt_id: u64, _within_timeout: bool) {
        let (next, quiesced) = {
            let mut burst = self.burst.lock().unwrap_or_else(|e| e.into_inner());
            let lane = &mut burst.access;
            if lane.attempt_id != attempt_id {
                return;
            }
            lane.in_flight = false;
            let next = if lane.follow_up_requested && lane.pass_count < 2 && self.is_alive() {
                lane.follow_up_requested = false;
                Self::start_lane(lane, &self.attempt_counter)
            } else {
                lane.follow_up_requested = false;
                None
            };
            let quiesced = Self::finish_if_quiescent(&mut burst);
            (next, quiesced)
        };
        if quiesced {
            self.quiesced.notify_waiters();
        }
        if let Some(attempt) = next {
            self.spawn_relay_access_job(attempt);
        }
    }

    fn finish_if_quiescent(burst: &mut BurstState) -> bool {
        if !burst.metadata.in_flight
            && !burst.access.in_flight
            && !burst.metadata.follow_up_requested
            && !burst.access.follow_up_requested
        {
            burst.active = false;
            true
        } else {
            false
        }
    }
}
