// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

mod support;

use std::fs::{self, File};
use std::io::{Cursor, Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use rcgen::{CertificateParams, KeyPair, PKCS_ECDSA_P256_SHA256};
use solstone_tmux::cli::MarkOption;
use solstone_tmux::health::{DiagnosticCode, HealthWriter};
use solstone_tmux::instance_lock::InstanceLock;
use solstone_tmux::journal_version::hex_encode;
use solstone_tmux::pairing_answer::*;
use solstone_tmux::paths::{PlatformKind, ensure_private_directory};
use solstone_tmux::post_connect::compute_pairing_generation;
use solstone_tmux::private_link::{
    CREDENTIALS_FILENAME, acquire_private_state_lock, confirm, format_spoken_mark, load_credential,
    persist_credential, setup_with_pairer,
};
use solstone_tmux::storage::{AtomicWriteFault, set_atomic_write_fault_for_path};
use solstone_tmux::sync::{RetentionFence, SyncTask, SyncWake};
use spl_core::ca::extract_spki_der;
use spl_core::relay_window::jid_from_spki;
use spl_transport::credential::Credential;
use support::pairing_peer::DirectPairingPeer;
use support::private_link_peer::{PrivateLinkPeer, RelayServer};
use support::{FakeEnvironment, IsolatedRoots, TestDirectory};

fn assert_journal_retire(
    peer: &PrivateLinkPeer,
    expected_sha: &str,
    expected_status: u16,
    index: usize,
) {
    assert_eq!(peer.requests().len(), index + 1);
    assert_eq!(peer.requests()[index].method(), "DELETE");
    assert_eq!(
        peer.requests()[index].path(),
        format!("/app/network/api/clients/sha256:{expected_sha}")
    );
    assert_eq!(
        peer.requests()[index].response_status(),
        Some(expected_status)
    );
    assert_eq!(peer.system_status_request_count(), 0);
    assert_eq!(peer.clients_self_request_count(), 0);
    assert_eq!(peer.relay_access_request_count(), 0);
    assert_eq!(peer.accepted_carriers(), index + 1);
}

fn hung_up_stdio() -> Stdio {
    let (reader, writer) = std::io::pipe().expect("pipe");
    drop(reader);
    Stdio::from(writer)
}

struct RefusalContext {
    _temp: TestDirectory,
    roots: IsolatedRoots,
    env: FakeEnvironment,
    peer: PrivateLinkPeer,
    relay_server: Option<RelayServer>,
    cred: Credential,
}

impl RefusalContext {
    async fn new(relay: bool, name: &str, instance_id: String) -> Self {
        let temp = TestDirectory::new(name);
        let roots = IsolatedRoots::new(temp.path());
        let env = FakeEnvironment::from_paths(roots.entries().iter().cloned());
        ensure_private_directory(&roots.config_root()).expect("config");

        let peer = PrivateLinkPeer::start().await;
        let (relay_server, mut cred) = if relay {
            let server = peer.start_relay_server().await;
            let cred = peer.relay_credential(server.origin());
            (Some(server), cred)
        } else {
            (None, peer.credential())
        };
        cred.instance_id = instance_id;

        Self {
            _temp: temp,
            roots,
            env,
            peer,
            relay_server,
            cred,
        }
    }

    async fn shutdown(self) {
        if let Some(server) = self.relay_server {
            server.shutdown().await;
        }
        self.peer.shutdown().await;
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
}

fn test_jid() -> String {
    let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("generate key");
    let params = CertificateParams::new(Vec::<String>::new()).expect("params");
    let cert = params.self_signed(&key).expect("cert");
    let spki = extract_spki_der(cert.der()).expect("spki");
    jid_from_spki(&spki).expect("jid")
}

struct TestTerminal {
    input: Cursor<Vec<u8>>,
    output: Arc<std::sync::Mutex<Vec<u8>>>,
}

impl TestTerminal {
    fn new(input: &str) -> Self {
        Self {
            input: Cursor::new(input.as_bytes().to_vec()),
            output: Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    #[allow(dead_code)]
    fn output_text(&self) -> String {
        let bytes = self.output.lock().expect("lock").clone();
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

impl Read for TestTerminal {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.input.read(buf)
    }
}

impl Write for TestTerminal {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.output.lock().expect("lock").write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.output.lock().expect("lock").flush()
    }
}

struct ErrorTerminal;

impl Read for ErrorTerminal {
    fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
        Err(std::io::Error::other("simulated terminal read error"))
    }
}

impl Write for ErrorTerminal {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct TrackingReader {
    read_called: Arc<AtomicBool>,
}

impl Read for TrackingReader {
    fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
        self.read_called.store(true, Ordering::SeqCst);
        Ok(0)
    }
}

struct SwappingTerminal {
    input: Cursor<Vec<u8>>,
    output: Arc<std::sync::Mutex<Vec<u8>>>,
    config_root: PathBuf,
    new_cred: Credential,
    swapped: bool,
}

impl SwappingTerminal {
    fn new(input: &str, config_root: PathBuf, new_cred: Credential) -> Self {
        Self {
            input: Cursor::new(input.as_bytes().to_vec()),
            output: Arc::new(std::sync::Mutex::new(Vec::new())),
            config_root,
            new_cred,
            swapped: false,
        }
    }
}

impl Read for SwappingTerminal {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if !self.swapped {
            self.swapped = true;
            persist_credential(&self.config_root, &self.new_cred).expect("persist swapped cred");
        }
        self.input.read(buf)
    }
}

impl Write for SwappingTerminal {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.output.lock().expect("lock").write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.output.lock().expect("lock").flush()
    }
}

fn open_pty() -> (File, PathBuf) {
    let master =
        rustix::pty::openpt(rustix::pty::OpenptFlags::RDWR | rustix::pty::OpenptFlags::NOCTTY)
            .expect("openpt failed");
    rustix::pty::grantpt(&master).expect("grantpt failed");
    rustix::pty::unlockpt(&master).expect("unlockpt failed");
    let slave_path = rustix::pty::ptsname(&master, Vec::new()).expect("ptsname failed");
    let slave_path = PathBuf::from(slave_path.to_str().expect("valid utf-8 ptsname"));
    (File::from(master), slave_path)
}

fn read_pty_until_pattern(
    master: &File,
    child: &mut std::process::Child,
    pattern: &str,
    timeout: Duration,
) -> String {
    let start = std::time::Instant::now();
    let mut accumulated = String::new();
    let mut buf = [0u8; 1024];

    let flags = rustix::fs::fcntl_getfl(master).expect("fcntl getfl");
    rustix::fs::fcntl_setfl(master, flags | rustix::fs::OFlags::NONBLOCK).expect("fcntl setfl");

    while start.elapsed() < timeout {
        if let Some(status) = child.try_wait().expect("child try_wait") {
            let mut remaining = Vec::new();
            use std::io::Read;
            let _ = (&*master).read_to_end(&mut remaining);
            accumulated.push_str(&String::from_utf8_lossy(&remaining));
            if accumulated.contains(pattern) {
                return accumulated;
            }
            panic!("child exited prematurely with status {status:?}; output: {accumulated:?}");
        }

        use std::io::Read;
        match (&*master).read(&mut buf) {
            Ok(0) => {
                if accumulated.contains(pattern) {
                    return accumulated;
                }
                let status = child.wait().expect("child wait");
                panic!("PTY master EOF; child exited with {status:?}; output: {accumulated:?}");
            }
            Ok(n) => {
                accumulated.push_str(&String::from_utf8_lossy(&buf[..n]));
                if accumulated.contains(pattern) {
                    return accumulated;
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(err) if err.raw_os_error() == Some(5) => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(err) => {
                panic!("PTY master read error: {err:?}");
            }
        }
    }
    panic!("timed out after {timeout:?} waiting for pattern {pattern:?}; output: {accumulated:?}");
}

// ===========================================================================
// 1. mark_wording_owner_transcript
// ===========================================================================

#[test]
fn mark_wording_owner_transcript() {
    assert_eq!(STEP, "one more step: check your journal's mark.");
    assert_eq!(MARK_PREFIX, "  your journal's mark: ");
    assert_eq!(
        MARK_UNAVAILABLE,
        "  your journal's mark: unavailable right now"
    );
    assert_eq!(
        SUBTEXT,
        "your journal shows this same mark in its network app. it should match, exactly."
    );
    assert_eq!(
        ASK_IDENTIFIED,
        "does this match your journal? type yes or no:"
    );
    assert_eq!(COULDNT_VERIFY, "couldn't verify.");
    assert_eq!(
        BODY_UNAVAILABLE,
        "this computer couldn't work out your journal's mark, so there's nothing to compare. continue only if you're sure the link came from your journal."
    );
    assert_eq!(
        ASK_UNAVAILABLE,
        "type continue to pair anyway, or cancel to stop:"
    );
    assert_eq!(PAIRED, "paired.");
    assert_eq!(NOT_PAIRED, "not paired.");
    assert_eq!(
        MISMATCH_BODY,
        "you said this mark doesn't match the one your journal shows, so this computer isn't paired, and nothing it has kept went to that journal through this link. you may have pasted the wrong link, or something isn't right. get a fresh pair link from your journal and run setup again, or email support@solstone.app and we'll help."
    );
    assert_eq!(
        CANCEL,
        "pairing cancelled. nothing this computer has kept went to that journal through this link. get a fresh pair link from your journal and run setup again when you're ready."
    );
    assert_eq!(
        HELD,
        "waiting for you to confirm your journal's mark. nothing waiting goes into your journal until you do."
    );
    assert_eq!(RUN_LINE, "when you're ready, run: solstone-tmux confirm");
    assert_eq!(
        CONFIRM_DONE,
        "your journal's mark is already confirmed. nothing to do."
    );
    assert_eq!(
        CONFIRM_UNPAIRED,
        "not paired. to pair, run: solstone-tmux setup"
    );
    assert_eq!(
        SETUP_NO_TERMINAL,
        "setup can't ask you about your journal's mark here. run it in a terminal, or give the mark's two words with --mark \"word word\"."
    );
    assert_eq!(
        CONFIRM_NO_TERMINAL,
        "confirm can't ask you about your journal's mark here. run it in a terminal, or give the mark's two words with --mark \"word word\"."
    );
    assert_eq!(
        MARK_HELP,
        "the two words of the mark your journal's network app shows. needed when there's no terminal to ask you on."
    );
    assert_eq!(
        MARK_USAGE,
        "give the two words of your journal's mark, like --mark \"bramble quokka\"."
    );
    assert_eq!(
        MARK_MISMATCH,
        "the mark words you gave don't match the journal's mark, so this computer isn't paired, and nothing it has kept went to that journal through this link. check the words, get a fresh pair link from your journal and run setup again."
    );
    assert_eq!(
        MARK_UNVERIFIABLE_SETUP,
        "this computer couldn't work out the journal's mark, so the words you gave can't be matched. this computer isn't paired, and nothing it has kept went to that journal through this link. get a fresh pair link from your journal and run setup in a terminal to decide for yourself."
    );
    assert_eq!(
        MARK_UNVERIFIABLE_CONFIRM,
        "this computer couldn't work out the journal's mark, so the words you gave can't be matched. waiting for you to confirm your journal's mark. nothing waiting goes into your journal until you do. run solstone-tmux confirm in a terminal to decide for yourself."
    );
    assert_eq!(
        STATUS_HELD_PAIRING,
        "pairing: waiting for you to confirm your journal's mark"
    );
    assert_eq!(STATUS_HELD_CONFIRM, "confirm with: solstone-tmux confirm");

    let jid = test_jid();
    let spoken = format_spoken_mark(&jid).expect("spoken mark");
    let (w1, w2) = extract_journal_mark_words(&jid).expect("words");
    assert!(spoken.contains(&format!("{w1}·{w2}")));

    let identified_prompt =
        format!("{STEP}\n\n{MARK_PREFIX}{spoken}\n\n{SUBTEXT}\n{ASK_IDENTIFIED}\n");
    let unavailable_prompt = format!(
        "{STEP}\n\n{MARK_UNAVAILABLE}\n\n{COULDNT_VERIFY}\n{BODY_UNAVAILABLE}\n{ASK_UNAVAILABLE}\n"
    );
    assert!(identified_prompt.contains(&spoken));
    assert!(unavailable_prompt.contains(MARK_UNAVAILABLE));

    runtime().block_on(async {
        let mut term = TestTerminal::new("yes\n");
        let dec = ask_terminal_question_scripted(&mut term, &jid).await;
        assert_eq!(dec, TerminalDecision::Yes);
        assert_eq!(term.output_text(), identified_prompt);

        let mut term_unavail = TestTerminal::new("continue\n");
        let dec_unavail =
            ask_terminal_question_scripted(&mut term_unavail, "test-pairing-instance").await;
        assert_eq!(dec_unavail, TerminalDecision::Yes);
        assert_eq!(term_unavail.output_text(), unavailable_prompt);
    });
}

// ===========================================================================
// 2. mark_argument_before_stdin
// ===========================================================================

#[test]
fn mark_argument_before_stdin() {
    runtime().block_on(async {
        let temp = TestDirectory::new("mark-arg-before-stdin");
        let roots = IsolatedRoots::new(temp.path());
        let env = FakeEnvironment::from_paths(roots.entries().iter().cloned());
        ensure_private_directory(&roots.config_root()).expect("config root");

        let jid = test_jid();
        let (w1, w2) = extract_journal_mark_words(&jid).expect("words");
        let bad_options = vec![
            MarkOption::Value(w1.clone()),
            MarkOption::Value("one two three".to_string()),
            MarkOption::Value("bramblequokka".to_string()),
            MarkOption::Value(format!("{w1}{w2}")),
            MarkOption::Value("".to_string()),
            MarkOption::Repeated,
            MarkOption::MissingValue,
        ];

        for opt in bad_options {
            let read_called = Arc::new(AtomicBool::new(false));
            let pairer_called = Arc::new(AtomicBool::new(false));
            let reader = TrackingReader {
                read_called: Arc::clone(&read_called),
            };
            let pairer_flag = Arc::clone(&pairer_called);

            let outcome = setup_with_pairer(
                PlatformKind::Linux,
                &env,
                reader,
                Ok::<String, &'static str>("test-host".to_string()),
                |_link, _dev, _fields| async move {
                    pairer_flag.store(true, Ordering::SeqCst);
                    unreachable!()
                },
                TerminalSeat::Scripted(None::<Cursor<Vec<u8>>>),
                opt,
            )
            .await;

            assert_eq!(outcome, Outcome::Usage(MARK_USAGE));
            assert!(!read_called.load(Ordering::SeqCst));
            assert!(!pairer_called.load(Ordering::SeqCst));
            assert!(!roots.config_root().join(ANSWER_FILENAME).exists());
        }

        // Test Absent mark + Scripted(None) -> exits 1 with SETUP_NO_TERMINAL before reading stdin
        let read_called = Arc::new(AtomicBool::new(false));
        let pairer_called = Arc::new(AtomicBool::new(false));
        let reader = TrackingReader {
            read_called: Arc::clone(&read_called),
        };
        let pairer_flag = Arc::clone(&pairer_called);
        let outcome = setup_with_pairer(
            PlatformKind::Linux,
            &env,
            reader,
            Ok::<String, &'static str>("test-host".to_string()),
            |_link, _dev, _fields| async move {
                pairer_flag.store(true, Ordering::SeqCst);
                unreachable!()
            },
            TerminalSeat::Scripted(None::<Cursor<Vec<u8>>>),
            MarkOption::Absent,
        )
        .await;
        assert_eq!(
            outcome,
            Outcome::Owner {
                code: 1,
                lines: vec![SETUP_NO_TERMINAL.to_string()],
            }
        );
        assert!(!read_called.load(Ordering::SeqCst));
        assert!(!pairer_called.load(Ordering::SeqCst));
        assert!(!roots.config_root().join(CREDENTIALS_FILENAME).exists());
        assert!(!roots.config_root().join(ANSWER_FILENAME).exists());

        // Test answer lock timeout scoped to 200ms with lock held -> returns Diagnostic(PrivateStateIo)
        let answer_lock_file =
            acquire_answer_lock_blocking(&roots.config_root(), Duration::from_secs(1))
                .expect("acquire answer lock");
        let read_called = Arc::new(AtomicBool::new(false));
        let pairer_called = Arc::new(AtomicBool::new(false));
        let reader = TrackingReader {
            read_called: Arc::clone(&read_called),
        };
        let pairer_flag = Arc::clone(&pairer_called);

        let outcome = ANSWER_LOCK_TIMEOUT_OVERRIDE
            .scope(Duration::from_millis(200), async {
                setup_with_pairer(
                    PlatformKind::Linux,
                    &env,
                    reader,
                    Ok::<String, &'static str>("test-host".to_string()),
                    |_link, _dev, _fields| async move {
                        pairer_flag.store(true, Ordering::SeqCst);
                        unreachable!()
                    },
                    TerminalSeat::Scripted(Some(TestTerminal::new("yes\n"))),
                    MarkOption::Absent,
                )
                .await
            })
            .await;

        assert_eq!(outcome, Outcome::Diagnostic(DiagnosticCode::PrivateStateIo));
        assert!(!read_called.load(Ordering::SeqCst));
        assert!(!pairer_called.load(Ordering::SeqCst));
        drop(answer_lock_file);

        // SyncTask answer-lock timeout: persist credential, no answer file, hold answer lock for 30s
        {
            let temp_task = TestDirectory::new("mark-arg-sync-lock-timeout");
            let roots_task = IsolatedRoots::new(temp_task.path());
            ensure_private_directory(&roots_task.config_root()).expect("config");
            ensure_private_directory(&roots_task.data_root()).expect("data");

            let peer = PrivateLinkPeer::start().await;
            let mut cred = peer.credential();
            cred.instance_id = test_jid();
            persist_credential(&roots_task.config_root(), &cred).expect("persist");

            let answer_lock_file =
                acquire_answer_lock_blocking(&roots_task.config_root(), Duration::from_secs(30))
                    .expect("acquire answer lock");

            let (stop_tx, shutdown_rx) = tokio::sync::watch::channel(false);
            let (activity_tx, _activity_rx) =
                tokio::sync::watch::channel(solstone_tmux::sync::SyncActivity::Idle);
            let clock = Arc::new(solstone_tmux::clock::SystemClock::utc());
            let config =
                solstone_tmux::config::RuntimeConfig::load(&roots_task.config_root(), "host")
                    .expect("config");

            let lock = InstanceLock::acquire(&roots_task.data_root()).expect("acquire data lock");
            let health = HealthWriter::new(roots_task.data_root(), &lock);

            let task = SyncTask {
                config_root: roots_task.config_root(),
                data_root: roots_task.data_root(),
                config,
                hostname: "host".to_string(),
                clock: clock.clone(),
                wake: SyncWake::default(),
                activity: activity_tx,
                health,
                retention_fence: Arc::new(RetentionFence::new()),
                identity: lock.identity().clone(),
                health_refresh_interval: Duration::from_millis(50),
                answer_lock_timeout: Duration::from_millis(200),
            };

            let task_handle = tokio::spawn(async move {
                let _ = task.run(shutdown_rx).await;
            });

            tokio::time::sleep(Duration::from_millis(500)).await;
            let _ = stop_tx.send(true);
            let start = Instant::now();
            let _ = task_handle.await;
            assert!(start.elapsed() < Duration::from_secs(2));

            assert!(!roots_task.config_root().join(ANSWER_FILENAME).exists());
            assert_eq!(peer.requests().len(), 0);
            assert_eq!(peer.accepted_carriers(), 0);

            drop(answer_lock_file);
            peer.shutdown().await;
        }
    });
}

// ===========================================================================
// 3. setup_answers_and_reread
// ===========================================================================

#[test]
fn setup_answers_and_reread() {
    runtime().block_on(async {
        let peer = PrivateLinkPeer::start().await;
        let cred = peer.credential();
        let expected_sha = peer.expected_client_sha256();

        // 1. "yes" with real jid -> code 0 [PAIRED], confirms
        {
            let temp = TestDirectory::new("setup-ans-yes");
            let roots = IsolatedRoots::new(temp.path());
            let env = FakeEnvironment::from_paths(roots.entries().iter().cloned());
            let mut mut_cred = cred.clone();
            mut_cred.instance_id = test_jid();
            let cred_call = mut_cred.clone();

            let outcome = setup_with_pairer(
                PlatformKind::Linux,
                &env,
                Cursor::new(b"link"),
                Ok::<String, &'static str>("test-host".to_string()),
                |_link, _dev, _fields| async move { Ok(cred_call) },
                TerminalSeat::Scripted(Some(TestTerminal::new("yes\n"))),
                MarkOption::Absent,
            )
            .await;

            assert_eq!(
                outcome,
                Outcome::Owner {
                    code: 0,
                    lines: vec![PAIRED.to_string()],
                }
            );
            assert!(roots.config_root().join(CREDENTIALS_FILENAME).exists());
            assert!(is_pairing_confirmed(&roots.config_root(), &mut_cred));
        }

        // 2. "y" with real jid -> code 0 [PAIRED], confirms
        {
            let temp = TestDirectory::new("setup-ans-y");
            let roots = IsolatedRoots::new(temp.path());
            let env = FakeEnvironment::from_paths(roots.entries().iter().cloned());
            let mut mut_cred = cred.clone();
            mut_cred.instance_id = test_jid();
            let cred_call = mut_cred.clone();

            let outcome = setup_with_pairer(
                PlatformKind::Linux,
                &env,
                Cursor::new(b"link"),
                Ok::<String, &'static str>("test-host".to_string()),
                |_link, _dev, _fields| async move { Ok(cred_call) },
                TerminalSeat::Scripted(Some(TestTerminal::new("y\n"))),
                MarkOption::Absent,
            )
            .await;

            assert_eq!(
                outcome,
                Outcome::Owner {
                    code: 0,
                    lines: vec![PAIRED.to_string()],
                }
            );
            assert!(roots.config_root().join(CREDENTIALS_FILENAME).exists());
            assert!(is_pairing_confirmed(&roots.config_root(), &mut_cred));
        }

        // 3. "no" with real jid -> code 1 [NOT_PAIRED, MISMATCH_BODY], saves nothing, deletes cert on peer
        {
            let temp = TestDirectory::new("setup-ans-no");
            let roots = IsolatedRoots::new(temp.path());
            let env = FakeEnvironment::from_paths(roots.entries().iter().cloned());
            let mut mut_cred = cred.clone();
            mut_cred.instance_id = test_jid();
            let cred_call = mut_cred.clone();

            let outcome = setup_with_pairer(
                PlatformKind::Linux,
                &env,
                Cursor::new(b"link"),
                Ok::<String, &'static str>("test-host".to_string()),
                |_link, _dev, _fields| async move { Ok(cred_call) },
                TerminalSeat::Scripted(Some(TestTerminal::new("no\n"))),
                MarkOption::Absent,
            )
            .await;

            assert_eq!(
                outcome,
                Outcome::Owner {
                    code: 1,
                    lines: vec![NOT_PAIRED.to_string(), MISMATCH_BODY.to_string()],
                }
            );
            assert!(!roots.config_root().join(CREDENTIALS_FILENAME).exists());
            assert_journal_retire(&peer, &expected_sha, 200, 0);
        }

        // 4. "cancel" with malformed jid ("test-pairing-instance") -> code 1 [CANCEL], saves nothing
        {
            let temp = TestDirectory::new("setup-ans-cancel");
            let roots = IsolatedRoots::new(temp.path());
            let env = FakeEnvironment::from_paths(roots.entries().iter().cloned());
            let mut mut_cred = cred.clone();
            mut_cred.instance_id = "test-pairing-instance".to_string();
            let cred_call = mut_cred.clone();

            let outcome = setup_with_pairer(
                PlatformKind::Linux,
                &env,
                Cursor::new(b"link"),
                Ok::<String, &'static str>("test-host".to_string()),
                |_link, _dev, _fields| async move { Ok(cred_call) },
                TerminalSeat::Scripted(Some(TestTerminal::new("cancel\n"))),
                MarkOption::Absent,
            )
            .await;

            assert_eq!(
                outcome,
                Outcome::Owner {
                    code: 1,
                    lines: vec![CANCEL.to_string()],
                }
            );
            assert!(!roots.config_root().join(CREDENTIALS_FILENAME).exists());
            assert_journal_retire(&peer, &expected_sha, 200, 1);
        }

        // 5. EOF -> code 5 [HELD, RUN_LINE], credential saved, unconfirmed
        {
            let temp = TestDirectory::new("setup-ans-eof");
            let roots = IsolatedRoots::new(temp.path());
            let env = FakeEnvironment::from_paths(roots.entries().iter().cloned());
            let mut mut_cred = cred.clone();
            mut_cred.instance_id = test_jid();
            let cred_call = mut_cred.clone();

            let outcome = setup_with_pairer(
                PlatformKind::Linux,
                &env,
                Cursor::new(b"link"),
                Ok::<String, &'static str>("test-host".to_string()),
                |_link, _dev, _fields| async move { Ok(cred_call) },
                TerminalSeat::Scripted(Some(TestTerminal::new(""))),
                MarkOption::Absent,
            )
            .await;

            assert_eq!(
                outcome,
                Outcome::Owner {
                    code: 5,
                    lines: vec![HELD.to_string(), RUN_LINE.to_string()],
                }
            );
            assert!(roots.config_root().join(CREDENTIALS_FILENAME).exists());
            assert!(!is_pairing_confirmed(&roots.config_root(), &mut_cred));
        }

        // 6. Read error -> code 5 [HELD, RUN_LINE], credential saved, unconfirmed
        {
            let temp = TestDirectory::new("setup-ans-err");
            let roots = IsolatedRoots::new(temp.path());
            let env = FakeEnvironment::from_paths(roots.entries().iter().cloned());
            let mut mut_cred = cred.clone();
            mut_cred.instance_id = test_jid();
            let cred_call = mut_cred.clone();

            let outcome = setup_with_pairer(
                PlatformKind::Linux,
                &env,
                Cursor::new(b"link"),
                Ok::<String, &'static str>("test-host".to_string()),
                |_link, _dev, _fields| async move { Ok(cred_call) },
                TerminalSeat::Scripted(Some(ErrorTerminal)),
                MarkOption::Absent,
            )
            .await;

            assert_eq!(
                outcome,
                Outcome::Owner {
                    code: 5,
                    lines: vec![HELD.to_string(), RUN_LINE.to_string()],
                }
            );
            assert!(roots.config_root().join(CREDENTIALS_FILENAME).exists());
            assert!(!is_pairing_confirmed(&roots.config_root(), &mut_cred));
        }

        // 7. Empty, 'c', 'continue' on identified branch re-asks only ASK_IDENTIFIED, then 'yes' confirms
        {
            let temp = TestDirectory::new("setup-ans-reread");
            let roots = IsolatedRoots::new(temp.path());
            let env = FakeEnvironment::from_paths(roots.entries().iter().cloned());
            let mut mut_cred = cred.clone();
            mut_cred.instance_id = test_jid();
            let cred_call = mut_cred.clone();
            let term = TestTerminal::new("\nc\ncontinue\nyes\n");
            let out_ref = Arc::clone(&term.output);

            let outcome = setup_with_pairer(
                PlatformKind::Linux,
                &env,
                Cursor::new(b"link"),
                Ok::<String, &'static str>("test-host".to_string()),
                |_link, _dev, _fields| async move { Ok(cred_call) },
                TerminalSeat::Scripted(Some(term)),
                MarkOption::Absent,
            )
            .await;

            assert_eq!(
                outcome,
                Outcome::Owner {
                    code: 0,
                    lines: vec![PAIRED.to_string()],
                }
            );
            let text = String::from_utf8_lossy(&out_ref.lock().expect("lock")).into_owned();
            assert_eq!(text.matches(ASK_IDENTIFIED).count(), 4);
            assert!(!text.contains(ASK_UNAVAILABLE));
        }

        // 7b. Empty, 'c', 'y' on unavailable branch re-asks only ASK_UNAVAILABLE, then 'continue' confirms
        {
            let temp = TestDirectory::new("setup-ans-reread-unavail");
            let roots = IsolatedRoots::new(temp.path());
            let env = FakeEnvironment::from_paths(roots.entries().iter().cloned());
            let mut mut_cred = cred.clone();
            mut_cred.instance_id = "test-pairing-instance".to_string();
            let cred_call = mut_cred.clone();
            let term = TestTerminal::new("\nc\ny\ncontinue\n");
            let out_ref = Arc::clone(&term.output);

            let outcome = setup_with_pairer(
                PlatformKind::Linux,
                &env,
                Cursor::new(b"link"),
                Ok::<String, &'static str>("test-host".to_string()),
                |_link, _dev, _fields| async move { Ok(cred_call) },
                TerminalSeat::Scripted(Some(term)),
                MarkOption::Absent,
            )
            .await;

            assert_eq!(
                outcome,
                Outcome::Owner {
                    code: 0,
                    lines: vec![PAIRED.to_string()],
                }
            );
            let text = String::from_utf8_lossy(&out_ref.lock().expect("lock")).into_owned();
            assert_eq!(text.matches(ASK_UNAVAILABLE).count(), 4);
            assert!(!text.contains(ASK_IDENTIFIED));
            assert!(roots.config_root().join(CREDENTIALS_FILENAME).exists());
            assert!(is_pairing_confirmed(&roots.config_root(), &mut_cred));
        }

        // 8. set_atomic_write_fault_for_path on answer path -> credential kept, exit 5, generation not recorded
        {
            let temp = TestDirectory::new("setup-ans-fault");
            let roots = IsolatedRoots::new(temp.path());
            let env = FakeEnvironment::from_paths(roots.entries().iter().cloned());
            ensure_private_directory(&roots.config_root()).expect("config");
            write_answer_file(&roots.config_root(), "").expect("initial settle");
            let mut mut_cred = cred.clone();
            mut_cred.instance_id = test_jid();
            let cred_call = mut_cred.clone();
            let ans_path = roots.config_root().join(ANSWER_FILENAME);
            set_atomic_write_fault_for_path(&ans_path, Some(AtomicWriteFault::FailBeforeRename));

            let outcome = setup_with_pairer(
                PlatformKind::Linux,
                &env,
                Cursor::new(b"link"),
                Ok::<String, &'static str>("test-host".to_string()),
                |_link, _dev, _fields| async move { Ok(cred_call) },
                TerminalSeat::Scripted(Some(TestTerminal::new("yes\n"))),
                MarkOption::Absent,
            )
            .await;

            set_atomic_write_fault_for_path(&ans_path, None);
            assert_eq!(
                outcome,
                Outcome::Owner {
                    code: 5,
                    lines: vec![HELD.to_string(), RUN_LINE.to_string()],
                }
            );
            assert!(roots.config_root().join(CREDENTIALS_FILENAME).exists());
            assert!(!is_pairing_confirmed(&roots.config_root(), &mut_cred));
        }

        peer.shutdown().await;
    });
}

// ===========================================================================
// 4. setup_signal_at_the_prompt
// ===========================================================================

#[test]
fn setup_signal_at_the_prompt() {
    fn run_setup_and_signal(sig: rustix::process::Signal) {
        let rt = runtime();
        let peer = rt.block_on(async { DirectPairingPeer::start().await });
        let (master, slave_path) = open_pty();
        let temp = TestDirectory::new(&format!("setup-sig-{sig:?}"));
        let roots = IsolatedRoots::new(temp.path());

        let mut child = Command::new(env!("CARGO_BIN_EXE_solstone-tmux"))
            .arg("setup")
            .env_clear()
            .envs(roots.entries().iter().cloned())
            .env("SOLSTONE_TMUX_TERMINAL", &slave_path)
            .stdin(Stdio::piped())
            .stdout(hung_up_stdio())
            .stderr(hung_up_stdio())
            .spawn()
            .expect("spawn setup");

        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(peer.pair_link().as_bytes())
            .expect("write link");

        read_pty_until_pattern(&master, &mut child, ASK_UNAVAILABLE, Duration::from_secs(5));

        let pid = rustix::process::Pid::from_raw(child.id() as i32).expect("pid");
        rustix::process::kill_process(pid, sig).expect("kill");

        let start = std::time::Instant::now();
        let status = loop {
            if let Some(status) = child.try_wait().expect("try wait") {
                break status;
            }
            if start.elapsed() > Duration::from_secs(5) {
                panic!("child did not exit within 5s");
            }
            std::thread::sleep(Duration::from_millis(20));
        };

        assert_eq!(status.code(), Some(5));
        let cred = load_credential(&roots.config_root())
            .expect("load cred")
            .expect("cred exists");
        assert!(!is_pairing_confirmed(&roots.config_root(), &cred));
        rt.block_on(async { peer.shutdown().await });
    }

    run_setup_and_signal(rustix::process::Signal::INT);
    run_setup_and_signal(rustix::process::Signal::HUP);

    // 3rd process: blocked pair_from_link gets SIGINT -> no credentials.json
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind non-answering listener");
    let port = listener.local_addr().unwrap().port();
    let mut blob = vec![0x05, 0x01, 1];
    blob.extend_from_slice(&port.to_be_bytes());
    blob.extend_from_slice(&[127, 0, 0, 1]);
    blob.extend_from_slice(&[0x11; 16]);
    blob.extend_from_slice(&[0u8; 16]);
    let block_link = format!(
        "https://go.solstone.app/p#{}",
        spl_core::crockford::encode(&blob)
    );
    let (master, slave_path) = open_pty();
    let temp = TestDirectory::new("setup-sig-blocked");
    let roots = IsolatedRoots::new(temp.path());

    let mut child = Command::new(env!("CARGO_BIN_EXE_solstone-tmux"))
        .arg("setup")
        .env_clear()
        .envs(roots.entries().iter().cloned())
        .env("SOLSTONE_TMUX_TERMINAL", &slave_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn setup");

    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(block_link.as_bytes())
        .expect("write block link");

    std::thread::sleep(Duration::from_millis(50));
    let pid = rustix::process::Pid::from_raw(child.id() as i32).expect("pid");
    assert!(child.try_wait().expect("try wait").is_none());
    rustix::process::kill_process(pid, rustix::process::Signal::INT).expect("kill");
    let _ = child.wait();
    drop(master);
    assert!(!roots.config_root().join(CREDENTIALS_FILENAME).exists());
}

// ===========================================================================
// 5. mark_argument_words
// ===========================================================================

#[test]
fn mark_argument_words() {
    runtime().block_on(async {
        let peer = PrivateLinkPeer::start().await;
        let cred = peer.credential();
        let expected_sha = peer.expected_client_sha256();
        let jid = test_jid();
        let (w1, w2) = extract_journal_mark_words(&jid).expect("words");

        // Case, spaces, and U+00B7 match and confirm
        let match_variations = vec![
            format!("{w1} {w2}"),
            format!("{} {}", w1.to_uppercase(), w2.to_uppercase()),
            format!("  {w1}   {w2}  "),
            format!("{w1}·{w2}"),
        ];
        for val in match_variations {
            let temp = TestDirectory::new("mark-words-match");
            let roots = IsolatedRoots::new(temp.path());
            let env = FakeEnvironment::from_paths(roots.entries().iter().cloned());
            let mut mut_cred = cred.clone();
            mut_cred.instance_id = jid.clone();
            let cred_call = mut_cred.clone();

            let outcome = setup_with_pairer(
                PlatformKind::Linux,
                &env,
                Cursor::new(b"link"),
                Ok::<String, &'static str>("test-host".to_string()),
                |_link, _dev, _fields| async move { Ok(cred_call) },
                TerminalSeat::Scripted(None::<Cursor<Vec<u8>>>),
                MarkOption::Value(val),
            )
            .await;

            assert_eq!(
                outcome,
                Outcome::Owner {
                    code: 0,
                    lines: vec![PAIRED.to_string()],
                }
            );
            assert!(is_pairing_confirmed(&roots.config_root(), &mut_cred));
        }

        // Swapped words, bram blequokka split, boundary split retire and exit 1
        let boundary = format!("{} {}{}", &w1[..1], &w1[1..], w2);
        let split_boundary = split_mark_words(&boundary);
        assert_eq!(split_boundary.len(), 2);
        assert!(
            !split_boundary[0].eq_ignore_ascii_case(&w1)
                || !split_boundary[1].eq_ignore_ascii_case(&w2)
        );

        let mismatch_variations = vec![
            format!("{w2} {w1}"),
            format!("{} {}", &w1[..1], &w1[1..]),
            boundary,
        ];
        for (index, val) in mismatch_variations.into_iter().enumerate() {
            let temp = TestDirectory::new("mark-words-mismatch");
            let roots = IsolatedRoots::new(temp.path());
            let env = FakeEnvironment::from_paths(roots.entries().iter().cloned());
            let mut mut_cred = cred.clone();
            mut_cred.instance_id = jid.clone();
            let cred_call = mut_cred.clone();

            let outcome = setup_with_pairer(
                PlatformKind::Linux,
                &env,
                Cursor::new(b"link"),
                Ok::<String, &'static str>("test-host".to_string()),
                |_link, _dev, _fields| async move { Ok(cred_call) },
                TerminalSeat::Scripted(None::<Cursor<Vec<u8>>>),
                MarkOption::Value(val),
            )
            .await;

            assert_eq!(
                outcome,
                Outcome::Owner {
                    code: 1,
                    lines: vec![NOT_PAIRED.to_string(), MARK_MISMATCH.to_string()],
                }
            );
            assert!(!roots.config_root().join(CREDENTIALS_FILENAME).exists());
            assert_journal_retire(&peer, &expected_sha, 200, index);
        }

        // Two words when format_spoken_mark is None: setup saves nothing, exit 1, [COULDNT_VERIFY, MARK_UNVERIFIABLE_SETUP], and DELETE happened
        {
            let temp = TestDirectory::new("mark-words-unverifiable-setup");
            let roots = IsolatedRoots::new(temp.path());
            let env = FakeEnvironment::from_paths(roots.entries().iter().cloned());
            let mut mut_cred = cred.clone();
            mut_cred.instance_id = "test-pairing-instance".to_string();
            let cred_call = mut_cred.clone();

            let outcome = setup_with_pairer(
                PlatformKind::Linux,
                &env,
                Cursor::new(b"link"),
                Ok::<String, &'static str>("test-host".to_string()),
                |_link, _dev, _fields| async move { Ok(cred_call) },
                TerminalSeat::Scripted(None::<Cursor<Vec<u8>>>),
                MarkOption::Value("some words".to_string()),
            )
            .await;

            assert_eq!(
                outcome,
                Outcome::Owner {
                    code: 1,
                    lines: vec![
                        COULDNT_VERIFY.to_string(),
                        MARK_UNVERIFIABLE_SETUP.to_string(),
                    ],
                }
            );
            assert!(!roots.config_root().join(CREDENTIALS_FILENAME).exists());
            assert_journal_retire(&peer, &expected_sha, 200, 3);
        }

        // Confirm, with credential already held:
        // one word, three words, empty, concatenation -> exit 2 with nothing changed and no DELETE (fresh peer)
        {
            let fresh_peer = PrivateLinkPeer::start().await;
            let mut fresh_cred = fresh_peer.credential();
            fresh_cred.instance_id = jid.clone();

            let temp = TestDirectory::new("mark-words-confirm-held");
            let roots = IsolatedRoots::new(temp.path());
            let env = FakeEnvironment::from_paths(roots.entries().iter().cloned());
            ensure_private_directory(&roots.config_root()).expect("config");
            persist_credential(&roots.config_root(), &fresh_cred).expect("persist");
            write_answer_file(&roots.config_root(), "").expect("write answer");
            let before_cred = fs::read(roots.config_root().join(CREDENTIALS_FILENAME)).unwrap();
            let before_ans = fs::read(roots.config_root().join(ANSWER_FILENAME)).unwrap();

            for bad_mark in ["word", "one two three", "", &format!("{w1}{w2}")] {
                let outcome = confirm(
                    PlatformKind::Linux,
                    &env,
                    TerminalSeat::Scripted(None::<Cursor<Vec<u8>>>),
                    MarkOption::Value(bad_mark.to_string()),
                )
                .await;
                assert_eq!(outcome, Outcome::Usage(MARK_USAGE));
                assert_eq!(
                    fs::read(roots.config_root().join(CREDENTIALS_FILENAME)).unwrap(),
                    before_cred
                );
                assert_eq!(
                    fs::read(roots.config_root().join(ANSWER_FILENAME)).unwrap(),
                    before_ans
                );
                assert_eq!(fresh_peer.requests().len(), 0);
                assert_eq!(fresh_peer.system_status_request_count(), 0);
                assert_eq!(fresh_peer.clients_self_request_count(), 0);
                assert_eq!(fresh_peer.relay_access_request_count(), 0);
                assert_eq!(fresh_peer.accepted_carriers(), 0);
            }

            fresh_peer.shutdown().await;
        }

        // Confirm of one word with no credentials.json (config dir exists):
        // Outcome::Usage(MARK_USAGE), and neither credentials.json nor pairing-answer.json appears
        {
            let temp = TestDirectory::new("mark-words-confirm-no-cred");
            let roots = IsolatedRoots::new(temp.path());
            let env = FakeEnvironment::from_paths(roots.entries().iter().cloned());
            ensure_private_directory(&roots.config_root()).expect("config");

            let outcome = confirm(
                PlatformKind::Linux,
                &env,
                TerminalSeat::Scripted(None::<Cursor<Vec<u8>>>),
                MarkOption::Value("word".to_string()),
            )
            .await;
            assert_eq!(outcome, Outcome::Usage(MARK_USAGE));
            assert!(!roots.config_root().join(CREDENTIALS_FILENAME).exists());
            assert!(!roots.config_root().join(ANSWER_FILENAME).exists());
        }

        // Confirm of one word against an already-confirmed pairing:
        // Outcome::Usage(MARK_USAGE), both files byte-identical, zero requests on a fresh peer
        {
            let fresh_peer = PrivateLinkPeer::start().await;
            let mut fresh_cred = fresh_peer.credential();
            fresh_cred.instance_id = jid.clone();
            let generation = hex_encode(&compute_pairing_generation(&fresh_cred.client_cert_pem));

            let temp = TestDirectory::new("mark-words-confirm-confirmed");
            let roots = IsolatedRoots::new(temp.path());
            let env = FakeEnvironment::from_paths(roots.entries().iter().cloned());
            ensure_private_directory(&roots.config_root()).expect("config");
            persist_credential(&roots.config_root(), &fresh_cred).expect("persist");
            write_answer_file(&roots.config_root(), &generation).expect("write answer");
            let before_cred = fs::read(roots.config_root().join(CREDENTIALS_FILENAME)).unwrap();
            let before_ans = fs::read(roots.config_root().join(ANSWER_FILENAME)).unwrap();

            let outcome = confirm(
                PlatformKind::Linux,
                &env,
                TerminalSeat::Scripted(None::<Cursor<Vec<u8>>>),
                MarkOption::Value("word".to_string()),
            )
            .await;
            assert_eq!(outcome, Outcome::Usage(MARK_USAGE));
            assert_eq!(
                fs::read(roots.config_root().join(CREDENTIALS_FILENAME)).unwrap(),
                before_cred
            );
            assert_eq!(
                fs::read(roots.config_root().join(ANSWER_FILENAME)).unwrap(),
                before_ans
            );
            assert_eq!(fresh_peer.requests().len(), 0);
            assert_eq!(fresh_peer.system_status_request_count(), 0);
            assert_eq!(fresh_peer.clients_self_request_count(), 0);
            assert_eq!(fresh_peer.relay_access_request_count(), 0);
            assert_eq!(fresh_peer.accepted_carriers(), 0);
            fresh_peer.shutdown().await;
        }

        // Confirm two words when mark unavailable: files unchanged, exit 5, [MARK_UNVERIFIABLE_CONFIRM]
        {
            let temp = TestDirectory::new("mark-words-confirm-unverifiable");
            let roots = IsolatedRoots::new(temp.path());
            let env = FakeEnvironment::from_paths(roots.entries().iter().cloned());
            ensure_private_directory(&roots.config_root()).expect("config");
            let mut mut_cred = cred.clone();
            mut_cred.instance_id = "test-pairing-instance".to_string();
            persist_credential(&roots.config_root(), &mut_cred).expect("persist");
            write_answer_file(&roots.config_root(), "").expect("write answer");

            let outcome = confirm(
                PlatformKind::Linux,
                &env,
                TerminalSeat::Scripted(None::<Cursor<Vec<u8>>>),
                MarkOption::Value("some words".to_string()),
            )
            .await;

            assert_eq!(
                outcome,
                Outcome::Owner {
                    code: 5,
                    lines: vec![MARK_UNVERIFIABLE_CONFIRM.to_string()],
                }
            );
            assert!(roots.config_root().join(CREDENTIALS_FILENAME).exists());
            assert!(roots.config_root().join(ANSWER_FILENAME).exists());
        }

        peer.shutdown().await;
    });
}

// ===========================================================================
// 6. repair_keeps_confirmed_credential
// ===========================================================================

#[test]
fn repair_keeps_confirmed_credential() {
    runtime().block_on(async {
        let peer_x = PrivateLinkPeer::start().await;
        let mut cred_x = peer_x.credential();
        cred_x.instance_id = test_jid();
        let gen_x = hex_encode(&compute_pairing_generation(&cred_x.client_cert_pem));

        let peer_new = PrivateLinkPeer::start().await;
        let mut cred_new = peer_new.credential();
        cred_new.instance_id = test_jid();
        let cred_new_call = cred_new.clone();
        let expected_sha_new = peer_new.expected_client_sha256();

        // 1. Re-pairing with confirmed cred X: "no" leaves X bytes identical and confirmed
        {
            let temp = TestDirectory::new("repair-ans-no");
            let roots = IsolatedRoots::new(temp.path());
            let env = FakeEnvironment::from_paths(roots.entries().iter().cloned());
            ensure_private_directory(&roots.config_root()).expect("config");
            persist_credential(&roots.config_root(), &cred_x).expect("persist X");
            write_answer_file(&roots.config_root(), &gen_x).expect("confirm X");
            let initial_bytes = fs::read(roots.config_root().join(CREDENTIALS_FILENAME)).unwrap();

            let outcome = setup_with_pairer(
                PlatformKind::Linux,
                &env,
                Cursor::new(b"link"),
                Ok::<String, &'static str>("test-host".to_string()),
                |_link, _dev, _fields| {
                    let c = cred_new_call.clone();
                    async move { Ok(c) }
                },
                TerminalSeat::Scripted(Some(TestTerminal::new("no\n"))),
                MarkOption::Absent,
            )
            .await;

            assert_eq!(
                outcome,
                Outcome::Owner {
                    code: 1,
                    lines: vec![NOT_PAIRED.to_string(), MISMATCH_BODY.to_string()],
                }
            );
            assert_eq!(
                fs::read(roots.config_root().join(CREDENTIALS_FILENAME)).unwrap(),
                initial_bytes
            );
            assert!(is_pairing_confirmed(&roots.config_root(), &cred_x));
            assert_journal_retire(&peer_new, &expected_sha_new, 200, 0);
            assert_eq!(peer_x.requests().len(), 0);
            assert_eq!(peer_x.accepted_carriers(), 0);
            assert_eq!(peer_x.system_status_request_count(), 0);
            assert_eq!(peer_x.clients_self_request_count(), 0);
            assert_eq!(peer_x.relay_access_request_count(), 0);
        }

        // 2. Re-pairing with confirmed cred X: "cancel" leaves X bytes identical and confirmed
        {
            let temp = TestDirectory::new("repair-ans-cancel");
            let roots = IsolatedRoots::new(temp.path());
            let env = FakeEnvironment::from_paths(roots.entries().iter().cloned());
            ensure_private_directory(&roots.config_root()).expect("config");
            persist_credential(&roots.config_root(), &cred_x).expect("persist X");
            write_answer_file(&roots.config_root(), &gen_x).expect("confirm X");
            let initial_bytes = fs::read(roots.config_root().join(CREDENTIALS_FILENAME)).unwrap();

            let outcome = setup_with_pairer(
                PlatformKind::Linux,
                &env,
                Cursor::new(b"link"),
                Ok::<String, &'static str>("test-host".to_string()),
                |_link, _dev, _fields| {
                    let mut c = cred_new_call.clone();
                    c.instance_id = "test-pairing-instance".to_string();
                    async move { Ok(c) }
                },
                TerminalSeat::Scripted(Some(TestTerminal::new("cancel\n"))),
                MarkOption::Absent,
            )
            .await;

            assert_eq!(
                outcome,
                Outcome::Owner {
                    code: 1,
                    lines: vec![CANCEL.to_string()],
                }
            );
            assert_eq!(
                fs::read(roots.config_root().join(CREDENTIALS_FILENAME)).unwrap(),
                initial_bytes
            );
            assert!(is_pairing_confirmed(&roots.config_root(), &cred_x));
            assert_journal_retire(&peer_new, &expected_sha_new, 200, 1);
            assert_eq!(peer_x.requests().len(), 0);
            assert_eq!(peer_x.accepted_carriers(), 0);
            assert_eq!(peer_x.system_status_request_count(), 0);
            assert_eq!(peer_x.clients_self_request_count(), 0);
            assert_eq!(peer_x.relay_access_request_count(), 0);
        }

        // 3. Re-pairing with confirmed cred X: mismatch --mark leaves X bytes identical and confirmed
        {
            let temp = TestDirectory::new("repair-ans-mismatch");
            let roots = IsolatedRoots::new(temp.path());
            let env = FakeEnvironment::from_paths(roots.entries().iter().cloned());
            ensure_private_directory(&roots.config_root()).expect("config");
            persist_credential(&roots.config_root(), &cred_x).expect("persist X");
            write_answer_file(&roots.config_root(), &gen_x).expect("confirm X");
            let initial_bytes = fs::read(roots.config_root().join(CREDENTIALS_FILENAME)).unwrap();

            let outcome = setup_with_pairer(
                PlatformKind::Linux,
                &env,
                Cursor::new(b"link"),
                Ok::<String, &'static str>("test-host".to_string()),
                |_link, _dev, _fields| {
                    let c = cred_new_call.clone();
                    async move { Ok(c) }
                },
                TerminalSeat::Scripted(None::<Cursor<Vec<u8>>>),
                MarkOption::Value("wrong mark".to_string()),
            )
            .await;

            assert_eq!(
                outcome,
                Outcome::Owner {
                    code: 1,
                    lines: vec![NOT_PAIRED.to_string(), MARK_MISMATCH.to_string()],
                }
            );
            assert_eq!(
                fs::read(roots.config_root().join(CREDENTIALS_FILENAME)).unwrap(),
                initial_bytes
            );
            assert!(is_pairing_confirmed(&roots.config_root(), &cred_x));
            assert_journal_retire(&peer_new, &expected_sha_new, 200, 2);
            assert_eq!(peer_x.requests().len(), 0);
            assert_eq!(peer_x.accepted_carriers(), 0);
            assert_eq!(peer_x.system_status_request_count(), 0);
            assert_eq!(peer_x.clients_self_request_count(), 0);
            assert_eq!(peer_x.relay_access_request_count(), 0);
        }

        // 4. Re-pairing with confirmed cred X: EOF (walk away) leaves X confirmed and prints CANCEL, exit 1
        {
            let temp = TestDirectory::new("repair-ans-eof");
            let roots = IsolatedRoots::new(temp.path());
            let env = FakeEnvironment::from_paths(roots.entries().iter().cloned());
            ensure_private_directory(&roots.config_root()).expect("config");
            persist_credential(&roots.config_root(), &cred_x).expect("persist X");
            write_answer_file(&roots.config_root(), &gen_x).expect("confirm X");
            let initial_bytes = fs::read(roots.config_root().join(CREDENTIALS_FILENAME)).unwrap();

            let outcome = setup_with_pairer(
                PlatformKind::Linux,
                &env,
                Cursor::new(b"link"),
                Ok::<String, &'static str>("test-host".to_string()),
                |_link, _dev, _fields| {
                    let c = cred_new_call.clone();
                    async move { Ok(c) }
                },
                TerminalSeat::Scripted(Some(TestTerminal::new(""))),
                MarkOption::Absent,
            )
            .await;

            assert_eq!(
                outcome,
                Outcome::Owner {
                    code: 1,
                    lines: vec![CANCEL.to_string()],
                }
            );
            assert_eq!(
                fs::read(roots.config_root().join(CREDENTIALS_FILENAME)).unwrap(),
                initial_bytes
            );
            assert!(is_pairing_confirmed(&roots.config_root(), &cred_x));
            assert_journal_retire(&peer_new, &expected_sha_new, 200, 3);
            assert_eq!(peer_x.requests().len(), 0);
            assert_eq!(peer_x.accepted_carriers(), 0);
            assert_eq!(peer_x.system_status_request_count(), 0);
            assert_eq!(peer_x.clients_self_request_count(), 0);
            assert_eq!(peer_x.relay_access_request_count(), 0);
        }

        // 5. Re-pairing with confirmed cred X: ceremony Err leaves X bytes identical and confirmed
        {
            let temp = TestDirectory::new("repair-ans-err");
            let roots = IsolatedRoots::new(temp.path());
            let env = FakeEnvironment::from_paths(roots.entries().iter().cloned());
            ensure_private_directory(&roots.config_root()).expect("config");
            persist_credential(&roots.config_root(), &cred_x).expect("persist X");
            write_answer_file(&roots.config_root(), &gen_x).expect("confirm X");
            let initial_bytes = fs::read(roots.config_root().join(CREDENTIALS_FILENAME)).unwrap();

            let outcome = setup_with_pairer(
                PlatformKind::Linux,
                &env,
                Cursor::new(b"link"),
                Ok::<String, &'static str>("test-host".to_string()),
                |_link, _dev, _fields| async move { Err(DiagnosticCode::PairingFailed) },
                TerminalSeat::Scripted(Some(TestTerminal::new(""))),
                MarkOption::Absent,
            )
            .await;

            assert_eq!(outcome, Outcome::Diagnostic(DiagnosticCode::PairingFailed));
            assert_eq!(
                fs::read(roots.config_root().join(CREDENTIALS_FILENAME)).unwrap(),
                initial_bytes
            );
            assert!(is_pairing_confirmed(&roots.config_root(), &cred_x));
            assert_eq!(peer_new.requests().len(), 4);
            assert_eq!(peer_x.requests().len(), 0);
            assert_eq!(peer_x.accepted_carriers(), 0);
            assert_eq!(peer_x.system_status_request_count(), 0);
            assert_eq!(peer_x.clients_self_request_count(), 0);
            assert_eq!(peer_x.relay_access_request_count(), 0);
        }

        // 5b. Absent answer file + EOF: grandfathering writes X's generation, X confirmed & byte-identical, DELETE sent for new cert to peer_new, 0 to peer_x
        {
            let temp = TestDirectory::new("repair-ans-absent-eof");
            let roots = IsolatedRoots::new(temp.path());
            let env = FakeEnvironment::from_paths(roots.entries().iter().cloned());
            ensure_private_directory(&roots.config_root()).expect("config");
            persist_credential(&roots.config_root(), &cred_x).expect("persist X");
            let initial_bytes = fs::read(roots.config_root().join(CREDENTIALS_FILENAME)).unwrap();

            let outcome = setup_with_pairer(
                PlatformKind::Linux,
                &env,
                Cursor::new(b"link"),
                Ok::<String, &'static str>("test-host".to_string()),
                |_link, _dev, _fields| {
                    let c = cred_new_call.clone();
                    async move { Ok(c) }
                },
                TerminalSeat::Scripted(Some(TestTerminal::new(""))),
                MarkOption::Absent,
            )
            .await;

            assert_eq!(
                outcome,
                Outcome::Owner {
                    code: 1,
                    lines: vec![CANCEL.to_string()],
                }
            );
            assert_eq!(
                fs::read(roots.config_root().join(CREDENTIALS_FILENAME)).unwrap(),
                initial_bytes
            );
            assert!(is_pairing_confirmed(&roots.config_root(), &cred_x));
            let ans = read_answer_file(&roots.config_root()).unwrap().unwrap();
            assert_eq!(ans.confirmed, gen_x);
            assert_journal_retire(&peer_new, &expected_sha_new, 200, 4);
            assert_eq!(peer_x.requests().len(), 0);
            assert_eq!(peer_x.accepted_carriers(), 0);
            assert_eq!(peer_x.system_status_request_count(), 0);
            assert_eq!(peer_x.clients_self_request_count(), 0);
            assert_eq!(peer_x.relay_access_request_count(), 0);
        }

        // 5c. Absent answer file + "no": grandfathering writes X's generation, X confirmed & byte-identical, DELETE sent for new cert to peer_new, 0 to peer_x
        {
            let temp = TestDirectory::new("repair-ans-absent-no");
            let roots = IsolatedRoots::new(temp.path());
            let env = FakeEnvironment::from_paths(roots.entries().iter().cloned());
            ensure_private_directory(&roots.config_root()).expect("config");
            persist_credential(&roots.config_root(), &cred_x).expect("persist X");
            let initial_bytes = fs::read(roots.config_root().join(CREDENTIALS_FILENAME)).unwrap();

            let outcome = setup_with_pairer(
                PlatformKind::Linux,
                &env,
                Cursor::new(b"link"),
                Ok::<String, &'static str>("test-host".to_string()),
                |_link, _dev, _fields| {
                    let c = cred_new_call.clone();
                    async move { Ok(c) }
                },
                TerminalSeat::Scripted(Some(TestTerminal::new("no\n"))),
                MarkOption::Absent,
            )
            .await;

            assert_eq!(
                outcome,
                Outcome::Owner {
                    code: 1,
                    lines: vec![NOT_PAIRED.to_string(), MISMATCH_BODY.to_string()],
                }
            );
            assert_eq!(
                fs::read(roots.config_root().join(CREDENTIALS_FILENAME)).unwrap(),
                initial_bytes
            );
            assert!(is_pairing_confirmed(&roots.config_root(), &cred_x));
            let ans = read_answer_file(&roots.config_root()).unwrap().unwrap();
            assert_eq!(ans.confirmed, gen_x);
            assert_journal_retire(&peer_new, &expected_sha_new, 200, 5);
            assert_eq!(peer_x.requests().len(), 0);
            assert_eq!(peer_x.accepted_carriers(), 0);
            assert_eq!(peer_x.system_status_request_count(), 0);
            assert_eq!(peer_x.clients_self_request_count(), 0);
            assert_eq!(peer_x.relay_access_request_count(), 0);
        }

        // 5d. Held-only walk-away with two fresh peers: credential H stored, answer file "", pairer returns N -> exit 5 HELD, RUN_LINE, N stored, is_status_held true, no DELETE
        {
            let peer_h = PrivateLinkPeer::start().await;
            let mut cred_h = peer_h.credential();
            cred_h.instance_id = test_jid();

            let peer_n = PrivateLinkPeer::start().await;
            let mut cred_n = peer_n.credential();
            cred_n.instance_id = test_jid();
            let cred_n_call = cred_n.clone();

            let temp = TestDirectory::new("repair-ans-held-walkaway");
            let roots = IsolatedRoots::new(temp.path());
            let env = FakeEnvironment::from_paths(roots.entries().iter().cloned());
            ensure_private_directory(&roots.config_root()).expect("config");
            persist_credential(&roots.config_root(), &cred_h).expect("persist H");
            write_answer_file(&roots.config_root(), "").expect("held answer");

            let outcome = setup_with_pairer(
                PlatformKind::Linux,
                &env,
                Cursor::new(b"link"),
                Ok::<String, &'static str>("test-host".to_string()),
                |_link, _dev, _fields| async move { Ok(cred_n_call) },
                TerminalSeat::Scripted(Some(TestTerminal::new(""))),
                MarkOption::Absent,
            )
            .await;

            assert_eq!(
                outcome,
                Outcome::Owner {
                    code: 5,
                    lines: vec![HELD.to_string(), RUN_LINE.to_string()],
                }
            );
            let stored_bytes = fs::read(roots.config_root().join(CREDENTIALS_FILENAME)).unwrap();
            let n_bytes = serde_json::to_vec(&cred_n).unwrap();
            let h_bytes = serde_json::to_vec(&cred_h).unwrap();
            assert_eq!(stored_bytes, n_bytes);
            assert_ne!(stored_bytes, h_bytes);
            assert!(!is_pairing_confirmed(&roots.config_root(), &cred_n));
            assert!(is_status_held(&roots.config_root()));
            assert_eq!(peer_h.requests().len(), 0);
            assert_eq!(peer_n.requests().len(), 0);

            peer_h.shutdown().await;
            peer_n.shutdown().await;
        }

        peer_x.shutdown().await;
        peer_new.shutdown().await;
    });

    // 6. Re-pairing with confirmed cred X: SIGINT/SIGHUP at prompt -> leaves X bytes identical and confirmed, exit 1, CANCEL
    let pl_peer = runtime().block_on(async { PrivateLinkPeer::start().await });
    let mut cred_x_base = pl_peer.credential();
    cred_x_base.instance_id = test_jid();
    runtime().block_on(async { pl_peer.shutdown().await });

    for sig in [rustix::process::Signal::INT, rustix::process::Signal::HUP] {
        let rt = runtime();
        let peer = rt.block_on(async { DirectPairingPeer::start().await });
        let (master, slave_path) = open_pty();
        let temp = TestDirectory::new(&format!("repair-sig-{sig:?}"));
        let roots = IsolatedRoots::new(temp.path());
        ensure_private_directory(&roots.config_root()).expect("config");
        let cred_x = cred_x_base.clone();
        let gen_x = hex_encode(&compute_pairing_generation(&cred_x.client_cert_pem));
        persist_credential(&roots.config_root(), &cred_x).expect("persist X");
        write_answer_file(&roots.config_root(), &gen_x).expect("confirm X");
        let initial_bytes = fs::read(roots.config_root().join(CREDENTIALS_FILENAME)).unwrap();

        let mut child = Command::new(env!("CARGO_BIN_EXE_solstone-tmux"))
            .arg("setup")
            .env_clear()
            .envs(roots.entries().iter().cloned())
            .env("SOLSTONE_TMUX_TERMINAL", &slave_path)
            .stdin(Stdio::piped())
            .stdout(hung_up_stdio())
            .stderr(hung_up_stdio())
            .spawn()
            .expect("spawn setup");

        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(peer.pair_link().as_bytes())
            .expect("write link");

        read_pty_until_pattern(&master, &mut child, ASK_UNAVAILABLE, Duration::from_secs(5));

        let pid = rustix::process::Pid::from_raw(child.id() as i32).expect("pid");
        rustix::process::kill_process(pid, sig).expect("kill");

        let start = std::time::Instant::now();
        let status = loop {
            if let Some(status) = child.try_wait().expect("try wait") {
                break status;
            }
            if start.elapsed() > Duration::from_secs(5) {
                panic!("child did not exit within 5s");
            }
            std::thread::sleep(Duration::from_millis(20));
        };

        assert_eq!(status.code(), Some(1));
        assert_eq!(
            fs::read(roots.config_root().join(CREDENTIALS_FILENAME)).unwrap(),
            initial_bytes
        );
        assert!(is_pairing_confirmed(&roots.config_root(), &cred_x));
        rt.block_on(async { peer.shutdown().await });
    }
}

// ===========================================================================
// 7. confirm_binds_the_displayed_generation
// ===========================================================================

#[test]
fn confirm_binds_the_displayed_generation() {
    runtime().block_on(async {
        let peer = PrivateLinkPeer::start().await;
        let mut cred = peer.credential();
        cred.instance_id = test_jid();
        let expected_sha = peer.expected_client_sha256();
        let _generation = hex_encode(&compute_pairing_generation(&cred.client_cert_pem));

        // 1. Held plus scripted yes writes confirmed and exits 0
        {
            let temp = TestDirectory::new("confirm-binds-yes");
            let roots = IsolatedRoots::new(temp.path());
            let env = FakeEnvironment::from_paths(roots.entries().iter().cloned());
            ensure_private_directory(&roots.config_root()).expect("config");
            persist_credential(&roots.config_root(), &cred).expect("persist");
            write_answer_file(&roots.config_root(), "").expect("write answer");

            let outcome = confirm(
                PlatformKind::Linux,
                &env,
                TerminalSeat::Scripted(Some(TestTerminal::new("yes\n"))),
                MarkOption::Absent,
            )
            .await;

            assert_eq!(
                outcome,
                Outcome::Owner {
                    code: 0,
                    lines: vec![PAIRED.to_string()],
                }
            );
            assert!(is_pairing_confirmed(&roots.config_root(), &cred));
        }

        // 2. Held plus no: DELETE that DER, credential file gone, answer file remains
        {
            let temp = TestDirectory::new("confirm-binds-no");
            let roots = IsolatedRoots::new(temp.path());
            let env = FakeEnvironment::from_paths(roots.entries().iter().cloned());
            ensure_private_directory(&roots.config_root()).expect("config");
            persist_credential(&roots.config_root(), &cred).expect("persist");
            write_answer_file(&roots.config_root(), "").expect("write answer");

            let outcome = confirm(
                PlatformKind::Linux,
                &env,
                TerminalSeat::Scripted(Some(TestTerminal::new("no\n"))),
                MarkOption::Absent,
            )
            .await;

            assert_eq!(
                outcome,
                Outcome::Owner {
                    code: 1,
                    lines: vec![NOT_PAIRED.to_string(), MISMATCH_BODY.to_string()],
                }
            );
            assert!(!roots.config_root().join(CREDENTIALS_FILENAME).exists());
            assert!(roots.config_root().join(ANSWER_FILENAME).exists());
            assert_journal_retire(&peer, &expected_sha, 200, 0);

            // Persist credential again with leftover confirmed == "" -> reads held
            persist_credential(&roots.config_root(), &cred).expect("persist after drop");
            assert!(is_status_held(&roots.config_root()));
            assert!(!is_pairing_confirmed(&roots.config_root(), &cred));
        }

        // 3. No terminal and no --mark on a held file changes nothing, exit 1, CONFIRM_NO_TERMINAL
        {
            let temp = TestDirectory::new("confirm-binds-noterm");
            let roots = IsolatedRoots::new(temp.path());
            let env = FakeEnvironment::from_paths(roots.entries().iter().cloned());
            ensure_private_directory(&roots.config_root()).expect("config");
            persist_credential(&roots.config_root(), &cred).expect("persist");
            write_answer_file(&roots.config_root(), "").expect("write answer");

            let outcome = confirm(
                PlatformKind::Linux,
                &env,
                TerminalSeat::Scripted(None::<Cursor<Vec<u8>>>),
                MarkOption::Absent,
            )
            .await;

            assert_eq!(
                outcome,
                Outcome::Owner {
                    code: 1,
                    lines: vec![CONFIRM_NO_TERMINAL.to_string()],
                }
            );
            assert!(roots.config_root().join(CREDENTIALS_FILENAME).exists());
        }

        // 4. Not paired settles an absent file to empty confirmed and prints CONFIRM_UNPAIRED
        {
            let temp = TestDirectory::new("confirm-binds-unpaired");
            let roots = IsolatedRoots::new(temp.path());
            let env = FakeEnvironment::from_paths(roots.entries().iter().cloned());

            let outcome = confirm(
                PlatformKind::Linux,
                &env,
                TerminalSeat::Scripted(None::<Cursor<Vec<u8>>>),
                MarkOption::Absent,
            )
            .await;

            assert_eq!(
                outcome,
                Outcome::Owner {
                    code: 1,
                    lines: vec![CONFIRM_UNPAIRED.to_string()],
                }
            );
            let ans = read_answer_file(&roots.config_root()).unwrap().unwrap();
            assert_eq!(ans.confirmed, "");
        }

        // 5. While holding InstanceLock and acquire_private_state_lock, confirm --mark with SOLSTONE_TMUX_TERMINAL=- succeeds
        {
            let temp = TestDirectory::new("confirm-binds-locks");
            let roots = IsolatedRoots::new(temp.path());
            ensure_private_directory(&roots.config_root()).expect("config root");
            ensure_private_directory(&roots.data_root()).expect("data root");
            persist_credential(&roots.config_root(), &cred).expect("persist");
            write_answer_file(&roots.config_root(), "").expect("write answer");
            let (w1, w2) = extract_journal_mark_words(&cred.instance_id).expect("words");

            let _data_lock = InstanceLock::acquire(&roots.data_root()).expect("data lock");
            let _state_lock = acquire_private_state_lock(&roots.config_root()).expect("state lock");

            let output = Command::new(env!("CARGO_BIN_EXE_solstone-tmux"))
                .arg("confirm")
                .arg("--mark")
                .arg(format!("{w1} {w2}"))
                .env_clear()
                .envs(roots.entries().iter().cloned())
                .env("SOLSTONE_TMUX_TERMINAL", "-")
                .output()
                .expect("run confirm");

            assert_eq!(output.status.code(), Some(0));
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(stdout.contains(PAIRED));
        }

        // 6. Already confirmed: exit 0 [CONFIRM_DONE], bytes unchanged, 0 peer requests
        {
            let peer_confirmed = PrivateLinkPeer::start().await;
            let mut cred_confirmed = peer_confirmed.credential();
            cred_confirmed.instance_id = test_jid();

            let temp = TestDirectory::new("confirm-binds-already-confirmed");
            let roots = IsolatedRoots::new(temp.path());
            let env = FakeEnvironment::from_paths(roots.entries().iter().cloned());
            ensure_private_directory(&roots.config_root()).expect("config");
            persist_credential(&roots.config_root(), &cred_confirmed).expect("persist");
            let generation =
                hex_encode(&compute_pairing_generation(&cred_confirmed.client_cert_pem));
            write_answer_file(&roots.config_root(), &generation).expect("write confirmed answer");
            let before_ans_bytes =
                fs::read(roots.config_root().join(ANSWER_FILENAME)).expect("read ans");
            let before_cred_bytes =
                fs::read(roots.config_root().join(CREDENTIALS_FILENAME)).expect("read cred");

            let outcome = confirm(
                PlatformKind::Linux,
                &env,
                TerminalSeat::Scripted(None::<Cursor<Vec<u8>>>),
                MarkOption::Absent,
            )
            .await;

            assert_eq!(
                outcome,
                Outcome::Owner {
                    code: 0,
                    lines: vec![CONFIRM_DONE.to_string()],
                }
            );
            assert_eq!(
                fs::read(roots.config_root().join(ANSWER_FILENAME)).expect("read ans"),
                before_ans_bytes
            );
            assert_eq!(
                fs::read(roots.config_root().join(CREDENTIALS_FILENAME)).expect("read cred"),
                before_cred_bytes
            );
            assert_eq!(peer_confirmed.requests().len(), 0);
            peer_confirmed.shutdown().await;
        }

        // 7. Credential swap during terminal read (Y -> Z)
        {
            // Y -> Z swap with "yes\n": returns PrivateStateIo, Z bytes kept, 0 peer requests on Y or Z
            let peer_y = PrivateLinkPeer::start().await;
            let mut cred_y = peer_y.credential();
            cred_y.instance_id = test_jid();

            let peer_z = PrivateLinkPeer::start().await;
            let mut cred_z = peer_z.credential();
            cred_z.instance_id = test_jid();

            let temp = TestDirectory::new("confirm-swap-yes");
            let roots = IsolatedRoots::new(temp.path());
            let env = FakeEnvironment::from_paths(roots.entries().iter().cloned());
            ensure_private_directory(&roots.config_root()).expect("config");
            persist_credential(&roots.config_root(), &cred_y).expect("persist Y");
            write_answer_file(&roots.config_root(), "").expect("write answer");

            let term = SwappingTerminal::new("yes\n", roots.config_root(), cred_z.clone());

            let outcome = confirm(
                PlatformKind::Linux,
                &env,
                TerminalSeat::Scripted(Some(term)),
                MarkOption::Absent,
            )
            .await;

            assert_eq!(outcome, Outcome::Diagnostic(DiagnosticCode::PrivateStateIo));
            let cred_bytes = fs::read(roots.config_root().join(CREDENTIALS_FILENAME)).unwrap();
            let z_expected_bytes = serde_json::to_vec(&cred_z).unwrap();
            assert_eq!(cred_bytes, z_expected_bytes);
            assert!(!is_pairing_confirmed(&roots.config_root(), &cred_y));
            assert!(!is_pairing_confirmed(&roots.config_root(), &cred_z));
            let ans_record = read_answer_file(&roots.config_root()).unwrap().unwrap();
            assert_eq!(ans_record.confirmed, "");
            assert_eq!(peer_y.requests().len(), 0);
            assert_eq!(peer_z.requests().len(), 0);

            peer_y.shutdown().await;
            peer_z.shutdown().await;
        }

        {
            // Y -> Z swap with "no\n": returns PrivateStateIo, Z bytes kept, exactly 1 DELETE request for Y on Y's peer, 0 on Z's peer
            let peer_y = PrivateLinkPeer::start().await;
            let mut cred_y = peer_y.credential();
            cred_y.instance_id = test_jid();

            let peer_z = PrivateLinkPeer::start().await;
            let mut cred_z = peer_z.credential();
            cred_z.instance_id = test_jid();

            let temp = TestDirectory::new("confirm-swap-no");
            let roots = IsolatedRoots::new(temp.path());
            let env = FakeEnvironment::from_paths(roots.entries().iter().cloned());
            ensure_private_directory(&roots.config_root()).expect("config");
            persist_credential(&roots.config_root(), &cred_y).expect("persist Y");
            write_answer_file(&roots.config_root(), "").expect("write answer");

            let term = SwappingTerminal::new("no\n", roots.config_root(), cred_z.clone());

            let outcome = confirm(
                PlatformKind::Linux,
                &env,
                TerminalSeat::Scripted(Some(term)),
                MarkOption::Absent,
            )
            .await;

            assert_eq!(outcome, Outcome::Diagnostic(DiagnosticCode::PrivateStateIo));
            let cred_bytes = fs::read(roots.config_root().join(CREDENTIALS_FILENAME)).unwrap();
            let z_expected_bytes = serde_json::to_vec(&cred_z).unwrap();
            assert_eq!(cred_bytes, z_expected_bytes);
            let ans_record = read_answer_file(&roots.config_root()).unwrap().unwrap();
            assert_eq!(ans_record.confirmed, "");
            assert_journal_retire(&peer_y, &peer_y.expected_client_sha256(), 200, 0);
            assert_eq!(peer_z.requests().len(), 0);
            assert_eq!(peer_z.accepted_carriers(), 0);

            peer_y.shutdown().await;
            peer_z.shutdown().await;
        }

        // 8. Mode 000 answer file then confirm yes ends confirmed
        if rustix::process::geteuid().as_raw() != 0 {
            let temp = TestDirectory::new("confirm-binds-mode000");
            let roots = IsolatedRoots::new(temp.path());
            ensure_private_directory(&roots.config_root()).expect("config root");
            persist_credential(&roots.config_root(), &cred).expect("persist");
            let (w1, w2) = extract_journal_mark_words(&cred.instance_id).expect("words");

            let ans_path = roots.config_root().join(ANSWER_FILENAME);
            fs::write(&ans_path, b"bad").expect("write bad");
            let mut perms = fs::metadata(&ans_path).expect("metadata").permissions();
            perms.set_mode(0o000);
            fs::set_permissions(&ans_path, perms).expect("chmod 000");

            let output = Command::new(env!("CARGO_BIN_EXE_solstone-tmux"))
                .arg("confirm")
                .arg("--mark")
                .arg(format!("{w1} {w2}"))
                .env_clear()
                .envs(roots.entries().iter().cloned())
                .env("SOLSTONE_TMUX_TERMINAL", "-")
                .output()
                .expect("run confirm");

            assert_eq!(output.status.code(), Some(0));
            assert!(is_pairing_confirmed(&roots.config_root(), &cred));
        }

        peer.shutdown().await;
    });
}

// ===========================================================================
// 8. grandfather_bytes_and_malformed_credential
// ===========================================================================

#[test]
fn grandfather_bytes_and_malformed_credential() {
    runtime().block_on(async {
        let peer = PrivateLinkPeer::start().await;
        let cred = peer.credential();
        let generation = hex_encode(&compute_pairing_generation(&cred.client_cert_pem));

        // Absent file plus credential becomes that generation
        {
            let temp = TestDirectory::new("gf-absent");
            let config_root = temp.path().join("config");
            ensure_private_directory(&config_root).expect("config");
            persist_credential(&config_root, &cred).expect("persist");
            assert!(!config_root.join(ANSWER_FILENAME).exists());

            grandfather_or_settle(&config_root).expect("settle");
            let record = read_answer_file(&config_root).unwrap().unwrap();
            assert_eq!(record.confirmed, generation);
        }

        // Malformed answer bytes stay byte-identical and are not confirmed (mode 000 too)
        {
            let temp = TestDirectory::new("gf-malformed");
            let config_root = temp.path().join("config");
            ensure_private_directory(&config_root).expect("config");
            persist_credential(&config_root, &cred).expect("persist");
            let ans_path = config_root.join(ANSWER_FILENAME);
            let bad_bytes = b"not valid json";
            fs::write(&ans_path, bad_bytes).expect("write");

            grandfather_or_settle(&config_root).expect("settle");
            assert_eq!(fs::read(&ans_path).unwrap(), bad_bytes);
            assert!(!is_pairing_confirmed(&config_root, &cred));

            if rustix::process::geteuid().as_raw() != 0 {
                let mut perms = fs::metadata(&ans_path).expect("metadata").permissions();
                perms.set_mode(0o000);
                fs::set_permissions(&ans_path, perms).expect("chmod 000");
                grandfather_or_settle(&config_root).expect("settle");
                assert!(!is_pairing_confirmed(&config_root, &cred));

                let mut perms = fs::metadata(&ans_path).expect("metadata").permissions();
                perms.set_mode(0o600);
                fs::set_permissions(&ans_path, perms).expect("restore 0600");
                assert_eq!(fs::read(&ans_path).unwrap(), bad_bytes);
            }
        }

        // Valid 64 lowercase hex confirmed value different from credential generation: is_status_held true, is_pairing_confirmed false
        {
            let temp = TestDirectory::new("gf-held-hex");
            let config_root = temp.path().join("config");
            ensure_private_directory(&config_root).expect("config");
            persist_credential(&config_root, &cred).expect("persist");
            let mut hex_val = "0".repeat(64);
            if hex_val == generation {
                hex_val = format!("1{}", &hex_val[1..]);
            }
            write_answer_file(&config_root, &hex_val).expect("write valid held hex");
            assert!(is_status_held(&config_root));
            assert!(!is_pairing_confirmed(&config_root, &cred));
        }

        // Fault the settle write: setup exits 1 before stdin
        {
            let temp = TestDirectory::new("gf-fault-settle");
            let roots = IsolatedRoots::new(temp.path());
            let env = FakeEnvironment::from_paths(roots.entries().iter().cloned());
            ensure_private_directory(&roots.config_root()).expect("config");
            let ans_path = roots.config_root().join(ANSWER_FILENAME);
            set_atomic_write_fault_for_path(&ans_path, Some(AtomicWriteFault::FailBeforeRename));

            let read_called = Arc::new(AtomicBool::new(false));
            let outcome = setup_with_pairer(
                PlatformKind::Linux,
                &env,
                TrackingReader {
                    read_called: Arc::clone(&read_called),
                },
                Ok::<String, &'static str>("test-host".to_string()),
                |_link, _dev, _fields| async move { unreachable!() },
                TerminalSeat::Scripted(Some(TestTerminal::new("yes\n"))),
                MarkOption::Absent,
            )
            .await;

            set_atomic_write_fault_for_path(&ans_path, None);
            assert_eq!(outcome, Outcome::Diagnostic(DiagnosticCode::PrivateStateIo));
            assert!(!read_called.load(Ordering::SeqCst));
        }

        // Malformed credentials.json and no answer file: setup writes empty confirmed, continues, and yes ends confirmed
        {
            let temp = TestDirectory::new("gf-malformed-cred");
            let roots = IsolatedRoots::new(temp.path());
            let env = FakeEnvironment::from_paths(roots.entries().iter().cloned());
            ensure_private_directory(&roots.config_root()).expect("config");
            fs::write(
                roots.config_root().join(CREDENTIALS_FILENAME),
                b"bad cred json",
            )
            .expect("write");

            let mut valid_cred = cred.clone();
            valid_cred.instance_id = test_jid();
            let cred_call = valid_cred.clone();

            let outcome = setup_with_pairer(
                PlatformKind::Linux,
                &env,
                Cursor::new(b"link"),
                Ok::<String, &'static str>("test-host".to_string()),
                |_link, _dev, _fields| async move { Ok(cred_call) },
                TerminalSeat::Scripted(Some(TestTerminal::new("yes\n"))),
                MarkOption::Absent,
            )
            .await;

            assert_eq!(
                outcome,
                Outcome::Owner {
                    code: 0,
                    lines: vec![PAIRED.to_string()],
                }
            );
            assert!(is_pairing_confirmed(&roots.config_root(), &valid_cred));
        }

        peer.shutdown().await;
    });
}

// ===========================================================================
// 9. pairing_gate_uploads_after_confirm
// ===========================================================================

#[test]
fn pairing_gate_uploads_after_confirm() {
    runtime().block_on(async {
        let peer = PrivateLinkPeer::start().await;
        peer.answer_uploads_with_received_descriptors();
        let temp = TestDirectory::new("pairing-gate-uploads");
        let roots = IsolatedRoots::new(temp.path());
        ensure_private_directory(&roots.config_root()).expect("config");
        ensure_private_directory(&roots.data_root()).expect("data");

        let mut cred = peer.credential();
        cred.instance_id = test_jid();
        persist_credential(&roots.config_root(), &cred).expect("persist");

        let generation = hex_encode(&compute_pairing_generation(&cred.client_cert_pem));
        let mut held_hex = "0".repeat(64);
        if held_hex == generation {
            held_hex = format!("1{}", &held_hex[1..]);
        }
        write_answer_file(&roots.config_root(), &held_hex).expect("write held answer");

        let cand1 = roots
            .data_root()
            .join("captures")
            .join("20260729")
            .join("host.tmux")
            .join("120000_300")
            .join("tmux_0_screen.jsonl");
        fs::create_dir_all(cand1.parent().expect("cand1 parent")).expect("cand1 dir");
        fs::write(&cand1, b"segment 1\n").expect("cand1 bytes");

        let cand2 = roots
            .data_root()
            .join("captures")
            .join("20260729")
            .join("host.tmux")
            .join("120100_300")
            .join("tmux_0_screen.jsonl");
        fs::create_dir_all(cand2.parent().expect("cand2 parent")).expect("cand2 dir");
        fs::write(&cand2, b"segment 2\n").expect("cand2 bytes");

        let (stop_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let (activity_tx, _activity_rx) =
            tokio::sync::watch::channel(solstone_tmux::sync::SyncActivity::Idle);
        let clock = Arc::new(solstone_tmux::clock::SystemClock::utc());
        let config = solstone_tmux::config::RuntimeConfig::load(&roots.config_root(), "host")
            .expect("config");

        let lock = InstanceLock::acquire(&roots.data_root()).expect("acquire data lock");
        let _state_lock =
            acquire_private_state_lock(&roots.config_root()).expect("acquire state lock");
        let health = HealthWriter::new(roots.data_root(), &lock);

        let task = SyncTask {
            config_root: roots.config_root(),
            data_root: roots.data_root(),
            config,
            hostname: "host".to_string(),
            clock: clock.clone(),
            wake: SyncWake::default(),
            activity: activity_tx,
            health,
            retention_fence: Arc::new(RetentionFence::new()),
            identity: lock.identity().clone(),
            health_refresh_interval: Duration::from_millis(50),
            answer_lock_timeout: Duration::from_millis(200),
        };

        let task_handle = tokio::spawn(async move {
            let _ = task.run(shutdown_rx).await;
        });

        // Across two intervals, peer.requests() is empty and peer accepted no carrier
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(peer.requests().len(), 0);
        assert_eq!(peer.accepted_carriers(), 0);
        let health_raw = fs::read(roots.data_root().join("sync-health.json")).expect("health file");
        let health_val: serde_json::Value =
            serde_json::from_slice(&health_raw).expect("health json");
        assert_eq!(health_val["state"], "held");

        // Removing the answer file during the held wait does not recreate it
        fs::remove_file(roots.config_root().join(ANSWER_FILENAME)).ok();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!roots.config_root().join(ANSWER_FILENAME).exists());
        assert_eq!(peer.requests().len(), 0);
        assert_eq!(peer.accepted_carriers(), 0);
        let health_raw = fs::read(roots.data_root().join("sync-health.json")).expect("health file");
        let health_val: serde_json::Value =
            serde_json::from_slice(&health_raw).expect("health json");
        assert_eq!(health_val["state"], "held");
        write_answer_file(&roots.config_root(), "").expect("write answer");

        // Spawn real binary confirm --mark "<words>" with SOLSTONE_TMUX_TERMINAL=- (while locks held)
        let (w1, w2) = extract_journal_mark_words(&cred.instance_id).expect("words");
        let output = Command::new(env!("CARGO_BIN_EXE_solstone-tmux"))
            .arg("confirm")
            .arg("--mark")
            .arg(format!("{w1} {w2}"))
            .env_clear()
            .envs(roots.entries().iter().cloned())
            .env("SOLSTONE_TMUX_TERMINAL", "-")
            .output()
            .expect("run confirm");
        assert_eq!(output.status.code(), Some(0));

        // Both segments upload within one interval, without restarting the task
        let wait_start = Instant::now();
        while wait_start.elapsed() < Duration::from_secs(2) {
            let count = peer
                .requests()
                .into_iter()
                .filter(|r| r.path_without_query() == "/app/devices/ingest")
                .count();
            if count >= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let ingest_requests = peer
            .requests()
            .into_iter()
            .filter(|r| r.path_without_query() == "/app/devices/ingest")
            .count();
        assert_eq!(ingest_requests, 2);

        // Shutdown finishes within 2s
        let start = std::time::Instant::now();
        let _ = stop_tx.send(true);
        let _ = task_handle.await;
        assert!(start.elapsed() < Duration::from_secs(2));

        // Removing the credential makes the next health snapshot unpaired
        drop(_state_lock);
        drop(lock);
        fs::remove_file(roots.config_root().join(CREDENTIALS_FILENAME)).ok();
        let status_out = Command::new(env!("CARGO_BIN_EXE_solstone-tmux"))
            .arg("status")
            .env_clear()
            .envs(roots.entries().iter().cloned())
            .output()
            .expect("status");
        let stdout = String::from_utf8_lossy(&status_out.stdout);
        assert!(!stdout.contains(STATUS_HELD_PAIRING));
        assert!(!stdout.contains(STATUS_HELD_CONFIRM));

        peer.shutdown().await;
    });
}

// ===========================================================================
// 10. retire_through_setup_and_confirm
// ===========================================================================

#[test]
fn retire_through_setup_and_confirm() {
    runtime().block_on(async {
        for relay in [false, true] {
            // 1. Setup No -> exactly 1 DELETE, exit 1
            {
                let ctx = RefusalContext::new(relay, "retire-setup-no", test_jid()).await;
                let expected_sha = ctx.peer.expected_client_sha256();
                let cred_call = ctx.cred.clone();

                let outcome = setup_with_pairer(
                    PlatformKind::Linux,
                    &ctx.env,
                    Cursor::new(b"link"),
                    Ok::<String, &'static str>("test-host".to_string()),
                    |_link, _dev, _fields| async move { Ok(cred_call) },
                    TerminalSeat::Scripted(Some(TestTerminal::new("no\n"))),
                    MarkOption::Absent,
                )
                .await;

                assert_eq!(
                    outcome,
                    Outcome::Owner {
                        code: 1,
                        lines: vec![NOT_PAIRED.to_string(), MISMATCH_BODY.to_string()],
                    }
                );
                assert!(!ctx.roots.config_root().join(CREDENTIALS_FILENAME).exists());
                assert_journal_retire(&ctx.peer, &expected_sha, 200, 0);
                if relay {
                    assert_eq!(
                        ctx.relay_server.as_ref().unwrap().refresh_requests().len(),
                        1
                    );
                }
                ctx.shutdown().await;
            }

            // 2. Setup Cancel (malformed instance ID) -> exactly 1 DELETE, exit 1, CANCEL
            {
                let ctx = RefusalContext::new(
                    relay,
                    "retire-setup-cancel",
                    "malformed-instance".to_string(),
                )
                .await;
                let expected_sha = ctx.peer.expected_client_sha256();
                let cred_call = ctx.cred.clone();

                let outcome = setup_with_pairer(
                    PlatformKind::Linux,
                    &ctx.env,
                    Cursor::new(b"link"),
                    Ok::<String, &'static str>("test-host".to_string()),
                    |_link, _dev, _fields| async move { Ok(cred_call) },
                    TerminalSeat::Scripted(Some(TestTerminal::new("cancel\n"))),
                    MarkOption::Absent,
                )
                .await;

                assert_eq!(
                    outcome,
                    Outcome::Owner {
                        code: 1,
                        lines: vec![CANCEL.to_string()],
                    }
                );
                assert!(!ctx.roots.config_root().join(CREDENTIALS_FILENAME).exists());
                assert_journal_retire(&ctx.peer, &expected_sha, 200, 0);
                if relay {
                    assert_eq!(
                        ctx.relay_server.as_ref().unwrap().refresh_requests().len(),
                        1
                    );
                }
                ctx.shutdown().await;
            }

            // 3. Setup --mark mismatch -> exactly 1 DELETE, exit 1
            {
                let ctx = RefusalContext::new(relay, "retire-setup-mismatch", test_jid()).await;
                let expected_sha = ctx.peer.expected_client_sha256();
                let cred_call = ctx.cred.clone();

                let outcome = setup_with_pairer(
                    PlatformKind::Linux,
                    &ctx.env,
                    Cursor::new(b"link"),
                    Ok::<String, &'static str>("test-host".to_string()),
                    |_link, _dev, _fields| async move { Ok(cred_call) },
                    TerminalSeat::Scripted(None::<Cursor<Vec<u8>>>),
                    MarkOption::Value("wrong mark".to_string()),
                )
                .await;

                assert_eq!(
                    outcome,
                    Outcome::Owner {
                        code: 1,
                        lines: vec![NOT_PAIRED.to_string(), MARK_MISMATCH.to_string()],
                    }
                );
                assert!(!ctx.roots.config_root().join(CREDENTIALS_FILENAME).exists());
                assert_journal_retire(&ctx.peer, &expected_sha, 200, 0);
                if relay {
                    assert_eq!(
                        ctx.relay_server.as_ref().unwrap().refresh_requests().len(),
                        1
                    );
                }
                ctx.shutdown().await;
            }

            // 4. Setup two-word --mark when mark unavailable -> exactly 1 DELETE, exit 1
            {
                let ctx = RefusalContext::new(
                    relay,
                    "retire-setup-unverifiable",
                    "test-pairing-instance".to_string(),
                )
                .await;
                let expected_sha = ctx.peer.expected_client_sha256();
                let cred_call = ctx.cred.clone();

                let outcome = setup_with_pairer(
                    PlatformKind::Linux,
                    &ctx.env,
                    Cursor::new(b"link"),
                    Ok::<String, &'static str>("test-host".to_string()),
                    |_link, _dev, _fields| async move { Ok(cred_call) },
                    TerminalSeat::Scripted(None::<Cursor<Vec<u8>>>),
                    MarkOption::Value("two words".to_string()),
                )
                .await;

                assert_eq!(
                    outcome,
                    Outcome::Owner {
                        code: 1,
                        lines: vec![
                            COULDNT_VERIFY.to_string(),
                            MARK_UNVERIFIABLE_SETUP.to_string(),
                        ],
                    }
                );
                assert!(!ctx.roots.config_root().join(CREDENTIALS_FILENAME).exists());
                assert_journal_retire(&ctx.peer, &expected_sha, 200, 0);
                if relay {
                    assert_eq!(
                        ctx.relay_server.as_ref().unwrap().refresh_requests().len(),
                        1
                    );
                }
                ctx.shutdown().await;
            }

            // 5. Setup refusal with existing credentials.json leaves bytes unchanged
            {
                let peer_x = PrivateLinkPeer::start().await;
                let mut cred_x = peer_x.credential();
                cred_x.instance_id = test_jid();
                let generation_x = hex_encode(&compute_pairing_generation(&cred_x.client_cert_pem));

                let ctx_y = RefusalContext::new(relay, "retire-existing-cred", test_jid()).await;
                let expected_sha_y = ctx_y.peer.expected_client_sha256();

                persist_credential(&ctx_y.roots.config_root(), &cred_x).expect("persist");
                write_answer_file(&ctx_y.roots.config_root(), &generation_x).expect("confirm");
                let initial_bytes =
                    fs::read(ctx_y.roots.config_root().join(CREDENTIALS_FILENAME)).unwrap();

                let cred_y_call = ctx_y.cred.clone();

                let outcome = setup_with_pairer(
                    PlatformKind::Linux,
                    &ctx_y.env,
                    Cursor::new(b"link"),
                    Ok::<String, &'static str>("test-host".to_string()),
                    |_link, _dev, _fields| async move { Ok(cred_y_call) },
                    TerminalSeat::Scripted(Some(TestTerminal::new("no\n"))),
                    MarkOption::Absent,
                )
                .await;

                assert_eq!(
                    outcome,
                    Outcome::Owner {
                        code: 1,
                        lines: vec![NOT_PAIRED.to_string(), MISMATCH_BODY.to_string()],
                    }
                );
                assert_eq!(
                    fs::read(ctx_y.roots.config_root().join(CREDENTIALS_FILENAME)).unwrap(),
                    initial_bytes
                );
                assert!(is_pairing_confirmed(&ctx_y.roots.config_root(), &cred_x));
                assert_eq!(peer_x.requests().len(), 0);
                assert_eq!(peer_x.accepted_carriers(), 0);
                assert_eq!(peer_x.system_status_request_count(), 0);
                assert_eq!(peer_x.clients_self_request_count(), 0);
                assert_eq!(peer_x.relay_access_request_count(), 0);
                assert_journal_retire(&ctx_y.peer, &expected_sha_y, 200, 0);
                if relay {
                    assert_eq!(
                        ctx_y
                            .relay_server
                            .as_ref()
                            .unwrap()
                            .refresh_requests()
                            .len(),
                        1
                    );
                }
                peer_x.shutdown().await;
                ctx_y.shutdown().await;
            }

            // 6. Confirm no: DELETE, credential gone, answer file remains
            {
                let ctx = RefusalContext::new(relay, "retire-confirm-no", test_jid()).await;
                let expected_sha = ctx.peer.expected_client_sha256();

                persist_credential(&ctx.roots.config_root(), &ctx.cred).expect("persist");
                write_answer_file(&ctx.roots.config_root(), "").expect("write answer");

                let outcome = confirm(
                    PlatformKind::Linux,
                    &ctx.env,
                    TerminalSeat::Scripted(Some(TestTerminal::new("no\n"))),
                    MarkOption::Absent,
                )
                .await;

                assert_eq!(
                    outcome,
                    Outcome::Owner {
                        code: 1,
                        lines: vec![NOT_PAIRED.to_string(), MISMATCH_BODY.to_string()],
                    }
                );
                assert!(!ctx.roots.config_root().join(CREDENTIALS_FILENAME).exists());
                assert!(ctx.roots.config_root().join(ANSWER_FILENAME).exists());
                assert_journal_retire(&ctx.peer, &expected_sha, 200, 0);
                if relay {
                    assert_eq!(
                        ctx.relay_server.as_ref().unwrap().refresh_requests().len(),
                        1
                    );
                }
                ctx.shutdown().await;
            }

            // 7. Confirm --mark mismatch: DELETE, credential gone, answer file remains
            {
                let ctx = RefusalContext::new(relay, "retire-confirm-mismatch", test_jid()).await;
                let expected_sha = ctx.peer.expected_client_sha256();

                persist_credential(&ctx.roots.config_root(), &ctx.cred).expect("persist");
                write_answer_file(&ctx.roots.config_root(), "").expect("write answer");

                let outcome = confirm(
                    PlatformKind::Linux,
                    &ctx.env,
                    TerminalSeat::Scripted(None::<Cursor<Vec<u8>>>),
                    MarkOption::Value("wrong mark".to_string()),
                )
                .await;

                assert_eq!(
                    outcome,
                    Outcome::Owner {
                        code: 1,
                        lines: vec![NOT_PAIRED.to_string(), MARK_MISMATCH.to_string()],
                    }
                );
                assert!(!ctx.roots.config_root().join(CREDENTIALS_FILENAME).exists());
                assert!(ctx.roots.config_root().join(ANSWER_FILENAME).exists());
                assert_journal_retire(&ctx.peer, &expected_sha, 200, 0);
                if relay {
                    assert_eq!(
                        ctx.relay_server.as_ref().unwrap().refresh_requests().len(),
                        1
                    );
                }
                ctx.shutdown().await;
            }

            // 8. Delayed DELETE (15s): setup no finishes in under 10s
            {
                let ctx = RefusalContext::new(relay, "retire-delayed", test_jid()).await;
                let expected_sha = ctx.peer.expected_client_sha256();
                ctx.peer
                    .enqueue_delayed_response(Duration::from_secs(15), 200, vec![]);
                let cred_call = ctx.cred.clone();

                let start = Instant::now();
                let outcome = setup_with_pairer(
                    PlatformKind::Linux,
                    &ctx.env,
                    Cursor::new(b"link"),
                    Ok::<String, &'static str>("test-host".to_string()),
                    |_link, _dev, _fields| async move { Ok(cred_call) },
                    TerminalSeat::Scripted(Some(TestTerminal::new("no\n"))),
                    MarkOption::Absent,
                )
                .await;

                assert!(start.elapsed() < Duration::from_secs(10));
                assert_eq!(
                    outcome,
                    Outcome::Owner {
                        code: 1,
                        lines: vec![NOT_PAIRED.to_string(), MISMATCH_BODY.to_string()],
                    }
                );
                assert!(!ctx.roots.config_root().join(CREDENTIALS_FILENAME).exists());
                assert_journal_retire(&ctx.peer, &expected_sha, 200, 0);
                if relay {
                    assert_eq!(
                        ctx.relay_server.as_ref().unwrap().refresh_requests().len(),
                        1
                    );
                }
                ctx.shutdown().await;
            }

            // 9. DELETE for any other ID is 404, still drops locally
            {
                let ctx = RefusalContext::new(relay, "retire-404", test_jid()).await;
                let expected_sha = ctx.peer.expected_client_sha256();
                ctx.peer
                    .set_expected_client_sha256(Some("different_sha".to_string()));

                persist_credential(&ctx.roots.config_root(), &ctx.cred).expect("persist");
                write_answer_file(&ctx.roots.config_root(), "").expect("write answer");

                let outcome = confirm(
                    PlatformKind::Linux,
                    &ctx.env,
                    TerminalSeat::Scripted(Some(TestTerminal::new("no\n"))),
                    MarkOption::Absent,
                )
                .await;

                assert_eq!(
                    outcome,
                    Outcome::Owner {
                        code: 1,
                        lines: vec![NOT_PAIRED.to_string(), MISMATCH_BODY.to_string()],
                    }
                );
                assert!(!ctx.roots.config_root().join(CREDENTIALS_FILENAME).exists());
                assert_journal_retire(&ctx.peer, &expected_sha, 404, 0);
                if relay {
                    assert_eq!(
                        ctx.relay_server.as_ref().unwrap().refresh_requests().len(),
                        1
                    );
                }
                ctx.shutdown().await;
            }

            // 10. Journal accepts the carrier and does not answer: setup no finishes within 10s and the carrier handler is gone
            {
                let ctx = RefusalContext::new(relay, "retire-unanswered", test_jid()).await;
                let expected_sha = ctx.peer.expected_client_sha256();
                ctx.peer
                    .enqueue_delayed_response(Duration::from_secs(60), 200, vec![]);
                let cred_call = ctx.cred.clone();

                let start = Instant::now();
                let outcome = setup_with_pairer(
                    PlatformKind::Linux,
                    &ctx.env,
                    Cursor::new(b"link"),
                    Ok::<String, &'static str>("test-host".to_string()),
                    |_link, _dev, _fields| async move { Ok(cred_call) },
                    TerminalSeat::Scripted(Some(TestTerminal::new("no\n"))),
                    MarkOption::Absent,
                )
                .await;

                assert!(start.elapsed() < Duration::from_secs(10));
                assert!(start.elapsed() < Duration::from_secs(60));
                assert_eq!(
                    outcome,
                    Outcome::Owner {
                        code: 1,
                        lines: vec![NOT_PAIRED.to_string(), MISMATCH_BODY.to_string()],
                    }
                );
                assert!(!ctx.roots.config_root().join(CREDENTIALS_FILENAME).exists());
                assert_eq!(ctx.peer.active_carrier_handlers(), 0);
                assert_journal_retire(&ctx.peer, &expected_sha, 200, 0);
                if relay {
                    assert_eq!(
                        ctx.relay_server.as_ref().unwrap().refresh_requests().len(),
                        1
                    );
                }
                ctx.shutdown().await;
            }
        }
    });
}

// ===========================================================================
// 11. held_status_without_a_daemon
// ===========================================================================

#[test]
fn held_status_without_a_daemon() {
    let temp = TestDirectory::new("held-status");
    let roots = IsolatedRoots::new(temp.path());
    ensure_private_directory(&roots.config_root()).expect("config");

    // 0. Status without daemon when answer file is absent has neither held lines nor state=held
    runtime().block_on(async {
        let peer = PrivateLinkPeer::start().await;
        let mut cred = peer.credential();
        cred.instance_id = test_jid();
        persist_credential(&roots.config_root(), &cred).expect("persist");
        peer.shutdown().await;
    });

    let output = Command::new(env!("CARGO_BIN_EXE_solstone-tmux"))
        .arg("status")
        .env_clear()
        .envs(roots.entries().iter().cloned())
        .output()
        .expect("run status");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains(STATUS_HELD_PAIRING));
    assert!(!stdout.contains(STATUS_HELD_CONFIRM));
    assert!(!stdout.contains("sync-health: held"));
    assert!(!stdout.contains("state=held"));

    // 1. Held status prints sync-health: held, both status constants, report URL contains held
    runtime().block_on(async {
        write_answer_file(&roots.config_root(), "").expect("write held answer");
    });

    let output = Command::new(env!("CARGO_BIN_EXE_solstone-tmux"))
        .arg("status")
        .env_clear()
        .envs(roots.entries().iter().cloned())
        .output()
        .expect("run status");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("sync-health: held"));
    assert!(stdout.contains(STATUS_HELD_PAIRING));
    assert!(stdout.contains(STATUS_HELD_CONFIRM));
    assert!(stdout.contains("state=held"));

    // 2. Confirmed status prints neither held line
    runtime().block_on(async {
        let cred = load_credential(&roots.config_root()).unwrap().unwrap();
        let generation = hex_encode(&compute_pairing_generation(&cred.client_cert_pem));
        write_answer_file(&roots.config_root(), &generation).expect("confirm");
    });

    let output = Command::new(env!("CARGO_BIN_EXE_solstone-tmux"))
        .arg("status")
        .env_clear()
        .envs(roots.entries().iter().cloned())
        .output()
        .expect("run status");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains(STATUS_HELD_PAIRING));
    assert!(!stdout.contains(STATUS_HELD_CONFIRM));

    // 3. Running daemon test: SyncTask with 50ms refresh, 200ms lock timeout, confirm --mark mismatch
    runtime().block_on(async {
        let temp_daemon = TestDirectory::new("held-status-daemon");
        let roots_daemon = IsolatedRoots::new(temp_daemon.path());
        ensure_private_directory(&roots_daemon.config_root()).expect("config");
        ensure_private_directory(&roots_daemon.data_root()).expect("data");

        let peer = PrivateLinkPeer::start().await;
        let mut cred = peer.credential();
        cred.instance_id = test_jid();
        let expected_sha = peer.expected_client_sha256();
        persist_credential(&roots_daemon.config_root(), &cred).expect("persist");
        write_answer_file(&roots_daemon.config_root(), "").expect("write held answer");

        let (stop_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let (activity_tx, _activity_rx) =
            tokio::sync::watch::channel(solstone_tmux::sync::SyncActivity::Idle);
        let clock = Arc::new(solstone_tmux::clock::SystemClock::utc());
        let config =
            solstone_tmux::config::RuntimeConfig::load(&roots_daemon.config_root(), "host")
                .expect("config");

        let lock = InstanceLock::acquire(&roots_daemon.data_root()).expect("acquire data lock");
        let _state_lock =
            acquire_private_state_lock(&roots_daemon.config_root()).expect("state lock");
        let health = HealthWriter::new(roots_daemon.data_root(), &lock);

        let task = SyncTask {
            config_root: roots_daemon.config_root(),
            data_root: roots_daemon.data_root(),
            config,
            hostname: "host".to_string(),
            clock: clock.clone(),
            wake: SyncWake::default(),
            activity: activity_tx,
            health,
            retention_fence: Arc::new(RetentionFence::new()),
            identity: lock.identity().clone(),
            health_refresh_interval: Duration::from_millis(50),
            answer_lock_timeout: Duration::from_millis(200),
        };

        let task_handle = tokio::spawn(async move {
            let _ = task.run(shutdown_rx).await;
        });

        let held_start = Instant::now();
        loop {
            if let Ok(health_raw) = fs::read(roots_daemon.data_root().join("sync-health.json"))
                && let Ok(val) = serde_json::from_slice::<serde_json::Value>(&health_raw)
                && val["state"] == "held"
            {
                break;
            }
            if held_start.elapsed() > Duration::from_secs(1) {
                panic!("health state did not become held within 1s");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(peer.requests().len(), 0);
        assert_eq!(peer.accepted_carriers(), 0);

        // Confirm with mismatching mark words while both daemon locks are held.
        let output = Command::new(env!("CARGO_BIN_EXE_solstone-tmux"))
            .arg("confirm")
            .arg("--mark")
            .arg("wrong words")
            .env_clear()
            .envs(roots_daemon.entries().iter().cloned())
            .env("SOLSTONE_TMUX_TERMINAL", "-")
            .output()
            .expect("run confirm");

        assert_eq!(output.status.code(), Some(1));
        assert!(
            !roots_daemon
                .config_root()
                .join(CREDENTIALS_FILENAME)
                .exists()
        );
        assert!(roots_daemon.config_root().join(ANSWER_FILENAME).exists());

        // Wait for sync task refresh to update health to unpaired
        let wait_start = Instant::now();
        loop {
            if let Ok(health_raw) = fs::read(roots_daemon.data_root().join("sync-health.json"))
                && let Ok(val) = serde_json::from_slice::<serde_json::Value>(&health_raw)
                && val["state"] == "unpaired"
            {
                break;
            }
            if wait_start.elapsed() > Duration::from_secs(2) {
                panic!("health state did not transition to unpaired within 2s");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        assert_journal_retire(&peer, &expected_sha, 200, 0);

        let start = Instant::now();
        let _ = stop_tx.send(true);
        let _ = task_handle.await;
        assert!(start.elapsed() < Duration::from_secs(2));

        peer.shutdown().await;
    });
}
