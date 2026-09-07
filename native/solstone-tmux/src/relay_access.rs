// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::sync::Arc;
use std::time::Duration;

use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use spl_transport::client::TransportClient;

use crate::journal::JournalClient;
use crate::private_link::PrivateLinkOpener;
use crate::sync::CredentialStore;

pub const RELAY_PROTOCOL_VERSION: u32 = 2;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RelayAccessReady {
    pub protocol_version: u32,
    pub relay_origin: String,
    pub instance_id: String,
    pub device_token: String,
    pub expires_at: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RelayAccessNotConfigured {
    pub protocol_version: u32,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum RelayAccessResponse {
    Ready(RelayAccessReady),
    NotConfigured(RelayAccessNotConfigured),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RelayAccessJwtClaims {
    pub iss: String,
    pub sub: String,
    pub aud: String,
    pub scope: String,
    pub ver: u32,
    pub instance_id: String,
    pub iat: i64,
    pub exp: i64,
    pub jti: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RelayAccessError {
    ContractInvalid,
    Expired,
    InstanceMismatch,
    OriginInvalid,
}

pub fn decode_base64url(input: &str) -> Option<Vec<u8>> {
    let mut buffer = Vec::with_capacity(input.len() * 3 / 4 + 4);
    let mut accumulator: u32 = 0;
    let mut bits: u32 = 0;

    for byte in input.bytes() {
        let val = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            b'=' | b' ' | b'\r' | b'\n' | b'\t' => continue,
            _ => return None,
        };
        accumulator = (accumulator << 6) | (val as u32);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            buffer.push(((accumulator >> bits) & 0xff) as u8);
        }
    }
    Some(buffer)
}

pub fn decode_jwt_v2_claims(
    token: &str,
    paired_instance_id: &str,
) -> Result<RelayAccessJwtClaims, RelayAccessError> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return Err(RelayAccessError::ContractInvalid);
    }
    let payload_bytes = decode_base64url(parts[1]).ok_or(RelayAccessError::ContractInvalid)?;
    let claims: RelayAccessJwtClaims =
        serde_json::from_slice(&payload_bytes).map_err(|_| RelayAccessError::ContractInvalid)?;

    if claims.ver != 2
        || claims.aud != "spl-relay"
        || claims.scope != "session.dial"
        || claims.sub != format!("instance:{paired_instance_id}")
        || claims.instance_id != paired_instance_id
        || claims.iss.is_empty()
        || claims.exp <= claims.iat
    {
        return Err(RelayAccessError::ContractInvalid);
    }

    Ok(claims)
}

fn is_leap_year(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0)
}

fn days_in_month(year: i64, month: u32) -> Option<i64> {
    match month {
        1 => Some(31),
        2 => Some(if is_leap_year(year) { 29 } else { 28 }),
        3 => Some(31),
        4 => Some(30),
        5 => Some(31),
        6 => Some(30),
        7 => Some(31),
        8 => Some(31),
        9 => Some(30),
        10 => Some(31),
        11 => Some(30),
        12 => Some(31),
        _ => None,
    }
}

pub fn parse_rfc3339_unix_seconds(ts: &str) -> Option<i64> {
    // Format: YYYY-MM-DDTHH:MM:SS[.fraction](Z|+HH:MM|-HH:MM)
    let ts = ts.trim();
    if ts.len() < 20 {
        return None;
    }
    let year: i64 = ts[0..4].parse().ok()?;
    if ts.as_bytes()[4] != b'-' || ts.as_bytes()[7] != b'-' {
        return None;
    }
    let month: u32 = ts[5..7].parse().ok()?;
    let day: i64 = ts[8..10].parse().ok()?;
    let max_days = days_in_month(year, month)?;
    if day < 1 || day > max_days {
        return None;
    }

    let sep = ts.as_bytes()[10];
    if sep != b'T' && sep != b't' {
        return None;
    }

    let hour: i64 = ts[11..13].parse().ok()?;
    if ts.as_bytes()[13] != b':' || ts.as_bytes()[16] != b':' {
        return None;
    }
    let minute: i64 = ts[14..16].parse().ok()?;
    let second: i64 = ts[17..19].parse().ok()?;
    if hour >= 24 || minute >= 60 || second > 60 {
        return None;
    }

    // Remainder can be fractional seconds followed by tz offset
    let rest = &ts[19..];
    let (tz_str, _fraction) = if let Some(dot_pos) = rest.find('.') {
        let after_dot = &rest[dot_pos + 1..];
        let tz_pos = after_dot.find(['Z', 'z', '+', '-']).ok_or(()).ok()?;
        (&after_dot[tz_pos..], Some(&after_dot[..tz_pos]))
    } else {
        (rest, None)
    };

    let offset_seconds: i64 = if tz_str == "Z" || tz_str == "z" {
        0
    } else if tz_str.len() == 6
        && (tz_str.starts_with('+') || tz_str.starts_with('-'))
        && &tz_str[3..4] == ":"
    {
        let sign = if tz_str.starts_with('+') { 1 } else { -1 };
        let tz_hour: i64 = tz_str[1..3].parse().ok()?;
        let tz_min: i64 = tz_str[4..6].parse().ok()?;
        if tz_hour >= 24 || tz_min >= 60 {
            return None;
        }
        sign * (tz_hour * 3600 + tz_min * 60)
    } else {
        return None;
    };

    // Calculate days from 1970-01-01 to (year, month, day)
    // Days from 1970 to year
    let mut total_days: i64 = 0;
    if year >= 1970 {
        for y in 1970..year {
            total_days += if is_leap_year(y) { 366 } else { 365 };
        }
    } else {
        for y in year..1970 {
            total_days -= if is_leap_year(y) { 366 } else { 365 };
        }
    }
    for m in 1..month {
        total_days += days_in_month(year, m)?;
    }
    total_days += day - 1;

    let total_seconds = total_days * 86400 + hour * 3600 + minute * 60 + second - offset_seconds;
    Some(total_seconds)
}

