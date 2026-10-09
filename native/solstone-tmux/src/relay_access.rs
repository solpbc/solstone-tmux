// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::sync::Arc;
use std::time::Duration;

use reqwest::StatusCode;
use serde::Deserialize;
use spl_core::relay_access::{RelayAccess, negotiated_claims};
use spl_transport::credential::EndpointAddr;
use spl_transport::{same_relay_origin, validate_relay_origin};

use crate::clock::Clock;
use crate::journal::JournalClient;
use crate::private_link::PrivateLinkOpener;
use crate::sync::CredentialStore;

pub const RELAY_PROTOCOL_VERSION: u8 = 2;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RelayAccessNotConfigured {
    pub protocol_version: u8,
    pub status: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(untagged)]
pub enum RelayAccessResponse {
    Ready(RelayAccess),
    NotConfigured(RelayAccessNotConfigured),
}

/// Fetch and apply a capability only after the shared SPL validation succeeds.
/// Non-200 and malformed optional responses deliberately preserve the useful
/// credential already installed in the opener.
pub async fn run_relay_access_job(
    client: &JournalClient,
    store: &Arc<CredentialStore>,
    opener: &Arc<PrivateLinkOpener>,
    lane_attempt_id: u64,
    clock: &dyn Clock,
    timeout: Duration,
) -> Result<(), ()> {
    let deadline = tokio::time::Instant::now() + timeout;
    tokio::time::timeout_at(deadline, async {
        // A Ready write which landed after a reported durability error remains
        // pending until this owner retry confirms it. This is optional work: a
        // retry failure must not turn access acquisition into PrivateStateIo.
        let _ = store.persist_pending().await;

        let attempt = store.capture_access_attempt_until(lane_attempt_id, deadline);

        let (status, body) = client
            .get_relay_access(deadline.saturating_duration_since(tokio::time::Instant::now()))
            .await
            .map_err(|_| ())?;
        if status != StatusCode::OK {
            return Err(());
        }

        let response = serde_json::from_slice::<RelayAccessResponse>(&body).map_err(|_| ())?;
        match response {
            RelayAccessResponse::Ready(ready) => {
                if ready.status != "ready" || ready.protocol_version != RELAY_PROTOCOL_VERSION {
                    return Err(());
                }
                let paired_instance_id = store.instance_id();
                if ready.instance_id != paired_instance_id {
                    return Err(());
                }
                validate_relay_origin(&ready.relay_origin).map_err(|_| ())?;
                // Capture time only after the response is complete, so expiry
                // during a blocked request cannot be admitted.
                let now_unix_seconds = clock.wall_now().unix_timestamp();
                let claims = negotiated_claims(
                    ready.protocol_version,
                    &ready.device_token,
                    &ready.expires_at,
                    &paired_instance_id,
                    now_unix_seconds,
                )
                .ok_or(())?;

                let current = store.live_credential();
                let same_origin = current
                    .relay_origin
                    .as_deref()
                    .map(|origin| same_relay_origin(origin, &ready.relay_origin).map_err(|_| ()))
                    .transpose()?
                    .unwrap_or(false);
                if same_origin
                    && current.device_token.as_deref() == Some(ready.device_token.as_str())
                    && current.device_token_expires_at == Some(claims.exp)
                {
                    return Ok(());
                }

                store
                    .submit_ready(
                        Arc::clone(opener),
                        attempt,
                        ready.relay_origin,
                        ready.device_token,
                        claims.exp,
                    )
                    .await
                    .map_err(|_| ())?;
                Ok(())
            }
            RelayAccessResponse::NotConfigured(response) => {
                if response.status != "not_configured"
                    || response.protocol_version != RELAY_PROTOCOL_VERSION
                {
                    return Err(());
                }
                store.submit_disable(Arc::clone(opener), attempt).await
            }
        }
    })
    .await
    .unwrap_or(Err(()))
}

