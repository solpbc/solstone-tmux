// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

mod support;

use std::collections::HashMap;
use std::ffi::OsString;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use std::path::PathBuf;
use std::time::Duration;

use solstone_tmux::command::{CommandInvocation, CommandOperation, TmuxOperation};
use solstone_tmux::indicator::{
    CommandIndicatorIo, IndicatorError, IndicatorIo, IndicatorOwnership, OBSERVING_VALUE,
    OptionValue, SOLSTONE_OPTION, STATUS_LEFT, SYNCING_VALUE, ServerIndicator,
};
use solstone_tmux::observer::{NoopShutdownIndicator, ShutdownIndicator};
use solstone_tmux::sync::SyncActivity;
use solstone_tmux::tmux::TMUX_TIMEOUT;
use support::{ExpectedInvocation, FixtureRunner, command_output};

const WRITTEN_STATUS_LEFT: &str = "owner status | ☼ #{@solstone} ☼";
const NORMALIZED_STATUS_LEFT: &str = "owner status | _ #{@solstone} _";

#[tokio::test]
async fn disabled_indicator_activity_and_restore_are_noops() {
    let mut indicator = NoopShutdownIndicator;
    ShutdownIndicator::set_activity(&mut indicator, SyncActivity::Working)
        .await
        .expect("working activity");
    ShutdownIndicator::set_activity(&mut indicator, SyncActivity::Idle)
        .await
        .expect("idle activity");
    ShutdownIndicator::restore(&mut indicator)
        .await
        .expect("restore");
}

#[tokio::test]
async fn production_install_uses_native_indicator_values() {
    let io = MemoryIndicator::with([
        (STATUS_LEFT, OptionValue::Present("owner status".to_owned())),
        (SOLSTONE_OPTION, OptionValue::Absent),
    ]);

    let mut ownership = IndicatorOwnership::install_default(io.clone())
        .await
        .expect("install production indicator");

    let status = io.value(STATUS_LEFT);
    let OptionValue::Present(status) = status else {
        panic!("status-left must be present");
    };
    assert!(status.contains("#{?@solstone"));
    assert!(status.ends_with("owner status"));
    assert_eq!(
        io.value(SOLSTONE_OPTION),
        OptionValue::Present(OBSERVING_VALUE.to_owned())
    );
    ownership.restore().await.expect("restore");
}

#[tokio::test]
async fn repeated_unclean_production_starts_keep_one_indicator_and_restore_cleanly() {
    let io = MemoryIndicator::with([
        (STATUS_LEFT, OptionValue::Present("owner status".to_owned())),
        (SOLSTONE_OPTION, OptionValue::Absent),
    ]);

    let _first = IndicatorOwnership::install_default(io.clone())
        .await
        .expect("first install");
    let _second = IndicatorOwnership::install_default(io.clone())
        .await
        .expect("second install");
    let _third = IndicatorOwnership::install_default(io.clone())
        .await
        .expect("third install");
    let mut fourth = IndicatorOwnership::install_default(io.clone())
        .await
        .expect("fourth install");

    let OptionValue::Present(status) = io.value(STATUS_LEFT) else {
        panic!("status-left must be present");
    };
    assert_eq!(status.matches("#{?@solstone").count(), 1);
    assert!(status.ends_with("owner status"));

    fourth.restore().await.expect("restore fourth install");
    assert_eq!(
        io.value(STATUS_LEFT),
        OptionValue::Present("owner status".to_owned())
    );
    assert_eq!(io.value(SOLSTONE_OPTION), OptionValue::Absent);
}

#[tokio::test]
async fn stale_owned_solstone_values_are_not_restored() {
    for stale in ["", OBSERVING_VALUE, SYNCING_VALUE] {
        let io = MemoryIndicator::with([
            (STATUS_LEFT, OptionValue::Present("owner status".to_owned())),
            (SOLSTONE_OPTION, OptionValue::Present(stale.to_owned())),
        ]);

        let mut ownership = IndicatorOwnership::install_default(io.clone())
            .await
            .expect("install over stale state");
        ownership.restore().await.expect("restore");

        assert_eq!(io.value(SOLSTONE_OPTION), OptionValue::Absent);
    }
}

