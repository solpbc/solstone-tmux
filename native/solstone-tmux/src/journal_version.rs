// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use serde::{Deserialize, Serialize};

use crate::health::{DiagnosticCode, HealthState, StatusHealth, read_status_health};
use crate::instance_lock::{ExistingLock, RunIdentity, inspect_existing};
use crate::journal::JournalClient;
use crate::private_link::load_credential;
use crate::storage::{StorageError, atomic_write_bytes, open_regular_readonly};

pub const JOURNAL_VERSION_FILENAME: &str = "journal-version.json";
const JOURNAL_VERSION_LOCK_FILENAME: &str = "journal-version.lock";
const SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct JournalVersionRecord {
    schema_version: u32,
    instance_id: String,
    ca_fp_prefix_hex: String,
    version: String,
    confirmed: bool,
    run_id: String,
    lock_inode: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum JournalVersionStatus {
    Unknown,
    LastKnown(String),
    Current(String),
}

impl JournalVersionStatus {
    pub fn render(&self) -> String {
        match self {
            Self::Unknown => "unknown".to_owned(),
            Self::LastKnown(version) => format!("{} (last known)", sanitize_for_terminal(version)),
            Self::Current(version) => sanitize_for_terminal(version),
        }
    }
}

pub fn read_journal_version(
    config_root: &Path,
    data_root: &Path,
    now_unix_seconds: i64,
) -> JournalVersionStatus {
    let Some(record) = read_record(config_root) else {
        return JournalVersionStatus::Unknown;
    };
    let Ok(Some(credential)) = load_credential(config_root) else {
        return JournalVersionStatus::Unknown;
    };
    if record.instance_id != credential.instance_id
        || record.ca_fp_prefix_hex != hex_encode(&credential.ca_fp_prefix)
    {
        return JournalVersionStatus::Unknown;
    }
    let identity_is_live = matches!(
        inspect_existing(data_root),
        ExistingLock::Locked(identity)
            if identity.run_id == record.run_id && identity.lock_inode == record.lock_inode
    );
    let connected = matches!(
        read_status_health(data_root, now_unix_seconds),
        StatusHealth::Live(HealthState::Connected | HealthState::Syncing)
    );
    if identity_is_live && record.confirmed && connected {
        JournalVersionStatus::Current(record.version)
    } else {
        JournalVersionStatus::LastKnown(record.version)
    }
}

fn sanitize_for_terminal(value: &str) -> String {
    let mut sanitized = String::with_capacity(value.len());
    for ch in value.chars() {
        if ch.is_control() {
            continue;
        }
        if ch == '#' {
            sanitized.push('#');
        }
        sanitized.push(ch);
    }
    sanitized
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn read_record(config_root: &Path) -> Option<JournalVersionRecord> {
    let mut file = open_regular_readonly(&config_root.join(JOURNAL_VERSION_FILENAME)).ok()?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn store_record(config_root: &Path, record: &JournalVersionRecord) -> Result<(), DiagnosticCode> {
    let bytes = serde_json::to_vec(record).map_err(|_| DiagnosticCode::PrivateStateInvalid)?;
    match atomic_write_bytes(
        &config_root.join(JOURNAL_VERSION_FILENAME),
        config_root,
        &bytes,
    ) {
        Ok(()) => Ok(()),
        Err(StorageError::InvalidTarget(_)) => Err(DiagnosticCode::PrivateStateInvalid),
        Err(_) => Err(DiagnosticCode::PrivateStateIo),
    }
}

fn with_write_lock<T>(config_root: &Path, action: impl FnOnce() -> T) -> Option<T> {
    let path = config_root.join(JOURNAL_VERSION_LOCK_FILENAME);
    let descriptor = rustix::fs::open(
        &path,
        rustix::fs::OFlags::RDWR
            | rustix::fs::OFlags::CLOEXEC
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CREATE,
        rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
    )
    .ok()?;
    let file = std::fs::File::from(descriptor);
    rustix::fs::flock(&file, rustix::fs::FlockOperation::LockExclusive).ok()?;
    let result = action();
    let _ = rustix::fs::flock(&file, rustix::fs::FlockOperation::Unlock);
    Some(result)
}

fn record_for_attempt(
    existing: Option<JournalVersionRecord>,
    instance_id: &str,
    ca_fp_prefix_hex: &str,
    run_identity: &RunIdentity,
    fetched: Option<String>,
) -> Option<JournalVersionRecord> {
    match fetched {
        Some(version) => Some(JournalVersionRecord {
            schema_version: SCHEMA_VERSION,
            instance_id: instance_id.to_owned(),
            ca_fp_prefix_hex: ca_fp_prefix_hex.to_owned(),
            version,
            confirmed: true,
            run_id: run_identity.run_id.clone(),
            lock_inode: run_identity.lock_inode,
        }),
        None => {
            let existing = existing?;
            if existing.instance_id != instance_id || existing.ca_fp_prefix_hex != ca_fp_prefix_hex
            {
                return None;
            }
            Some(JournalVersionRecord {
                confirmed: false,
                run_id: run_identity.run_id.clone(),
                lock_inode: run_identity.lock_inode,
                ..existing
            })
        }
    }
}

#[derive(Clone)]
pub struct VersionRefreshState {
    config_root: PathBuf,
    data_root: PathBuf,
    instance_id: String,
    ca_fp_prefix_hex: String,
    run_identity: RunIdentity,
    generation: Arc<AtomicU64>,
    generation_guard: Arc<Mutex<()>>,
    journal_client: Arc<Mutex<Option<JournalClient>>>,
}

impl VersionRefreshState {
    pub fn new(
        config_root: PathBuf,
        data_root: PathBuf,
        instance_id: String,
        ca_fp_prefix: &[u8],
        run_identity: RunIdentity,
    ) -> Self {
        Self {
            config_root,
            data_root,
            instance_id,
            ca_fp_prefix_hex: hex_encode(ca_fp_prefix),
            run_identity,
            generation: Arc::new(AtomicU64::new(0)),
            generation_guard: Arc::new(Mutex::new(())),
            journal_client: Arc::new(Mutex::new(None)),
        }
    }

    pub fn attach_client(&self, client: JournalClient) {
        *lock(&self.journal_client) = Some(client);
        self.spawn_refresh();
    }

    pub(crate) fn note_redial(&self) {
        self.spawn_refresh();
    }

    pub(crate) fn note_dial_failed(&self) {
        let generation = self.begin_refresh();
        let refresh = self.clone();
        tokio::task::spawn_blocking(move || refresh.validate_and_store(generation, None));
    }

    pub(crate) fn note_session_started(&self) {
        *lock(&self.journal_client) = None;
        self.note_dial_failed();
    }

    fn identity_and_credential_live(&self) -> bool {
        let identity_is_live = matches!(
            inspect_existing(&self.data_root),
            ExistingLock::Locked(identity)
                if identity.run_id == self.run_identity.run_id
                    && identity.lock_inode == self.run_identity.lock_inode
        );
        identity_is_live
            && matches!(
                load_credential(&self.config_root),
                Ok(Some(credential))
                    if credential.instance_id == self.instance_id
                        && hex_encode(&credential.ca_fp_prefix) == self.ca_fp_prefix_hex
            )
    }

    fn begin_refresh(&self) -> u64 {
        let _guard = lock(&self.generation_guard);
        self.generation.fetch_add(1, Ordering::SeqCst) + 1
    }

    pub(crate) fn validate_and_store(&self, this_generation: u64, fetched: Option<String>) -> bool {
        let _guard = lock(&self.generation_guard);
        with_write_lock(&self.config_root, || {
            if self.generation.load(Ordering::SeqCst) != this_generation {
                return false;
            }
            if !self.identity_and_credential_live() {
                return false;
            }
            let Some(record) = record_for_attempt(
                read_record(&self.config_root),
                &self.instance_id,
                &self.ca_fp_prefix_hex,
                &self.run_identity,
                fetched,
            ) else {
                return false;
            };
            store_record(&self.config_root, &record).is_ok()
        })
        .unwrap_or(false)
    }

    fn spawn_refresh(&self) {
        let client = lock(&self.journal_client).clone();
        let Some(client) = client else { return };
        // Claim the generation at the lifecycle event, before queued work can
        // race a subsequent session or dial. Only the network read is deferred.
        let this_generation = self.begin_refresh();
        let refresh = self.clone();
        tokio::spawn(async move {
            let invalidator = refresh.clone();
            let _ = tokio::task::spawn_blocking(move || {
                invalidator.validate_and_store(this_generation, None)
            })
            .await;
            let fetched = client.system_status().await.ok();
            let _ = tokio::task::spawn_blocking(move || {
                refresh.validate_and_store(this_generation, fetched)
            })
            .await;
        });
    }
}

pub(crate) fn clear_cached_version(config_root: &Path) {
    let _ = std::fs::remove_file(config_root.join(JOURNAL_VERSION_FILENAME));
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::Ordering;
    use std::time::{SystemTime, UNIX_EPOCH};

    use spl_transport::credential::Credential;

    use super::{
        JOURNAL_VERSION_FILENAME, JournalVersionRecord, JournalVersionStatus, SCHEMA_VERSION,
        VersionRefreshState, clear_cached_version, hex_encode, read_journal_version, read_record,
        sanitize_for_terminal, store_record,
    };
    use crate::health::{HealthState, HealthWriter, SyncFacts};
    use crate::instance_lock::{InstanceLock, RunIdentity};
    use crate::paths::ensure_private_directory;
    use crate::private_link::persist_credential;

    fn test_tempdir(prefix: &str) -> PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("solstone-{prefix}-{}-{suffix}", std::process::id()));
        ensure_private_directory(&path).expect("create test tempdir");
        path
    }

    fn write_test_health(data_root: &Path, lock: &InstanceLock, state: HealthState, now: i64) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let writer = HealthWriter::new(data_root.to_path_buf(), lock);
        let mut facts = SyncFacts {
            paired: true,
            ..SyncFacts::default()
        };
        match state {
            HealthState::Connected => {
                facts.successful_contact(now);
            }
            HealthState::Syncing => {
                facts.successful_contact(now);
                facts.sync_in_progress = true;
            }
            _ => {}
        }
        rt.block_on(writer.write(&facts, now))
            .expect("write health");
    }

    fn sample_credential(instance_id: &str, ca_fp_prefix: &[u8]) -> Credential {
        Credential {
            client_key_pem: "test-key".to_owned(),
            client_cert_pem: "test-cert".to_owned(),
            ca_chain_pem: vec!["test-ca".to_owned()],
            ca_fp_prefix: ca_fp_prefix.to_vec(),
            instance_id: instance_id.to_owned(),
            home_label: "test-home".to_owned(),
            endpoints: vec![],
            home_attestation: None,
            local_endpoints: None,
            relay_origin: None,
            device_token: None,
            device_token_expires_at: None,
        }
    }

    #[test]
    fn terminal_sanitizer_removes_controls_and_doubles_hash() {
        assert_eq!(sanitize_for_terminal("2026.8.0"), "2026.8.0");
        assert_eq!(sanitize_for_terminal("2026.8.0-rc.1"), "2026.8.0-rc.1");
        assert_eq!(
            sanitize_for_terminal("\x1b[31m2026.8.0\x1b[0m\x00\x07\x7f"),
            "[31m2026.8.0[0m"
        );
        assert_eq!(
            sanitize_for_terminal("version#1.0#tag"),
            "version##1.0##tag"
        );
    }

    #[test]
    fn journal_version_status_render_exact_strings() {
        assert_eq!(JournalVersionStatus::Unknown.render(), "unknown");
        assert_eq!(
            JournalVersionStatus::LastKnown("2026.8.0".to_owned()).render(),
            "2026.8.0 (last known)"
        );
        assert_eq!(
            JournalVersionStatus::LastKnown("v#1\x1b[0m".to_owned()).render(),
            "v##1[0m (last known)"
        );
        assert_eq!(
            JournalVersionStatus::Current("2026.8.0".to_owned()).render(),
            "2026.8.0"
        );
        assert_eq!(
            JournalVersionStatus::Current("v#1\x00".to_owned()).render(),
            "v##1"
        );
    }

    #[test]
    fn generation_guard_race_discards_stale_completion() {
        let config_root = test_tempdir("gen-guard-config");
        let data_root = test_tempdir("gen-guard-data");

        let cred = sample_credential("instance-1", &[0xaa, 0xbb]);
        persist_credential(&config_root, &cred).expect("persist cred");
        let lock = InstanceLock::acquire(&data_root).expect("acquire lock");
        let run_identity = lock.identity().clone();

        let state = VersionRefreshState::new(
            config_root.clone(),
            data_root.clone(),
            "instance-1".to_owned(),
            &[0xaa, 0xbb],
            run_identity,
        );

        // Advance generation to 2
        state.generation.store(2, Ordering::SeqCst);

        // Generation 2 completes and writes its record
        let stored = state.validate_and_store(2, Some("2026.9.0".to_owned()));
        assert!(stored);

        let record = read_record(&config_root).expect("record written by gen 2");
        assert_eq!(record.version, "2026.9.0");
        assert!(record.confirmed);

        // Generation 1 completes late; should be discarded
        let stored = state.validate_and_store(1, Some("2026.8.0".to_owned()));
        assert!(!stored);

        let record = read_record(&config_root).expect("record after gen 1");
        assert_eq!(record.version, "2026.9.0");
        assert!(record.confirmed);

        let _ = fs::remove_dir_all(config_root);
        let _ = fs::remove_dir_all(data_root);
    }

    #[test]
    fn replaced_session_obsolete_completion_cannot_clobber_newer_write() {
        let config_root = test_tempdir("obsolete-session-config");
        let data_root = test_tempdir("obsolete-session-data");

        // 1. Same journal identity, but old/replaced RunIdentity
        let cred = sample_credential("instance-1", &[0xaa, 0xbb]);
        persist_credential(&config_root, &cred).expect("persist cred");

        let live_lock = InstanceLock::acquire(&data_root).expect("acquire live lock");
        let live_identity = live_lock.identity().clone();

        let old_identity = RunIdentity {
            run_id: "00000000000000000000000000000000".to_owned(),
            lock_inode: 99999,
        };

        let live_state = VersionRefreshState::new(
            config_root.clone(),
            data_root.clone(),
            "instance-1".to_owned(),
            &[0xaa, 0xbb],
            live_identity.clone(),
        );

        // Live session wrote its record
        live_state.generation.store(1, Ordering::SeqCst);
        let stored = live_state.validate_and_store(1, Some("2026.10.0".to_owned()));
        assert!(stored);

        // Obsolete in-flight completion for old session (even with matching gen=1 in its own context)
        let old_state = VersionRefreshState::new(
            config_root.clone(),
            data_root.clone(),
            "instance-1".to_owned(),
            &[0xaa, 0xbb],
            old_identity,
        );
        old_state.generation.store(1, Ordering::SeqCst);
        let stored_old = old_state.validate_and_store(1, Some("2026.9.0".to_owned()));
        assert!(
            !stored_old,
            "obsolete session with same journal identity must be refused"
        );

        let current_record = read_record(&config_root).expect("record");
        assert_eq!(current_record.version, "2026.10.0");

        // 2. Different journal identity should also be refused
        let other_state = VersionRefreshState::new(
            config_root.clone(),
            data_root.clone(),
            "instance-other".to_owned(),
            &[0x11, 0x22],
            live_identity,
        );
        other_state.generation.store(1, Ordering::SeqCst);
        let stored_other = other_state.validate_and_store(1, Some("2026.8.0".to_owned()));
        assert!(!stored_other, "mismatched credential must be refused");

        let current_record = read_record(&config_root).expect("record");
        assert_eq!(current_record.version, "2026.10.0");

        let _ = fs::remove_dir_all(config_root);
        let _ = fs::remove_dir_all(data_root);
    }

    #[test]
    fn failed_fetch_flips_confirmed_to_false_and_recovers() {
        let config_root = test_tempdir("failed-fetch-config");
        let data_root = test_tempdir("failed-fetch-data");
        let now = 1_800_000_000;

        let cred = sample_credential("inst-1", &[0xaa, 0xbb]);
        persist_credential(&config_root, &cred).expect("persist cred");
        let lock = InstanceLock::acquire(&data_root).expect("lock");
        let identity = lock.identity().clone();
        write_test_health(&data_root, &lock, HealthState::Connected, now);

        let state = VersionRefreshState::new(
            config_root.clone(),
            data_root.clone(),
            "inst-1".to_owned(),
            &[0xaa, 0xbb],
            identity,
        );

        // 1. Successful fetch -> Current
        state.generation.store(1, Ordering::SeqCst);
        let stored = state.validate_and_store(1, Some("2026.8.0".to_owned()));
        assert!(stored);
        assert_eq!(
            read_journal_version(&config_root, &data_root, now),
            JournalVersionStatus::Current("2026.8.0".to_owned())
        );

        // 2. Failed fetch for same identity -> confirmed flips false -> LastKnown (version preserved)
        state.generation.store(2, Ordering::SeqCst);
        let stored = state.validate_and_store(2, None);
        assert!(stored);
        assert_eq!(
            read_journal_version(&config_root, &data_root, now),
            JournalVersionStatus::LastKnown("2026.8.0".to_owned())
        );

        // 3. New successful fetch with newer version -> Current("2026.8.1")
        state.generation.store(3, Ordering::SeqCst);
        let stored = state.validate_and_store(3, Some("2026.8.1".to_owned()));
        assert!(stored);
        assert_eq!(
            read_journal_version(&config_root, &data_root, now),
            JournalVersionStatus::Current("2026.8.1".to_owned())
        );

        let _ = fs::remove_dir_all(config_root);
        let _ = fs::remove_dir_all(data_root);
    }

    #[test]
    fn read_journal_version_states_and_mismatch() {
        let config_root = test_tempdir("read-version-config");
        let data_root = test_tempdir("read-version-data");
        let now = 1_800_000_000;

        let cred = sample_credential("inst-42", &[0x12, 0x34]);
        persist_credential(&config_root, &cred).expect("persist credential");

        // 1. No record -> Unknown
        assert_eq!(
            read_journal_version(&config_root, &data_root, now),
            JournalVersionStatus::Unknown
        );

        let lock = InstanceLock::acquire(&data_root).expect("acquire lock");
        let identity = lock.identity().clone();
        write_test_health(&data_root, &lock, HealthState::Connected, now);

        let record = JournalVersionRecord {
            schema_version: SCHEMA_VERSION,
            instance_id: "inst-42".to_owned(),
            ca_fp_prefix_hex: hex_encode(&[0x12, 0x34]),
            version: "2026.8.0".to_owned(),
            confirmed: true,
            run_id: identity.run_id.clone(),
            lock_inode: identity.lock_inode,
        };
        store_record(&config_root, &record).expect("store record");

        // 2. Lock held with matching identity and confirmed=true and connected -> Current
        assert_eq!(
            read_journal_version(&config_root, &data_root, now),
            JournalVersionStatus::Current("2026.8.0".to_owned())
        );

        // 3. Record confirmed=false -> LastKnown
        let unconfirmed_record = JournalVersionRecord {
            confirmed: false,
            ..record.clone()
        };
        store_record(&config_root, &unconfirmed_record).expect("store record");
        assert_eq!(
            read_journal_version(&config_root, &data_root, now),
            JournalVersionStatus::LastKnown("2026.8.0".to_owned())
        );

        // 4. Drop lock -> LastKnown
        store_record(&config_root, &record).expect("store record");
        drop(lock);
        assert_eq!(
            read_journal_version(&config_root, &data_root, now),
            JournalVersionStatus::LastKnown("2026.8.0".to_owned())
        );

        // 5. Mismatched credential -> Unknown
        let mismatched_cred = sample_credential("inst-other", &[0x12, 0x34]);
        persist_credential(&config_root, &mismatched_cred).expect("persist mismatched credential");
        assert_eq!(
            read_journal_version(&config_root, &data_root, now),
            JournalVersionStatus::Unknown
        );

        let _ = fs::remove_dir_all(config_root);
        let _ = fs::remove_dir_all(data_root);
    }

    #[test]
    fn read_journal_version_requires_both_confirmed_and_live_health() {
        let config_root = test_tempdir("both-signals-config");
        let data_root = test_tempdir("both-signals-data");
        let now = 1_800_000_000;

        let cred = sample_credential("inst-1", &[0xaa, 0xbb]);
        persist_credential(&config_root, &cred).expect("persist cred");
        let lock = InstanceLock::acquire(&data_root).expect("lock");
        let identity = lock.identity().clone();

        // 1. confirmed: true + live identity + Offline -> LastKnown
        write_test_health(&data_root, &lock, HealthState::Offline, now);
        let record = JournalVersionRecord {
            schema_version: SCHEMA_VERSION,
            instance_id: "inst-1".to_owned(),
            ca_fp_prefix_hex: hex_encode(&[0xaa, 0xbb]),
            version: "2026.8.0".to_owned(),
            confirmed: true,
            run_id: identity.run_id.clone(),
            lock_inode: identity.lock_inode,
        };
        store_record(&config_root, &record).expect("store");
        assert_eq!(
            read_journal_version(&config_root, &data_root, now),
            JournalVersionStatus::LastKnown("2026.8.0".to_owned())
        );

        // 2. confirmed: false + live identity + Connected -> LastKnown
        write_test_health(&data_root, &lock, HealthState::Connected, now);
        let unconfirmed_record = JournalVersionRecord {
            confirmed: false,
            ..record.clone()
        };
        store_record(&config_root, &unconfirmed_record).expect("store");
        assert_eq!(
            read_journal_version(&config_root, &data_root, now),
            JournalVersionStatus::LastKnown("2026.8.0".to_owned())
        );

        // 3. confirmed: true + live identity + Connected -> Current
        store_record(&config_root, &record).expect("store");
        assert_eq!(
            read_journal_version(&config_root, &data_root, now),
            JournalVersionStatus::Current("2026.8.0".to_owned())
        );

        // 4. confirmed: true + live identity + Syncing -> Current
        write_test_health(&data_root, &lock, HealthState::Syncing, now);
        assert_eq!(
            read_journal_version(&config_root, &data_root, now),
            JournalVersionStatus::Current("2026.8.0".to_owned())
        );

        let _ = fs::remove_dir_all(config_root);
        let _ = fs::remove_dir_all(data_root);
    }

    #[test]
    fn clear_cached_version_removes_file_and_reports_unknown() {
        let config_root = test_tempdir("clear-cache-config");
        let data_root = test_tempdir("clear-cache-data");
        let now = 1_800_000_000;

        let cred = sample_credential("inst-1", &[0xaa, 0xbb]);
        persist_credential(&config_root, &cred).expect("persist cred");
        let lock = InstanceLock::acquire(&data_root).expect("lock");
        let identity = lock.identity().clone();

        let record = JournalVersionRecord {
            schema_version: SCHEMA_VERSION,
            instance_id: "inst-1".to_owned(),
            ca_fp_prefix_hex: hex_encode(&[0xaa, 0xbb]),
            version: "2026.8.0".to_owned(),
            confirmed: true,
            run_id: identity.run_id.clone(),
            lock_inode: identity.lock_inode,
        };
        store_record(&config_root, &record).expect("store");
        write_test_health(&data_root, &lock, HealthState::Connected, now);
        assert_eq!(
            read_journal_version(&config_root, &data_root, now),
            JournalVersionStatus::Current("2026.8.0".to_owned())
        );

        clear_cached_version(&config_root);

        assert!(!config_root.join(JOURNAL_VERSION_FILENAME).exists());
        assert_eq!(
            read_journal_version(&config_root, &data_root, now),
            JournalVersionStatus::Unknown
        );

        let _ = fs::remove_dir_all(config_root);
        let _ = fs::remove_dir_all(data_root);
    }
}
