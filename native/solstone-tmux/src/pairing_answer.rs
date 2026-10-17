// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::fs::{self, File};
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use spl_transport::credential::Credential;

use crate::health::DiagnosticCode;
use crate::instance_lock::RunIdentity;
use crate::journal::JournalClient;
use crate::journal_version::{VersionRefreshState, hex_encode};
use crate::post_connect::compute_pairing_generation;
use crate::private_link::{
    CREDENTIALS_FILENAME, PrivateLinkBridge, format_spoken_mark, load_credential,
};
use crate::storage::{StorageError, atomic_write_bytes, open_regular_readonly};

pub const STEP: crate::i18n::MessageId = crate::i18n::MessageId::CliPairingAnswerCopyStep;
pub const MARK_PREFIX: crate::i18n::MessageId =
    crate::i18n::MessageId::CliPairingAnswerCopyMarkPrefix;
pub const MARK_UNAVAILABLE: crate::i18n::MessageId =
    crate::i18n::MessageId::CliPairingAnswerCopyMarkUnavailable;
pub const SUBTEXT: crate::i18n::MessageId = crate::i18n::MessageId::CliPairingAnswerCopySubtext;
pub const ASK_IDENTIFIED: crate::i18n::MessageId =
    crate::i18n::MessageId::CliPairingAnswerCopyAskIdentified;
pub const COULDNT_VERIFY: crate::i18n::MessageId =
    crate::i18n::MessageId::CliPairingAnswerCopyCouldntVerify;
pub const BODY_UNAVAILABLE: crate::i18n::MessageId =
    crate::i18n::MessageId::CliPairingAnswerCopyBodyUnavailable;
pub const ASK_UNAVAILABLE: crate::i18n::MessageId =
    crate::i18n::MessageId::CliPairingAnswerCopyAskUnavailable;
pub const PAIRED: crate::i18n::MessageId = crate::i18n::MessageId::CliPairingAnswerCopyPaired;
pub const NOT_PAIRED: crate::i18n::MessageId =
    crate::i18n::MessageId::CliPairingAnswerCopyNotPaired;
pub const MISMATCH_BODY: crate::i18n::MessageId =
    crate::i18n::MessageId::CliPairingAnswerCopyMismatchBody;
pub const CANCEL: crate::i18n::MessageId = crate::i18n::MessageId::CliPairingAnswerCopyCancel;
pub const HELD: crate::i18n::MessageId = crate::i18n::MessageId::CliPairingAnswerCopyHeld;
pub const RUN_LINE: crate::i18n::MessageId = crate::i18n::MessageId::CliPairingAnswerCopyRunLine;
pub const CONFIRM_DONE: crate::i18n::MessageId =
    crate::i18n::MessageId::CliPairingAnswerCopyConfirmDone;
pub const CONFIRM_UNPAIRED: crate::i18n::MessageId =
    crate::i18n::MessageId::CliPairingAnswerCopyConfirmUnpaired;
pub const SETUP_NO_TERMINAL: crate::i18n::MessageId =
    crate::i18n::MessageId::CliPairingAnswerCopySetupNoTerminal;
pub const CONFIRM_NO_TERMINAL: crate::i18n::MessageId =
    crate::i18n::MessageId::CliPairingAnswerCopyConfirmNoTerminal;
pub const MARK_HELP: crate::i18n::MessageId = crate::i18n::MessageId::CliPairingAnswerCopyMarkHelp;
pub const MARK_USAGE: crate::i18n::MessageId =
    crate::i18n::MessageId::CliPairingAnswerCopyMarkUsage;
pub const MARK_MISMATCH: crate::i18n::MessageId =
    crate::i18n::MessageId::CliPairingAnswerCopyMarkMismatch;
pub const MARK_UNVERIFIABLE_SETUP: crate::i18n::MessageId =
    crate::i18n::MessageId::CliPairingAnswerCopyMarkUnverifiableSetup;
pub const MARK_UNVERIFIABLE_CONFIRM: crate::i18n::MessageId =
    crate::i18n::MessageId::CliPairingAnswerCopyMarkUnverifiableConfirm;
pub const STATUS_HELD_PAIRING: crate::i18n::MessageId =
    crate::i18n::MessageId::CliPairingAnswerCopyStatusHeldPairing;