pub struct AccessLaneResult {
    pub relay: Result<(), ()>,
    pub addresses: Result<(), ()>,
}

pub async fn run_access_lane(
    client: &JournalClient,
    store: &Arc<CredentialStore>,
    opener: &Arc<PrivateLinkOpener>,
    lane_attempt_id: u64,
    clock: &dyn Clock,
    timeout: Duration,
) -> AccessLaneResult {
    let relay = run_relay_access_job(client, store, opener, lane_attempt_id, clock, timeout).await;
    let addresses = run_dial_address_job(client, store, opener, timeout).await;
    AccessLaneResult { relay, addresses }
}

const FAILURE_LINE: &str = "solstone-tmux: journal dial addresses were not refreshed";

enum DialAddressJobStep {
    Success,
    CommitError,
    FetchError,
}

pub async fn run_dial_address_job(
    client: &JournalClient,
    store: &Arc<CredentialStore>,
    opener: &Arc<PrivateLinkOpener>,
    timeout: Duration,
) -> Result<(), ()> {
    let endpoints = store.live_credential().endpoints;
    if !endpoints.is_empty()
        && endpoints.iter().all(|endpoint| {
            endpoint
                .host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
        })
    {
        return Ok(());
    }

    let deadline = tokio::time::Instant::now() + timeout;
    let result = tokio::time::timeout_at(deadline, async {
        let (status, body) = match client
            .get_local_endpoints(deadline.saturating_duration_since(tokio::time::Instant::now()))
            .await
        {
            Ok(res) => res,
            Err(_) => return DialAddressJobStep::FetchError,
        };
        if status != StatusCode::OK {
            return DialAddressJobStep::FetchError;
        }
        match parse_dial_endpoints(&body) {
            DialEndpointsParseResult::Commit(endpoints) => {
                match store
                    .submit_dial_endpoints(Arc::clone(opener), endpoints)
                    .await
                {
                    Ok(()) => DialAddressJobStep::Success,
                    Err(()) => DialAddressJobStep::CommitError,
                }
            }
            DialEndpointsParseResult::Leave => DialAddressJobStep::Success,
            DialEndpointsParseResult::Undecodable => DialAddressJobStep::FetchError,
        }
    })
    .await;

    match result {
        Ok(DialAddressJobStep::Success) => Ok(()),
        Ok(DialAddressJobStep::CommitError) => Err(()),
        Ok(DialAddressJobStep::FetchError) => {
            eprintln!("{FAILURE_LINE}");
            Err(())
        }
        Err(_elapsed) => {
            eprintln!("{FAILURE_LINE}");
            Err(())
        }
    }
}

#[derive(Debug, PartialEq)]
enum DialEndpointsParseResult {
    Commit(Vec<EndpointAddr>),
    Leave,
    Undecodable,
}

fn parse_dial_endpoints(body: &[u8]) -> DialEndpointsParseResult {
    let Ok(val) = serde_json::from_slice::<serde_json::Value>(body) else {
        return DialEndpointsParseResult::Undecodable;
    };
    let Some(obj) = val.as_object() else {
        return DialEndpointsParseResult::Undecodable;
    };
    let v_num = obj.get("v").and_then(|v| v.as_i64());
    if !v_num.is_some_and(|v| v >= 2) {
        // Journals that answer v 1 omit the paired address, so that body changes nothing.
        return DialEndpointsParseResult::Leave;
    }
    let Some(endpoints_val) = obj.get("endpoints") else {
        return DialEndpointsParseResult::Undecodable;
    };
    let Some(arr) = endpoints_val.as_array() else {
        return DialEndpointsParseResult::Undecodable;
    };
    let mut kept = Vec::new();
    for item in arr {
        let Some(item_obj) = item.as_object() else {
            continue;
        };
        let Some(ip_str) = item_obj.get("ip").and_then(|ip| ip.as_str()) else {
            continue;
        };
        let Ok(ip) = ip_str.parse::<std::net::IpAddr>() else {
            continue;
        };
        let Some(port) = item_obj.get("port").and_then(|p| p.as_u64()) else {
            continue;
        };
        if !(1..=65535).contains(&port) {
            continue;
        }
        kept.push(EndpointAddr {
            host: ip.to_string(),
            port: port as u16,
        });
    }
    if kept.is_empty() {
        DialEndpointsParseResult::Leave
    } else {
        DialEndpointsParseResult::Commit(kept)
    }
}

