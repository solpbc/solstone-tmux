// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use solstone_tmux::i18n::{ALL_MESSAGES, ArgKind, ArgValue, Locale, try_render};
use unicode_width::UnicodeWidthStr;

fn main() {
    let mut rows = Vec::new();
    for &id in ALL_MESSAGES {
        for count in [1, 2] {
            let args = id
                .schema()
                .iter()
                .map(|&(name, kind)| {
                    (
                        name,
                        match kind {
                            ArgKind::Text => ArgValue::Text(String::new()),
                            ArgKind::Number => ArgValue::Number(count),
                            ArgKind::Datetime => ArgValue::Datetime(0),
                        },
                    )
                })
                .collect::<Vec<_>>();
            let en = try_render(Locale::EnUs, id, &args).expect("English catalog renders");
            let xa = try_render(Locale::EnXa, id, &args).expect("pseudo catalog renders");
            rows.push(serde_json::json!({
                "id": id.key(), "count": count,
                "english": en, "pseudo": xa,
                "english_characters": en.chars().count(),
                "pseudo_characters": xa.chars().count(),
            }));
        }
    }
    let widths = ["中文", "e\u{301}", "العربية", "👩‍💻"].map(|value| {
        serde_json::json!({
            "text": value, "bytes": value.len(),
            "characters": value.chars().count(), "cells": value.width(),
        })
    });
    println!(
        "{}",
        serde_json::json!({"messages": rows, "widths": widths})
    );
}
