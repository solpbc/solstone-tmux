// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use solstone_tmux_observer::indicator::{
    IndicatorError, IndicatorIo, IndicatorOwnership, OptionValue, SOLSTONE_OPTION, STATUS_LEFT,
};

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

async fn install(io: MemoryIndicator) -> IndicatorOwnership<MemoryIndicator> {
    IndicatorOwnership::install(
        io,
        "owner status | #{@solstone}".to_owned(),
        "observer".to_owned(),
    )
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