pub(crate) fn merge_dial_endpoints(
    listed: &[EndpointAddr],
    stored: &[EndpointAddr],
) -> Vec<EndpointAddr> {
    let mut merged = Vec::new();
    let mut seen_ips = std::collections::HashSet::new();

    for endpoint in listed {
        if let Ok(ip) = endpoint.host.parse::<std::net::IpAddr>()
            && seen_ips.insert((ip, endpoint.port))
        {
            merged.push(EndpointAddr {
                host: ip.to_string(),
                port: endpoint.port,
            });
        }
    }

    let mut stored_added = 0;
    for endpoint in stored {
        if stored_added >= 2 {
            break;
        }
        match endpoint.host.parse::<std::net::IpAddr>() {
            Ok(ip) => {
                if seen_ips.insert((ip, endpoint.port)) {
                    merged.push(endpoint.clone());
                    stored_added += 1;
                }
            }
            Err(_) => {
                merged.push(endpoint.clone());
                stored_added += 1;
            }
        }
    }

    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_gate_leaves_on_v1_or_invalid_v() {
        let body = br#"{"v": 1, "endpoints": "not-an-array"}"#;
        assert_eq!(parse_dial_endpoints(body), DialEndpointsParseResult::Leave);

        let body = br#"{"endpoints": "not-an-array"}"#;
        assert_eq!(parse_dial_endpoints(body), DialEndpointsParseResult::Leave);

        let body = br#"{"v": "2", "endpoints": []}"#;
        assert_eq!(parse_dial_endpoints(body), DialEndpointsParseResult::Leave);

        let body = br#"{"v": 2.0, "endpoints": []}"#;
        assert_eq!(parse_dial_endpoints(body), DialEndpointsParseResult::Leave);

        let body = br#"{"v": true, "endpoints": []}"#;
        assert_eq!(parse_dial_endpoints(body), DialEndpointsParseResult::Leave);

        let body = br#"{"v": 0, "endpoints": []}"#;
        assert_eq!(parse_dial_endpoints(body), DialEndpointsParseResult::Leave);
    }

    #[test]
    fn version_gate_v2_with_non_array_is_undecodable() {
        let body = br#"{"v": 2, "endpoints": "not-an-array"}"#;
        assert_eq!(
            parse_dial_endpoints(body),
            DialEndpointsParseResult::Undecodable
        );

        let body = br#"{"v": 2}"#;
        assert_eq!(
            parse_dial_endpoints(body),
            DialEndpointsParseResult::Undecodable
        );

        let body = b"not json";
        assert_eq!(
            parse_dial_endpoints(body),
            DialEndpointsParseResult::Undecodable
        );

        let body = b"[]";
        assert_eq!(
            parse_dial_endpoints(body),
            DialEndpointsParseResult::Undecodable
        );
    }

    #[test]
    fn version_gate_v2_with_unknown_fields_parses() {
        let body = br#"{
            "v": 2,
            "ttl_s": 300,
            "generated_at": "2026-10-08T18:00:00Z",
            "scope": "local",
            "unknown_extra": 42,
            "endpoints": [
                {"ip": "192.0.2.9", "port": 7657, "extra": "field"}
            ]
        }"#;
        assert_eq!(
            parse_dial_endpoints(body),
            DialEndpointsParseResult::Commit(vec![EndpointAddr {
                host: "192.0.2.9".to_owned(),
                port: 7657,
            }])
        );
    }

    #[test]
    fn skip_bad_entry_and_empty_leave() {
        let body = br#"{
            "v": 2,
            "endpoints": [
                "not-an-object",
                {"ip": "not-an-ip", "port": 7657},
                {"ip": "192.0.2.9", "port": 0},
                {"ip": "192.0.2.9", "port": 65536},
                {"ip": "192.0.2.9", "port": "7657"},
                {"port": 7657},
                {"ip": "192.0.2.9"}
            ]
        }"#;
        assert_eq!(parse_dial_endpoints(body), DialEndpointsParseResult::Leave);

        let body = br#"{
            "v": 2,
            "endpoints": [
                {"ip": "invalid", "port": 7657},
                {"ip": "192.0.2.9", "port": 7657}
            ]
        }"#;
        assert_eq!(
            parse_dial_endpoints(body),
            DialEndpointsParseResult::Commit(vec![EndpointAddr {
                host: "192.0.2.9".to_owned(),
                port: 7657,
            }])
        );
    }

    #[test]
    fn ipv6_collapse_and_bracket_skip() {
        let body = br#"{
            "v": 2,
            "endpoints": [
                {"ip": "[::1]", "port": 7657},
                {"ip": "0:0:0:0:0:0:0:1", "port": 7657},
                {"ip": "::1", "port": 7657}
            ]
        }"#;
        assert_eq!(
            parse_dial_endpoints(body),
            DialEndpointsParseResult::Commit(vec![
                EndpointAddr {
                    host: "::1".to_owned(),
                    port: 7657,
                },
                EndpointAddr {
                    host: "::1".to_owned(),
                    port: 7657,
                },
            ])
        );
    }

    #[test]
    fn merge_dial_endpoints_rules() {
        let listed = vec![
            EndpointAddr {
                host: "::1".to_owned(),
                port: 7657,
            },
            EndpointAddr {
                host: "0:0:0:0:0:0:0:1".to_owned(),
                port: 7657,
            },
            EndpointAddr {
                host: "192.0.2.9".to_owned(),
                port: 7657,
            },
        ];
        let stored = vec![];
        let merged = merge_dial_endpoints(&listed, &stored);
        assert_eq!(
            merged,
            vec![
                EndpointAddr {
                    host: "::1".to_owned(),
                    port: 7657,
                },
                EndpointAddr {
                    host: "192.0.2.9".to_owned(),
                    port: 7657,
                },
            ]
        );

        let o = EndpointAddr {
            host: "127.0.0.1".to_owned(),
            port: 8000,
        };
        let n1 = EndpointAddr {
            host: "192.0.2.9".to_owned(),
            port: 7657,
        };
        let n2 = EndpointAddr {
            host: "192.0.2.10".to_owned(),
            port: 7657,
        };
        let n3 = EndpointAddr {
            host: "192.0.2.11".to_owned(),
            port: 7657,
        };

        let step1 = merge_dial_endpoints(&[n1.clone(), n2.clone()], std::slice::from_ref(&o));
        assert_eq!(step1, vec![n1.clone(), n2.clone(), o.clone()]);

        let step2 = merge_dial_endpoints(&[n1.clone(), n2.clone()], &step1);
        assert_eq!(step2, vec![n1.clone(), n2.clone(), o.clone()]);

        let step3 = merge_dial_endpoints(std::slice::from_ref(&n3), &step2);
        assert_eq!(step3, vec![n3.clone(), n1.clone(), n2.clone()]);

        let non_ip = EndpointAddr {
            host: "custom.domain".to_owned(),
            port: 7657,
        };
        let merged_non_ip =
            merge_dial_endpoints(std::slice::from_ref(&n1), &[non_ip.clone(), o.clone()]);
        assert_eq!(merged_non_ip, vec![n1.clone(), non_ip.clone(), o.clone()]);
    }
}
