// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::fs::{self, File};
use std::future::Future;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};
use spl_core::bridge::BridgeNames;
use spl_transport::TransportError;
use spl_transport::client::{DialedCarrier, TokenPersistHook, TransportClient};
use spl_transport::credential::Credential;
use spl_transport::journal_bridge::{
    BridgePolicy, CapabilityGate, CarrierOpener, JournalBridgeConfig, JournalBridgeHandle,
};
use spl_transport::pairing::pair_from_link;

use crate::config::system_hostname;
use crate::health::DiagnosticCode;
use crate::instance_lock::InstanceLock;
use crate::paths::{
    Environment, PlatformKind, ensure_private_directory, resolve_config_root, resolve_data_root,
};
use crate::storage::{StorageError, atomic_write_bytes};

pub const CREDENTIALS_FILENAME: &str = "credentials.json";
pub const OBSERVER_FILENAME: &str = "observer.json";
const MAX_PAIR_LINK_BYTES: u64 = 4096;
pub const MAX_REQUEST_BODY_BYTES: usize = 16 * 1024 * 1024;
const CAPABILITY_COOKIE_NAME: &str = "solstone_tmux_cap";
const UPSTREAM_COOKIE_PREFIX: &str = "solstone_tmux_";
pub const OBSERVER_HEADER_NAME: &str = "x-solstone-observer";
pub const PROTOCOL_VERSION_HEADER_NAME: &str = "x-solstone-protocol-version";
const AUTHORIZATION_HEADER_NAME: &str = "authorization";
const PROTOCOL_VERSION: &str = "2";

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ObserverState {
    pub credential_instance_id: String,
    pub key: String,
    pub prefix: String,
    pub name: String,
    pub ingest_url: String,
    pub protocol_version: u64,
}

#[derive(Clone)]
enum OpenerAuth {
    Unregistered,
    Registered {
        observer_key: String,
        protocol_version: String,
    },
}

pub struct PrivateLinkOpener {
    transport: Arc<TransportClient>,
    auth: RwLock<OpenerAuth>,
}

impl PrivateLinkOpener {
    fn new(transport: TransportClient) -> Self {
        Self {
            transport: Arc::new(transport),
            auth: RwLock::new(OpenerAuth::Unregistered),
        }
    }

    pub fn set_registered(&self, observer: &ObserverState) -> Result<(), DiagnosticCode> {
        if observer.key.is_empty()
            || contains_invalid_header_value(&observer.key)
            || observer.protocol_version != 2
        {
            return Err(DiagnosticCode::JournalContractInvalid);
        }
        let mut auth = match self.auth.write() {
            Ok(auth) => auth,
            Err(poisoned) => poisoned.into_inner(),
        };
        *auth = OpenerAuth::Registered {
            observer_key: observer.key.clone(),
            protocol_version: observer.protocol_version.to_string(),
        };
        Ok(())
    }
}

impl CarrierOpener for PrivateLinkOpener {
    fn proxy_headers(
        &self,
        upstream_headers: &[(String, String)],
    ) -> Result<Vec<(String, String)>, TransportError> {
        let auth = match self.auth.read() {
            Ok(auth) => auth,
            Err(poisoned) => poisoned.into_inner(),
        };
        let mut headers = upstream_headers.to_vec();
        match &*auth {
            OpenerAuth::Unregistered => headers.push((
                PROTOCOL_VERSION_HEADER_NAME.to_owned(),
                PROTOCOL_VERSION.to_owned(),
            )),
            OpenerAuth::Registered {
                observer_key,
                protocol_version,
            } => {
                headers.push((OBSERVER_HEADER_NAME.to_owned(), observer_key.to_owned()));
                headers.push((
                    AUTHORIZATION_HEADER_NAME.to_owned(),
                    format!("Bearer {observer_key}"),
                ));
                headers.push((
                    PROTOCOL_VERSION_HEADER_NAME.to_owned(),
                    protocol_version.to_owned(),
                ));
            }
        }
        Ok(headers)
    }

