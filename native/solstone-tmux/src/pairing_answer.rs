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

pub const STEP: &str = "one more step: check your journal's mark.";
pub const MARK_PREFIX: &str = "  your journal's mark: ";
pub const MARK_UNAVAILABLE: &str = "  your journal's mark: unavailable right now";
pub const SUBTEXT: &str =
    "your journal shows this same mark in its network app. it should match, exactly.";
pub const ASK_IDENTIFIED: &str = "does this match your journal? type yes or no:";
pub const COULDNT_VERIFY: &str = "couldn't verify.";
pub const BODY_UNAVAILABLE: &str = "this computer couldn't work out your journal's mark, so there's nothing to compare. continue only if you're sure the link came from your journal.";
pub const ASK_UNAVAILABLE: &str = "type continue to pair anyway, or cancel to stop:";
pub const PAIRED: &str = "paired.";
pub const NOT_PAIRED: &str = "not paired.";
pub const MISMATCH_BODY: &str = "you said this mark doesn't match the one your journal shows, so this computer isn't paired, and nothing it has kept went to that journal through this link. you may have pasted the wrong link, or something isn't right. get a fresh pair link from your journal and run setup again, or email support@solstone.app and we'll help.";
pub const CANCEL: &str = "pairing cancelled. nothing this computer has kept went to that journal through this link. get a fresh pair link from your journal and run setup again when you're ready.";
pub const HELD: &str = "waiting for you to confirm your journal's mark. nothing waiting goes into your journal until you do.";
pub const RUN_LINE: &str = "when you're ready, run: solstone-tmux confirm";
pub const CONFIRM_DONE: &str = "your journal's mark is already confirmed. nothing to do.";
pub const CONFIRM_UNPAIRED: &str = "not paired. to pair, run: solstone-tmux setup";
pub const SETUP_NO_TERMINAL: &str = "setup can't ask you about your journal's mark here. run it in a terminal, or give the mark's two words with --mark \"word word\".";
pub const CONFIRM_NO_TERMINAL: &str = "confirm can't ask you about your journal's mark here. run it in a terminal, or give the mark's two words with --mark \"word word\".";
pub const MARK_HELP: &str = "the two words of the mark your journal's network app shows. needed when there's no terminal to ask you on.";
pub const MARK_USAGE: &str =
    "give the two words of your journal's mark, like --mark \"bramble quokka\".";
pub const MARK_MISMATCH: &str = "the mark words you gave don't match the journal's mark, so this computer isn't paired, and nothing it has kept went to that journal through this link. check the words, get a fresh pair link from your journal and run setup again.";
pub const MARK_UNVERIFIABLE_SETUP: &str = "this computer couldn't work out the journal's mark, so the words you gave can't be matched. this computer isn't paired, and nothing it has kept went to that journal through this link. get a fresh pair link from your journal and run setup in a terminal to decide for yourself.";
pub const MARK_UNVERIFIABLE_CONFIRM: &str = "this computer couldn't work out the journal's mark, so the words you gave can't be matched. waiting for you to confirm your journal's mark. nothing waiting goes into your journal until you do. run solstone-tmux confirm in a terminal to decide for yourself.";
pub const STATUS_HELD_PAIRING: &str = "pairing: waiting for you to confirm your journal's mark";
pub const STATUS_HELD_CONFIRM: &str = "confirm with: solstone-tmux confirm";

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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminalDecision {
    Yes,
    No,
    WalkedAway,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Diagnostic(DiagnosticCode),
    Usage(&'static str),
    Owner { code: i32, lines: Vec<String> },
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
        let prompt = format!("{STEP}\n\n{MARK_PREFIX}{spoken}\n\n{SUBTEXT}\n{ASK_IDENTIFIED}\n");
        use std::io::Write;
        let _ = (&mut async_fd.get_ref()).write_all(prompt.as_bytes());
        let _ = (&mut async_fd.get_ref()).flush();
    } else {
        let prompt = format!(
            "{STEP}\n\n{MARK_UNAVAILABLE}\n\n{COULDNT_VERIFY}\n{BODY_UNAVAILABLE}\n{ASK_UNAVAILABLE}\n"
        );
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

        if is_identified {
            if trimmed == "yes" || trimmed == "y" {
                return TerminalDecision::Yes;
            }
            if trimmed == "no" || trimmed == "n" {
                return TerminalDecision::No;
            }
            use std::io::Write;
            let _ = (&mut async_fd.get_ref()).write_all(format!("{ASK_IDENTIFIED}\n").as_bytes());
            let _ = (&mut async_fd.get_ref()).flush();
        } else {
            if trimmed == "continue" {
                return TerminalDecision::Yes;
            }
            if trimmed == "cancel" {
                return TerminalDecision::No;
            }
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
        let prompt = format!("{STEP}\n\n{MARK_PREFIX}{spoken}\n\n{SUBTEXT}\n{ASK_IDENTIFIED}\n");
        let _ = terminal.write_all(prompt.as_bytes());
        let _ = terminal.flush();
    } else {
        let prompt = format!(
            "{STEP}\n\n{MARK_UNAVAILABLE}\n\n{COULDNT_VERIFY}\n{BODY_UNAVAILABLE}\n{ASK_UNAVAILABLE}\n"
        );
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
        if is_identified {
            if trimmed == "yes" || trimmed == "y" {
                return TerminalDecision::Yes;
            }
            if trimmed == "no" || trimmed == "n" {
                return TerminalDecision::No;
            }
            let _ = terminal.write_all(format!("{ASK_IDENTIFIED}\n").as_bytes());
            let _ = terminal.flush();
        } else {
            if trimmed == "continue" {
                return TerminalDecision::Yes;
            }
            if trimmed == "cancel" {
                return TerminalDecision::No;
            }
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
        let Ok(bridge) = PrivateLinkBridge::start(credential.clone(), None, refresh).await else {
            return;
        };
        let Ok(client) = JournalClient::bootstrap(&bridge).await else {
            bridge.shutdown().await;
            return;
        };
        if let Ok(request) = client.request(reqwest::Method::DELETE, &path) {
            let _ = request.send().await;
        }
        bridge.shutdown().await;
    })
    .await;
}
