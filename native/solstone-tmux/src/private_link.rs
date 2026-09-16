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
    JournalBridgeTerminalReason,
};
use spl_transport::pairing::pair_from_link;

use crate::config::system_hostname;
use crate::health::DiagnosticCode;
use crate::instance_lock::InstanceLock;
use crate::journal_version::VersionRefreshState;
use crate::paths::{
    Environment, PlatformKind, ensure_private_directory, resolve_config_root, resolve_data_root,
};
use crate::post_connect::PostConnectCoordinator;
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
        let bridge_names_for_hook = bridge_names.clone();
        let policy = BridgePolicy {
            port: 0,
            capability_gate: CapabilityGate::Enabled,
            max_request_body_bytes: MAX_REQUEST_BODY_BYTES,
            local_response: Arc::new(move |head, _| {
                if spl_core::bridge::check_caller_auth(head, &bridge_names_for_hook).is_err() {
                    Some(spl_transport::journal_bridge::LocalResponse {
                        status: 403,
                        content_type: "text/plain".to_owned(),
                        body: b"forbidden".to_vec(),
                    })
                } else {
                    None
                }
            }),
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

    /// Whether the journal refused this device with TLS access denied (alert 49).
    /// The bridge latches this once and never dials again.
    pub fn access_denied(&self) -> bool {
        self.handle.status().terminal_reason == Some(JournalBridgeTerminalReason::TlsAccessDenied)
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

pub async fn setup<R>(
    platform: PlatformKind,
    environment: &dyn Environment,
    input: R,
) -> Result<(), DiagnosticCode>
where
    R: Read,
{
    setup_with_identity(platform, environment, input, system_hostname()).await
}

pub async fn setup_with_identity<R, E>(
    platform: PlatformKind,
    environment: &dyn Environment,
    input: R,
    hostname: Result<String, E>,
) -> Result<(), DiagnosticCode>
where
    R: Read,
    E: std::fmt::Debug,
{
    setup_with_pairer(
        platform,
        environment,
        input,
        hostname,
        |link, device_label, additional_fields| async move {
            pair_from_link(&link, &device_label, &additional_fields)
                .await
                .map_err(|_| DiagnosticCode::PairingFailed)
        },
    )
    .await
}

async fn setup_with_pairer<R, E, F, Fut>(
    platform: PlatformKind,
    environment: &dyn Environment,
    input: R,
    hostname: Result<String, E>,
    pairer: F,
) -> Result<(), DiagnosticCode>
where
    R: Read,
    E: std::fmt::Debug,
    F: FnOnce(String, String, Map<String, Value>) -> Fut,
    Fut: Future<Output = Result<Credential, DiagnosticCode>>,
{
    let data_root =
        resolve_data_root(platform, environment).map_err(|_| DiagnosticCode::SetupUnavailable)?;
    let _instance_lock =
        InstanceLock::acquire_existing(&data_root).map_err(|_| DiagnosticCode::SetupUnavailable)?;
    let config_root =
        resolve_config_root(platform, environment).map_err(|_| DiagnosticCode::SetupUnavailable)?;
    ensure_private_directory(&config_root).map_err(|_| DiagnosticCode::SetupUnavailable)?;
    let _private_state_lock = acquire_private_state_lock(&config_root)?;
    let (device_label, additional_fields) = pairing_ceremony_identity(platform, hostname);
    let link = read_pair_link(input)?;
    let credential = pairer(link, device_label, additional_fields).await?;
    persist_credential(&config_root, &credential)?;
    crate::journal_version::clear_cached_version(&config_root);
    Ok(())
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