pub const STATUS_HELD_CONFIRM: crate::i18n::MessageId =
    crate::i18n::MessageId::CliPairingAnswerCopyStatusHeldConfirm;

pub const ANSWER_FILENAME: &str = "pairing-answer.json";
pub const ANSWER_LOCK_FILENAME: &str = ".solstone-tmux.answer.lock";
pub const ANSWER_LOCK_TIMEOUT: Duration = Duration::from_secs(5);
pub const RETIRE_TIMEOUT: Duration = Duration::from_secs(8);

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AnswerRecord {
    pub confirmed: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MarkOption {
    Absent,
    MissingValue,
    Repeated,
    Value(String),
}

#[derive(Debug)]
pub enum TerminalSeat<T> {
    Production,
    Scripted(Option<T>),
}

impl<T> From<Option<T>> for TerminalSeat<T> {
    fn from(opt: Option<T>) -> Self {
        Self::Scripted(opt)
    }
}

/// Parser codes are protocol choices, independent of the display language.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AnswerToken {
    Yes,
    No,
    Continue,
    Cancel,
}

pub fn parse_answer_token(value: &str) -> Option<AnswerToken> {
    match value.trim().to_ascii_lowercase().as_str() {
        "yes" | "y" => Some(AnswerToken::Yes),
        "no" | "n" => Some(AnswerToken::No),
        "continue" => Some(AnswerToken::Continue),
        "cancel" => Some(AnswerToken::Cancel),
        _ => None,
    }
}

pub fn answer_decision(token: Option<AnswerToken>, identified: bool) -> Option<TerminalDecision> {
    match (identified, token) {
        (true, Some(AnswerToken::Yes)) | (false, Some(AnswerToken::Continue)) => {
            Some(TerminalDecision::Yes)
        }
        (true, Some(AnswerToken::No)) | (false, Some(AnswerToken::Cancel)) => {
            Some(TerminalDecision::No)
        }
        _ => None,
    }
}

/// An isolated experiment, never called by the production parser. Acceptance
/// of localized aliases remains a product decision.
#[cfg(test)]
fn parse_alias_option(value: &str, aliases: &[(&str, AnswerToken)]) -> Option<AnswerToken> {
    let canonical = parse_answer_token(value);
    let matches = aliases
        .iter()
        .filter(|(alias, _)| *alias == value.trim())
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [] => canonical,
        [(_, token)] if canonical.is_none_or(|original| original == *token) => Some(*token),
        _ => None,
    }
}

fn identified_prompt(mark: &str) -> String {
    crate::i18n::render(
        crate::i18n::MessageId::CliPairingPromptIdentified,
        &[
            ("step", crate::i18n::ArgValue::Text(STEP.text().into())),
            (
                "mark_prefix",
                crate::i18n::ArgValue::Text(MARK_PREFIX.text().into()),
            ),
            ("mark", crate::i18n::ArgValue::Text(mark.into())),
            (
                "subtext",
                crate::i18n::ArgValue::Text(SUBTEXT.text().into()),
            ),
            (
                "question",
                crate::i18n::ArgValue::Text(ASK_IDENTIFIED.text().into()),
            ),
        ],
    )
}

