// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs;
use std::future::Future;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use spl_transport::client::TokenPersistHook;
use spl_transport::credential::Credential;
use spl_transport::journal_bridge::JournalBridgeTerminalReason;
use time::{Date, Month};
use tokio::sync::{Mutex as AsyncMutex, Notify, OwnedSemaphorePermit, Semaphore, oneshot, watch};
use tokio::time::Instant;

use serde::{Deserialize, Serialize};

use crate::clock::Clock;
use crate::config::{RuntimeConfig, default_stream};
use crate::health::{DiagnosticCode, HealthWriter, SyncFacts};
use crate::instance_lock::RunIdentity;
use crate::journal::{
    AcknowledgedFile, JournalClient, JournalError, JournalReasonCode, LocalFile,
    OPTIONAL_JOB_TIMEOUT, Receipt, ReceiptFault, UploadResult, assess_receipt, inventory_files,
    stream_sha256_hex,
};
use crate::journal_version::{VersionRefreshState, hex_encode};
use crate::name::{DerivedName, derive_component};
use crate::paths::{self, PlatformKind};
use crate::private_link::{
    PrivateLinkBridge, PrivateLinkOpener, load_credential, persist_credential,
};
use crate::segment::SegmentClose;
use crate::storage::{
    atomic_write_bytes, open_directory_readonly, open_regular_readonly, open_regular_readonly_at,
    sync_directory,
};