    fn dial_carrier(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<DialedCarrier, TransportError>> + Send + '_>> {
        Box::pin(self.transport.dial_carrier())
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
    ) -> Result<Self, DiagnosticCode> {
        let endpoint_hosts = credential
            .endpoints
            .iter()
            .map(|endpoint| endpoint.host.clone())
            .collect();
        let transport = TransportClient::new(credential, token_persist)
            .map_err(|_| DiagnosticCode::BridgeUnavailable)?;
        let opener = Arc::new(PrivateLinkOpener::new(transport));
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

    pub async fn shutdown(self) {
        self.handle.shutdown_and_wait().await;
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
    setup_with_pairer(
        platform,
        environment,
        input,
        |link, device_label| async move {
            let additional_fields = serde_json::Map::new();
            pair_from_link(&link, &device_label, &additional_fields)
                .await
                .map_err(|_| DiagnosticCode::PairingFailed)
        },
    )
    .await
}

async fn setup_with_pairer<R, F, Fut>(
    platform: PlatformKind,
    environment: &dyn Environment,
    input: R,
    pairer: F,
) -> Result<(), DiagnosticCode>
where
    R: Read,
    F: FnOnce(String, String) -> Fut,
    Fut: Future<Output = Result<Credential, DiagnosticCode>>,
{
    let data_root =
        resolve_data_root(platform, environment).map_err(|_| DiagnosticCode::SetupUnavailable)?;
    let _instance_lock =
        InstanceLock::acquire_existing(&data_root).map_err(|_| DiagnosticCode::SetupUnavailable)?;
    let config_root =
        resolve_config_root(platform, environment).map_err(|_| DiagnosticCode::SetupUnavailable)?;
    ensure_private_directory(&config_root).map_err(|_| DiagnosticCode::SetupUnavailable)?;
    let device_label = system_hostname().map_err(|_| DiagnosticCode::SetupUnavailable)?;
    let link = read_pair_link(input)?;
    let credential = pairer(link, device_label).await?;
    persist_credential(&config_root, &credential)
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

pub fn load_observer(
    config_root: &Path,
    credential_instance_id: &str,
) -> Result<Option<ObserverState>, DiagnosticCode> {
    let Some(bytes) = read_private_file(&config_root.join(OBSERVER_FILENAME))? else {
        return Ok(None);
    };
    let observer = serde_json::from_slice::<ObserverState>(&bytes)
        .map_err(|_| DiagnosticCode::PrivateStateInvalid)?;
    if observer.credential_instance_id != credential_instance_id {
        return Ok(None);
    }
    Ok(Some(observer))
}

pub fn persist_observer(
    config_root: &Path,
    observer: &ObserverState,
) -> Result<(), DiagnosticCode> {
    let bytes = serde_json::to_vec(observer).map_err(|_| DiagnosticCode::PrivateStateInvalid)?;
    persist_private_file(config_root, OBSERVER_FILENAME, &bytes)
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
    let descriptor = match rustix::fs::open(
        path,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC | rustix::fs::OFlags::NOFOLLOW,
        rustix::fs::Mode::empty(),
    ) {
        Ok(descriptor) => descriptor,
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(rustix::io::Errno::LOOP) => return Err(DiagnosticCode::PrivateStateInvalid),
        Err(_) => return Err(DiagnosticCode::PrivateStateIo),
    };
    let mut file = File::from(descriptor);
    let metadata = file
        .metadata()
        .map_err(|_| DiagnosticCode::PrivateStateIo)?;
    if !metadata.is_file() {
        return Err(DiagnosticCode::PrivateStateInvalid);
    }
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

pub(crate) fn contains_invalid_header_value(value: &str) -> bool {
    value.bytes().any(|byte| matches!(byte, b'\r' | b'\n' | 0))
}