#[tokio::test]
async fn production_install_preserves_foreign_solstone_value() {
    let io = MemoryIndicator::with([
        (STATUS_LEFT, OptionValue::Present("owner status".to_owned())),
        (
            SOLSTONE_OPTION,
            OptionValue::Present("owner value".to_owned()),
        ),
    ]);

    let mut ownership = IndicatorOwnership::install_default(io.clone())
        .await
        .expect("install over owner value");
    ownership.restore().await.expect("restore");

    assert_eq!(
        io.value(SOLSTONE_OPTION),
        OptionValue::Present("owner value".to_owned())
    );
}

#[tokio::test]
async fn activity_seam_sets_yellow_only_while_working() {
    let io = MemoryIndicator::with([
        (STATUS_LEFT, OptionValue::Present("owner status".to_owned())),
        (SOLSTONE_OPTION, OptionValue::Absent),
    ]);
    let mut ownership = IndicatorOwnership::install_default(io.clone())
        .await
        .expect("install production indicator");

    ShutdownIndicator::set_activity(&mut ownership, SyncActivity::Working)
        .await
        .expect("start sync activity");
    assert_eq!(
        io.value(SOLSTONE_OPTION),
        OptionValue::Present(SYNCING_VALUE.to_owned())
    );
    ShutdownIndicator::set_activity(&mut ownership, SyncActivity::Idle)
        .await
        .expect("clear sync activity");
    assert_eq!(
        io.value(SOLSTONE_OPTION),
        OptionValue::Present(OBSERVING_VALUE.to_owned())
    );
}

#[tokio::test]
async fn owned_activity_updates_are_idempotent_but_real_changes_write_once() {
    let io = MemoryIndicator::with([
        (STATUS_LEFT, OptionValue::Present("owner status".to_owned())),
        (SOLSTONE_OPTION, OptionValue::Absent),
    ]);
    let mut ownership = install(io.clone()).await;
    io.clear_log();

    assert!(
        ownership
            .update_solstone("observer".to_owned())
            .await
            .expect("repeat owned value")
    );
    assert_eq!(io.log(), vec![format!("read:{SOLSTONE_OPTION}")]);

    io.clear_log();
    assert!(
        ownership
            .update_solstone("observer-new".to_owned())
            .await
            .expect("change owned value")
    );
    assert_eq!(
        io.log(),
        vec![
            format!("read:{SOLSTONE_OPTION}"),
            format!("write:{SOLSTONE_OPTION}"),
        ]
    );
}

#[tokio::test]
async fn matching_owned_status_left_is_restored() {
    let io = MemoryIndicator::with([
        (STATUS_LEFT, OptionValue::Present("owner status".to_owned())),
        (SOLSTONE_OPTION, OptionValue::Absent),
    ]);
    let mut ownership = install(io.clone()).await;

    ownership.restore().await.expect("restore");

    assert_eq!(
        io.value(STATUS_LEFT),
        OptionValue::Present("owner status".to_owned())
    );
}

#[tokio::test]
async fn newer_status_left_is_preserved() {
    let io = MemoryIndicator::with([
        (STATUS_LEFT, OptionValue::Present("owner status".to_owned())),
        (SOLSTONE_OPTION, OptionValue::Absent),
    ]);
    let mut ownership = install(io.clone()).await;
    io.set_external(
        STATUS_LEFT,
        OptionValue::Present("newer owner value".to_owned()),
    );

    ownership.restore().await.expect("restore");

    assert_eq!(
        io.value(STATUS_LEFT),
        OptionValue::Present("newer owner value".to_owned())
    );
}

#[tokio::test]
async fn matching_owned_solstone_is_restored_or_cleared() {
    let absent = MemoryIndicator::with([
        (STATUS_LEFT, OptionValue::Present("owner status".to_owned())),
        (SOLSTONE_OPTION, OptionValue::Absent),
    ]);
    let mut absent_ownership = install(absent.clone()).await;
    absent_ownership.restore().await.expect("restore absent");
    assert_eq!(absent.value(SOLSTONE_OPTION), OptionValue::Absent);

    let empty = MemoryIndicator::with([
        (STATUS_LEFT, OptionValue::Present("owner status".to_owned())),
        (SOLSTONE_OPTION, OptionValue::Present(String::new())),
    ]);
    let mut empty_ownership = install(empty.clone()).await;
    empty_ownership.restore().await.expect("restore empty");
    assert_eq!(
        empty.value(SOLSTONE_OPTION),
        OptionValue::Present(String::new())
    );
}

