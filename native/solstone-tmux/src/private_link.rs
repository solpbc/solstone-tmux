// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::fs::{self, File};
use std::future::Future;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex, RwLock, Weak};

use serde_json::{Map, Value};
use spl_core::bridge::BridgeNames;
use spl_transport::TransportError;
use spl_transport::client::{DialedCarrier, TokenPersistHook, TransportClient};
use spl_transport::credential::Credential;
use spl_transport::journal_bridge::{
    BridgePolicy, CapabilityGate, CarrierOpener, JournalBridgeConfig, JournalBridgeHandle,
    JournalBridgeStatus, JournalBridgeStatusSubscription, JournalBridgeTerminalReason,
};
use spl_transport::pairing::pair_from_link;

use crate::config::system_hostname;
use crate::health::DiagnosticCode;
use crate::instance_lock::InstanceLock;
use crate::journal_version::{VersionRefreshState, hex_encode};
use crate::paths::{
    Environment, PlatformKind, ensure_private_directory, resolve_config_root, resolve_data_root,
};
use crate::post_connect::{PostConnectCoordinator, compute_pairing_generation};
use crate::storage::{StorageError, atomic_write_bytes, open_regular_readonly};

pub const CREDENTIALS_FILENAME: &str = "credentials.json";
const PRIVATE_STATE_LOCK_FILENAME: &str = ".solstone-tmux.private-state.lock";
const MAX_PAIR_LINK_BYTES: u64 = 4096;
pub const MAX_REQUEST_BODY_BYTES: usize = 128 * 1024 * 1024;
const CAPABILITY_COOKIE_NAME: &str = "solstone_tmux_cap";
const UPSTREAM_COOKIE_PREFIX: &str = "solstone_tmux_";
pub const OBSERVER_HEADER_NAME: &str = "x-solstone-observer";
pub const PROTOCOL_VERSION_HEADER_NAME: &str = "x-solstone-protocol-version";
pub const PROTOCOL_VERSION: &str = "3";
pub const PROTOCOL_VERSION_NUMBER: u64 = 3;

pub struct PrivateLinkOpener {
    state: RwLock<OpenerState>,
    coordinator: Mutex<Option<Weak<PostConnectCoordinator>>>,
}

struct OpenerState {
    incarnation: u64,
    retired: bool,
    mode: OpenerMode,
}

enum OpenerMode {
    Transport {
        transport: Arc<TransportClient>,
        credential: Credential,
        access_revision: u64,
    },
    Disabled {
        credential: Credential,
        access_revision: u64,
    },
}

impl PrivateLinkOpener {
    fn new(
        transport: TransportClient,
        credential: Credential,
        _refresh: VersionRefreshState,
    ) -> Self {
        Self {
            state: RwLock::new(OpenerState {
                incarnation: 0,
                retired: false,
                mode: OpenerMode::Transport {
                    transport: Arc::new(transport),
                    credential,
                    access_revision: 0,
                },
            }),
            coordinator: Mutex::new(None),
        }
    }

    pub(crate) fn attach_coordinator(&self, coordinator: &Arc<PostConnectCoordinator>) {
        *self.coordinator.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(Arc::downgrade(coordinator));
    }

    pub(crate) fn install_transport(
        &self,
        transport: Arc<TransportClient>,
        credential: Credential,
        access_revision: u64,
    ) {
        let mut state = self.state.write().unwrap_or_else(|e| e.into_inner());
        if state.retired {
            return;
        }
        state.incarnation = state.incarnation.wrapping_add(1);
        state.mode = OpenerMode::Transport {
            transport,
            credential,
            access_revision,
        };
    }

    pub(crate) fn install_disabled(&self, credential: Credential, access_revision: u64) {
        let mut state = self.state.write().unwrap_or_else(|e| e.into_inner());
        if state.retired {
            return;
        }
        state.incarnation = state.incarnation.wrapping_add(1);
        state.mode = OpenerMode::Disabled {
            credential,
            access_revision,
        };
    }

    pub(crate) fn retire(&self) {
        let mut state = self.state.write().unwrap_or_else(|e| e.into_inner());
        state.retired = true;
        state.incarnation = state.incarnation.wrapping_add(1);
    }

    pub fn live_dial_credential(&self) -> Credential {
        let state = self.state.read().unwrap_or_else(|e| e.into_inner());
        match &state.mode {
            OpenerMode::Transport { credential, .. } | OpenerMode::Disabled { credential, .. } => {
                credential.clone()
            }
        }
    }
}