fn unavailable_prompt() -> String {
    crate::i18n::render(
        crate::i18n::MessageId::CliPairingPromptUnavailable,
        &[
            ("step", crate::i18n::ArgValue::Text(STEP.text().into())),
            (
                "mark_line",
                crate::i18n::ArgValue::Text(MARK_UNAVAILABLE.text().into()),
            ),
            (
                "summary",
                crate::i18n::ArgValue::Text(COULDNT_VERIFY.text().into()),
            ),
            (
                "body",
                crate::i18n::ArgValue::Text(BODY_UNAVAILABLE.text().into()),
            ),
            (
                "question",
                crate::i18n::ArgValue::Text(ASK_UNAVAILABLE.text().into()),
            ),
        ],
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminalDecision {
    Yes,
    No,
    WalkedAway,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Diagnostic(DiagnosticCode),
    Usage(crate::i18n::MessageId),
    Owner {
        code: i32,
        lines: Vec<crate::i18n::MessageId>,
    },
}

pub fn open_owner_terminal() -> Option<File> {
    // Tests substitute the terminal device via SOLSTONE_TMUX_TERMINAL.
    let path = match std::env::var_os("SOLSTONE_TMUX_TERMINAL") {
        Some(val) if val == "-" => return None,
        Some(val) => std::path::PathBuf::from(val),
        None => std::path::PathBuf::from("/dev/tty"),
    };
    let descriptor = rustix::fs::open(
        &path,
        rustix::fs::OFlags::RDWR | rustix::fs::OFlags::CLOEXEC | rustix::fs::OFlags::NOFOLLOW,
        rustix::fs::Mode::empty(),
    )
    .ok()?;
    Some(File::from(descriptor))
}

pub async fn ask_terminal_question_production(file: File, instance_id: &str) -> TerminalDecision {
    let Ok(flags) = rustix::fs::fcntl_getfl(&file) else {
        return TerminalDecision::WalkedAway;
    };
    if rustix::fs::fcntl_setfl(&file, flags | rustix::fs::OFlags::NONBLOCK).is_err() {
        return TerminalDecision::WalkedAway;
    }
    let Ok(async_fd) = tokio::io::unix::AsyncFd::new(file) else {
        return TerminalDecision::WalkedAway;
    };

    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()).ok();
    let mut sighup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()).ok();

    let spoken_mark = format_spoken_mark(instance_id);
    let is_identified = spoken_mark.is_some();
    if let Some(ref spoken) = spoken_mark {
        let prompt = identified_prompt(spoken);
        use std::io::Write;
        let _ = (&mut async_fd.get_ref()).write_all(prompt.as_bytes());
        let _ = (&mut async_fd.get_ref()).flush();
    } else {
        let prompt = unavailable_prompt();
        use std::io::Write;
        let _ = (&mut async_fd.get_ref()).write_all(prompt.as_bytes());
        let _ = (&mut async_fd.get_ref()).flush();
    }

    let mut line_buf = Vec::new();
    loop {
        let mut got_newline = false;
        loop {
            tokio::select! {
                _ = async {
                    if let Some(ref mut s) = sigint {
                        s.recv().await
                    } else {
                        std::future::pending().await
                    }
                } => {
                    return TerminalDecision::WalkedAway;
                }
                _ = async {
                    if let Some(ref mut s) = sighup {
                        s.recv().await
                    } else {
                        std::future::pending().await
                    }
                } => {
                    return TerminalDecision::WalkedAway;
                }
                guard_res = async_fd.readable() => {
                    let Ok(mut guard) = guard_res else {
                        return TerminalDecision::WalkedAway;
                    };
                    let mut buf = [0u8; 128];
                    match guard.try_io(|inner| {
                        use std::io::Read;
                        (&mut inner.get_ref()).read(&mut buf)
                    }) {
                        Ok(Ok(0)) => {
                            return TerminalDecision::WalkedAway;
                        }
                        Ok(Ok(n)) => {
                            for &b in &buf[..n] {
                                if b == b'\n' {
                                    got_newline = true;
                                    break;
                                }
                                if b != b'\r' {
                                    line_buf.push(b);
                                }
                            }
                            if got_newline {
                                break;
                            }
                        }
                        Ok(Err(err)) if err.kind() == std::io::ErrorKind::WouldBlock => {
                            continue;
                        }
                        Ok(Err(_)) | Err(_) => {
                            return TerminalDecision::WalkedAway;
                        }
                    }
                }
            }
        }

        let line_str = String::from_utf8_lossy(&line_buf);
        let trimmed = line_str.trim().to_ascii_lowercase();
        line_buf.clear();

        if let Some(decision) = answer_decision(parse_answer_token(&trimmed), is_identified) {
            return decision;
        }
        if is_identified {
            use std::io::Write;
            let _ = (&mut async_fd.get_ref()).write_all(format!("{ASK_IDENTIFIED}\n").as_bytes());
            let _ = (&mut async_fd.get_ref()).flush();
        } else {
            use std::io::Write;
            let _ = (&mut async_fd.get_ref()).write_all(format!("{ASK_UNAVAILABLE}\n").as_bytes());
            let _ = (&mut async_fd.get_ref()).flush();
        }
    }
}