pub fn validate_relay_origin(origin: &str, instance_id: &str) -> Result<String, RelayAccessError> {
    spl_core::relay::dial_url(origin, instance_id).map_err(|_| RelayAccessError::OriginInvalid)?;
    Ok(origin.to_owned())
}

pub async fn run_relay_access_job(
    client: &JournalClient,
    store: &Arc<CredentialStore>,
    opener: &Arc<PrivateLinkOpener>,
    now_unix_seconds: i64,
    timeout: Duration,
) -> Result<(), ()> {
    // 1. Retry any pending durable clear first if gens match
    store.retry_durable_clear_if_pending().await;

    let (status, body) = client.get_relay_access(timeout).await.map_err(|_| ())?;
    if status == StatusCode::NOT_FOUND || status == StatusCode::SERVICE_UNAVAILABLE {
        return Ok(());
    }
    if status != StatusCode::OK {
        return Err(());
    }

    let response = serde_json::from_slice::<RelayAccessResponse>(&body).map_err(|_| ())?;
    match response {
        RelayAccessResponse::Ready(ready) => {
            if ready.protocol_version != RELAY_PROTOCOL_VERSION {
                return Err(());
            }
            let paired_instance_id = store.instance_id();
            if ready.instance_id != paired_instance_id {
                return Err(());
            }
            validate_relay_origin(&ready.relay_origin, &paired_instance_id).map_err(|_| ())?;

            let claims =
                decode_jwt_v2_claims(&ready.device_token, &paired_instance_id).map_err(|_| ())?;
            let parsed_expiry = parse_rfc3339_unix_seconds(&ready.expires_at).ok_or(())?;
            if parsed_expiry != claims.exp || claims.exp <= now_unix_seconds {
                return Err(());
            }

            // Persist-first before live replace
            let (new_credential, new_mutation_gen) = store
                .commit_ready_access(ready.relay_origin, ready.device_token, parsed_expiry)
                .await
                .map_err(|_| ())?;

            let token_persist_hook = store.token_persist_hook(new_mutation_gen);
            let new_transport = if new_credential.endpoints.is_empty() {
                TransportClient::new_relay_only(new_credential.clone(), Some(token_persist_hook))
            } else {
                TransportClient::new(new_credential.clone(), Some(token_persist_hook))
            }
            .map_err(|_| ())?;

            opener.replace_transport(Arc::new(new_transport), new_credential);
            Ok(())
        }
        RelayAccessResponse::NotConfigured(not_conf) => {
            if not_conf.protocol_version != RELAY_PROTOCOL_VERSION {
                return Err(());
            }
            // Live disable immediately
            let (direct_credential, intent_gen) = store.live_clear_relay_credential();
            let token_persist_hook = store.token_persist_hook(intent_gen);
            let new_transport = if direct_credential.endpoints.is_empty() {
                TransportClient::new_relay_only(direct_credential.clone(), Some(token_persist_hook))
            } else {
                TransportClient::new(direct_credential.clone(), Some(token_persist_hook))
            }
            .map_err(|_| ())?;
            opener.replace_transport(Arc::new(new_transport), direct_credential);

            // Commit durable clear
            let _ = store.commit_durable_clear(intent_gen).await;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_rfc3339_unix_seconds_utc_and_offsets() {
        assert_eq!(parse_rfc3339_unix_seconds("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_rfc3339_unix_seconds("2026-09-07T12:00:00Z"),
            Some(1788782400)
        );
        assert_eq!(
            parse_rfc3339_unix_seconds("2026-09-07T14:00:00+02:00"),
            Some(1788782400)
        );
        assert_eq!(
            parse_rfc3339_unix_seconds("2026-09-07T06:00:00-06:00"),
            Some(1788782400)
        );
        assert_eq!(
            parse_rfc3339_unix_seconds("2026-09-07T12:00:00.123456Z"),
            Some(1788782400)
        );
    }

    #[test]
    fn decode_base64url_round_trip() {
        let payload = r#"{"ver":2,"aud":"spl-relay"}"#;
        let mut encoded = String::new();
        // base64 standard without padding converted to url safe
        for chunk in payload.as_bytes().chunks(3) {
            let b0 = chunk[0] as u32;
            let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
            let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
            let triple = (b0 << 16) | (b1 << 8) | b2;
            let table = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
            encoded.push(table[((triple >> 18) & 0x3f) as usize] as char);
            encoded.push(table[((triple >> 12) & 0x3f) as usize] as char);
            if chunk.len() > 1 {
                encoded.push(table[((triple >> 6) & 0x3f) as usize] as char);
            }
            if chunk.len() > 2 {
                encoded.push(table[(triple & 0x3f) as usize] as char);
            }
        }
        let decoded = decode_base64url(&encoded).expect("decode base64url");
        assert_eq!(decoded, payload.as_bytes());
    }

    #[test]
    fn validate_jwt_v2_rejects_extra_claims_and_mismatched_sub() {
        let claims = RelayAccessJwtClaims {
            iss: "auth.solstone.example".to_owned(),
            sub: "instance:inst-1".to_owned(),
            aud: "spl-relay".to_owned(),
            scope: "session.dial".to_owned(),
            ver: 2,
            instance_id: "inst-1".to_owned(),
            iat: 100,
            exp: 200,
            jti: "jti-1".to_owned(),
        };
        let payload_json = serde_json::to_string(&claims).unwrap();
        // Construct token
        let token = format!("header.{}.sig", {
            let mut s = String::new();
            for chunk in payload_json.as_bytes().chunks(3) {
                let b0 = chunk[0] as u32;
                let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
                let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
                let triple = (b0 << 16) | (b1 << 8) | b2;
                let table = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
                s.push(table[((triple >> 18) & 0x3f) as usize] as char);
                s.push(table[((triple >> 12) & 0x3f) as usize] as char);
                if chunk.len() > 1 {
                    s.push(table[((triple >> 6) & 0x3f) as usize] as char);
                }
                if chunk.len() > 2 {
                    s.push(table[(triple & 0x3f) as usize] as char);
                }
            }
            s
        });

        let decoded = decode_jwt_v2_claims(&token, "inst-1").expect("valid claims");
        assert_eq!(decoded.ver, 2);
        assert_eq!(decoded.instance_id, "inst-1");

        assert!(decode_jwt_v2_claims(&token, "inst-2").is_err());
    }

    #[test]
    fn validate_relay_origin_checks_schemes_via_spl_core() {
        assert!(validate_relay_origin("https://relay.example.com", "inst-1").is_ok());
        assert!(validate_relay_origin("http://127.0.0.1:8080", "inst-1").is_ok());
        assert!(validate_relay_origin("wss://relay.example.com", "inst-1").is_err());
        assert!(validate_relay_origin("ftp://relay.example.com", "inst-1").is_err());
    }

    #[test]
    fn relay_access_response_deny_unknown_fields() {
        let valid_ready = r#"{
            "status": "ready",
            "protocol_version": 2,
            "relay_origin": "https://relay.example.com",
            "instance_id": "inst-1",
            "device_token": "tok",
            "expires_at": "2026-09-07T12:00:00Z"
        }"#;
        assert!(serde_json::from_str::<RelayAccessResponse>(valid_ready).is_ok());

        let invalid_ready_extra = r#"{
            "status": "ready",
            "protocol_version": 2,
            "relay_origin": "https://relay.example.com",
            "instance_id": "inst-1",
            "device_token": "tok",
            "expires_at": "2026-09-07T12:00:00Z",
            "extra": 123
        }"#;
        assert!(serde_json::from_str::<RelayAccessResponse>(invalid_ready_extra).is_err());

        let valid_not_conf = r#"{
            "status": "not_configured",
            "protocol_version": 2
        }"#;
        assert!(serde_json::from_str::<RelayAccessResponse>(valid_not_conf).is_ok());

        let invalid_not_conf_extra = r#"{
            "status": "not_configured",
            "protocol_version": 2,
            "extra": true
        }"#;
        assert!(serde_json::from_str::<RelayAccessResponse>(invalid_not_conf_extra).is_err());
    }
}