impl CarrierOpener for PrivateLinkOpener {
    fn proxy_headers(
        &self,
        upstream_headers: &[(String, String)],
    ) -> Result<Vec<(String, String)>, TransportError> {
        let mut headers = upstream_headers.to_vec();
        headers.push((
            PROTOCOL_VERSION_HEADER_NAME.to_owned(),
            PROTOCOL_VERSION.to_owned(),
        ));
        Ok(headers)
    }

    fn dial_carrier(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<DialedCarrier, TransportError>> + Send + '_>> {
        let coordinator = self
            .coordinator
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .and_then(Weak::upgrade);
        let job_induced = coordinator
            .as_ref()
            .is_some_and(|coordinator| coordinator.burst_is_active());
        let snapshot = {
            let state = self.state.read().unwrap_or_else(|e| e.into_inner());
            match &state.mode {
                _ if state.retired => None,
                OpenerMode::Transport {
                    transport,
                    access_revision,
                    ..
                } => Some((state.incarnation, *access_revision, Arc::clone(transport))),
                OpenerMode::Disabled {
                    access_revision, ..
                } => {
                    let _ = access_revision;
                    None
                }
            }
        };
        Box::pin(async move {
            let Some((incarnation, _access_revision, transport)) = snapshot else {
                return Err(TransportError::NoEndpoint);
            };
            match transport.dial_carrier().await {
                Ok(dialed) => {
                    let current = self.state.read().unwrap_or_else(|e| e.into_inner());
                    if current.retired || current.incarnation != incarnation {
                        drop(current);
                        drop(dialed);
                        return Err(TransportError::NoEndpoint);
                    }
                    drop(current);
                    // A job-induced dial can finish after its request deadline.
                    // Its completion must not be reclassified as a fresh reconnect.
                    if !job_induced && let Some(coordinator) = coordinator {
                        coordinator.note_successful_dial();
                    }
                    Ok(dialed)
                }
                Err(error) => Err(error),
            }
        })
    }
}

pub struct PrivateLinkBridge {
    opener: Arc<PrivateLinkOpener>,
    handle: JournalBridgeHandle,
}

impl PrivateLinkBridge {
    pub async fn start(
        credential: Credential,
        token_persist: Option<TokenPersistHook>,
        refresh: VersionRefreshState,
    ) -> Result<Self, DiagnosticCode> {
        let endpoint_hosts = credential
            .endpoints
            .iter()
            .map(|endpoint| endpoint.host.clone())
            .collect();
        let transport = if credential.endpoints.is_empty() {
            TransportClient::new_relay_only(credential.clone(), token_persist)
        } else {
            TransportClient::new(credential.clone(), token_persist)
        }
        .map_err(|_| DiagnosticCode::BridgeUnavailable)?;
        let opener = Arc::new(PrivateLinkOpener::new(transport, credential, refresh));
        let bridge_names = BridgeNames {
            capability_cookie_name: CAPABILITY_COOKIE_NAME.to_owned(),
            upstream_cookie_prefix: UPSTREAM_COOKIE_PREFIX.to_owned(),
            observer_header_name: OBSERVER_HEADER_NAME.to_owned(),
            protocol_version_header_name: PROTOCOL_VERSION_HEADER_NAME.to_owned(),
        };
        let policy = BridgePolicy {
            port: 0,
            capability_gate: CapabilityGate::Enabled,
            max_request_body_bytes: MAX_REQUEST_BODY_BYTES,
            ..BridgePolicy::default()
        };
        let handle = spl_transport::journal_bridge::start(JournalBridgeConfig {
            opener: opener.clone(),
            bridge_names,
            endpoint_hosts,
            policy,
        })
        .await
        .map_err(|_| DiagnosticCode::BridgeUnavailable)?;
        Ok(Self { opener, handle })
    }

    pub fn bootstrap_url(&self) -> Result<String, DiagnosticCode> {
        self.handle
            .bootstrap_url()
            .ok_or(DiagnosticCode::BridgeUnavailable)
    }

    pub fn loopback_origin(&self) -> String {
        format!("http://127.0.0.1:{}", self.handle.port())
    }

    pub fn opener(&self) -> &Arc<PrivateLinkOpener> {
        &self.opener
    }

    pub fn status(&self) -> JournalBridgeStatus {
        self.handle.status()
    }

    pub fn subscribe_status(&self) -> JournalBridgeStatusSubscription {
        self.handle.subscribe_status()
    }

    /// Why the bridge stopped dialing, if it has: the journal refused this device
    /// with access denied (alert 49), or other refusals reached the bridge's bound.
    /// Either way the bridge never dials again and every request answers 502.
    pub fn stop_reason(&self) -> Option<JournalBridgeTerminalReason> {
        self.handle.status().terminal_reason
    }

    pub async fn shutdown(self) {
        self.opener.retire();
        self.handle.shutdown_and_wait().await;
    }
}