pub async fn ask_terminal_question_scripted<T: std::io::Write + std::io::Read + Send + 'static>(
    terminal: &mut T,
    instance_id: &str,
) -> TerminalDecision {
    let spoken_mark = format_spoken_mark(instance_id);
    let is_identified = spoken_mark.is_some();
    if let Some(ref spoken) = spoken_mark {
        let prompt = identified_prompt(spoken);
        let _ = terminal.write_all(prompt.as_bytes());
        let _ = terminal.flush();
    } else {
        let prompt = unavailable_prompt();
        let _ = terminal.write_all(prompt.as_bytes());
        let _ = terminal.flush();
    }

    loop {
        let mut line = String::new();
        let mut buf = [0u8; 1];
        loop {
            match terminal.read(&mut buf) {
                Ok(0) => {
                    return TerminalDecision::WalkedAway;
                }
                Ok(1) => {
                    if buf[0] == b'\n' {
                        break;
                    }
                    if buf[0] != b'\r' {
                        line.push(buf[0] as char);
                    }
                }
                Ok(_) => unreachable!(),
                Err(_) => return TerminalDecision::WalkedAway,
            }
        }
        let trimmed = line.trim().to_ascii_lowercase();
        if let Some(decision) = answer_decision(parse_answer_token(&trimmed), is_identified) {
            return decision;
        }
        if is_identified {
            let _ = terminal.write_all(format!("{ASK_IDENTIFIED}\n").as_bytes());
            let _ = terminal.flush();
        } else {
            let _ = terminal.write_all(format!("{ASK_UNAVAILABLE}\n").as_bytes());
            let _ = terminal.flush();
        }
    }
}

tokio::task_local! {
    pub static ANSWER_LOCK_TIMEOUT_OVERRIDE: Duration;
}

pub fn acquire_answer_lock_blocking(
    config_root: &Path,
    timeout: Duration,
) -> Result<File, DiagnosticCode> {
    let path = config_root.join(ANSWER_LOCK_FILENAME);
    let descriptor = rustix::fs::open(
        &path,
        rustix::fs::OFlags::RDWR
            | rustix::fs::OFlags::CLOEXEC
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CREATE,
        rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
    )
    .map_err(|_| DiagnosticCode::PrivateStateIo)?;
    let file = File::from(descriptor);
    let _ = file.set_permissions(fs::Permissions::from_mode(0o600));

    let start = Instant::now();
    loop {
        match rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => return Ok(file),
            Err(err) if err == rustix::io::Errno::AGAIN || err == rustix::io::Errno::WOULDBLOCK => {
                if start.elapsed() >= timeout {
                    return Err(DiagnosticCode::PrivateStateIo);
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(_) => return Err(DiagnosticCode::PrivateStateIo),
        }
    }
}

pub async fn acquire_answer_lock(config_root: &Path) -> Result<File, DiagnosticCode> {
    let timeout = ANSWER_LOCK_TIMEOUT_OVERRIDE
        .try_with(|&d| d)
        .unwrap_or(ANSWER_LOCK_TIMEOUT);
    let root = config_root.to_path_buf();
    tokio::task::spawn_blocking(move || acquire_answer_lock_blocking(&root, timeout))
        .await
        .map_err(|_| DiagnosticCode::PrivateStateIo)?
}

pub fn read_answer_file(config_root: &Path) -> Result<Option<AnswerRecord>, DiagnosticCode> {
    let path = config_root.join(ANSWER_FILENAME);
    let mut file = match open_regular_readonly(&path) {
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
        Err(StorageError::Io { source, .. })
            if source.kind() == std::io::ErrorKind::PermissionDenied =>
        {
            return Err(DiagnosticCode::PrivateStateIo);
        }
        Err(_) => return Err(DiagnosticCode::PrivateStateIo),
    };
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|_| DiagnosticCode::PrivateStateIo)?;
    let record: AnswerRecord =
        serde_json::from_slice(&bytes).map_err(|_| DiagnosticCode::PrivateStateInvalid)?;
    if !is_valid_confirmed_value(&record.confirmed) {
        return Err(DiagnosticCode::PrivateStateInvalid);
    }
    Ok(Some(record))
}