const RETRY_DELAYS: [Duration; 4] = [
    Duration::from_secs(5),
    Duration::from_secs(30),
    Duration::from_secs(120),
    Duration::from_secs(300),
];
const PERIODIC_SYNC_INTERVAL: Duration = Duration::from_secs(60);
const CANDIDATES_PER_BATCH: usize = 8;
const HEALTH_REFRESH_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SyncActivity {
    Idle,
    Working,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SyncInstrumentationSnapshot {
    pub candidate_scans: usize,
    pub batches: usize,
    pub batch_yields: usize,
    /// Points at which sweep progress was published to the health file.
    pub health_writes: usize,
    pub hashed_files: usize,
    pub hashed_bytes: u64,
}

#[derive(Clone, Default)]
pub struct SyncInstrumentation {
    candidate_scans: Arc<AtomicUsize>,
    batches: Arc<AtomicUsize>,
    batch_yields: Arc<AtomicUsize>,
    health_writes: Arc<AtomicUsize>,
    hashed_files: Arc<AtomicUsize>,
    hashed_bytes: Arc<AtomicUsize>,
}

impl SyncInstrumentation {
    pub fn snapshot(&self) -> SyncInstrumentationSnapshot {
        SyncInstrumentationSnapshot {
            candidate_scans: self.candidate_scans.load(Ordering::Relaxed),
            batches: self.batches.load(Ordering::Relaxed),
            batch_yields: self.batch_yields.load(Ordering::Relaxed),
            health_writes: self.health_writes.load(Ordering::Relaxed),
            hashed_files: self.hashed_files.load(Ordering::Relaxed),
            hashed_bytes: self.hashed_bytes.load(Ordering::Relaxed) as u64,
        }
    }

    pub(crate) fn candidate_scan(&self) {
        self.candidate_scans.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn batch(&self) {
        self.batches.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn batch_yield(&self) {
        self.batch_yields.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn health_write(&self) {
        self.health_writes.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn hashed_file(&self, bytes: u64) {
        self.hashed_files.fetch_add(1, Ordering::Relaxed);
        self.hashed_bytes.fetch_add(
            usize::try_from(bytes).unwrap_or(usize::MAX),
            Ordering::Relaxed,
        );
    }
}

use crate::post_connect::{PostConnectCoordinator, compute_pairing_generation};

#[derive(Clone, Eq, PartialEq)]
struct TokenUpdate {
    id: u64,
    expected_mutation_gen: u64,
}

/// Ownership captured before an access GET begins.  A response may publish
/// only while this exact accepted credential revision remains current.
#[derive(Clone)]
pub struct AccessAttempt {
    pairing_generation: [u8; 32],
    lane_attempt_id: u64,
    owner_attempt_id: u64,
    access_revision: u64,
    relay_origin: Option<String>,
    device_token: Option<String>,
    deadline: Instant,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReadyPublication {
    Confirmed,
    Uncertain,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CredentialPersistenceIssue {
    Failed,
    Uncertain,
}

struct CredentialStoreState {
    credential: Credential,
    mutation_generation: u64,
    pending: Option<TokenUpdate>,
    durable_clear_pending_gen: Option<u64>,
    shutdown: bool,
    current_access_attempt_id: u64,
    persistence_issue: Option<CredentialPersistenceIssue>,
}

pub struct CredentialStore {
    config_root: PathBuf,
    pairing_generation: [u8; 32],
    state: Mutex<CredentialStoreState>,
    next_pending_id: AtomicU64,
    next_access_attempt_id: AtomicU64,
    // Each optional publication is chained behind this task. Dropping its
    // waiter cannot drop the blocking write already owned by the store.
    owner_tail: Mutex<Option<tokio::task::JoinHandle<()>>>,
    #[cfg(test)]
    publication_queued: Notify,
}

impl CredentialStore {
    pub fn new(
        config_root: PathBuf,
        credential: Credential,
        pairing_generation: [u8; 32],
    ) -> (Arc<Self>, TokenPersistHook) {
        let store = Arc::new(Self {
            config_root,
            pairing_generation,
            state: Mutex::new(CredentialStoreState {
                credential,
                mutation_generation: 0,
                pending: None,
                durable_clear_pending_gen: None,
                shutdown: false,
                current_access_attempt_id: 0,
                persistence_issue: None,
            }),
            next_pending_id: AtomicU64::new(0),
            next_access_attempt_id: AtomicU64::new(0),
            owner_tail: Mutex::new(None),
            #[cfg(test)]
            publication_queued: Notify::new(),
        });
        let hook = store.token_persist_hook(0);
        (store, hook)
    }

    pub fn pairing_generation(&self) -> [u8; 32] {
        self.pairing_generation
    }

    pub fn instance_id(&self) -> String {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.credential.instance_id.clone()
    }

    pub fn live_credential(&self) -> Credential {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.credential.clone()
    }

    pub fn capture_access_attempt(&self, lane_attempt_id: u64) -> AccessAttempt {
        self.capture_access_attempt_until(lane_attempt_id, Instant::now() + OPTIONAL_JOB_TIMEOUT)
    }

    pub(crate) fn capture_access_attempt_until(
        &self,
        lane_attempt_id: u64,
        deadline: Instant,
    ) -> AccessAttempt {
        let owner_attempt_id = self.next_access_attempt_id.fetch_add(1, Ordering::SeqCst) + 1;
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.current_access_attempt_id = owner_attempt_id;
        AccessAttempt {
            pairing_generation: self.pairing_generation,
            lane_attempt_id,
            owner_attempt_id,
            access_revision: state.mutation_generation,
            relay_origin: state.credential.relay_origin.clone(),
            device_token: state.credential.device_token.clone(),
            deadline,
        }
    }

    fn access_attempt_is_current(
        &self,
        state: &CredentialStoreState,
        attempt: &AccessAttempt,
    ) -> bool {
        Instant::now() < attempt.deadline
            && attempt.pairing_generation == self.pairing_generation
            && attempt.lane_attempt_id != 0
            && state.current_access_attempt_id == attempt.owner_attempt_id
            && !state.shutdown
            && state.mutation_generation == attempt.access_revision
            && state.credential.relay_origin == attempt.relay_origin
            && state.credential.device_token == attempt.device_token
    }

    pub fn invalidate(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.shutdown = true;
    }

    pub fn token_persist_hook(self: &Arc<Self>, for_mutation_gen: u64) -> TokenPersistHook {
        let weak = Arc::downgrade(self);
        let expected_pairing_gen = self.pairing_generation;
        Arc::new(move |token, expires_at| {
            let Some(store) = weak.upgrade() else {
                return;
            };
            if store.pairing_generation != expected_pairing_gen {
                return;
            }
            store.enqueue_token_refresh(for_mutation_gen, token.to_owned(), expires_at);
        })
    }

    fn enqueue_owned<T, F>(self: &Arc<Self>, action: F) -> oneshot::Receiver<T>
    where
        T: Send + 'static,
        F: Future<Output = T> + Send + 'static,
    {
        let mut owner_tail = self.owner_tail.lock().unwrap_or_else(|e| e.into_inner());
        let previous = owner_tail.take();
        let (sender, receiver) = oneshot::channel();
        let handle = tokio::spawn(async move {
            if let Some(previous) = previous {
                let _ = previous.await;
            }
            let _ = sender.send(action.await);
        });
        *owner_tail = Some(handle);
        receiver
    }

    /// A transport callback has no async return path. It leases the accepted
    /// adapter generation and queues publication; it never mutates state in
    /// the callback itself.
    pub fn enqueue_token_refresh(
        self: &Arc<Self>,
        expected_mutation_gen: u64,
        token: String,
        expires_at: i64,
    ) {
        let store = Arc::clone(self);
        drop(self.enqueue_owned(async move {
            #[cfg(test)]
            let receipt = Arc::clone(&store);
            let worker = tokio::task::spawn_blocking(move || {
                let mut state = store.state.lock().unwrap_or_else(|e| e.into_inner());
                if state.shutdown || state.mutation_generation != expected_mutation_gen {
                    return;
                }
                state.credential.device_token = Some(token);
                state.credential.device_token_expires_at = Some(expires_at);
                let id = store.next_pending_id.fetch_add(1, Ordering::SeqCst) + 1;
                state.pending = Some(TokenUpdate {
                    id,
                    expected_mutation_gen,
                });
                let candidate = state.credential.clone();
                drop(state);
                let persisted = persist_credential(&store.config_root, &candidate).is_ok();
                let mut state = store.state.lock().unwrap_or_else(|e| e.into_inner());
                if persisted {
                    state.pending = None;
                    state.persistence_issue = None;
                } else {
                    Self::record_persistence_issue(&mut state, CredentialPersistenceIssue::Failed);
                }
            });
            #[cfg(test)]
            receipt.publication_queued.notify_one();
            let _ = worker.await;
        }));
    }

    pub fn persistence_issue(&self) -> Option<CredentialPersistenceIssue> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .persistence_issue
    }

    fn record_persistence_issue(
        state: &mut CredentialStoreState,
        issue: CredentialPersistenceIssue,
    ) {
        if state.persistence_issue != Some(issue) {
            crate::health::emit_diagnostic(DiagnosticCode::PrivateStateIo);
        }
        state.persistence_issue = Some(issue);
    }

    /// Persist and install Ready as a single ordered owner operation.  The
    /// receiver may be dropped by a caller timeout; the queued write and the
    /// corresponding opener replacement continue to completion.
    pub async fn submit_ready(
        self: &Arc<Self>,
        opener: Arc<PrivateLinkOpener>,
        attempt: AccessAttempt,
        relay_origin: String,
        device_token: String,
        expires_at: i64,
    ) -> Result<ReadyPublication, ()> {
        let store = Arc::clone(self);
        self.enqueue_owned(async move {
            #[cfg(test)]
            let receipt = Arc::clone(&store);
            let worker = tokio::task::spawn_blocking(move || {
                // Claim inside the blocking worker, not before it queues. The
                // owner chain retains this executing publication through acceptance;
                // state readers and shutdown never wait on a filesystem lock.
                let state = store.state.lock().unwrap_or_else(|e| e.into_inner());
                if !store.access_attempt_is_current(&state, &attempt) {
                    return Err(());
                }
                let mut updated = state.credential.clone();
                updated.relay_origin = Some(relay_origin);
                updated.device_token = Some(device_token);
                updated.device_token_expires_at = Some(expires_at);
                let next_revision = state.mutation_generation.wrapping_add(1);
                let hook = store.token_persist_hook(next_revision);
                let transport = if updated.endpoints.is_empty() {
                    spl_transport::client::TransportClient::new_relay_only(
                        updated.clone(),
                        Some(hook),
                    )
                } else {
                    spl_transport::client::TransportClient::new(updated.clone(), Some(hook))
                }
                .map_err(|_| ())?;
                drop(state);
                let confirmed = persist_credential(&store.config_root, &updated).is_ok();
                if !confirmed {
                    let matches = load_credential(&store.config_root)
                        .ok()
                        .flatten()
                        .is_some_and(|loaded| {
                            loaded.relay_origin == updated.relay_origin
                                && loaded.device_token == updated.device_token
                                && loaded.device_token_expires_at == updated.device_token_expires_at
                                && loaded.client_cert_pem == updated.client_cert_pem
                        });
                    if !matches {
                        let mut state = store.state.lock().unwrap_or_else(|e| e.into_inner());
                        Self::record_persistence_issue(
                            &mut state,
                            CredentialPersistenceIssue::Failed,
                        );
                        return Err(());
                    }
                }
                let mut state = store.state.lock().unwrap_or_else(|e| e.into_inner());
                state.credential = updated.clone();
                state.mutation_generation = next_revision;
                state.durable_clear_pending_gen = None;
                state.pending = if confirmed {
                    None
                } else {
                    Some(TokenUpdate {
                        id: store.next_pending_id.fetch_add(1, Ordering::SeqCst) + 1,
                        expected_mutation_gen: next_revision,
                    })
                };
                if confirmed {
                    state.persistence_issue = None;
                } else {
                    Self::record_persistence_issue(
                        &mut state,
                        CredentialPersistenceIssue::Uncertain,
                    );
                }
                opener.install_transport(Arc::new(transport), updated, next_revision);
                Ok(if confirmed {
                    ReadyPublication::Confirmed
                } else {
                    ReadyPublication::Uncertain
                })
            });
            #[cfg(test)]
            receipt.publication_queued.notify_one();
            worker.await.unwrap_or(Err(()))
        })
        .await
        .unwrap_or(Err(()))
    }

    /// Ordered disable is the sole route used by relay access. It changes the
    /// live opener before attempting durability, so a failed disk write cannot
    /// leave a relay-only credential usable in this process.
    pub async fn submit_disable(
        self: &Arc<Self>,
        opener: Arc<PrivateLinkOpener>,
        attempt: AccessAttempt,
    ) -> Result<(), ()> {
        let store = Arc::clone(self);
        self.enqueue_owned(async move {
            let (direct, revision) = {
                let mut state = store.state.lock().unwrap_or_else(|e| e.into_inner());
                if !store.access_attempt_is_current(&state, &attempt) {
                    return Err(());
                }
                let mut direct = state.credential.clone();
                direct.relay_origin = None;
                direct.device_token = None;
                direct.device_token_expires_at = None;
                state.credential = direct.clone();
                state.pending = None;
                state.mutation_generation = state.mutation_generation.wrapping_add(1);
                let revision = state.mutation_generation;
                state.durable_clear_pending_gen = Some(revision);
                (direct, revision)
            };
            opener.install_disabled(direct.clone(), revision);
            if !direct.endpoints.is_empty() {
                let hook = store.token_persist_hook(revision);
                let transport =
                    spl_transport::client::TransportClient::new(direct.clone(), Some(hook))
                        .map_err(|_| ())?;
                opener.install_transport(Arc::new(transport), direct.clone(), revision);
            }
            let config_root = store.config_root.clone();
            let persisted =
                tokio::task::spawn_blocking(move || persist_credential(&config_root, &direct))
                    .await
                    .ok()
                    .and_then(Result::ok)
                    .is_some();
            if persisted {
                let mut state = store.state.lock().unwrap_or_else(|e| e.into_inner());
                if state.mutation_generation == revision
                    && state.durable_clear_pending_gen == Some(revision)
                {
                    state.durable_clear_pending_gen = None;
                    state.persistence_issue = None;
                }
                Ok(())
            } else {
                let mut state = store.state.lock().unwrap_or_else(|e| e.into_inner());
                Self::record_persistence_issue(&mut state, CredentialPersistenceIssue::Failed);
                Err(())
            }
        })
        .await
        .unwrap_or(Err(()))
    }

    /// Serialize journal-version publication with credential mutations while
    /// keeping its separate file and identity checks in VersionRefreshState.
    pub async fn publish_journal_info(
        self: &Arc<Self>,
        refresh: crate::journal_version::VersionRefreshState,
        metadata_attempt: u64,
        name: Option<Option<String>>,
        version: String,
    ) -> bool {
        let store = Arc::clone(self);
        self.enqueue_owned(async move {
            if !refresh.metadata_attempt_is_current(metadata_attempt) {
                return false;
            }
            let worker_store = Arc::clone(&store);
            let persisted = tokio::task::spawn_blocking(move || {
                let state = worker_store.state.lock().unwrap_or_else(|e| e.into_inner());
                if state.shutdown {
                    return false;
                }
                let certificate = state.credential.client_cert_pem.clone();
                drop(state);
                if !matches!(load_credential(&worker_store.config_root), Ok(Some(credential))
                    if credential.client_cert_pem == certificate)
                {
                    return false;
                }
                refresh.apply_validated_journal_info_for_attempt(
                    metadata_attempt,
                    name.as_ref().map(|name| name.as_deref()),
                    &version,
                )
            })
            .await
            .unwrap_or(false);
            // The store's shutdown fence is checked after blocking I/O too;
            // a late worker may have written nothing, but cannot claim a
            // successful publication for a retired session.
            let state = store.state.lock().unwrap_or_else(|e| e.into_inner());
            !state.shutdown && persisted
        })
        .await
        .unwrap_or(false)
    }

    pub async fn retry_durable_clear_if_pending(self: &Arc<Self>) {
        let _ = self.persist_pending().await;
    }

    pub async fn persist_pending(self: &Arc<Self>) -> Result<(), DiagnosticCode> {
        let store = Arc::clone(self);
        self.enqueue_owned(async move { store.persist_pending_unordered().await })
            .await
            .unwrap_or(Err(DiagnosticCode::PrivateStateIo))
    }

    async fn persist_pending_unordered(self: &Arc<Self>) -> Result<(), DiagnosticCode> {
        let store = Arc::clone(self);
        tokio::task::spawn_blocking(move || {
            let state = store.state.lock().unwrap_or_else(|e| e.into_inner());
            let has_pending =
                state.pending.as_ref().is_some_and(|pending| {
                    pending.expected_mutation_gen == state.mutation_generation
                }) || state.durable_clear_pending_gen == Some(state.mutation_generation);
            if !has_pending {
                return Ok(());
            }
            // Shutdown retires network work, not this already accepted intent.
            let candidate = state.credential.clone();
            drop(state);
            let persisted = persist_credential(&store.config_root, &candidate);
            let mut state = store.state.lock().unwrap_or_else(|e| e.into_inner());
            match persisted {
                Ok(()) => {
                    state.pending = None;
                    state.durable_clear_pending_gen = None;
                    state.persistence_issue = None;
                    Ok(())
                }
                Err(error) => {
                    let issue = state
                        .persistence_issue
                        .unwrap_or(CredentialPersistenceIssue::Failed);
                    Self::record_persistence_issue(&mut state, issue);
                    Err(error)
                }
            }
        })
        .await
        .map_err(|_| DiagnosticCode::PrivateStateIo)?
    }
}

pub struct JournalSession {
    bridge: PrivateLinkBridge,
    journal: JournalClient,
    credential_store: Arc<CredentialStore>,
    coordinator: Option<Arc<PostConnectCoordinator>>,
}

impl JournalSession {
    pub async fn start(
        credential: Credential,
        config_root: PathBuf,
        refresh: VersionRefreshState,
    ) -> Result<Self, DiagnosticCode> {
        let platform = if cfg!(target_os = "macos") {
            PlatformKind::Macos
        } else {
            PlatformKind::Linux
        };
        Self::start_with(
            credential,
            config_root,
            refresh,
            OPTIONAL_JOB_TIMEOUT,
            Arc::new(|| crate::config::system_hostname().ok()),
            platform,
            Arc::new(crate::clock::SystemClock::new(time::UtcOffset::UTC)),
        )
        .await
    }

    pub async fn start_with(
        credential: Credential,
        config_root: PathBuf,
        refresh: VersionRefreshState,
        optional_timeout: Duration,
        hostname_source: Arc<dyn Fn() -> Option<String> + Send + Sync>,
        platform: PlatformKind,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, DiagnosticCode> {
        refresh.note_session_started();
        let pairing_generation = compute_pairing_generation(&credential.client_cert_pem);
        let (credential_store, hook) =
            CredentialStore::new(config_root.clone(), credential.clone(), pairing_generation);
        let bridge = PrivateLinkBridge::start(credential, Some(hook), refresh.clone()).await?;
        let journal = match JournalClient::bootstrap(&bridge).await {
            Ok(journal) => journal,
            Err(code) => {
                bridge.shutdown().await;
                return Err(code);
            }
        };
        let journal_client = Arc::new(journal.clone());
        let coordinator = PostConnectCoordinator::new_with_options(
            pairing_generation,
            Arc::clone(&journal_client),
            Arc::clone(&credential_store),
            Arc::clone(bridge.opener()),
            refresh.clone(),
            hostname_source,
            platform,
            clock,
            optional_timeout,
        );
        bridge.opener().attach_coordinator(&coordinator);
        coordinator.trigger_post_bootstrap();
        Ok(Self {
            bridge,
            journal: (*journal_client).clone(),
            credential_store,
            coordinator: Some(coordinator),
        })
    }

    pub fn trigger_post_connect(&self) {
        if let Some(ref coordinator) = self.coordinator {
            coordinator.trigger_external();
        }
    }

    pub fn credential_store(&self) -> &Arc<CredentialStore> {
        &self.credential_store
    }

    pub fn journal(&self) -> &JournalClient {
        &self.journal
    }

    pub fn opener(&self) -> &Arc<PrivateLinkOpener> {
        self.bridge.opener()
    }

    /// Test and lifecycle receipt for the bounded optional burst. It does not
    /// affect ingest work and is useful when callers need publication to have
    /// reached its terminal owner state.
    pub async fn wait_for_post_connect_quiescence(&self, timeout: Duration) {
        if let Some(coordinator) = &self.coordinator {
            coordinator.wait_for_quiescence(timeout).await;
        }
    }

    pub async fn shutdown(mut self) -> Result<(), DiagnosticCode> {
        if let Some(coordinator) = self.coordinator.take() {
            coordinator.shutdown();
        }
        // Optional credential publication is best-effort at shutdown; it must
        // never turn a clean observer shutdown into PrivateStateIo.
        let _ = self.credential_store.persist_pending().await;
        self.bridge.shutdown().await;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SegmentRemovalStage {
    FilesUnlinked,
    DirectoryUnlinked,
}

pub type SegmentRemovalObserver = Arc<dyn Fn(&SegmentCandidate, SegmentRemovalStage) + Send + Sync>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RemovalResult {
    Removed,
    Absent,
    Refused,
    Failed,
}

#[derive(Clone, Eq, PartialEq)]
pub struct FileIdentity {
    pub name: String,
    pub device: u64,
    pub inode: u64,
    pub size: u64,
    pub mtime: i64,
    pub mtime_nsec: i64,
    pub ctime: i64,
    pub ctime_nsec: i64,
}

#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SegmentCandidate {
    day: String,
    stream: String,
    segment: String,
}

impl SegmentCandidate {
    pub fn new(
        day: impl Into<String>,
        stream: impl Into<String>,
        segment: impl Into<String>,
    ) -> Self {
        Self {
            day: day.into(),
            stream: stream.into(),
            segment: segment.into(),
        }
    }

    pub fn day(&self) -> &str {
        &self.day
    }

    pub fn stream(&self) -> &str {
        &self.stream
    }

    pub fn segment(&self) -> &str {
        &self.segment
    }
}

pub struct RetentionFence {
    accepting: AsyncMutex<bool>,
    irreversible: Arc<Semaphore>,
}

impl RetentionFence {
    pub fn new() -> Self {
        Self {
            accepting: AsyncMutex::new(true),
            irreversible: Arc::new(Semaphore::new(1)),
        }
    }

    async fn begin_irreversible(&self) -> Option<OwnedSemaphorePermit> {
        let accepting = self.accepting.lock().await;
        if !*accepting {
            return None;
        }
        self.irreversible.clone().acquire_owned().await.ok()
    }

    pub async fn close_and_drain(&self) {
        let mut accepting = self.accepting.lock().await;
        *accepting = false;
        let _permit = self.irreversible.clone().acquire_owned().await;
    }
}

impl Default for RetentionFence {
    fn default() -> Self {
        Self::new()
    }
}

pub async fn delete_custodied_segment(
    captures_root: &Path,
    candidate: &SegmentCandidate,
    expected_digests: &[(&str, &str)],
    expected_identities: &[FileIdentity],
    fence: Arc<RetentionFence>,
) -> bool {
    delete_custodied_segment_with_hook(
        captures_root,
        candidate,
        expected_digests,
        expected_identities,
        None,
        fence,
    )
    .await
}

pub async fn delete_custodied_segment_with_hook(
    captures_root: &Path,
    candidate: &SegmentCandidate,
    expected_digests: &[(&str, &str)],
    expected_identities: &[FileIdentity],
    delete_hook: Option<Arc<dyn Fn(usize) + Send + Sync>>,
    fence: Arc<RetentionFence>,
) -> bool {
    let digests_map: HashMap<String, String> = expected_digests
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    let root = captures_root.to_owned();
    let target = candidate.clone();
    let expected_paths: Vec<PathBuf> = match resolve_segment_files(&root, &target) {
        ResolvedSegmentFiles::Found(paths) => paths,
        _ => return false,
    };
    let expected_identities_vec = expected_identities.to_vec();
    let Some(permit) = fence.begin_irreversible().await else {
        return false;
    };
    match tokio::task::spawn_blocking(move || {
        let _permit = permit;
        delete_revalidated_segment(
            &root,
            &target,
            &expected_paths,
            &expected_identities_vec,
            &digests_map,
            delete_hook.as_deref(),
            None,
        )
    })
    .await
    {
        Ok(RemovalResult::Removed) => true,
        Ok(RemovalResult::Failed) => {
            let cap_dir = captures_root
                .join(candidate.day())
                .join(candidate.stream())
                .join(candidate.segment());
            if cap_dir.exists() {
                eprintln!("solstone-tmux: confirmed segment was not removed");
            }
            false
        }
        _ => false,
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn delete_confirmed_segment(
    captures_root: &Path,
    ledger_root: &Path,
    candidate: &SegmentCandidate,
    expected_digests: &[(&str, &str)],
    expected_identities: &[FileIdentity],
    delete_hook: Option<Arc<dyn Fn(usize) + Send + Sync>>,
    observer: Option<SegmentRemovalObserver>,
    fence: Arc<RetentionFence>,
) -> bool {
    let digests_map: HashMap<String, String> = expected_digests
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    let root = captures_root.to_owned();
    let ledger = ledger_root.to_owned();
    let target = candidate.clone();
    let expected_paths: Vec<PathBuf> = match resolve_segment_files(&root, &target) {
        ResolvedSegmentFiles::Found(paths) => paths,
        _ => return false,
    };
    let expected_identities_vec = expected_identities.to_vec();
    let Some(permit) = fence.begin_irreversible().await else {
        return false;
    };
    match tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let obs_ref = observer.as_ref().map(|o| (&target, o));
        let res = delete_revalidated_segment(
            &root,
            &target,
            &expected_paths,
            &expected_identities_vec,
            &digests_map,
            delete_hook.as_deref(),
            obs_ref,
        );
        if res == RemovalResult::Removed || res == RemovalResult::Absent {
            remove_segment_ledger_dir(&ledger, &target);
        }
        res
    })
    .await
    {
        Ok(RemovalResult::Removed) | Ok(RemovalResult::Absent) => true,
        Ok(RemovalResult::Failed) => {
            let cap_dir = captures_root
                .join(candidate.day())
                .join(candidate.stream())
                .join(candidate.segment());
            if cap_dir.exists() {
                eprintln!("solstone-tmux: confirmed segment was not removed");
            }
            false
        }
        _ => false,
    }
}

pub async fn delete_empty_segment(
    captures_root: &Path,
    ledger_root: &Path,
    candidate: &SegmentCandidate,
    fence: Arc<RetentionFence>,
) -> bool {
    let root = captures_root.to_owned();
    let ledger = ledger_root.to_owned();
    let target = candidate.clone();
    let Some(permit) = fence.begin_irreversible().await else {
        return false;
    };
    match tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let res = remove_empty_segment_directory(&root, &target);
        if res == RemovalResult::Removed || res == RemovalResult::Absent {
            remove_segment_ledger_dir(&ledger, &target);
        }
        res
    })
    .await
    {
        Ok(RemovalResult::Removed) | Ok(RemovalResult::Absent) => true,
        Ok(RemovalResult::Failed) => {
            let cap_dir = captures_root
                .join(candidate.day())
                .join(candidate.stream())
                .join(candidate.segment());
            if cap_dir.exists() {
                eprintln!("solstone-tmux: confirmed segment was not removed");
            }
            false
        }
        _ => false,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SyncFailureClass {
    Direct,
    Relay,
    Auth,
    Timeout,
    Contract,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SyncOperationError {
    RetainCandidate {
        diagnostic: DiagnosticCode,
        answer: String,
    },
    SegmentRemoved,
    EndSweep(SyncFailureClass),
    EndSweepDiagnostic(SyncFailureClass, DiagnosticCode),
}

impl fmt::Display for SyncOperationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RetainCandidate { diagnostic, .. } | Self::EndSweepDiagnostic(_, diagnostic) => {
                formatter.write_str(diagnostic.message())
            }
            Self::SegmentRemoved => {
                formatter.write_str("segment removed on journal; unlinked locally")
            }
            Self::EndSweep(failure) => {
                formatter.write_str(diagnostic_for_failure(*failure).message())
            }
        }
    }
}

impl std::error::Error for SyncOperationError {}

pub trait SyncJournal: Send {
    fn upload<'a>(
        &'a mut self,
        candidate: &'a SegmentCandidate,
        files: Vec<PathBuf>,
        source: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<UploadResult, SyncOperationError>> + Send + 'a>>;

    fn system_status<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<(), SyncOperationError>> + Send + 'a>>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JournalIdentity {
    pub instance_id: String,
    pub ca_fp_prefix_hex: String,
    pub pairing_generation_hex: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct JournalIdentityRecord {
    instance_id: String,
    ca_fp_prefix_hex: String,
    pairing_generation_hex: String,
}

impl From<&JournalIdentity> for JournalIdentityRecord {
    fn from(id: &JournalIdentity) -> Self {
        Self {
            instance_id: id.instance_id.clone(),
            ca_fp_prefix_hex: id.ca_fp_prefix_hex.clone(),
            pairing_generation_hex: id.pairing_generation_hex.clone(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct AckRecord {
    location: String,
    day: String,
    stream: String,
    segment: String,
    stored_key: String,
    instance_id: String,
    ca_fp_prefix_hex: String,
    pairing_generation_hex: String,
    proof: String,
    files: Vec<AcknowledgedFile>,
}

fn is_zero_u32(val: &u32) -> bool {
    *val == 0
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct StateRecord {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    next_attempt_unix: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    next_attempt_interval_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    answer: Option<String>,
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    answer_count: u32,
    #[serde(default, skip_serializing)]
    terminal_keep: Option<JournalIdentityRecord>,
    #[serde(default, skip_serializing)]
    retention_recheck_unix: Option<i64>,
    #[serde(default, skip_serializing)]
    retention_recheck_interval_seconds: Option<u64>,
}

#[derive(Default)]
struct SyncMemoryFloors {
    next_attempt: HashMap<SegmentCandidate, i64>,
}

fn candidate_ledger_dir(ledger_root: &Path, candidate: &SegmentCandidate) -> PathBuf {
    ledger_root
        .join(candidate.day())
        .join(candidate.stream())
        .join(candidate.segment())
}

fn read_ack_record(
    ledger_root: &Path,
    candidate: &SegmentCandidate,
    identity: &JournalIdentity,
    local_files: &[LocalFile],
) -> Option<AckRecord> {
    let ack_path = candidate_ledger_dir(ledger_root, candidate).join("ack.json");
    let file = open_regular_readonly(&ack_path).ok()?;
    let mut reader = std::io::BufReader::new(file);
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes).ok()?;
    let ack: AckRecord = serde_json::from_slice(&bytes).ok()?;

    let expected_location = ack_path.to_str()?;
    if ack.location != expected_location {
        return None;
    }
    if ack.proof != "upload"
        || ack.day != candidate.day()
        || ack.stream != candidate.stream()
        || ack.segment != candidate.segment()
        || ack.instance_id != identity.instance_id
        || ack.ca_fp_prefix_hex != identity.ca_fp_prefix_hex
        || ack.pairing_generation_hex != identity.pairing_generation_hex
    {
        return None;
    }

    let mut seen_ack_submitted = HashSet::new();
    for f in &ack.files {
        if !seen_ack_submitted.insert(f.submitted.as_str()) {
            let _ = fs::remove_file(&ack_path);
            return None;
        }
    }

    let ack_by_submitted: HashMap<&str, &AcknowledgedFile> = ack
        .files
        .iter()
        .map(|f| (f.submitted.as_str(), f))
        .collect();

    let files_valid = local_files.iter().all(|local| {
        if let Some(ack_file) = ack_by_submitted.get(local.name.as_str()) {
            (ack_file.disposition == "written" || ack_file.disposition == "already_held")
                && ack_file.size == local.size
                && ack_file.sha256 == local.sha256
        } else {
            false
        }
    });

    if !files_valid {
        let _ = fs::remove_file(&ack_path);
        return None;
    }

    Some(ack)
}

fn write_ack_record(
    ledger_root: &Path,
    candidate: &SegmentCandidate,
    identity: &JournalIdentity,
    stored_key: &str,
    files: Vec<AcknowledgedFile>,
) -> Result<(), ()> {
    let seg_dir = candidate_ledger_dir(ledger_root, candidate);
    paths::ensure_private_directory(&seg_dir).map_err(|_| ())?;
    let ack_path = seg_dir.join("ack.json");
    let location = ack_path.to_str().ok_or(())?.to_owned();
    let record = AckRecord {
        location,
        day: candidate.day().to_owned(),
        stream: candidate.stream().to_owned(),
        segment: candidate.segment().to_owned(),
        stored_key: stored_key.to_owned(),
        instance_id: identity.instance_id.clone(),
        ca_fp_prefix_hex: identity.ca_fp_prefix_hex.clone(),
        pairing_generation_hex: identity.pairing_generation_hex.clone(),
        proof: "upload".to_owned(),
        files,
    };
    let bytes = serde_json::to_vec_pretty(&record).map_err(|_| ())?;
    atomic_write_bytes(&ack_path, &seg_dir, &bytes).map_err(|_| ())?;
    Ok(())
}

fn read_state_record(ledger_root: &Path, candidate: &SegmentCandidate) -> StateRecord {
    let state_path = candidate_ledger_dir(ledger_root, candidate).join("state.json");
    let Ok(file) = open_regular_readonly(&state_path) else {
        return StateRecord::default();
    };
    let mut reader = std::io::BufReader::new(file);
    let mut bytes = Vec::new();
    if reader.read_to_end(&mut bytes).is_err() {
        return StateRecord::default();
    }
    let Ok(record) = serde_json::from_slice::<StateRecord>(&bytes) else {
        return StateRecord::default();
    };
    if record.terminal_keep.is_some()
        || record.retention_recheck_unix.is_some()
        || record.retention_recheck_interval_seconds.is_some()
    {
        let cleaned = StateRecord {
            next_attempt_unix: record.next_attempt_unix,
            next_attempt_interval_seconds: record.next_attempt_interval_seconds,
            answer: record.answer.clone(),
            answer_count: record.answer_count,
            terminal_keep: None,
            retention_recheck_unix: None,
            retention_recheck_interval_seconds: None,
        };
        if cleaned == StateRecord::default() {
            let _ = fs::remove_file(&state_path);
        } else {
            let _ = write_state_record(ledger_root, candidate, &cleaned);
        }
        return cleaned;
    }
    record
}

fn write_state_record(
    ledger_root: &Path,
    candidate: &SegmentCandidate,
    record: &StateRecord,
) -> Result<(), ()> {
    let seg_dir = candidate_ledger_dir(ledger_root, candidate);
    paths::ensure_private_directory(&seg_dir).map_err(|_| ())?;
    let state_path = seg_dir.join("state.json");
    let bytes = serde_json::to_vec_pretty(record).map_err(|_| ())?;
    atomic_write_bytes(&state_path, &seg_dir, &bytes).map_err(|_| ())?;
    Ok(())
}

fn cleanup_hold_files(ledger_root: &Path) {
    let Ok(entries) = fs::read_dir(ledger_root) else {
        return;
    };
    for day_entry in entries.flatten() {
        let Ok(file_type) = day_entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let day_path = day_entry.path();
        let hold_path = day_path.join("hold.json");
        if open_regular_readonly(&hold_path).is_ok() {
            let _ = fs::remove_file(&hold_path);
        }
    }
}

fn remove_segment_ledger_dir(ledger_root: &Path, candidate: &SegmentCandidate) {
    let seg_dir = candidate_ledger_dir(ledger_root, candidate);
    let _ = fs::remove_file(seg_dir.join("ack.json"));
    let _ = fs::remove_file(seg_dir.join("state.json"));
    let _ = fs::remove_dir(&seg_dir);
    if let Some(stream_dir) = seg_dir.parent() {
        let _ = fs::remove_dir(stream_dir);
        if let Some(day_dir) = stream_dir.parent() {
            let _ = fs::remove_dir(day_dir);
        }
    }
}

fn prune_ledger_dirs(ledger_root: &Path, active_candidates: &HashSet<SegmentCandidate>) {
    let Ok(entries) = fs::read_dir(ledger_root) else {
        return;
    };
    for day_entry in entries.flatten() {
        let Ok(file_type) = day_entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let Ok(day_name) = day_entry.file_name().into_string() else {
            continue;
        };
        let day_path = day_entry.path();
        let Ok(stream_entries) = fs::read_dir(&day_path) else {
            continue;
        };
        for stream_entry in stream_entries.flatten() {
            let Ok(file_type) = stream_entry.file_type() else {
                continue;
            };
            if !file_type.is_dir() {
                continue;
            }
            let Ok(stream_name) = stream_entry.file_name().into_string() else {
                continue;
            };
            let stream_path = stream_entry.path();
            let Ok(segment_entries) = fs::read_dir(&stream_path) else {
                continue;
            };
            for segment_entry in segment_entries.flatten() {
                let Ok(file_type) = segment_entry.file_type() else {
                    continue;
                };
                if !file_type.is_dir() {
                    continue;
                }
                let Ok(segment_name) = segment_entry.file_name().into_string() else {
                    continue;
                };
                let candidate = SegmentCandidate::new(&day_name, &stream_name, &segment_name);
                if !active_candidates.contains(&candidate) {
                    let seg_path = segment_entry.path();
                    let _ = fs::remove_file(seg_path.join("ack.json"));
                    let _ = fs::remove_file(seg_path.join("state.json"));
                    let _ = fs::remove_dir(&seg_path);
                }
            }
            let _ = fs::remove_dir(&stream_path);
        }
        let _ = fs::remove_dir(&day_path);
    }
}

#[derive(Clone, Default)]
pub struct SyncWake {
    notify: Arc<Notify>,
    pending: Arc<AtomicBool>,
}

impl SyncWake {
    pub fn segment_closed(&self, close: &SegmentClose) {
        if matches!(close, SegmentClose::Finalized(_)) {
            self.pending.store(true, Ordering::Release);
            self.notify.notify_one();
        }
    }

    pub async fn wait(&self) {
        loop {
            let notified = self.notify.notified();
            if self.pending.swap(false, Ordering::AcqRel) {
                return;
            }
            notified.await;
        }
    }

    fn take_pending(&self) -> bool {
        self.pending.swap(false, Ordering::AcqRel)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SyncSweepSummary {
    pub attempted: usize,
    pub contacted: bool,
    pub custodied: usize,
    pub cancelled: bool,
    pub failure: Option<SyncFailureClass>,
    pub diagnostic: Option<DiagnosticCode>,
}

impl SyncSweepSummary {
    fn empty() -> Self {
        Self {
            attempted: 0,
            contacted: false,
            custodied: 0,
            cancelled: false,
            failure: None,
            diagnostic: None,
        }
    }
}

struct CachedInventory {
    names: Vec<String>,
    identities: Vec<FileIdentity>,
    inventory: Vec<LocalFile>,
}

struct Backoff {
    next_delay: usize,
    deadline: Option<Instant>,
}

impl Backoff {
    fn new() -> Self {
        Self {
            next_delay: 0,
            deadline: None,
        }
    }

    fn successful_operation(&mut self) {
        self.next_delay = 0;
        self.deadline = None;
    }

    fn failed_operation(&mut self) {
        let delay = RETRY_DELAYS[self.next_delay];
        self.next_delay = (self.next_delay + 1).min(RETRY_DELAYS.len() - 1);
        self.deadline = Some(Instant::now() + delay);
    }

    fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    fn deadline_reached(&mut self) {
        self.deadline = None;
    }
}

pub struct SyncScheduler {
    captures_root: PathBuf,
    ledger_root: PathBuf,
    stream: DerivedName,
    source: String,
    clock: Arc<dyn Clock>,
    wake: SyncWake,
    identity: JournalIdentity,
    inventories: HashMap<SegmentCandidate, CachedInventory>,
    floors: SyncMemoryFloors,
    instrumentation: SyncInstrumentation,
    backoff: Backoff,
    activity: Option<watch::Sender<SyncActivity>>,
    health: Option<HealthWriter>,
    facts: SyncFacts,
    retention_fence: Arc<RetentionFence>,
    delete_hook: Option<Arc<dyn Fn(usize) + Send + Sync>>,
    removal_observer: Option<SegmentRemovalObserver>,
}

impl SyncScheduler {
    pub fn new(
        data_root: PathBuf,
        stream: DerivedName,
        source: String,
        clock: Arc<dyn Clock>,
        wake: SyncWake,
        journal: JournalIdentity,
    ) -> Self {
        let captures_root = data_root.join("captures");
        let ledger_root = data_root.join("sync-ledger");
        Self {
            captures_root,
            ledger_root,
            stream,
            source,
            clock,
            wake,
            identity: journal,
            inventories: HashMap::new(),
            floors: SyncMemoryFloors::default(),
            instrumentation: SyncInstrumentation::default(),
            backoff: Backoff::new(),
            activity: None,
            health: None,
            facts: SyncFacts {
                paired: true,
                ..Default::default()
            },
            retention_fence: Arc::new(RetentionFence::new()),
            delete_hook: None,
            removal_observer: None,
        }
    }

    pub fn with_observability(
        mut self,
        activity: watch::Sender<SyncActivity>,
        health: HealthWriter,
    ) -> Self {
        self.activity = Some(activity);
        self.health = Some(health);
        self
    }

    pub fn with_activity(mut self, activity: watch::Sender<SyncActivity>) -> Self {
        self.activity = Some(activity);
        self
    }

    pub fn with_retention_fence(mut self, fence: Arc<RetentionFence>) -> Self {
        self.retention_fence = fence;
        self
    }

    pub fn with_delete_hook(mut self, hook: Arc<dyn Fn(usize) + Send + Sync>) -> Self {
        self.delete_hook = Some(hook);
        self
    }

    pub fn with_removal_observer(mut self, observer: SegmentRemovalObserver) -> Self {
        self.removal_observer = Some(observer);
        self
    }

    pub fn instrumentation(&self) -> SyncInstrumentationSnapshot {
        self.instrumentation.snapshot()
    }

    pub fn instrumentation_handle(&self) -> SyncInstrumentation {
        self.instrumentation.clone()
    }

    pub fn cached_inventories(&self) -> usize {
        self.inventories.len()
    }

    pub async fn run<S>(&mut self, journal: &mut dyn SyncJournal, shutdown_future: S)
    where
        S: Future<Output = ()> + Send + 'static,
    {
        let (stop, receiver) = watch::channel(false);
        tokio::spawn(async move {
            shutdown_future.await;
            stop.send_replace(true);
        });
        self.run_with_shutdown(journal, receiver).await;
    }

    pub async fn run_with_shutdown(
        &mut self,
        journal: &mut dyn SyncJournal,
        mut shutdown: watch::Receiver<bool>,
    ) {
        let mut periodic = tokio::time::interval_at(
            Instant::now() + PERIODIC_SYNC_INTERVAL,
            PERIODIC_SYNC_INTERVAL,
        );
        periodic.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut requested = true;

        loop {
            if requested {
                if let Some(deadline) = self.backoff.deadline() {
                    self.local_finish().await;
                    if Instant::now() >= deadline {
                        self.backoff.deadline_reached();
                    } else {
                        tokio::select! {
                            biased;
                            () = wait_for_shutdown(&mut shutdown) => return,
                            () = self.wake.wait() => {},
                            _ = periodic.tick() => {
                                self.write_health().await;
                            },
                            () = tokio::time::sleep_until(deadline) => {
                                self.backoff.deadline_reached();
                            }
                        }
                        continue;
                    }
                }

                let summary = self.run_sweep(journal, shutdown.clone()).await;
                if summary.cancelled {
                    return;
                }
                self.update_facts(&summary);
                self.write_health().await;
                requested = summary.failure.is_some();
                if self.wake.take_pending() {
                    requested = true;
                }
                if requested {
                    tokio::task::yield_now().await;
                }
                continue;
            }

            tokio::select! {
                biased;
                () = wait_for_shutdown(&mut shutdown) => return,
                () = self.wake.wait() => requested = true,
                _ = periodic.tick() => requested = true,
            }
        }
    }

    pub async fn local_finish(&mut self) {
        let captures_root = self.captures_root.clone();
        let stream = self.stream.clone();
        let candidates =
            match tokio::task::spawn_blocking(move || scan_candidates(&captures_root, &stream))
                .await
            {
                Ok(Ok(c)) => c,
                _ => return,
            };

        let snapshot = candidates.iter().cloned().collect::<HashSet<_>>();
        self.inventories
            .retain(|candidate, _| snapshot.contains(candidate));
        let ledger_root_clone = self.ledger_root.clone();
        let snapshot_clone = snapshot.clone();
        let _ = tokio::task::spawn_blocking(move || {
            cleanup_hold_files(&ledger_root_clone);
            prune_ledger_dirs(&ledger_root_clone, &snapshot_clone);
        })
        .await;

        for candidate in candidates {
            let root = self.captures_root.clone();
            let target = candidate.clone();
            let resolved =
                tokio::task::spawn_blocking(move || resolve_segment_files(&root, &target)).await;

            match resolved {
                Ok(ResolvedSegmentFiles::Empty) => {
                    self.inventories.remove(&candidate);
                    let _ = delete_empty_segment(
                        &self.captures_root,
                        &self.ledger_root,
                        &candidate,
                        Arc::clone(&self.retention_fence),
                    )
                    .await;
                }
                Ok(ResolvedSegmentFiles::Found(files)) => {
                    let local_files = match self.inventory_for(&candidate, &files).await {
                        Ok(f) => f,
                        Err(()) => continue,
                    };
                    let ledger_root = self.ledger_root.clone();
                    let target = candidate.clone();
                    let id = self.identity.clone();
                    let local_files_clone = local_files.clone();
                    let ack_opt = tokio::task::spawn_blocking(move || {
                        read_ack_record(&ledger_root, &target, &id, &local_files_clone)
                    })
                    .await
                    .ok()
                    .flatten();

                    if let Some(ack) = ack_opt {
                        let expected_digests: Vec<(&str, &str)> = ack
                            .files
                            .iter()
                            .map(|f| (f.submitted.as_str(), f.sha256.as_str()))
                            .collect();
                        let identities = self
                            .inventories
                            .get(&candidate)
                            .map(|cached| cached.identities.clone())
                            .unwrap_or_default();
                        let removed = delete_confirmed_segment(
                            &self.captures_root,
                            &self.ledger_root,
                            &candidate,
                            &expected_digests,
                            &identities,
                            self.delete_hook.clone(),
                            self.removal_observer.clone(),
                            Arc::clone(&self.retention_fence),
                        )
                        .await;
                        if removed {
                            self.inventories.remove(&candidate);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    pub async fn run_sweep(
        &mut self,
        journal: &mut dyn SyncJournal,
        mut shutdown: watch::Receiver<bool>,
    ) -> SyncSweepSummary {
        let mut summary = SyncSweepSummary::empty();
        let instrumentation = self.instrumentation.clone();
        tokio::task::spawn_blocking(move || {
            instrumentation.candidate_scan();
        })
        .await
        .ok();

        self.local_finish().await;

        let now = self.clock.wall_now().unix_timestamp();
        let captures_root = self.captures_root.clone();
        let stream = self.stream.clone();
        let remaining_candidates =
            match tokio::task::spawn_blocking(move || scan_candidates(&captures_root, &stream))
                .await
            {
                Ok(Ok(candidates)) => candidates,
                _ => {
                    self.facts.pending_segments = 0;
                    return self.end_sweep(
                        summary,
                        SyncOperationError::EndSweepDiagnostic(
                            SyncFailureClass::Contract,
                            DiagnosticCode::LocalSegmentInvalid,
                        ),
                    );
                }
            };
        let remaining_snapshot = remaining_candidates.iter().cloned().collect::<HashSet<_>>();
        self.inventories
            .retain(|candidate, _| remaining_snapshot.contains(candidate));

        let mut upload_due = Vec::new();
        let mut non_acked_count = 0usize;

        for candidate in &remaining_candidates {
            if shutdown_requested(&mut shutdown) {
                return self.cancelled_sweep(summary).await;
            }
            let root = self.captures_root.clone();
            let target = candidate.clone();
            let files =
                match tokio::task::spawn_blocking(move || resolve_segment_files(&root, &target))
                    .await
                {
                    Ok(ResolvedSegmentFiles::Found(files)) => files,
                    Ok(ResolvedSegmentFiles::Empty) => {
                        self.inventories.remove(candidate);
                        continue;
                    }
                    Ok(ResolvedSegmentFiles::Missing) => {
                        self.inventories.remove(candidate);
                        continue;
                    }
                    Ok(ResolvedSegmentFiles::Invalid) | Err(_) => {
                        non_acked_count += 1;
                        summary.diagnostic = Some(DiagnosticCode::LocalSegmentInvalid);
                        continue;
                    }
                };
            let local_files = match self.inventory_for(candidate, &files).await {
                Ok(files) => files,
                Err(()) => {
                    non_acked_count += 1;
                    summary.diagnostic = Some(DiagnosticCode::LocalSegmentInvalid);
                    continue;
                }
            };

            let ledger_root = self.ledger_root.clone();
            let target = candidate.clone();
            let id = self.identity.clone();
            let local_files_clone = local_files.clone();
            let ack_opt = tokio::task::spawn_blocking(move || {
                read_ack_record(&ledger_root, &target, &id, &local_files_clone)
            })
            .await
            .ok()
            .flatten();

            if ack_opt.is_none() {
                non_acked_count += 1;
                let ledger_root = self.ledger_root.clone();
                let target = candidate.clone();
                let state =
                    tokio::task::spawn_blocking(move || read_state_record(&ledger_root, &target))
                        .await
                        .unwrap_or_default();

                let disk_next_attempt = state.next_attempt_unix.map(|stored| {
                    let interval = state.next_attempt_interval_seconds.unwrap_or(3600) as i64;
                    stored.min(now + interval)
                });
                let floor_next_attempt = self.floors.next_attempt.get(candidate).copied();
                let next_attempt = match (disk_next_attempt, floor_next_attempt) {
                    (Some(d), Some(f)) => d.max(f),
                    (Some(d), None) => d,
                    (None, Some(f)) => f,
                    (None, None) => 0,
                };

                if next_attempt > now {
                    continue;
                }

                upload_due.push((candidate.clone(), files, local_files, state));
            }
        }

        self.facts.pending_segments = u64::try_from(non_acked_count).unwrap_or(u64::MAX);
        let mut made_requests = false;
        let mut activity = None;

        if !upload_due.is_empty() {
            self.facts.sync_in_progress = true;
            self.write_health().await;
            let mut consecutive_ack_write_errors = 0usize;
            for (batch_index, batch) in upload_due.chunks(CANDIDATES_PER_BATCH).enumerate() {
                self.instrumentation.batch();
                for (candidate, files, local_files, mut state) in batch.iter().cloned() {
                    if shutdown_requested(&mut shutdown) {
                        return self.cancelled_sweep_with_activity(summary, activity).await;
                    }
                    summary.attempted += 1;
                    if activity.is_none() {
                        activity = Some(ActivityGuard::new(self.activity.as_ref()));
                    }
                    made_requests = true;
                    let upload = match cancellable(
                        &mut shutdown,
                        journal.upload(&candidate, files.clone(), &self.source),
                    )
                    .await
                    {
                        Err(()) => {
                            return self.cancelled_sweep_with_activity(summary, activity).await;
                        }
                        Ok(Ok(upload)) => {
                            summary.contacted = true;
                            self.backoff.successful_operation();
                            upload
                        }
                        Ok(Err(SyncOperationError::RetainCandidate { diagnostic, answer })) => {
                            summary.contacted = true;
                            self.backoff.successful_operation();
                            summary.diagnostic = Some(diagnostic);
                            let new_answer = answer;
                            if state.answer.as_deref() == Some(&new_answer) {
                                state.answer_count = state.answer_count.saturating_add(1);
                            } else {
                                state.answer = Some(new_answer);
                                state.answer_count = 1;
                            }
                            let interval: u64 = if state.answer_count >= 3 { 86400 } else { 3600 };
                            let due = now + interval as i64;
                            state.next_attempt_unix = Some(due);
                            state.next_attempt_interval_seconds = Some(interval);
                            let ledger_root = self.ledger_root.clone();
                            let target = candidate.clone();
                            let state_clone = state.clone();
                            let write_ok = tokio::task::spawn_blocking(move || {
                                write_state_record(&ledger_root, &target, &state_clone)
                            })
                            .await
                            .is_ok_and(|r| r.is_ok());
                            if !write_ok {
                                self.floors.next_attempt.insert(candidate.clone(), due);
                            }
                            continue;
                        }
                        Ok(Err(SyncOperationError::SegmentRemoved)) => {
                            summary.contacted = true;
                            self.backoff.successful_operation();
                            let ack_files: Vec<AcknowledgedFile> = local_files
                                .iter()
                                .map(|f| AcknowledgedFile {
                                    submitted: f.name.clone(),
                                    written: f.name.clone(),
                                    disposition: "written".to_owned(),
                                    size: f.size,
                                    sha256: f.sha256.clone(),
                                })
                                .collect();
                            let stored_key = candidate.segment().to_owned();
                            let ledger_root = self.ledger_root.clone();
                            let target = candidate.clone();
                            let id = self.identity.clone();
                            let ack_files_clone = ack_files.clone();
                            let ack_write_ok = tokio::task::spawn_blocking(move || {
                                write_ack_record(
                                    &ledger_root,
                                    &target,
                                    &id,
                                    &stored_key,
                                    ack_files_clone,
                                )
                            })
                            .await
                            .is_ok_and(|r| r.is_ok());

                            if ack_write_ok {
                                consecutive_ack_write_errors = 0;
                                let ledger_root = self.ledger_root.clone();
                                let target = candidate.clone();
                                let _ = tokio::task::spawn_blocking(move || {
                                    write_state_record(
                                        &ledger_root,
                                        &target,
                                        &StateRecord::default(),
                                    )
                                })
                                .await;
                                self.floors.next_attempt.remove(&candidate);
                                self.facts.pending_segments =
                                    self.facts.pending_segments.saturating_sub(1);

                                let expected_digests: Vec<(&str, &str)> = ack_files
                                    .iter()
                                    .map(|f| (f.submitted.as_str(), f.sha256.as_str()))
                                    .collect();
                                let identities = self
                                    .inventories
                                    .get(&candidate)
                                    .map(|cached| cached.identities.clone())
                                    .unwrap_or_default();
                                let removed = delete_confirmed_segment(
                                    &self.captures_root,
                                    &self.ledger_root,
                                    &candidate,
                                    &expected_digests,
                                    &identities,
                                    self.delete_hook.clone(),
                                    self.removal_observer.clone(),
                                    Arc::clone(&self.retention_fence),
                                )
                                .await;
                                if removed {
                                    self.inventories.remove(&candidate);
                                }
                            } else {
                                consecutive_ack_write_errors += 1;
                                if consecutive_ack_write_errors >= 3 {
                                    summary.diagnostic = Some(DiagnosticCode::PrivateStateIo);
                                }
                            }
                            continue;
                        }
                        Ok(Err(error)) => {
                            drop(activity);
                            return self.end_sweep(summary, error);
                        }
                    };

                    let receipt = assess_receipt(&upload, &local_files);
                    match receipt {
                        Receipt::Valid(ack_files) => {
                            let stored_key = upload
                                .authoritative_key
                                .unwrap_or_else(|| candidate.segment().to_owned());
                            let ledger_root = self.ledger_root.clone();
                            let target = candidate.clone();
                            let id = self.identity.clone();
                            let ack_files_clone = ack_files.clone();
                            let stored_key_clone = stored_key.clone();
                            let ack_write_ok = tokio::task::spawn_blocking(move || {
                                write_ack_record(
                                    &ledger_root,
                                    &target,
                                    &id,
                                    &stored_key_clone,
                                    ack_files_clone,
                                )
                            })
                            .await
                            .is_ok_and(|r| r.is_ok());

                            if ack_write_ok {
                                consecutive_ack_write_errors = 0;
                                let ledger_root = self.ledger_root.clone();
                                let target = candidate.clone();
                                let _ = tokio::task::spawn_blocking(move || {
                                    write_state_record(
                                        &ledger_root,
                                        &target,
                                        &StateRecord::default(),
                                    )
                                })
                                .await;
                                self.floors.next_attempt.remove(&candidate);
                                summary.custodied += 1;
                                self.facts.pending_segments =
                                    self.facts.pending_segments.saturating_sub(1);

                                let expected_digests: Vec<(&str, &str)> = ack_files
                                    .iter()
                                    .map(|f| (f.submitted.as_str(), f.sha256.as_str()))
                                    .collect();
                                let identities = self
                                    .inventories
                                    .get(&candidate)
                                    .map(|cached| cached.identities.clone())
                                    .unwrap_or_default();
                                let removed = delete_confirmed_segment(
                                    &self.captures_root,
                                    &self.ledger_root,
                                    &candidate,
                                    &expected_digests,
                                    &identities,
                                    self.delete_hook.clone(),
                                    self.removal_observer.clone(),
                                    Arc::clone(&self.retention_fence),
                                )
                                .await;
                                if removed {
                                    self.inventories.remove(&candidate);
                                }
                            } else {
                                consecutive_ack_write_errors += 1;
                                if consecutive_ack_write_errors >= 3 {
                                    summary.diagnostic = Some(DiagnosticCode::PrivateStateIo);
                                }
                            }
                        }
                        Receipt::Invalid(fault) => {
                            eprintln!(
                                "solstone-tmux: upload was not acknowledged ({})",
                                fault.as_str()
                            );
                            let answer = format!("invalid:{}", fault.as_str());
                            if fault == ReceiptFault::ReceivedNotWritten {
                                state.answer = Some(answer);
                                state.answer_count = 1;
                                let interval: u64 = 86400;
                                let due = now + interval as i64;
                                state.next_attempt_unix = Some(due);
                                state.next_attempt_interval_seconds = Some(interval);
                            } else {
                                if state.answer.as_deref() == Some(&answer) {
                                    state.answer_count = state.answer_count.saturating_add(1);
                                } else {
                                    state.answer = Some(answer);
                                    state.answer_count = 1;
                                }
                                let interval: u64 =
                                    if state.answer_count >= 3 { 86400 } else { 3600 };
                                let due = now + interval as i64;
                                state.next_attempt_unix = Some(due);
                                state.next_attempt_interval_seconds = Some(interval);
                            }
                            let ledger_root = self.ledger_root.clone();
                            let target = candidate.clone();
                            let state_clone = state.clone();
                            let write_ok = tokio::task::spawn_blocking(move || {
                                write_state_record(&ledger_root, &target, &state_clone)
                            })
                            .await
                            .is_ok_and(|r| r.is_ok());
                            if !write_ok {
                                let due = state.next_attempt_unix.unwrap_or(now + 3600);
                                self.floors.next_attempt.insert(candidate.clone(), due);
                            }
                        }
                        Receipt::Absent => {
                            eprintln!(
                                "solstone-tmux: upload was not acknowledged (descriptors_absent)"
                            );
                            let answer = "absent:descriptors_absent".to_owned();
                            if state.answer.as_deref() == Some(&answer) {
                                state.answer_count = state.answer_count.saturating_add(1);
                            } else {
                                state.answer = Some(answer);
                                state.answer_count = 1;
                            }
                            let interval: u64 = if state.answer_count >= 3 { 86400 } else { 3600 };
                            let due = now + interval as i64;
                            state.next_attempt_unix = Some(due);
                            state.next_attempt_interval_seconds = Some(interval);
                            let ledger_root = self.ledger_root.clone();
                            let target = candidate.clone();
                            let state_clone = state.clone();
                            let write_ok = tokio::task::spawn_blocking(move || {
                                write_state_record(&ledger_root, &target, &state_clone)
                            })
                            .await
                            .is_ok_and(|r| r.is_ok());
                            if !write_ok {
                                let due = state.next_attempt_unix.unwrap_or(now + 3600);
                                self.floors.next_attempt.insert(candidate.clone(), due);
                            }
                        }
                    }
                }
                self.write_health().await;
                if batch_index + 1 != upload_due.chunks(CANDIDATES_PER_BATCH).len() {
                    self.yield_between_batches().await;
                    if shutdown_requested(&mut shutdown) {
                        return self.cancelled_sweep_with_activity(summary, activity).await;
                    }
                }
            }
        }

        if !made_requests {
            match cancellable(&mut shutdown, journal.system_status()).await {
                Err(()) => return self.cancelled_sweep_with_activity(summary, activity).await,
                Ok(Ok(())) => {
                    summary.contacted = true;
                    self.backoff.successful_operation();
                }
                Ok(Err(error)) => {
                    drop(activity);
                    return self.end_sweep(summary, error);
                }
            }
        }

        drop(activity);
        summary
    }

    async fn yield_between_batches(&self) {
        tokio::task::yield_now().await;
        self.instrumentation.batch_yield();
    }

    async fn inventory_for(
        &mut self,
        candidate: &SegmentCandidate,
        paths: &[PathBuf],
    ) -> Result<Vec<LocalFile>, ()> {
        let paths = paths.to_vec();
        let names = paths
            .iter()
            .map(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .map(str::to_owned)
            })
            .collect::<Option<Vec<_>>>()
            .ok_or(())?;
        let identity_paths = paths.clone();
        let identities = tokio::task::spawn_blocking(move || file_identities(&identity_paths))
            .await
            .ok()
            .flatten()
            .ok_or(())?;
        if let Some(cached) = self.inventories.get(candidate)
            && cached.names == names
            && cached.identities == identities
        {
            return Ok(cached.inventory.clone());
        }
        let inventory = inventory_files(paths, Some(self.instrumentation.clone()))
            .await
            .map_err(|_| ())?;
        self.inventories.insert(
            candidate.clone(),
            CachedInventory {
                names,
                identities,
                inventory: inventory.clone(),
            },
        );
        Ok(inventory)
    }

    async fn cancelled_sweep(&mut self, summary: SyncSweepSummary) -> SyncSweepSummary {
        self.cancelled_sweep_with_activity(summary, None).await
    }

    async fn cancelled_sweep_with_activity(
        &mut self,
        mut summary: SyncSweepSummary,
        activity: Option<ActivityGuard>,
    ) -> SyncSweepSummary {
        drop(activity);
        summary.cancelled = true;
        self.facts.sync_in_progress = false;
        self.write_health().await;
        summary
    }

    fn update_facts(&mut self, summary: &SyncSweepSummary) {
        let now = self.clock.wall_now().unix_timestamp();
        if summary.custodied > 0 {
            self.facts.successful_sync(now);
        } else if summary.contacted {
            self.facts.successful_contact(now);
        }
        if let Some(code) = summary.diagnostic {
            self.facts.failed(code);
        } else if let Some(failure) = summary.failure {
            self.facts.failed(diagnostic_for_failure(failure));
        }
        self.facts.sync_in_progress = false;
    }

    async fn write_health(&self) {
        self.instrumentation.health_write();
        if let Some(health) = &self.health {
            let _ = health
                .write(&self.facts, self.clock.wall_now().unix_timestamp())
                .await;
        }
    }

    fn end_sweep(
        &mut self,
        mut summary: SyncSweepSummary,
        error: SyncOperationError,
    ) -> SyncSweepSummary {
        let failure = match error {
            SyncOperationError::RetainCandidate { diagnostic, .. } => {
                summary.diagnostic = Some(diagnostic);
                SyncFailureClass::Contract
            }
            SyncOperationError::SegmentRemoved => SyncFailureClass::Contract,
            SyncOperationError::EndSweep(failure) => failure,
            SyncOperationError::EndSweepDiagnostic(failure, code) => {
                summary.diagnostic = Some(code);
                failure
            }
        };
        self.backoff.failed_operation();
        summary.failure = Some(failure);
        summary
    }
}

struct ActivityGuard {
    sender: Option<watch::Sender<SyncActivity>>,
}

impl ActivityGuard {
    fn new(sender: Option<&watch::Sender<SyncActivity>>) -> Self {
        let sender = sender.cloned();
        if let Some(sender) = &sender {
            sender.send_replace(SyncActivity::Working);
        }
        Self { sender }
    }
}

impl Drop for ActivityGuard {
    fn drop(&mut self) {
        if let Some(sender) = &self.sender {
            sender.send_replace(SyncActivity::Idle);
        }
    }
}

impl JournalSession {
    fn map_error(&self, error: JournalError) -> SyncOperationError {
        map_bridge_error(error, self.bridge.stop_reason())
    }
}

/// A bridge 502 after the bridge stopped dialing is the journal refusing this device, not an
/// outage: the bridge stops on access denied (49) and once other refusals reach its bound.
pub fn map_bridge_error(
    error: JournalError,
    stop: Option<JournalBridgeTerminalReason>,
) -> SyncOperationError {
    if error.diagnostic() == DiagnosticCode::JournalUnavailable && stop.is_some() {
        return SyncOperationError::EndSweepDiagnostic(
            SyncFailureClass::Auth,
            DiagnosticCode::JournalRevoked,
        );
    }
    map_journal_error(error)
}

impl SyncJournal for JournalSession {
    fn upload<'a>(
        &'a mut self,
        candidate: &'a SegmentCandidate,
        files: Vec<PathBuf>,
        source: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<UploadResult, SyncOperationError>> + Send + 'a>> {
        Box::pin(async move {
            self.journal
                .ingest_upload(candidate.day(), candidate.segment(), files, source)
                .await
                .map_err(|error| self.map_error(error))
        })
    }

    fn system_status<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<(), SyncOperationError>> + Send + 'a>> {
        Box::pin(async move {
            self.journal
                .system_status()
                .await
                .map(|_| ())
                .map_err(|error| self.map_error(error))
        })
    }
}

pub struct SyncTask {
    pub config_root: PathBuf,
    pub data_root: PathBuf,
    pub config: RuntimeConfig,
    pub hostname: String,
    pub clock: Arc<dyn Clock>,
    pub wake: SyncWake,
    pub activity: watch::Sender<SyncActivity>,
    pub health: HealthWriter,
    pub retention_fence: Arc<RetentionFence>,
    pub identity: RunIdentity,
}

impl SyncTask {
    pub async fn run(self, mut shutdown: watch::Receiver<bool>) -> Result<(), DiagnosticCode> {
        let SyncTask {
            config_root,
            data_root,
            config,
            hostname,
            clock,
            wake,
            activity,
            health,
            retention_fence,
            identity,
        } = self;
        let load_root = config_root.clone();
        let loaded = tokio::task::spawn_blocking(move || load_credential(&load_root))
            .await
            .map_err(|_| DiagnosticCode::PrivateStateIo)?;
        let credential = match loaded {
            Ok(Some(credential)) => credential,
            Ok(None) => {
                let facts = SyncFacts::default();
                refresh_waiting_health(&health, &facts, clock.as_ref(), &mut shutdown).await;
                return Ok(());
            }
            Err(code) => {
                let mut facts = SyncFacts::default();
                facts.failed(code);
                refresh_waiting_health(&health, &facts, clock.as_ref(), &mut shutdown).await;
                return Ok(());
            }
        };
        let expected_stream = default_stream(&hostname)
            .ok()
            .and_then(|stream| derive_component(&stream).ok())
            .ok_or(DiagnosticCode::ConfiguredStreamMismatch)?;
        if config.stream != expected_stream {
            let mut facts = SyncFacts::default();
            facts.failed(DiagnosticCode::ConfiguredStreamMismatch);
            refresh_waiting_health(&health, &facts, clock.as_ref(), &mut shutdown).await;
            return Ok(());
        }
        let refresh = VersionRefreshState::new(
            config_root.clone(),
            data_root.clone(),
            credential.instance_id.clone(),
            &credential.ca_fp_prefix,
            identity,
        );
        let journal_identity = JournalIdentity {
            instance_id: credential.instance_id.clone(),
            ca_fp_prefix_hex: hex_encode(&credential.ca_fp_prefix),
            pairing_generation_hex: hex_encode(&compute_pairing_generation(
                &credential.client_cert_pem,
            )),
        };
        let mut reconnect = Backoff::new();
        let mut reconnect_facts = SyncFacts {
            paired: true,
            ..SyncFacts::default()
        };
        loop {
            if shutdown_requested(&mut shutdown) {
                return Ok(());
            }
            let mut journal = match JournalSession::start(
                credential.clone(),
                config_root.clone(),
                refresh.clone(),
            )
            .await
            {
                Ok(journal) => journal,
                Err(code) => {
                    reconnect_facts.failed(code);
                    let _ = health
                        .write(&reconnect_facts, clock.wall_now().unix_timestamp())
                        .await;
                    reconnect.failed_operation();
                    let deadline = reconnect
                        .deadline()
                        .expect("failed reconnect has a retry deadline");
                    if !wait_for_retry_or_shutdown(&mut shutdown, deadline).await {
                        return Ok(());
                    }
                    reconnect.deadline_reached();
                    continue;
                }
            };
            let mut scheduler = SyncScheduler::new(
                data_root.clone(),
                config.stream,
                config.source,
                clock,
                wake,
                journal_identity.clone(),
            )
            .with_observability(activity, health)
            .with_retention_fence(retention_fence);
            scheduler.facts.paired = true;
            scheduler
                .run_with_shutdown(&mut journal, shutdown.clone())
                .await;
            if let Err(code) = journal.shutdown().await {
                scheduler.facts.failed(code);
                scheduler.write_health().await;
                return Err(code);
            }
            return Ok(());
        }
    }
}

async fn wait_for_retry_or_shutdown(
    shutdown: &mut watch::Receiver<bool>,
    deadline: Instant,
) -> bool {
    tokio::select! {
        biased;
        () = wait_for_shutdown(shutdown) => false,
        () = tokio::time::sleep_until(deadline) => true,
    }
}

async fn wait_for_shutdown(receiver: &mut watch::Receiver<bool>) {
    while !*receiver.borrow_and_update() {
        if receiver.changed().await.is_err() {
            return;
        }
    }
}

fn shutdown_requested(receiver: &mut watch::Receiver<bool>) -> bool {
    *receiver.borrow_and_update()
}

async fn cancellable<T>(
    shutdown: &mut watch::Receiver<bool>,
    operation: impl Future<Output = T>,
) -> Result<T, ()> {
    tokio::select! {
        biased;
        () = wait_for_shutdown(shutdown) => Err(()),
        value = operation => Ok(value),
    }
}

async fn refresh_waiting_health(
    health: &HealthWriter,
    facts: &SyncFacts,
    clock: &dyn Clock,
    shutdown: &mut watch::Receiver<bool>,
) {
    let mut heartbeat = tokio::time::interval(HEALTH_REFRESH_INTERVAL);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            biased;
            () = wait_for_shutdown(shutdown) => return,
            _ = heartbeat.tick() => {
                let _ = health.write(facts, clock.wall_now().unix_timestamp()).await;
            }
        }
    }
}

fn map_journal_error(error: JournalError) -> SyncOperationError {
    let diagnostic = error.diagnostic();
    match diagnostic {
        DiagnosticCode::RequestTooLarge => {
            let answer = if let Some(status) = error.http_status() {
                format!("{status}:request_too_large")
            } else {
                "local:request_too_large".to_owned()
            };
            SyncOperationError::RetainCandidate { diagnostic, answer }
        }
        DiagnosticCode::LocalSegmentInvalid => {
            let answer = if let Some(status) = error.http_status() {
                format!("{status}:local_segment_invalid")
            } else {
                "local:local_segment_invalid".to_owned()
            };
            SyncOperationError::RetainCandidate { diagnostic, answer }
        }
        DiagnosticCode::JournalTimeout => SyncOperationError::EndSweepDiagnostic(
            SyncFailureClass::Timeout,
            DiagnosticCode::JournalTimeout,
        ),
        DiagnosticCode::JournalRejected => match error.reason_code() {
            Some(JournalReasonCode::SegmentRemoved) => SyncOperationError::SegmentRemoved,
            Some(JournalReasonCode::ContentConflict) => {
                let answer = if let Some(status) = error.http_status() {
                    format!("{status}:content_conflict")
                } else {
                    "409:content_conflict".to_owned()
                };
                SyncOperationError::RetainCandidate {
                    diagnostic: DiagnosticCode::JournalRejected,
                    answer,
                }
            }
            // device-wide refusals end the sweep
            Some(
                JournalReasonCode::LinkedDeviceRequired
                | JournalReasonCode::ProtocolVersionLegacy
                | JournalReasonCode::ProtocolVersionFuture,
            ) => SyncOperationError::EndSweepDiagnostic(
                SyncFailureClass::Contract,
                DiagnosticCode::JournalRejected,
            ),
            Some(
                JournalReasonCode::AuthKeyInvalid
                | JournalReasonCode::AuthRequired
                | JournalReasonCode::PlRevoked,
            ) => SyncOperationError::EndSweepDiagnostic(
                SyncFailureClass::Auth,
                DiagnosticCode::JournalRevoked,
            ),
            Some(
                JournalReasonCode::IngestContractInvalid
                | JournalReasonCode::IngestNoFiles
                | JournalReasonCode::IngestSidecarConflict,
            ) => {
                let code_str = match error.reason_code() {
                    Some(JournalReasonCode::IngestNoFiles) => "ingest_no_files",
                    Some(JournalReasonCode::IngestSidecarConflict) => "ingest_sidecar_conflict",
                    _ => "ingest_contract_invalid",
                };
                let answer = if let Some(status) = error.http_status() {
                    format!("{status}:{code_str}")
                } else {
                    format!("400:{code_str}")
                };
                SyncOperationError::RetainCandidate {
                    diagnostic: DiagnosticCode::LocalSegmentInvalid,
                    answer,
                }
            }
            Some(JournalReasonCode::IngestStorageFailed) => SyncOperationError::EndSweepDiagnostic(
                SyncFailureClass::Direct,
                DiagnosticCode::JournalRejected,
            ),
            _ => SyncOperationError::EndSweepDiagnostic(
                SyncFailureClass::Contract,
                DiagnosticCode::JournalRejected,
            ),
        },
        DiagnosticCode::JournalContractInvalid
        | DiagnosticCode::JournalResponseTooLarge
        | DiagnosticCode::ConfiguredStreamMismatch
        | DiagnosticCode::PrivateStateInvalid
        | DiagnosticCode::PrivateStateIo => {
            SyncOperationError::EndSweepDiagnostic(SyncFailureClass::Contract, diagnostic)
        }
        _ => SyncOperationError::EndSweepDiagnostic(SyncFailureClass::Direct, diagnostic),
    }
}

#[cfg(test)]
fn map_diagnostic(code: DiagnosticCode) -> SyncOperationError {
    match code {
        DiagnosticCode::JournalTimeout => SyncOperationError::EndSweepDiagnostic(
            SyncFailureClass::Timeout,
            DiagnosticCode::JournalTimeout,
        ),
        DiagnosticCode::JournalContractInvalid
        | DiagnosticCode::JournalResponseTooLarge
        | DiagnosticCode::ConfiguredStreamMismatch
        | DiagnosticCode::PrivateStateInvalid
        | DiagnosticCode::PrivateStateIo => {
            SyncOperationError::EndSweepDiagnostic(SyncFailureClass::Contract, code)
        }
        _ => SyncOperationError::EndSweepDiagnostic(SyncFailureClass::Direct, code),
    }
}

fn diagnostic_for_failure(failure: SyncFailureClass) -> DiagnosticCode {
    match failure {
        SyncFailureClass::Auth => DiagnosticCode::JournalRevoked,
        SyncFailureClass::Timeout => DiagnosticCode::JournalTimeout,
        SyncFailureClass::Contract => DiagnosticCode::JournalContractInvalid,
        SyncFailureClass::Direct | SyncFailureClass::Relay => DiagnosticCode::JournalUnavailable,
    }
}

fn scan_candidates(
    captures_root: &Path,
    stream: &DerivedName,
) -> Result<Vec<SegmentCandidate>, ()> {
    match fs::symlink_metadata(captures_root) {
        Ok(metadata) if is_plain_directory(&metadata) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        _ => return Err(()),
    }
    let mut candidates = Vec::new();
    for entry in fs::read_dir(captures_root).map_err(|_| ())? {
        let entry = entry.map_err(|_| ())?;
        let day = entry.file_name().into_string().map_err(|_| ())?;
        if parse_day(&day).is_none() {
            continue;
        }
        if !plain_directory_entry(&entry) {
            return Err(());
        }
        let derived_day = derive_component(&day).map_err(|_| ())?;
        let day_path = derived_day.join_checked(captures_root).map_err(|_| ())?;
        if day_path != entry.path() {
            return Err(());
        }
        let stream_path = stream.join_checked(&day_path).map_err(|_| ())?;
        match fs::symlink_metadata(&stream_path) {
            Ok(metadata) if is_plain_directory(&metadata) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            _ => return Err(()),
        }
        for segment_entry in fs::read_dir(&stream_path).map_err(|_| ())? {
            let segment_entry = segment_entry.map_err(|_| ())?;
            let segment = segment_entry.file_name().into_string().map_err(|_| ())?;
            if !valid_segment_name(&segment) {
                continue;
            }
            if !plain_directory_entry(&segment_entry) {
                return Err(());
            }
            candidates.push(SegmentCandidate::new(day.clone(), stream.as_str(), segment));
        }
    }
    candidates.sort_by(|left, right| right.cmp(left));
    Ok(candidates)
}

enum ResolvedSegmentFiles {
    Found(Vec<PathBuf>),
    Empty,
    Missing,
    Invalid,
}

fn resolve_segment_files(
    captures_root: &Path,
    candidate: &SegmentCandidate,
) -> ResolvedSegmentFiles {
    let Ok(root_metadata) = fs::symlink_metadata(captures_root) else {
        return ResolvedSegmentFiles::Missing;
    };
    if !is_plain_directory(&root_metadata) || parse_day(candidate.day()).is_none() {
        return ResolvedSegmentFiles::Invalid;
    }
    let Some(day) = exact_component(candidate.day()) else {
        return ResolvedSegmentFiles::Invalid;
    };
    let Some(stream) = exact_component(candidate.stream()) else {
        return ResolvedSegmentFiles::Invalid;
    };
    let Some(segment) = exact_component(candidate.segment()) else {
        return ResolvedSegmentFiles::Invalid;
    };
    if !valid_segment_name(candidate.segment()) {
        return ResolvedSegmentFiles::Invalid;
    }
    let Ok(day_path) = day.join_checked(captures_root) else {
        return ResolvedSegmentFiles::Invalid;
    };
    let Ok(stream_path) = stream.join_checked(&day_path) else {
        return ResolvedSegmentFiles::Invalid;
    };
    let Ok(segment_path) = segment.join_checked(&stream_path) else {
        return ResolvedSegmentFiles::Invalid;
    };
    for directory in [&day_path, &stream_path, &segment_path] {
        let metadata = match fs::symlink_metadata(directory) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return ResolvedSegmentFiles::Missing;
            }
            Err(_) => return ResolvedSegmentFiles::Invalid,
        };
        if !is_plain_directory(&metadata) {
            return ResolvedSegmentFiles::Invalid;
        }
    }

    let mut files = Vec::new();
    let entries = match fs::read_dir(&segment_path) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return ResolvedSegmentFiles::Missing;
        }
        Err(_) => return ResolvedSegmentFiles::Invalid,
    };
    for entry in entries {
        let Ok(entry) = entry else {
            return ResolvedSegmentFiles::Invalid;
        };
        let Ok(name) = entry.file_name().into_string() else {
            return ResolvedSegmentFiles::Invalid;
        };
        if !valid_capture_filename(&name) || !plain_regular_entry(&entry) {
            return ResolvedSegmentFiles::Invalid;
        }
        let Some(component) = exact_component(&name) else {
            return ResolvedSegmentFiles::Invalid;
        };
        let Ok(path) = component.join_checked(&segment_path) else {
            return ResolvedSegmentFiles::Invalid;
        };
        if path != entry.path() {
            return ResolvedSegmentFiles::Invalid;
        }
        files.push(path);
    }
    if files.is_empty() {
        return ResolvedSegmentFiles::Empty;
    }
    files.sort();
    ResolvedSegmentFiles::Found(files)
}

fn remove_empty_segment_directory(
    captures_root: &Path,
    candidate: &SegmentCandidate,
) -> RemovalResult {
    let Ok(root_metadata) = fs::symlink_metadata(captures_root) else {
        return RemovalResult::Absent;
    };
    if !is_plain_directory(&root_metadata) || parse_day(candidate.day()).is_none() {
        return RemovalResult::Refused;
    }
    let Some(day) = exact_component(candidate.day()) else {
        return RemovalResult::Refused;
    };
    let Some(stream) = exact_component(candidate.stream()) else {
        return RemovalResult::Refused;
    };
    let Some(segment) = exact_component(candidate.segment()) else {
        return RemovalResult::Refused;
    };
    if !valid_segment_name(candidate.segment()) {
        return RemovalResult::Refused;
    }
    let Ok(day_path) = day.join_checked(captures_root) else {
        return RemovalResult::Refused;
    };
    let Ok(stream_path) = stream.join_checked(&day_path) else {
        return RemovalResult::Refused;
    };
    let Ok(segment_path) = segment.join_checked(&stream_path) else {
        return RemovalResult::Refused;
    };
    for directory in [&day_path, &stream_path, &segment_path] {
        let metadata = match fs::symlink_metadata(directory) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return RemovalResult::Absent;
            }
            Err(_) => return RemovalResult::Refused,
        };
        if !is_plain_directory(&metadata) {
            return RemovalResult::Refused;
        }
    }

    let entries = match fs::read_dir(&segment_path) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return RemovalResult::Absent;
        }
        Err(_) => return RemovalResult::Refused,
    };
    if entries.into_iter().next().is_some() {
        return RemovalResult::Refused;
    }

    let Ok(stream_directory) = open_directory_readonly(&stream_path) else {
        return RemovalResult::Refused;
    };
    if rustix::fs::unlinkat(
        &stream_directory,
        candidate.segment(),
        rustix::fs::AtFlags::REMOVEDIR,
    )
    .is_err()
    {
        return RemovalResult::Failed;
    }
    let _ = sync_directory(&stream_path);
    RemovalResult::Removed
}

fn file_identities(paths: &[PathBuf]) -> Option<Vec<FileIdentity>> {
    paths
        .iter()
        .map(|path| {
            let file = open_regular_readonly(path).ok()?;
            let metadata = file.metadata().ok()?;
            Some(FileIdentity {
                name: path.file_name()?.to_str()?.to_owned(),
                device: metadata.dev(),
                inode: metadata.ino(),
                size: metadata.len(),
                mtime: metadata.mtime(),
                mtime_nsec: metadata.mtime_nsec(),
                ctime: metadata.ctime(),
                ctime_nsec: metadata.ctime_nsec(),
            })
        })
        .collect()
}

fn delete_revalidated_segment(
    captures_root: &Path,
    candidate: &SegmentCandidate,
    expected_paths: &[PathBuf],
    expected_identities: &[FileIdentity],
    expected_digests: &HashMap<String, String>,
    delete_hook: Option<&(dyn Fn(usize) + Send + Sync)>,
    observer: Option<(&SegmentCandidate, &SegmentRemovalObserver)>,
) -> RemovalResult {
    let paths = match resolve_segment_files(captures_root, candidate) {
        ResolvedSegmentFiles::Found(paths) => paths,
        ResolvedSegmentFiles::Missing => return RemovalResult::Absent,
        _ => return RemovalResult::Refused,
    };
    if paths != expected_paths || file_identities(&paths).as_deref() != Some(expected_identities) {
        return RemovalResult::Refused;
    }
    let Some(segment_path) = expected_paths.first().and_then(|path| path.parent()) else {
        return RemovalResult::Refused;
    };
    let Some(stream_path) = segment_path.parent() else {
        return RemovalResult::Refused;
    };
    let Ok(segment_directory) = open_directory_readonly(segment_path) else {
        return RemovalResult::Refused;
    };
    let Ok(stream_directory) = open_directory_readonly(stream_path) else {
        return RemovalResult::Refused;
    };
    let mut retained_files = Vec::with_capacity(paths.len());
    for (path, expected) in paths.iter().zip(expected_identities) {
        let Ok(file) = open_regular_readonly_at(&segment_directory, &expected.name, path) else {
            return RemovalResult::Refused;
        };
        let Ok(metadata) = file.metadata() else {
            return RemovalResult::Refused;
        };
        if !metadata_matches(&metadata, expected) {
            return RemovalResult::Refused;
        }
        retained_files.push(file);
    }
    for (index, (path, expected)) in paths.iter().zip(expected_identities).enumerate() {
        if let Some(hook) = delete_hook {
            hook(index);
        }
        let mut current = open_regular_readonly_at(&segment_directory, &expected.name, path);
        let current_matches = current
            .as_ref()
            .ok()
            .and_then(|file| file.metadata().ok())
            .is_some_and(|metadata| metadata_matches(&metadata, expected));
        let current_proves_custody = current_matches
            && expected_digests
                .get(expected.name.as_str())
                .is_some_and(|expected_digest| {
                    current.as_mut().ok().is_some_and(|file| {
                        stream_sha256_hex(file).is_ok_and(|digest| digest == *expected_digest)
                    })
                });
        if !current_proves_custody
            || rustix::fs::unlinkat(
                &segment_directory,
                expected.name.as_str(),
                rustix::fs::AtFlags::empty(),
            )
            .is_err()
        {
            let _ = restore_expected_files(
                &paths,
                expected_identities,
                segment_path,
                &segment_directory,
                &mut retained_files,
            );
            return RemovalResult::Failed;
        }
    }
    if let Some((cand, obs)) = observer {
        obs(cand, SegmentRemovalStage::FilesUnlinked);
    }
    if rustix::fs::unlinkat(
        &stream_directory,
        candidate.segment(),
        rustix::fs::AtFlags::REMOVEDIR,
    )
    .is_err()
    {
        let _ = restore_expected_files(
            &paths,
            expected_identities,
            segment_path,
            &segment_directory,
            &mut retained_files,
        );
        return RemovalResult::Failed;
    }
    if let Some((cand, obs)) = observer {
        obs(cand, SegmentRemovalStage::DirectoryUnlinked);
    }
    let _ = sync_directory(stream_path);
    RemovalResult::Removed
}

fn restore_expected_files(
    paths: &[PathBuf],
    expected_identities: &[FileIdentity],
    segment_path: &Path,
    segment_directory: &fs::File,
    retained_files: &mut [fs::File],
) -> bool {
    let mut restored = true;
    for (index, ((path, expected), file)) in paths
        .iter()
        .zip(expected_identities)
        .zip(retained_files)
        .enumerate()
    {
        let current_matches = open_regular_readonly_at(segment_directory, &expected.name, path)
            .ok()
            .and_then(|current| current.metadata().ok())
            .is_some_and(|metadata| metadata_matches(&metadata, expected));
        if current_matches {
            continue;
        }
        match rustix::fs::statat(
            segment_directory,
            expected.name.as_str(),
            rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
        ) {
            Ok(_) => {
                let preserved_name = format!(".retention-conflict-{index}");
                if rustix::fs::statat(
                    segment_directory,
                    preserved_name.as_str(),
                    rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
                )
                .is_ok()
                    || rustix::fs::renameat(
                        segment_directory,
                        expected.name.as_str(),
                        segment_directory,
                        preserved_name.as_str(),
                    )
                    .is_err()
                {
                    restored = false;
                    continue;
                }
            }
            Err(rustix::io::Errno::NOENT) => {}
            Err(_) => {
                restored = false;
                continue;
            }
        }
        if file.seek(SeekFrom::Start(0)).is_err() {
            restored = false;
            continue;
        }
        let mut bytes = Vec::new();
        if file.read_to_end(&mut bytes).is_err()
            || atomic_write_bytes(path, segment_path, &bytes).is_err()
        {
            restored = false;
        }
    }
    restored
}

fn metadata_matches(metadata: &fs::Metadata, expected: &FileIdentity) -> bool {
    metadata.dev() == expected.device
        && metadata.ino() == expected.inode
        && metadata.len() == expected.size
}

fn exact_component(value: &str) -> Option<DerivedName> {
    derive_component(value)
        .ok()
        .filter(|component| component.as_str() == value)
}

fn parse_day(value: &str) -> Option<Date> {
    if value.len() != 8 || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let year = value[0..4].parse().ok()?;
    let month = Month::try_from(value[4..6].parse::<u8>().ok()?).ok()?;
    let day = value[6..8].parse().ok()?;
    Date::from_calendar_date(year, month, day).ok()
}

fn valid_segment_name(value: &str) -> bool {
    let Some((time, duration)) = value.split_once('_') else {
        return false;
    };
    if time.len() != 6
        || duration.len() < 3
        || !time.bytes().all(|byte| byte.is_ascii_digit())
        || !duration.bytes().all(|byte| byte.is_ascii_digit())
    {
        return false;
    }
    let hour = time[0..2].parse::<u8>().ok();
    let minute = time[2..4].parse::<u8>().ok();
    let second = time[4..6].parse::<u8>().ok();
    matches!(
        (hour, minute, second),
        (Some(hour), Some(minute), Some(second))
            if time::Time::from_hms(hour, minute, second).is_ok()
    )
}

fn valid_capture_filename(value: &str) -> bool {
    let Some(session) = value
        .strip_prefix("tmux_")
        .and_then(|value| value.strip_suffix("_screen.jsonl"))
    else {
        return false;
    };
    exact_component(value).is_some() && exact_component(session).is_some()
}

fn plain_directory_entry(entry: &fs::DirEntry) -> bool {
    let Ok(file_type) = entry.file_type() else {
        return false;
    };
    let Ok(metadata) = fs::symlink_metadata(entry.path()) else {
        return false;
    };
    !file_type.is_symlink()
        && file_type.is_dir()
        && !metadata.file_type().is_symlink()
        && metadata.is_dir()
}

fn plain_regular_entry(entry: &fs::DirEntry) -> bool {
    let Ok(file_type) = entry.file_type() else {
        return false;
    };
    let Ok(metadata) = fs::symlink_metadata(entry.path()) else {
        return false;
    };
    !file_type.is_symlink()
        && file_type.is_file()
        && !metadata.file_type().is_symlink()
        && metadata.is_file()
}

fn is_plain_directory(metadata: &fs::Metadata) -> bool {
    !metadata.file_type().is_symlink() && metadata.is_dir()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use spl_transport::credential::{Credential, EndpointAddr};

    use super::{CredentialStore, SyncFailureClass, SyncOperationError, map_diagnostic};
    use crate::health::DiagnosticCode;
    use crate::post_connect::compute_pairing_generation;
    use crate::private_link::{CREDENTIALS_FILENAME, load_credential};

    #[test]
    fn failed_token_persistence_is_reported_and_retried() {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("build test runtime")
            .block_on(async {
                let suffix = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos();
                let root = std::env::temp_dir().join(format!(
                    "solstone-token-persistence-{}-{suffix}",
                    std::process::id()
                ));
                fs::create_dir(&root).expect("create token test root");
                fs::create_dir(root.join(CREDENTIALS_FILENAME))
                    .expect("create invalid credential target");
                let cred = credential();
                let pairing_gen = compute_pairing_generation(&cred.client_cert_pem);
                let (store, hook) = CredentialStore::new(root.clone(), cred, pairing_gen);
                hook("refreshed-token", 1_900_000_000);

                let code = store
                    .persist_pending()
                    .await
                    .expect_err("invalid credential target was accepted");
                assert!(matches!(
                    map_diagnostic(code),
                    SyncOperationError::EndSweepDiagnostic(
                        SyncFailureClass::Contract,
                        DiagnosticCode::PrivateStateInvalid
                    )
                ));

                fs::remove_dir(root.join(CREDENTIALS_FILENAME))
                    .expect("remove invalid credential target");
                store
                    .persist_pending()
                    .await
                    .expect("retry pending token persistence");
                let loaded = load_credential(&root)
                    .expect("load persisted credential")
                    .expect("persisted credential exists");
                assert!(loaded.device_token.is_some());
                assert!(loaded.device_token_expires_at.is_some());
                fs::remove_dir_all(root).expect("remove token test root");
            });
    }

    #[test]
    fn queued_refresh_cannot_publish_after_shutdown() {
        for retire in [false, true] {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .max_blocking_threads(1)
                .build()
                .unwrap()
                .block_on(async {
                    let root = std::env::temp_dir().join(format!(
                        "tmux-queued-refresh-{}-{}",
                        std::process::id(),
                        SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap()
                            .as_nanos()
                    ));
                    crate::paths::ensure_private_directory(&root).unwrap();
                    let initial = credential();
                    crate::private_link::persist_credential(&root, &initial).unwrap();
                    let (store, hook) = CredentialStore::new(
                        root.clone(),
                        initial.clone(),
                        compute_pairing_generation(&initial.client_cert_pem),
                    );
                    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
                    let (release_tx, release_rx) = std::sync::mpsc::channel();
                    let blocker = tokio::task::spawn_blocking(move || {
                        entered_tx.send(()).unwrap();
                        release_rx
                            .recv_timeout(std::time::Duration::from_secs(5))
                            .unwrap();
                    });
                    entered_rx.await.unwrap();
                    hook("new-token", 1_900_000_000);
                    tokio::time::timeout(
                        std::time::Duration::from_secs(5),
                        store.publication_queued.notified(),
                    )
                    .await
                    .unwrap();
                    if retire {
                        store.invalidate();
                    }
                    release_tx.send(()).unwrap();
                    blocker.await.unwrap();
                    store.persist_pending().await.unwrap();
                    let disk = load_credential(&root).unwrap().unwrap();
                    let expected = if retire { None } else { Some("new-token") };
                    assert_eq!(disk.device_token.as_deref(), expected);
                    assert_eq!(store.live_credential().device_token.as_deref(), expected);
                    fs::remove_dir_all(root).unwrap();
                });
        }
    }

    #[test]
    fn queued_ready_rechecks_new_attempt_and_shutdown_before_disk_publication() {
        use crate::instance_lock::InstanceLock;
        use crate::journal_version::VersionRefreshState;
        use crate::private_link::{PrivateLinkBridge, persist_credential};
        use std::sync::Arc;
        for supersession in [0, 1, 2] {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .max_blocking_threads(1)
                .build()
                .unwrap()
                .block_on(async {
                    let root = std::env::temp_dir().join(format!(
                        "tmux-queued-ready-{}-{}",
                        std::process::id(),
                        SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap()
                            .as_nanos()
                    ));
                    crate::paths::ensure_private_directory(&root).unwrap();
                    let data = root.join("data");
                    crate::paths::ensure_private_directory(&data).unwrap();
                    let identity =
                        rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
                    let mut initial = credential();
                    initial.client_key_pem = identity.key_pair.serialize_pem();
                    initial.client_cert_pem = identity.cert.pem();
                    initial.ca_chain_pem = vec![identity.cert.pem()];
                    persist_credential(&root, &initial).unwrap();
                    let lock = InstanceLock::acquire(&data).unwrap();
                    let refresh = VersionRefreshState::new(
                        root.clone(),
                        data,
                        initial.instance_id.clone(),
                        &initial.ca_fp_prefix,
                        lock.identity().clone(),
                    );
                    let bridge = PrivateLinkBridge::start(initial.clone(), None, refresh)
                        .await
                        .unwrap();
                    let opener = Arc::clone(bridge.opener());
                    let (store, _) = CredentialStore::new(
                        root.clone(),
                        initial.clone(),
                        compute_pairing_generation(&initial.client_cert_pem),
                    );
                    let attempt = store.capture_access_attempt(1);
                    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
                    let (release_tx, release_rx) = std::sync::mpsc::channel();
                    let blocker = tokio::task::spawn_blocking(move || {
                        entered_tx.send(()).unwrap();
                        release_rx
                            .recv_timeout(std::time::Duration::from_secs(5))
                            .unwrap();
                    });
                    entered_rx.await.unwrap();
                    let task_store = Arc::clone(&store);
                    let task_opener = Arc::clone(&opener);
                    let ready = tokio::spawn(async move {
                        task_store
                            .submit_ready(
                                task_opener,
                                attempt,
                                "https://relay.example".to_owned(),
                                "new-token".to_owned(),
                                1_900_000_000,
                            )
                            .await
                    });
                    tokio::time::timeout(
                        std::time::Duration::from_secs(5),
                        store.publication_queued.notified(),
                    )
                    .await
                    .unwrap();
                    if supersession == 1 {
                        let _ = store.capture_access_attempt(2);
                    }
                    if supersession == 2 {
                        store.invalidate();
                    }
                    release_tx.send(()).unwrap();
                    blocker.await.unwrap();
                    let result = ready.await.unwrap();
                    assert_eq!(result.is_ok(), supersession == 0);
                    let expected = if supersession == 0 {
                        Some("new-token")
                    } else {
                        None
                    };
                    assert_eq!(
                        load_credential(&root)
                            .unwrap()
                            .unwrap()
                            .device_token
                            .as_deref(),
                        expected
                    );
                    assert_eq!(store.live_credential().device_token.as_deref(), expected);
                    assert_eq!(
                        opener.live_dial_credential().device_token.as_deref(),
                        expected
                    );
                    bridge.shutdown().await;
                    fs::remove_dir_all(root).unwrap();
                });
        }
    }

    fn credential() -> Credential {
        Credential {
            client_key_pem: "test-key".to_owned(),
            client_cert_pem: "test-cert".to_owned(),
            ca_chain_pem: vec!["test-ca".to_owned()],
            ca_fp_prefix: vec![1, 2, 3, 4],
            instance_id: "test-instance".to_owned(),
            home_label: "test-home".to_owned(),
            endpoints: vec![EndpointAddr {
                host: "127.0.0.1".to_owned(),
                port: 7657,
            }],
            home_attestation: None,
            local_endpoints: None,
            relay_origin: None,
            device_token: None,
            device_token_expires_at: None,
        }
    }
}
