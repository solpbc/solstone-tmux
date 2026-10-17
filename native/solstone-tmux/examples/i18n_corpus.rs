// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Reproduce the independently captured original English corpus after migration.
use solstone_tmux::i18n::{ALL_MESSAGES, ArgValue, Locale, try_render, with_locale};

fn main() {
    let mut rows: Vec<serde_json::Value> =
        serde_json::from_str(include_str!("../tests/data/i18n/en-US-before.json"))
            .expect("baseline fixture");
    with_locale(Locale::EnUs, || {
        for row in &mut rows {
            let key = row["id"].as_str().expect("message key").to_owned();
            let output = if let Some(seconds) = key.strip_prefix("real.about.age.") {
                solstone_tmux::about::journal_line(
                    &solstone_tmux::journal_version::JournalVersionStatus::LastKnown(
                        "1.2.3".into(),
                    ),
                    None,
                    Some(1_000_000),
                    1_000_000 + seconds.parse::<i64>().expect("age"),
                )
            } else if key == "real.cli.help" {
                solstone_tmux::cli::usage()
            } else if key == "real.cli.unknown" {
                solstone_tmux::cli::parse_args(
                    ["solstone-tmux", "synthetic-command"]
                        .into_iter()
                        .map(Into::into),
                )
                .expect_err("unknown command")
                .to_string()
            } else {
                let id = *ALL_MESSAGES
                    .iter()
                    .find(|id| id.key() == key.replace('.', "-"))
                    .expect("message ID");
                let meta: serde_json::Value = serde_json::from_str(
                    &std::fs::read_to_string(format!(
                        "{}/i18n/meta/{}.json",
                        env!("CARGO_MANIFEST_DIR"),
                        id.key()
                    ))
                    .expect("metadata"),
                )
                .expect("valid metadata");
                let values = row["args"]
                    .as_object()
                    .expect("args")
                    .iter()
                    .map(|(name, value)| {
                        let value = value.as_str().expect("text argument");
                        let spec = meta["args"]
                            .as_array()
                            .expect("argument schema")
                            .iter()
                            .find(|arg| arg["name"] == *name)
                            .expect("named argument")["rust_format_spec"]
                            .as_str()
                            .expect("Rust format");
                        (
                            name.as_str(),
                            ArgValue::Text(if spec == "?" {
                                format!("{value:?}")
                            } else {
                                value.to_owned()
                            }),
                        )
                    })
                    .collect::<Vec<_>>();
                try_render(Locale::EnUs, id, &values).expect("message render")
            };
            row["output"] = output.into();
        }
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&rows).expect("corpus JSON")
    );
}