fn is_valid_confirmed_value(value: &str) -> bool {
    if value.is_empty() {
        return true;
    }
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub fn write_answer_file(config_root: &Path, confirmed: &str) -> Result<(), DiagnosticCode> {
    let record = AnswerRecord {
        confirmed: confirmed.to_owned(),
    };
    let bytes = serde_json::to_vec(&record).map_err(|_| DiagnosticCode::PrivateStateInvalid)?;
    match atomic_write_bytes(&config_root.join(ANSWER_FILENAME), config_root, &bytes) {
        Ok(()) => Ok(()),
        Err(StorageError::InvalidTarget(_)) => Err(DiagnosticCode::PrivateStateInvalid),
        Err(_) => Err(DiagnosticCode::PrivateStateIo),
    }
}

pub fn grandfather_or_settle(config_root: &Path) -> Result<(), DiagnosticCode> {
    match read_answer_file(config_root) {
        Ok(Some(_)) => Ok(()),
        Ok(None) => {
            // Absent answer file: grandfather if valid credential, settle empty otherwise
            match load_credential(config_root) {
                Ok(Some(credential)) if !credential.client_cert_pem.is_empty() => {
                    let generation_hex =
                        hex_encode(&compute_pairing_generation(&credential.client_cert_pem));
                    write_answer_file(config_root, &generation_hex)
                }
                Ok(Some(_)) | Ok(None) | Err(DiagnosticCode::PrivateStateInvalid) => {
                    write_answer_file(config_root, "")
                }
                Err(err) => Err(err),
            }
        }
        Err(_) => {
            // Unreadable or malformed stays held, leave bytes unchanged
            Ok(())
        }
    }
}

pub fn is_pairing_confirmed(config_root: &Path, credential: &Credential) -> bool {
    if credential.client_cert_pem.is_empty() {
        return false;
    }
    let expected_generation = hex_encode(&compute_pairing_generation(&credential.client_cert_pem));
    match read_answer_file(config_root) {
        Ok(Some(record)) => !record.confirmed.is_empty() && record.confirmed == expected_generation,
        _ => false,
    }
}

pub fn is_status_held(config_root: &Path) -> bool {
    let Ok(Some(credential)) = load_credential(config_root) else {
        return false;
    };
    if credential.client_cert_pem.is_empty() {
        return false;
    }
    let expected_generation = hex_encode(&compute_pairing_generation(&credential.client_cert_pem));
    match read_answer_file(config_root) {
        Ok(Some(record)) => record.confirmed != expected_generation,
        Ok(None) => false, // status exception: absent answer file is not held
        Err(_) => true,
    }
}

pub fn delete_credential_file(config_root: &Path) -> Result<(), DiagnosticCode> {
    let path = config_root.join(CREDENTIALS_FILENAME);
    match open_regular_readonly(&path) {
        Ok(_) => fs::remove_file(&path).map_err(|_| DiagnosticCode::PrivateStateIo),
        Err(StorageError::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
            Ok(())
        }
        Err(StorageError::InvalidTarget(_)) => Err(DiagnosticCode::PrivateStateInvalid),
        Err(StorageError::Io { source, .. })
            if source.raw_os_error() == Some(rustix::io::Errno::LOOP.raw_os_error()) =>
        {
            Err(DiagnosticCode::PrivateStateInvalid)
        }
        Err(_) => Err(DiagnosticCode::PrivateStateIo),
    }
}

pub fn split_mark_words(value: &str) -> Vec<&str> {
    value
        .split(|ch: char| ch.is_ascii_whitespace() || ch == '\u{00B7}')
        .map(str::trim)
        .filter(|piece| !piece.is_empty())
        .collect()
}

pub fn extract_journal_mark_words(instance_id: &str) -> Option<(String, String)> {
    let spoken = format_spoken_mark(instance_id)?;
    // Format: "<colour>, <colour> · <word>·<word>"
    let (_, word_part) = spoken.split_once(" · ")?;
    let (w1, w2) = word_part.split_once('·')?;
    Some((w1.trim().to_lowercase(), w2.trim().to_lowercase()))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MarkMatch {
    Match,
    Mismatch,
    Usage,
}

pub fn evaluate_mark_words(input_value: &str, instance_id: &str) -> MarkMatch {
    let words = split_mark_words(input_value);
    let Some((target_w1, target_w2)) = extract_journal_mark_words(instance_id) else {
        // Mark is unavailable
        if words.len() == 2 {
            return MarkMatch::Mismatch; // two words when unavailable -> unverifiable branch
        }
        return MarkMatch::Usage;
    };

    if words.len() == 2 {
        if words[0].eq_ignore_ascii_case(&target_w1) && words[1].eq_ignore_ascii_case(&target_w2) {
            MarkMatch::Match
        } else {
            MarkMatch::Mismatch
        }
    } else {
        MarkMatch::Usage
    }
}

pub async fn retire_credential(credential: &Credential, config_root: &Path) {
    let cert_pem = &credential.client_cert_pem;
    if cert_pem.is_empty() {
        return;
    }
    let Ok(certs) = spl_transport::tls::parse_certs(cert_pem) else {
        return;
    };
    let Some(leaf) = certs.first() else {
        return;
    };
    let cert_sha256 = spl_core::ca::sha256_hex(leaf.as_ref());
    let path = format!("/app/network/api/clients/sha256:{cert_sha256}");

    let _ = tokio::time::timeout(RETIRE_TIMEOUT, async {
        let refresh = VersionRefreshState::new(
            config_root.to_path_buf(),
            config_root.to_path_buf(),
            credential.instance_id.clone(),
            &credential.ca_fp_prefix,
            RunIdentity {
                run_id: "00000000000000000000000000000000".to_owned(),
                lock_inode: 0,
            },
        );
        // A relay pairing may also advertise LAN addresses that are no longer
        // reachable. Use its relay for this bounded rejection cleanup so those
        // addresses cannot consume the entire retirement deadline.
        let mut retirement_credential = credential.clone();
        if matches!(
            (credential.relay_origin.as_deref(), credential.device_token.as_deref()),
            (Some(origin), Some(token)) if !origin.is_empty() && !token.is_empty()
        ) {
            retirement_credential.endpoints.clear();
            retirement_credential.local_endpoints = None;
        }
        let Ok(bridge) = PrivateLinkBridge::start(retirement_credential, None, refresh).await
        else {
            return;
        };
        let Ok(client) = JournalClient::bootstrap(&bridge).await else {
            bridge.shutdown().await;
            return;
        };
        if let Ok(request) = client.request(reqwest::Method::DELETE, &path) {
            let mut status = bridge.subscribe_status();
            let mut carrier_live = status.initial().carrier_live;
            let send = request.send();
            tokio::pin!(send);
            let mut send_finished = false;
            loop {
                if carrier_live {
                    break;
                }
                tokio::select! {
                    biased;
                    _ = &mut send => {
                        send_finished = true;
                        break;
                    }
                    update = status.recv() => {
                        carrier_live = match update {
                            Ok(snapshot) => snapshot.carrier_live,
                            Err(_) => bridge.status().carrier_live,
                        };
                    }
                }
            }
            if !send_finished {
                // The bridge marks the carrier live when the dial finishes, before the
                // DELETE frames are written. Shutdown aborts the carrier writer. Wait
                // until those frames can be flushed, then close.
                tokio::select! {
                    biased;
                    _ = &mut send => {}
                    _ = tokio::time::sleep(Duration::from_millis(50)) => {}
                }
            }
        }
        bridge.shutdown().await;
    })
    .await;
}

#[cfg(test)]
mod locale_parser_tests {
    use super::*;
    #[test]
    fn aliases_are_an_isolated_non_enabled_option() {
        let aliases = [
            ("sí", AnswerToken::Yes),
            ("nein", AnswerToken::No),
            ("weiter", AnswerToken::Continue),
            ("abbrechen", AnswerToken::Cancel),
        ];
        for (word, token) in aliases {
            assert_eq!(parse_answer_token(word), None);
            assert_eq!(parse_alias_option(word, &aliases), Some(token));
        }
        assert_eq!(parse_alias_option("unknown", &aliases), None);
        assert_eq!(
            parse_alias_option(
                "go",
                &[("go", AnswerToken::Yes), ("go", AnswerToken::Continue)]
            ),
            None
        );
        assert_eq!(parse_alias_option("yes", &[("yes", AnswerToken::No)]), None);
        assert_eq!(answer_decision(Some(AnswerToken::Continue), true), None);
        assert_eq!(answer_decision(Some(AnswerToken::Yes), false), None);
    }
}