const MAX_CLIENT_LABEL_BYTES: usize = 253;
const FALLBACK_DEVICE_LABEL: &str = "tmux";

pub fn pairing_ceremony_identity(
    platform: PlatformKind,
    hostname: Result<String, impl std::fmt::Debug>,
) -> (String, Map<String, Value>) {
    let platform = Value::String(platform.pairing_platform().to_owned());
    match hostname {
        Ok(hostname) if (1..=MAX_CLIENT_LABEL_BYTES).contains(&hostname.len()) => {
            let mut additional_fields = Map::new();
            additional_fields.insert("client_label".to_owned(), Value::String(hostname.clone()));
            additional_fields.insert("platform".to_owned(), platform);
            (hostname, additional_fields)
        }
        Ok(_) | Err(_) => {
            let mut additional_fields = Map::new();
            additional_fields.insert("platform".to_owned(), platform);
            (FALLBACK_DEVICE_LABEL.to_owned(), additional_fields)
        }
    }
}

/// The spoken form of a paired journal's mark: the two chip-tint names and
/// the two identity words, in the fixed order the mark is read in.
/// `None` when the paired instance ID is not a well-formed journal ID.
pub fn format_spoken_mark(jid: &str) -> Option<String> {
    let mark = spl_core::mark::mark_from_jid(jid).ok()?;
    let spec = mark.to_render_spec();
    Some(format!(
        "{}, {} · {}·{}",
        spec.icon1.color.name, spec.icon2.color.name, spec.words[0], spec.words[1]
    ))
}

pub async fn setup<R, T>(
    platform: PlatformKind,
    environment: &dyn Environment,
    input: R,
    seat: impl Into<crate::pairing_answer::TerminalSeat<T>>,
    mark: crate::pairing_answer::MarkOption,
) -> crate::pairing_answer::Outcome
where
    R: Read,
    T: std::io::Read + std::io::Write + Send + 'static,
{
    let marker_digest =
        crate::device_migration::host_marker_digest(platform, &crate::command::TokioCommandRunner)
            .await
            .ok();
    setup_with_identity_and_marker(
        platform,
        environment,
        input,
        system_hostname(),
        seat,
        mark,
        marker_digest,
    )
    .await
}

pub async fn setup_with_identity<R, T, E>(
    platform: PlatformKind,
    environment: &dyn Environment,
    input: R,
    hostname: Result<String, E>,
    seat: impl Into<crate::pairing_answer::TerminalSeat<T>>,
    mark: crate::pairing_answer::MarkOption,
) -> crate::pairing_answer::Outcome
where
    R: Read,
    T: std::io::Read + std::io::Write + Send + 'static,
    E: std::fmt::Debug,
{
    setup_with_identity_and_marker(platform, environment, input, hostname, seat, mark, None).await
}

async fn setup_with_identity_and_marker<R, T, E>(
    platform: PlatformKind,
    environment: &dyn Environment,
    input: R,
    hostname: Result<String, E>,
    seat: impl Into<crate::pairing_answer::TerminalSeat<T>>,
    mark: crate::pairing_answer::MarkOption,
    marker_digest: Option<String>,
) -> crate::pairing_answer::Outcome
where
    R: Read,
    T: std::io::Read + std::io::Write + Send + 'static,
    E: std::fmt::Debug,
{
    setup_with_pairer_and_marker(
        platform,
        environment,
        input,
        hostname,
        |link, device_label, additional_fields| async move {
            pair_from_link(&link, &device_label, &additional_fields)
                .await
                .map_err(|_| DiagnosticCode::PairingFailed)
        },
        seat,
        mark,
        marker_digest,
    )
    .await
}