#[tokio::test]
async fn newer_solstone_is_preserved() {
    let io = MemoryIndicator::with([
        (STATUS_LEFT, OptionValue::Present("owner status".to_owned())),
        (SOLSTONE_OPTION, OptionValue::Present("old".to_owned())),
    ]);
    let mut ownership = install(io.clone()).await;
    io.set_external(
        SOLSTONE_OPTION,
        OptionValue::Present("newer owner value".to_owned()),
    );

    ownership.restore().await.expect("restore");

    assert_eq!(
        io.value(SOLSTONE_OPTION),
        OptionValue::Present("newer owner value".to_owned())
    );
}

#[tokio::test]
async fn ownership_is_relinquished_after_external_change() {
    let io = MemoryIndicator::with([
        (STATUS_LEFT, OptionValue::Present("owner status".to_owned())),
        (SOLSTONE_OPTION, OptionValue::Absent),
    ]);
    let mut ownership = install(io.clone()).await;
    io.set_external(SOLSTONE_OPTION, OptionValue::Present("external".to_owned()));

    assert!(
        !ownership
            .update_solstone("observer-new".to_owned())
            .await
            .expect("relinquish")
    );
    io.set_external(
        SOLSTONE_OPTION,
        OptionValue::Present("external-again".to_owned()),
    );
    assert!(
        !ownership
            .update_solstone("observer-later".to_owned())
            .await
            .expect("stay relinquished")
    );
    ownership.restore().await.expect("restore");
    assert_eq!(
        io.value(SOLSTONE_OPTION),
        OptionValue::Present("external-again".to_owned())
    );
}

#[tokio::test]
async fn externally_changed_cached_value_relinquishes_without_writing() {
    let io = MemoryIndicator::with([
        (STATUS_LEFT, OptionValue::Present("owner status".to_owned())),
        (SOLSTONE_OPTION, OptionValue::Absent),
    ]);
    let mut ownership = install(io.clone()).await;
    io.set_external(SOLSTONE_OPTION, OptionValue::Present("external".to_owned()));
    io.clear_log();

    assert!(
        !ownership
            .update_solstone("observer".to_owned())
            .await
            .expect("relinquish cached value")
    );
    assert_eq!(io.log(), vec![format!("read:{SOLSTONE_OPTION}")]);

    io.clear_log();
    assert!(
        !ownership
            .update_solstone("observer".to_owned())
            .await
            .expect("remain relinquished")
    );
    ownership
        .restore()
        .await
        .expect("restore relinquished ownership");
    assert!(
        io.log()
            .iter()
            .all(|entry| entry != &format!("write:{SOLSTONE_OPTION}"))
    );
}

#[tokio::test]
async fn sun_to_underscore_status_left_is_preserved() {
    // Pin that these constants model only the observed sun-to-underscore substitution,
    // so the preservation assertion below specifically exercises that normalization.
    assert_eq!(
        WRITTEN_STATUS_LEFT.replace('☼', "_"),
        NORMALIZED_STATUS_LEFT
    );
    let io = MemoryIndicator::with([
        (STATUS_LEFT, OptionValue::Present("owner status".to_owned())),
        (SOLSTONE_OPTION, OptionValue::Absent),
    ]);
    let mut ownership = install(io.clone()).await;
    // Without a locale launchd made tmux return each UTF-8 sun byte sequence as `_`.
    // That readback is not byte-for-byte what the observer wrote, so it must be
    // preserved as a current external value rather than restoring the prior status.
    io.set_external(
        STATUS_LEFT,
        OptionValue::Present(NORMALIZED_STATUS_LEFT.to_owned()),
    );

    ownership.restore().await.expect("restore");

    assert_eq!(
        io.value(STATUS_LEFT),
        OptionValue::Present(NORMALIZED_STATUS_LEFT.to_owned())
    );
}

