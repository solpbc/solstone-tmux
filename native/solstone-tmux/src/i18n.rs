// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Embedded, restricted Fluent messages. Protocol values never enter a catalog.
use std::cell::{Cell, RefCell};
use std::ffi::OsString;
use std::fmt;
use std::sync::OnceLock;

use fluent_bundle::{FluentArgs, FluentBundle, FluentResource};
use unicode_width::UnicodeWidthStr;

#[path = "i18n_ids.rs"]
mod ids;
pub use ids::{ALL_MESSAGES, MessageId};

pub const EN_US: &str = include_str!("../i18n/en-US.ftl");
pub const EN_XA: &str = include_str!("../i18n/en-XA.ftl");

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Locale {
    EnUs,
    EnXa,
}

impl Locale {
    pub fn resolve(
        override_value: Option<&str>,
        mut environment: impl FnMut(&str) -> Option<String>,
    ) -> Self {
        let value = override_value.map(str::to_owned).or_else(|| {
            ["LC_ALL", "LC_MESSAGES", "LANG"]
                .into_iter()
                .find_map(|key| environment(key).filter(|value| !value.is_empty()))
        });
        match value
            .as_deref()
            .unwrap_or("en-US")
            .split(['.', '@'])
            .next()
            .unwrap_or_default()
            .replace('_', "-")
            .to_ascii_lowercase()
            .as_str()
        {
            "en-xa" => Self::EnXa,
            _ => Self::EnUs,
        }
    }
}

static OVERRIDE: OnceLock<Locale> = OnceLock::new();
thread_local! {
    static TEST_LOCALE: Cell<Option<Locale>> = const { Cell::new(None) };
    static BUNDLES: RefCell<(FluentBundle<FluentResource>,FluentBundle<FluentResource>)> = RefCell::new((bundle(Locale::EnUs),bundle(Locale::EnXa)));
}

pub fn current_locale() -> Locale {
    TEST_LOCALE
        .with(Cell::get)
        .or_else(|| OVERRIDE.get().copied())
        .unwrap_or_else(|| Locale::resolve(None, |key| std::env::var(key).ok()))
}

/// Scoped locale injection does not mutate the process environment.
pub fn with_locale<T>(locale: Locale, operation: impl FnOnce() -> T) -> T {
    struct Restore(Option<Locale>);
    impl Drop for Restore {
        fn drop(&mut self) {
            TEST_LOCALE.with(|slot| slot.set(self.0));
        }
    }
    let _restore = Restore(TEST_LOCALE.with(|slot| slot.replace(Some(locale))));
    operation()
}

/// A global override is accepted only before the command, so mark values cannot
/// accidentally become options. Accepted pairing tokens stay independent.
pub fn process_arguments(mut args: Vec<OsString>) -> Result<Vec<OsString>, &'static str> {
    if args.get(1).is_some_and(|value| value == "--lang") {
        let Some(value) = args.get(2).and_then(|value| value.to_str()) else {
            return Err(MessageId::CliLocaleInvalid.text());
        };
        if !matches!(value, "en-US" | "en-XA") {
            return Err(MessageId::CliLocaleInvalid.text());
        }
        let locale = Locale::resolve(Some(value), |_| None);
        let _ = OVERRIDE.set(locale);
        args.drain(1..3);
    }
    Ok(args)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArgKind {
    Text,
    Number,
    Datetime,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ArgValue {
    Text(String),
    Number(u64),
    /// Unix seconds. No date-bearing message is currently shipped.
    Datetime(i64),
}

/// A semantic message is available before presentation, so tests and command
/// callers can inspect its stable identifier and named typed arguments.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Message {
    pub id: MessageId,
    pub arguments: Vec<(&'static str, ArgValue)>,
}
impl Message {
    pub fn new(id: MessageId, arguments: Vec<(&'static str, ArgValue)>) -> Self {
        Self { id, arguments }
    }
}
impl fmt::Display for Message {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&render(self.id, &self.arguments))
    }
}