pub async fn setup_with_pairer<R, T, E, F, Fut>(
    platform: PlatformKind,
    environment: &dyn Environment,
    input: R,
    hostname: Result<String, E>,
    pairer: F,
    seat: impl Into<crate::pairing_answer::TerminalSeat<T>>,
    mark: crate::pairing_answer::MarkOption,
) -> crate::pairing_answer::Outcome
where
    R: Read,
    T: std::io::Read + std::io::Write + Send + 'static,
    E: std::fmt::Debug,
    F: FnOnce(String, String, Map<String, Value>) -> Fut,
    Fut: Future<Output = Result<Credential, DiagnosticCode>>,
{
    setup_with_pairer_and_marker(
        platform,
        environment,
        input,
        hostname,
        pairer,
        seat,
        mark,
        None,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn setup_with_pairer_and_marker<R, T, E, F, Fut>(
    platform: PlatformKind,
    environment: &dyn Environment,
    input: R,
    hostname: Result<String, E>,
    pairer: F,
    seat: impl Into<crate::pairing_answer::TerminalSeat<T>>,
    mark: crate::pairing_answer::MarkOption,
    marker_digest: Option<String>,
) -> crate::pairing_answer::Outcome
where
    R: Read,
    T: std::io::Read + std::io::Write + Send + 'static,
    E: std::fmt::Debug,
    F: FnOnce(String, String, Map<String, Value>) -> Fut,
    Fut: Future<Output = Result<Credential, DiagnosticCode>>,
{
    use crate::pairing_answer::*;
    let seat = seat.into();

    let data_root = match resolve_data_root(platform, environment) {
        Ok(root) => root,
        Err(_) => return Outcome::Diagnostic(DiagnosticCode::SetupUnavailable),
    };
    let _instance_lock = match InstanceLock::acquire_existing(&data_root) {
        Ok(lock) => lock,
        Err(_) => return Outcome::Diagnostic(DiagnosticCode::SetupUnavailable),
    };
    let config_root = match resolve_config_root(platform, environment) {
        Ok(root) => root,
        Err(_) => return Outcome::Diagnostic(DiagnosticCode::SetupUnavailable),
    };
    if ensure_private_directory(&config_root).is_err() {
        return Outcome::Diagnostic(DiagnosticCode::SetupUnavailable);
    }
    let _private_state_lock = match acquire_private_state_lock(&config_root) {
        Ok(lock) => lock,
        Err(code) => return Outcome::Diagnostic(code),
    };

    enum TerminalQuestionReader<T> {
        Production(File),
        Scripted(T),
    }

    let mut terminal_reader = match &mark {
        MarkOption::MissingValue | MarkOption::Repeated => return Outcome::Usage(MARK_USAGE),
        MarkOption::Value(val) => {
            let words = split_mark_words(val);
            if words.len() != 2 {
                return Outcome::Usage(MARK_USAGE);
            }
            None
        }
        MarkOption::Absent => match seat {
            TerminalSeat::Production => {
                let Some(file) = open_owner_terminal() else {
                    return Outcome::Owner {
                        code: 1,
                        lines: vec![SETUP_NO_TERMINAL.to_owned()],
                    };
                };
                Some(TerminalQuestionReader::Production(file))
            }
            TerminalSeat::Scripted(Some(term)) => Some(TerminalQuestionReader::Scripted(term)),
            TerminalSeat::Scripted(None) => {
                return Outcome::Owner {
                    code: 1,
                    lines: vec![SETUP_NO_TERMINAL.to_owned()],
                };
            }
        },
    };

    // Grandfather or settle
    let answer_lock = match acquire_answer_lock(&config_root).await {
        Ok(lock) => lock,
        Err(code) => return Outcome::Diagnostic(code),
    };
    let grandfather_root = config_root.clone();
    let grandfather_res = tokio::task::spawn_blocking(move || {
        crate::device_migration::grandfather_or_settle_if_idle(&grandfather_root)
    })
    .await
    .map_err(|_| DiagnosticCode::PrivateStateIo)
    .and_then(|result| result);
    drop(answer_lock);
    if let Err(code) = grandfather_res {
        return Outcome::Diagnostic(code);
    }

    let (device_label, additional_fields) = pairing_ceremony_identity(platform, hostname);
    let link = match read_pair_link(input) {
        Ok(link) => link,
        Err(code) => return Outcome::Diagnostic(code),
    };

    let credential = match pairer(link, device_label, additional_fields).await {
        Ok(cred) => cred,
        Err(code) => return Outcome::Diagnostic(code),
    };

    let spoken_mark = format_spoken_mark(&credential.instance_id);
    let is_identified = spoken_mark.is_some();
    let generation_hex = hex_encode(&compute_pairing_generation(&credential.client_cert_pem));

    match mark {
        MarkOption::Value(val) => {
            let eval = evaluate_mark_words(&val, &credential.instance_id);
            match eval {
                MarkMatch::Usage => {
                    retire_credential(&credential, &config_root).await;
                    Outcome::Usage(MARK_USAGE)
                }
                MarkMatch::Match => {
                    let answer_lock = match acquire_answer_lock(&config_root).await {
                        Ok(lock) => lock,
                        Err(code) => return Outcome::Diagnostic(code),
                    };
                    if let Err(code) = persist_credential(&config_root, &credential) {
                        drop(answer_lock);
                        return Outcome::Diagnostic(code);
                    }
                    crate::journal_version::clear_cached_version(&config_root);
                    let write_ans = write_answer_file(&config_root, &generation_hex);
                    let root = config_root.clone();
                    let marker = marker_digest.clone();
                    let result = tokio::task::spawn_blocking(move || {
                        crate::device_migration::record_setup_baseline(&root, marker.as_deref())
                    })
                    .await;
                    if !matches!(result, Ok(Ok(()))) {
                        drop(answer_lock);
                        return Outcome::Diagnostic(DiagnosticCode::PrivateStateIo);
                    }
                    drop(answer_lock);
                    if write_ans.is_err() {
                        Outcome::Owner {
                            code: 5,
                            lines: vec![HELD.to_owned(), RUN_LINE.to_owned()],
                        }
                    } else {
                        Outcome::Owner {
                            code: 0,
                            lines: vec![PAIRED.to_owned()],
                        }
                    }
                }
                MarkMatch::Mismatch => {
                    retire_credential(&credential, &config_root).await;
                    if !is_identified {
                        Outcome::Owner {
                            code: 1,
                            lines: vec![
                                COULDNT_VERIFY.to_owned(),
                                MARK_UNVERIFIABLE_SETUP.to_owned(),
                            ],
                        }
                    } else {
                        Outcome::Owner {
                            code: 1,
                            lines: vec![NOT_PAIRED.to_owned(), MARK_MISMATCH.to_owned()],
                        }
                    }
                }
            }
        }
        MarkOption::Absent => {
            let decision = match terminal_reader.take().unwrap() {
                TerminalQuestionReader::Production(file) => {
                    ask_terminal_question_production(file, &credential.instance_id).await
                }
                TerminalQuestionReader::Scripted(mut term) => {
                    ask_terminal_question_scripted(&mut term, &credential.instance_id).await
                }
            };
            match decision {
                TerminalDecision::Yes => {
                    let answer_lock = match acquire_answer_lock(&config_root).await {
                        Ok(lock) => lock,
                        Err(code) => return Outcome::Diagnostic(code),
                    };
                    if let Err(code) = persist_credential(&config_root, &credential) {
                        drop(answer_lock);
                        return Outcome::Diagnostic(code);
                    }
                    crate::journal_version::clear_cached_version(&config_root);
                    let write_ans = write_answer_file(&config_root, &generation_hex);
                    let root = config_root.clone();
                    let marker = marker_digest.clone();
                    let result = tokio::task::spawn_blocking(move || {
                        crate::device_migration::record_setup_baseline(&root, marker.as_deref())
                    })
                    .await;
                    if !matches!(result, Ok(Ok(()))) {
                        drop(answer_lock);
                        return Outcome::Diagnostic(DiagnosticCode::PrivateStateIo);
                    }
                    drop(answer_lock);
                    if write_ans.is_err() {
                        Outcome::Owner {
                            code: 5,
                            lines: vec![HELD.to_owned(), RUN_LINE.to_owned()],
                        }
                    } else {
                        Outcome::Owner {
                            code: 0,
                            lines: vec![PAIRED.to_owned()],
                        }
                    }
                }
                TerminalDecision::No => {
                    retire_credential(&credential, &config_root).await;
                    if is_identified {
                        Outcome::Owner {
                            code: 1,
                            lines: vec![NOT_PAIRED.to_owned(), MISMATCH_BODY.to_owned()],
                        }
                    } else {
                        Outcome::Owner {
                            code: 1,
                            lines: vec![CANCEL.to_owned()],
                        }
                    }
                }
                TerminalDecision::WalkedAway => {
                    let answer_lock = match acquire_answer_lock(&config_root).await {
                        Ok(lock) => lock,
                        Err(code) => return Outcome::Diagnostic(code),
                    };
                    let existing_confirmed = match load_credential(&config_root) {
                        Ok(Some(existing)) => is_pairing_confirmed(&config_root, &existing),
                        _ => false,
                    };
                    if existing_confirmed {
                        drop(answer_lock);
                        retire_credential(&credential, &config_root).await;
                        Outcome::Owner {
                            code: 1,
                            lines: vec![CANCEL.to_owned()],
                        }
                    } else {
                        let persist_res = persist_credential(&config_root, &credential);
                        if persist_res.is_ok() {
                            let root = config_root.clone();
                            let marker = marker_digest.clone();
                            let result = tokio::task::spawn_blocking(move || {
                                crate::device_migration::record_setup_baseline(
                                    &root,
                                    marker.as_deref(),
                                )
                            })
                            .await;
                            if !matches!(result, Ok(Ok(()))) {
                                drop(answer_lock);
                                return Outcome::Diagnostic(DiagnosticCode::PrivateStateIo);
                            }
                        }
                        drop(answer_lock);
                        if let Err(code) = persist_res {
                            return Outcome::Diagnostic(code);
                        }
                        crate::journal_version::clear_cached_version(&config_root);
                        Outcome::Owner {
                            code: 5,
                            lines: vec![HELD.to_owned(), RUN_LINE.to_owned()],
                        }
                    }
                }
            }
        }
        _ => unreachable!(),
    }
}

pub async fn confirm<T>(
    platform: PlatformKind,
    environment: &dyn Environment,
    seat: impl Into<crate::pairing_answer::TerminalSeat<T>>,
    mark: crate::pairing_answer::MarkOption,
) -> crate::pairing_answer::Outcome
where
    T: std::io::Read + std::io::Write + Send + 'static,
{
    use crate::pairing_answer::*;
    let seat = seat.into();

    match &mark {
        MarkOption::MissingValue | MarkOption::Repeated => return Outcome::Usage(MARK_USAGE),
        MarkOption::Value(val) => {
            let words = split_mark_words(val);
            if words.len() != 2 {
                return Outcome::Usage(MARK_USAGE);
            }
        }
        MarkOption::Absent => {}
    }

    let config_root = match resolve_config_root(platform, environment) {
        Ok(root) => root,
        Err(_) => return Outcome::Diagnostic(DiagnosticCode::PrivateStateIo),
    };

    let _ = ensure_private_directory(&config_root);

    let migration_root = config_root.clone();
    let migration_record = tokio::task::spawn_blocking(move || {
        crate::device_migration::MigrationRecord::load(&migration_root)
    })
    .await;
    let migration_record = match migration_record {
        Ok(Ok(record)) => record,
        Ok(Err(code)) => return Outcome::Diagnostic(code),
        Err(_) => return Outcome::Diagnostic(DiagnosticCode::PrivateStateIo),
    };
    if migration_record
        .is_some_and(|record| record.phase == crate::device_migration::MigrationPhase::Publishing)
    {
        let marker = crate::device_migration::host_marker_digest(
            platform,
            &crate::command::TokioCommandRunner,
        )
        .await;
        let Ok(marker) = marker else {
            return Outcome::Diagnostic(DiagnosticCode::PrivateStateIo);
        };
        if crate::device_migration::recover_publication_for_marker(&config_root, &marker)
            .await
            .is_err()
        {
            return Outcome::Diagnostic(DiagnosticCode::PrivateStateIo);
        }
    }
    let answer_lock = match acquire_answer_lock(&config_root).await {
        Ok(lock) => lock,
        Err(code) => return Outcome::Diagnostic(code),
    };
    let grandfather_root = config_root.clone();
    let grandfather_res = tokio::task::spawn_blocking(move || {
        crate::device_migration::grandfather_or_settle_if_idle(&grandfather_root)
    })
    .await
    .map_err(|_| DiagnosticCode::PrivateStateIo)
    .and_then(|result| result);
    drop(answer_lock);
    if let Err(code) = grandfather_res {
        return Outcome::Diagnostic(code);
    }

    let saved_cred = match load_credential(&config_root) {
        Ok(Some(cred)) => cred,
        Ok(None) => {
            return Outcome::Owner {
                code: 1,
                lines: vec![CONFIRM_UNPAIRED.to_owned()],
            };
        }
        Err(DiagnosticCode::PrivateStateInvalid) => {
            return Outcome::Diagnostic(DiagnosticCode::PrivateStateInvalid);
        }
        Err(err) => return Outcome::Diagnostic(err),
    };

    if is_pairing_confirmed(&config_root, &saved_cred) {
        return Outcome::Owner {
            code: 0,
            lines: vec![CONFIRM_DONE.to_owned()],
        };
    }

    enum TerminalQuestionReader<T> {
        Production(File),
        Scripted(T),
    }

    let mut terminal_reader = match &mark {
        MarkOption::Value(_) => None,
        MarkOption::Absent => match seat {
            TerminalSeat::Production => {
                let Some(file) = open_owner_terminal() else {
                    return Outcome::Owner {
                        code: 1,
                        lines: vec![CONFIRM_NO_TERMINAL.to_owned()],
                    };
                };
                Some(TerminalQuestionReader::Production(file))
            }
            TerminalSeat::Scripted(Some(term)) => Some(TerminalQuestionReader::Scripted(term)),
            TerminalSeat::Scripted(None) => {
                return Outcome::Owner {
                    code: 1,
                    lines: vec![CONFIRM_NO_TERMINAL.to_owned()],
                };
            }
        },
        _ => unreachable!(),
    };

    let displayed_gen = hex_encode(&compute_pairing_generation(&saved_cred.client_cert_pem));
    let is_identified = format_spoken_mark(&saved_cred.instance_id).is_some();

    match mark {
        MarkOption::Value(val) => {
            let eval = evaluate_mark_words(&val, &saved_cred.instance_id);
            match eval {
                MarkMatch::Usage => Outcome::Usage(MARK_USAGE),
                MarkMatch::Match => {
                    let answer_lock = match acquire_answer_lock(&config_root).await {
                        Ok(lock) => lock,
                        Err(code) => return Outcome::Diagnostic(code),
                    };
                    let current_cred = match load_credential(&config_root) {
                        Ok(Some(cred)) => cred,
                        _ => {
                            drop(answer_lock);
                            return Outcome::Diagnostic(DiagnosticCode::PrivateStateIo);
                        }
                    };
                    let current_gen =
                        hex_encode(&compute_pairing_generation(&current_cred.client_cert_pem));
                    if current_gen != displayed_gen {
                        drop(answer_lock);
                        return Outcome::Diagnostic(DiagnosticCode::PrivateStateIo);
                    }
                    let write_ans = write_answer_file(&config_root, &displayed_gen);
                    drop(answer_lock);
                    if let Err(code) = write_ans {
                        Outcome::Diagnostic(code)
                    } else {
                        Outcome::Owner {
                            code: 0,
                            lines: vec![PAIRED.to_owned()],
                        }
                    }
                }
                MarkMatch::Mismatch => {
                    if !is_identified {
                        Outcome::Owner {
                            code: 5,
                            lines: vec![MARK_UNVERIFIABLE_CONFIRM.to_owned()],
                        }
                    } else {
                        retire_credential(&saved_cred, &config_root).await;
                        let answer_lock = match acquire_answer_lock(&config_root).await {
                            Ok(lock) => lock,
                            Err(code) => return Outcome::Diagnostic(code),
                        };
                        let current_cred = match load_credential(&config_root) {
                            Ok(Some(cred)) => cred,
                            _ => {
                                drop(answer_lock);
                                return Outcome::Diagnostic(DiagnosticCode::PrivateStateIo);
                            }
                        };
                        let current_gen =
                            hex_encode(&compute_pairing_generation(&current_cred.client_cert_pem));
                        if current_gen != displayed_gen {
                            drop(answer_lock);
                            return Outcome::Diagnostic(DiagnosticCode::PrivateStateIo);
                        }
                        let del_res = delete_credential_file(&config_root);
                        drop(answer_lock);
                        if let Err(code) = del_res {
                            Outcome::Diagnostic(code)
                        } else {
                            Outcome::Owner {
                                code: 1,
                                lines: vec![NOT_PAIRED.to_owned(), MARK_MISMATCH.to_owned()],
                            }
                        }
                    }
                }
            }
        }
        MarkOption::Absent => {
            let decision = match terminal_reader.take().unwrap() {
                TerminalQuestionReader::Production(file) => {
                    ask_terminal_question_production(file, &saved_cred.instance_id).await
                }
                TerminalQuestionReader::Scripted(mut term) => {
                    ask_terminal_question_scripted(&mut term, &saved_cred.instance_id).await
                }
            };
            match decision {
                TerminalDecision::Yes => {
                    let answer_lock = match acquire_answer_lock(&config_root).await {
                        Ok(lock) => lock,
                        Err(code) => return Outcome::Diagnostic(code),
                    };
                    let current_cred = match load_credential(&config_root) {
                        Ok(Some(cred)) => cred,
                        _ => {
                            drop(answer_lock);
                            return Outcome::Diagnostic(DiagnosticCode::PrivateStateIo);
                        }
                    };
                    let current_gen =
                        hex_encode(&compute_pairing_generation(&current_cred.client_cert_pem));
                    if current_gen != displayed_gen {
                        drop(answer_lock);
                        return Outcome::Diagnostic(DiagnosticCode::PrivateStateIo);
                    }
                    let write_ans = write_answer_file(&config_root, &displayed_gen);
                    drop(answer_lock);
                    if let Err(code) = write_ans {
                        Outcome::Diagnostic(code)
                    } else {
                        Outcome::Owner {
                            code: 0,
                            lines: vec![PAIRED.to_owned()],
                        }
                    }
                }
                TerminalDecision::No => {
                    retire_credential(&saved_cred, &config_root).await;
                    let answer_lock = match acquire_answer_lock(&config_root).await {
                        Ok(lock) => lock,
                        Err(code) => return Outcome::Diagnostic(code),
                    };
                    let current_cred = match load_credential(&config_root) {
                        Ok(Some(cred)) => cred,
                        _ => {
                            drop(answer_lock);
                            return Outcome::Diagnostic(DiagnosticCode::PrivateStateIo);
                        }
                    };
                    let current_gen =
                        hex_encode(&compute_pairing_generation(&current_cred.client_cert_pem));
                    if current_gen != displayed_gen {
                        drop(answer_lock);
                        return Outcome::Diagnostic(DiagnosticCode::PrivateStateIo);
                    }
                    let del_res = delete_credential_file(&config_root);
                    drop(answer_lock);
                    if let Err(code) = del_res {
                        Outcome::Diagnostic(code)
                    } else if is_identified {
                        Outcome::Owner {
                            code: 1,
                            lines: vec![NOT_PAIRED.to_owned(), MISMATCH_BODY.to_owned()],
                        }
                    } else {
                        Outcome::Owner {
                            code: 1,
                            lines: vec![CANCEL.to_owned()],
                        }
                    }
                }
                TerminalDecision::WalkedAway => Outcome::Owner {
                    code: 5,
                    lines: vec![HELD.to_owned(), RUN_LINE.to_owned()],
                },
            }
        }
        _ => unreachable!(),
    }
}

pub fn acquire_private_state_lock(config_root: &Path) -> Result<File, DiagnosticCode> {
    let path = config_root.join(PRIVATE_STATE_LOCK_FILENAME);
    let descriptor = rustix::fs::open(
        &path,
        rustix::fs::OFlags::RDWR
            | rustix::fs::OFlags::CLOEXEC
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CREATE,
        rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
    )
    .map_err(|_| DiagnosticCode::SetupUnavailable)?;
    let file = File::from(descriptor);
    let metadata = file
        .metadata()
        .map_err(|_| DiagnosticCode::SetupUnavailable)?;
    if !metadata.is_file() {
        return Err(DiagnosticCode::SetupUnavailable);
    }
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(|_| DiagnosticCode::SetupUnavailable)?;
    rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive)
        .map_err(|_| DiagnosticCode::SetupUnavailable)?;
    Ok(file)
}

pub fn load_credential(config_root: &Path) -> Result<Option<Credential>, DiagnosticCode> {
    let Some(bytes) = read_private_file(&config_root.join(CREDENTIALS_FILENAME))? else {
        return Ok(None);
    };
    let credential = serde_json::from_slice::<Credential>(&bytes)
        .map_err(|_| DiagnosticCode::PrivateStateInvalid)?;
    if credential.instance_id.is_empty() {
        return Err(DiagnosticCode::PrivateStateInvalid);
    }
    Ok(Some(credential))
}

pub fn persist_credential(
    config_root: &Path,
    credential: &Credential,
) -> Result<(), DiagnosticCode> {
    let bytes = serde_json::to_vec(credential).map_err(|_| DiagnosticCode::PrivateStateInvalid)?;
    persist_private_file(config_root, CREDENTIALS_FILENAME, &bytes)
}

fn read_pair_link<R: Read>(input: R) -> Result<String, DiagnosticCode> {
    let mut bytes = Vec::new();
    input
        .take(MAX_PAIR_LINK_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| DiagnosticCode::SetupInputInvalid)?;
    if bytes.len() as u64 > MAX_PAIR_LINK_BYTES {
        return Err(DiagnosticCode::SetupInputInvalid);
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| DiagnosticCode::SetupInputInvalid)?;
    let link = text.trim_end_matches(char::is_whitespace);
    if link.is_empty() || link.chars().any(char::is_whitespace) {
        return Err(DiagnosticCode::SetupInputInvalid);
    }
    Ok(link.to_owned())
}

fn read_private_file(path: &Path) -> Result<Option<Vec<u8>>, DiagnosticCode> {
    let mut file = match open_regular_readonly(path) {
        Ok(file) => file,
        Err(StorageError::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
            return Ok(None);
        }
        Err(StorageError::InvalidTarget(_)) => {
            return Err(DiagnosticCode::PrivateStateInvalid);
        }
        Err(StorageError::Io { source, .. })
            if source.raw_os_error() == Some(rustix::io::Errno::LOOP.raw_os_error()) =>
        {
            return Err(DiagnosticCode::PrivateStateInvalid);
        }
        Err(_) => return Err(DiagnosticCode::PrivateStateIo),
    };
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(|_| DiagnosticCode::PrivateStateIo)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|_| DiagnosticCode::PrivateStateIo)?;
    Ok(Some(bytes))
}

fn persist_private_file(
    config_root: &Path,
    filename: &str,
    bytes: &[u8],
) -> Result<(), DiagnosticCode> {
    match atomic_write_bytes(&config_root.join(filename), config_root, bytes) {
        Ok(()) => Ok(()),
        Err(StorageError::InvalidTarget(_)) => Err(DiagnosticCode::PrivateStateInvalid),
        Err(_) => Err(DiagnosticCode::PrivateStateIo),
    }
}