#[tokio::test]
async fn matching_owned_solstone_restores_nonempty_original() {
    let io = MemoryIndicator::with([
        (STATUS_LEFT, OptionValue::Present("owner status".to_owned())),
        (
            SOLSTONE_OPTION,
            OptionValue::Present("owner solstone".to_owned()),
        ),
    ]);
    let mut ownership = install(io.clone()).await;
    io.set_external(
        STATUS_LEFT,
        OptionValue::Present(NORMALIZED_STATUS_LEFT.to_owned()),
    );

    ownership.restore().await.expect("restore");

    assert_eq!(
        io.value(STATUS_LEFT),
        OptionValue::Present(NORMALIZED_STATUS_LEFT.to_owned())
    );
    assert_eq!(
        io.value(SOLSTONE_OPTION),
        OptionValue::Present("owner solstone".to_owned())
    );
}

#[tokio::test]
async fn externally_cleared_or_emptied_solstone_is_preserved() {
    for external in [OptionValue::Absent, OptionValue::Present(String::new())] {
        let io = MemoryIndicator::with([
            (STATUS_LEFT, OptionValue::Present("owner status".to_owned())),
            (
                SOLSTONE_OPTION,
                OptionValue::Present("owner solstone".to_owned()),
            ),
        ]);
        let mut ownership = install(io.clone()).await;
        io.set_external(
            STATUS_LEFT,
            OptionValue::Present(NORMALIZED_STATUS_LEFT.to_owned()),
        );
        io.set_external(SOLSTONE_OPTION, external.clone());

        ownership.restore().await.expect("restore");

        assert_eq!(
            io.value(STATUS_LEFT),
            OptionValue::Present(NORMALIZED_STATUS_LEFT.to_owned())
        );
        assert_eq!(io.value(SOLSTONE_OPTION), external);
    }
}

#[tokio::test]
async fn matching_owned_status_left_restores_original_absence() {
    let io = MemoryIndicator::with([
        (STATUS_LEFT, OptionValue::Absent),
        (SOLSTONE_OPTION, OptionValue::Absent),
    ]);
    let mut ownership = install(io.clone()).await;

    ownership.restore().await.expect("restore");

    assert_eq!(io.value(STATUS_LEFT), OptionValue::Absent);
}

const REFRESH: Duration = Duration::from_secs(5);

fn installed_prefix_count(io: &MemoryIndicator) -> usize {
    match io.value(STATUS_LEFT) {
        OptionValue::Present(status) => status.matches("#{?@solstone").count(),
        OptionValue::Absent => 0,
    }
}

#[tokio::test]
async fn no_tmux_server_defers_installation_until_one_starts() {
    let io = MemoryIndicator::default();
    io.stop_server();

    let mut indicator = ServerIndicator::start(io.clone(), REFRESH)
        .await
        .expect("a missing server is not a startup error");
    assert!(!indicator.is_installed());
    indicator.refresh().await.expect("refresh without a server");
    assert!(!indicator.is_installed());
    assert!(
        io.log().iter().all(|entry| !entry.starts_with("write:")),
        "nothing may be written without a server: {:?}",
        io.log()
    );

    io.start_server("first", "owner status");
    indicator
        .refresh()
        .await
        .expect("refresh once a server runs");
    assert!(indicator.is_installed());
    assert_eq!(installed_prefix_count(&io), 1);
    assert_eq!(
        io.value(SOLSTONE_OPTION),
        OptionValue::Present(OBSERVING_VALUE.to_owned())
    );

    io.clear_log();
    indicator
        .refresh()
        .await
        .expect("refresh on the same server");
    indicator
        .refresh()
        .await
        .expect("refresh on the same server again");
    assert_eq!(io.log(), vec!["identity", "identity"]);
    assert_eq!(installed_prefix_count(&io), 1);

    indicator.restore().await.expect("restore");
    assert_eq!(
        io.value(STATUS_LEFT),
        OptionValue::Present("owner status".to_owned())
    );
    assert_eq!(io.value(SOLSTONE_OPTION), OptionValue::Absent);
}

