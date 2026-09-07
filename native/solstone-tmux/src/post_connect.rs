// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::client_metadata::run_metadata_job;
use crate::clock::{Clock, SystemClock};
use crate::config::system_hostname;
use crate::journal::{JournalClient, OPTIONAL_JOB_TIMEOUT};
use crate::journal_version::{PostConnectTrigger, VersionRefreshState};
use crate::paths::PlatformKind;
use crate::private_link::PrivateLinkOpener;
use crate::relay_access::run_relay_access_job;
use crate::sync::CredentialStore;

pub fn compute_pairing_generation(client_cert_pem: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(client_cert_pem.as_bytes());
    hasher.finalize().into()
}

#[derive(Default)]
struct JobSlot {
    in_flight: bool,
    pending: bool,
    current_job_id: u64,
}

pub struct PostConnectCoordinator {
    pairing_generation: [u8; 32],
    alive: AtomicBool,
    job_counter: AtomicU64,
    metadata_slot: Mutex<JobSlot>,
    access_slot: Mutex<JobSlot>,
    journal_client: Arc<JournalClient>,
    store: Arc<CredentialStore>,
    opener: Arc<PrivateLinkOpener>,
    version_refresh: VersionRefreshState,
    hostname_source: Arc<dyn Fn() -> Option<String> + Send + Sync>,
    platform: PlatformKind,
    clock: Arc<dyn Clock>,
    timeout: Duration,
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
            job_counter: AtomicU64::new(0),
            metadata_slot: Mutex::new(JobSlot::default()),
            access_slot: Mutex::new(JobSlot::default()),
            journal_client,
            store,
            opener,
            version_refresh,
            hostname_source,
            platform,
            clock,
            timeout,
        })
    }

    pub fn pairing_generation(&self) -> [u8; 32] {
        self.pairing_generation
    }

    pub fn shutdown(&self) {
        self.alive.store(false, Ordering::SeqCst);
        self.store.invalidate();
        let mut meta = self.metadata_slot.lock().unwrap_or_else(|e| e.into_inner());
        meta.pending = false;
        let mut access = self.access_slot.lock().unwrap_or_else(|e| e.into_inner());
        access.pending = false;
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    pub fn trigger_metadata(self: &Arc<Self>) {
        if !self.is_alive() {
            return;
        }
        let (should_spawn, job_id) = {
            let mut slot = self.metadata_slot.lock().unwrap_or_else(|e| e.into_inner());
            if slot.in_flight {
                slot.pending = true;
                (false, 0)
            } else {
                slot.in_flight = true;
                slot.pending = false;
                let job_id = self.job_counter.fetch_add(1, Ordering::SeqCst) + 1;
                slot.current_job_id = job_id;
                (true, job_id)
            }
        };
        if should_spawn {
            self.spawn_metadata_job(job_id);
        }
    }

    pub fn trigger_relay_access(self: &Arc<Self>) {
        if !self.is_alive() {
            return;
        }
        let (should_spawn, job_id) = {
            let mut slot = self.access_slot.lock().unwrap_or_else(|e| e.into_inner());
            if slot.in_flight {
                slot.pending = true;
                (false, 0)
            } else {
                slot.in_flight = true;
                slot.pending = false;
                let job_id = self.job_counter.fetch_add(1, Ordering::SeqCst) + 1;
                slot.current_job_id = job_id;
                (true, job_id)
            }
        };
        if should_spawn {
            self.spawn_relay_access_job(job_id);
        }
    }

    fn spawn_metadata_job(self: &Arc<Self>, job_id: u64) {
        let coordinator = Arc::clone(self);
        tokio::spawn(async move {
            let client = Arc::clone(&coordinator.journal_client);
            let refresh = coordinator.version_refresh.clone();
            let hostname_source = Arc::clone(&coordinator.hostname_source);
            let platform = coordinator.platform;
            let timeout = coordinator.timeout;

            let result = tokio::time::timeout(
                timeout,
                run_metadata_job(
                    &client,
                    &refresh,
                    move || hostname_source(),
                    platform,
                    timeout,
                ),
            )
            .await;

            coordinator.on_metadata_complete(job_id, result.is_ok());
        });
    }

    fn on_metadata_complete(self: &Arc<Self>, job_id: u64, _completed_within_timeout: bool) {
        if !self.is_alive() {
            return;
        }
        let next_job_id = {
            let mut slot = self.metadata_slot.lock().unwrap_or_else(|e| e.into_inner());
            if slot.current_job_id != job_id {
                return;
            }
            if slot.pending {
                slot.pending = false;
                slot.in_flight = true;
                let new_id = self.job_counter.fetch_add(1, Ordering::SeqCst) + 1;
                slot.current_job_id = new_id;
                Some(new_id)
            } else {
                slot.in_flight = false;
                None
            }
        };
        if let Some(new_id) = next_job_id {
            self.spawn_metadata_job(new_id);
        }
    }

    fn spawn_relay_access_job(self: &Arc<Self>, job_id: u64) {
        let coordinator = Arc::clone(self);
        tokio::spawn(async move {
            let client = Arc::clone(&coordinator.journal_client);
            let store = Arc::clone(&coordinator.store);
            let opener = Arc::clone(&coordinator.opener);
            let now = coordinator.clock.wall_now().unix_timestamp();
            let timeout = coordinator.timeout;

            let result = tokio::time::timeout(
                timeout,
                run_relay_access_job(&client, &store, &opener, now, timeout),
            )
            .await;

            coordinator.on_relay_access_complete(job_id, result.is_ok());
        });
    }

    fn on_relay_access_complete(self: &Arc<Self>, job_id: u64, _completed_within_timeout: bool) {
        if !self.is_alive() {
            return;
        }
        let next_job_id = {
            let mut slot = self.access_slot.lock().unwrap_or_else(|e| e.into_inner());
            if slot.current_job_id != job_id {
                return;
            }
            if slot.pending {
                slot.pending = false;
                slot.in_flight = true;
                let new_id = self.job_counter.fetch_add(1, Ordering::SeqCst) + 1;
                slot.current_job_id = new_id;
                Some(new_id)
            } else {
                slot.in_flight = false;
                None
            }
        };
        if let Some(new_id) = next_job_id {
            self.spawn_relay_access_job(new_id);
        }
    }
}

impl PostConnectTrigger for Arc<PostConnectCoordinator> {
    fn trigger_all(&self) {
        self.trigger_metadata();
        self.trigger_relay_access();
    }
}
