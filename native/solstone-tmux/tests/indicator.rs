// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use solstone_tmux::indicator::{
    IndicatorError, IndicatorIo, IndicatorOwnership, OBSERVING_VALUE, OptionValue, SOLSTONE_OPTION,
    STATUS_LEFT, SYNCING_VALUE,
};
use solstone_tmux::observer::{NoopShutdownIndicator, ShutdownIndicator};
use solstone_tmux::sync::SyncActivity;

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

async fn install(io: MemoryIndicator) -> IndicatorOwnership<MemoryIndicator> {
    IndicatorOwnership::install(io, WRITTEN_STATUS_LEFT.to_owned(), "observer".to_owned())
        .await
        .expect("install indicator")
}

#[derive(Clone, Default)]
struct MemoryIndicator {
    values: Arc<Mutex<HashMap<String, OptionValue>>>,
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
}

impl IndicatorIo for MemoryIndicator {
    fn read<'a>(
        &'a self,
        name: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<OptionValue, IndicatorError>> + Send + 'a>> {
        Box::pin(async move { Ok(self.value(name)) })
    }

    fn write<'a>(
        &'a self,
        name: &'a str,
        value: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), IndicatorError>> + Send + 'a>> {
        Box::pin(async move {
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
            self.values
                .lock()
                .expect("indicator state poisoned")
                .insert(name.to_owned(), OptionValue::Absent);
            Ok(())
        })
    }
}