#[tokio::test]
async fn server_exit_and_replacement_reinstalls_once_on_the_new_server() {
    let io = MemoryIndicator::with([
        (STATUS_LEFT, OptionValue::Present("owner status".to_owned())),
        (SOLSTONE_OPTION, OptionValue::Absent),
    ]);
    let mut indicator = ServerIndicator::start(io.clone(), REFRESH)
        .await
        .expect("start with a server");
    assert!(indicator.is_installed());

    io.stop_server();
    indicator
        .set_working(true)
        .await
        .expect("activity while the server is gone");
    assert!(!indicator.is_installed());
    indicator.refresh().await.expect("refresh without a server");
    assert!(!indicator.is_installed());

    // A replacement server started between two refreshes has none of our options.
    io.start_server("second", "second status");
    indicator
        .refresh()
        .await
        .expect("refresh on the new server");
    assert!(indicator.is_installed());
    assert_eq!(installed_prefix_count(&io), 1);
    assert_eq!(
        io.value(SOLSTONE_OPTION),
        OptionValue::Present(SYNCING_VALUE.to_owned()),
        "the install must carry the activity seen while waiting"
    );

    io.stop_server();
    io.start_server("third", "third status");
    indicator
        .refresh()
        .await
        .expect("refresh on a replaced server");
    assert_eq!(installed_prefix_count(&io), 1);

    indicator.restore().await.expect("restore");
    assert_eq!(
        io.value(STATUS_LEFT),
        OptionValue::Present("third status".to_owned())
    );
}

#[tokio::test]
async fn restore_after_the_server_exits_is_clean() {
    let io = MemoryIndicator::with([
        (STATUS_LEFT, OptionValue::Present("owner status".to_owned())),
        (SOLSTONE_OPTION, OptionValue::Absent),
    ]);
    let mut ownership = IndicatorOwnership::install_default(io.clone())
        .await
        .expect("install");
    io.stop_server();

    ownership
        .restore()
        .await
        .expect("a server that exited took its options with it");
}

#[tokio::test]
async fn real_tmux_errors_still_surface() {
    let io = MemoryIndicator::default();
    io.break_server();

    let error = ServerIndicator::start(io.clone(), REFRESH)
        .await
        .err()
        .expect("a failing tmux is a startup error");
    assert!(matches!(error, IndicatorError::Nonzero { .. }));
}

#[tokio::test]
async fn a_failed_update_says_what_tmux_said() {
    let io = MemoryIndicator::with([
        (STATUS_LEFT, OptionValue::Present("owner status".to_owned())),
        (SOLSTONE_OPTION, OptionValue::Absent),
    ]);
    let mut indicator = ServerIndicator::start(io.clone(), REFRESH)
        .await
        .expect("start indicator");
    io.break_server();

    let error = ShutdownIndicator::set_activity(&mut indicator, SyncActivity::Working)
        .await
        .expect_err("a failing tmux fails the update");
    assert_eq!(
        error.to_string(),
        "indicator update failed: tmux option command for @solstone exited 1: lost server"
    );
}

#[tokio::test]
async fn a_failed_tmux_option_command_keeps_tmux_stderr() {
    let tmux = PathBuf::from("/usr/bin/tmux");
    let runner = FixtureRunner::new([ExpectedInvocation {
        invocation: CommandInvocation {
            operation: CommandOperation::Tmux(TmuxOperation::ServerIdentity),
            executable: tmux.clone(),
            args: ["display-message", "-p", "#{pid} #{start_time}"]
                .into_iter()
                .map(OsString::from)
                .collect(),
            timeout: TMUX_TIMEOUT,
        },
        outcome: command_output(&[], b"lost server\n", 1),
    }]);
    let io = CommandIndicatorIo::new(runner.clone(), tmux).expect("absolute tmux");

    let error = io.server_identity().await.expect_err("tmux exited nonzero");
    assert_eq!(
        error.to_string(),
        "tmux option command for server identity exited 1: lost server"
    );
    runner.assert_finished().expect("one tmux command");
}