impl ArgValue {
    fn kind(&self) -> ArgKind {
        match self {
            Self::Text(_) => ArgKind::Text,
            Self::Number(_) => ArgKind::Number,
            Self::Datetime(_) => ArgKind::Datetime,
        }
    }
}

fn bundle(locale: Locale) -> FluentBundle<FluentResource> {
    let resource = FluentResource::try_new(
        match locale {
            Locale::EnUs => EN_US,
            Locale::EnXa => EN_XA,
        }
        .to_owned(),
    )
    .expect("embedded Fluent parses");
    let mut bundle = FluentBundle::new(vec!["en-US".parse().expect("fixed locale")]);
    // Isolation would change the existing English CLI bytes. Arguments are
    // already explicit units; a production RTL policy needs its own decision.
    bundle.set_use_isolating(false);
    bundle
        .add_resource(resource)
        .expect("embedded IDs are unique");
    bundle
}

pub fn try_render(
    locale: Locale,
    id: MessageId,
    values: &[(&str, ArgValue)],
) -> Result<String, String> {
    let schema = ids::SCHEMAS[id as usize];
    if schema.len() != values.len()
        || schema.iter().any(|(name, kind)| {
            values.iter().filter(|(key, _)| key == name).count() != 1
                || !values
                    .iter()
                    .any(|(key, value)| key == name && value.kind() == *kind)
        })
    {
        return Err(format!("invalid arguments for {}", id.key()));
    }
    let mut args = FluentArgs::new();
    for (name, value) in values {
        match value {
            ArgValue::Text(value) => args.set(*name, value.as_str()),
            ArgValue::Number(value) => args.set(*name, *value),
            ArgValue::Datetime(value) => args.set(*name, *value),
        }
    }
    BUNDLES.with(|bundles| {
        let bundles = bundles.borrow();
        let bundle = match locale {
            Locale::EnUs => &bundles.0,
            Locale::EnXa => &bundles.1,
        };
        let pattern = bundle
            .get_message(id.key())
            .and_then(|message| message.value())
            .ok_or_else(|| format!("missing message {}", id.key()))?;
        let mut errors = Vec::new();
        let output = bundle
            .format_pattern(pattern, Some(&args), &mut errors)
            .into_owned();
        if errors.is_empty() {
            Ok(output)
        } else {
            Err(format!("{}: {errors:?}", id.key()))
        }
    })
}

pub fn render(id: MessageId, values: &[(&str, ArgValue)]) -> String {
    try_render(current_locale(), id, values)
        .expect("generated message arguments match the embedded catalog")
}

impl MessageId {
    pub fn key(self) -> &'static str {
        ids::IDS[self as usize]
    }
    pub fn schema(self) -> &'static [(&'static str, ArgKind)] {
        ids::SCHEMAS[self as usize]
    }
    pub fn text(self) -> &'static str {
        assert!(
            self.schema().is_empty(),
            "text requires a message without arguments"
        );
        static US: OnceLock<Vec<String>> = OnceLock::new();
        static XA: OnceLock<Vec<String>> = OnceLock::new();
        let locale = current_locale();
        let cache = match locale {
            Locale::EnUs => &US,
            Locale::EnXa => &XA,
        };
        &cache.get_or_init(|| {
            ALL_MESSAGES
                .iter()
                .map(|id| {
                    if id.schema().is_empty() {
                        try_render(locale, *id, &[]).expect("embedded message formats")
                    } else {
                        String::new()
                    }
                })
                .collect()
        })[self as usize]
    }
}

impl fmt::Display for MessageId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.text())
    }
}

/// Terminal cells, not UTF-8 bytes, determine a text column's padding.
pub fn pad_terminal(value: &str, columns: usize) -> String {
    format!(
        "{value}{}",
        " ".repeat(columns.saturating_sub(value.width()))
    )
}