async fn install(io: MemoryIndicator) -> IndicatorOwnership<MemoryIndicator> {
    IndicatorOwnership::install(io, WRITTEN_STATUS_LEFT.to_owned(), "observer".to_owned())
        .await
        .expect("install indicator")
}

#[derive(Clone, Default)]
struct MemoryIndicator {
    values: Arc<Mutex<HashMap<String, OptionValue>>>,
    log: Arc<Mutex<Vec<String>>>,
    server: Arc<Mutex<MemoryServer>>,
}

#[derive(Clone)]
enum MemoryServer {
    Running(String),
    Absent,
    Broken,
}

impl Default for MemoryServer {
    fn default() -> Self {
        Self::Running("default".to_owned())
    }
}

impl MemoryIndicator {
    fn with<const N: usize>(values: [(&str, OptionValue); N]) -> Self {
        Self {
            values: Arc::new(Mutex::new(
                values
                    .into_iter()
                    .map(|(name, value)| (name.to_owned(), value))
                    .collect(),
            )),
            log: Arc::new(Mutex::new(Vec::new())),
            server: Arc::default(),
        }
    }

    /// A server's options exist only while it runs.
    fn stop_server(&self) {
        *self.server.lock().expect("server state poisoned") = MemoryServer::Absent;
        self.values
            .lock()
            .expect("indicator state poisoned")
            .clear();
    }

    fn start_server(&self, identity: &str, status_left: &str) {
        *self.server.lock().expect("server state poisoned") =
            MemoryServer::Running(identity.to_owned());
        self.set_external(STATUS_LEFT, OptionValue::Present(status_left.to_owned()));
    }

    fn break_server(&self) {
        *self.server.lock().expect("server state poisoned") = MemoryServer::Broken;
    }

    fn reachable(&self, name: &str) -> Result<String, IndicatorError> {
        match &*self.server.lock().expect("server state poisoned") {
            MemoryServer::Running(identity) => Ok(identity.clone()),
            MemoryServer::Absent => Err(IndicatorError::NoServer),
            MemoryServer::Broken => Err(IndicatorError::Nonzero {
                name: name.to_owned(),
                status: 1,
                stderr: "lost server".to_owned(),
            }),
        }
    }

    fn value(&self, name: &str) -> OptionValue {
        self.values
            .lock()
            .expect("indicator state poisoned")
            .get(name)
            .cloned()
            .unwrap_or(OptionValue::Absent)
    }

    fn set_external(&self, name: &str, value: OptionValue) {
        self.values
            .lock()
            .expect("indicator state poisoned")
            .insert(name.to_owned(), value);
    }

    fn clear_log(&self) {
        self.log.lock().expect("indicator log poisoned").clear();
    }

    fn log(&self) -> Vec<String> {
        self.log.lock().expect("indicator log poisoned").clone()
    }
}

impl IndicatorIo for MemoryIndicator {
    fn server_identity<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<String, IndicatorError>> + Send + 'a>> {
        Box::pin(async move {
            self.log
                .lock()
                .expect("indicator log poisoned")
                .push("identity".to_owned());
            self.reachable("server identity")
        })
    }

    fn read<'a>(
        &'a self,
        name: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<OptionValue, IndicatorError>> + Send + 'a>> {
        Box::pin(async move {
            self.log
                .lock()
                .expect("indicator log poisoned")
                .push(format!("read:{name}"));
            self.reachable(name)?;
            Ok(self.value(name))
        })
    }

    fn write<'a>(
        &'a self,
        name: &'a str,
        value: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), IndicatorError>> + Send + 'a>> {
        Box::pin(async move {
            self.log
                .lock()
                .expect("indicator log poisoned")
                .push(format!("write:{name}"));
            self.reachable(name)?;
            self.values
                .lock()
                .expect("indicator state poisoned")
                .insert(name.to_owned(), OptionValue::Present(value.to_owned()));
            Ok(())
        })
    }

    fn clear<'a>(
        &'a self,
        name: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), IndicatorError>> + Send + 'a>> {
        Box::pin(async move {
            self.reachable(name)?;
            self.values
                .lock()
                .expect("indicator state poisoned")
                .insert(name.to_owned(), OptionValue::Absent);
            Ok(())
        })
    }
}
