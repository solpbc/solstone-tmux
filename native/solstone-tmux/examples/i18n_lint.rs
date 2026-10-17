// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! AST-based owner-visible literal gate. No translation service is involved.
#[path = "support/inventory.rs"]
mod inventory;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::path::Path;

fn failures() -> (Vec<Value>, usize) {
    let exclusions: Vec<Value> =
        serde_json::from_str(include_str!("../i18n/exclusions.json")).expect("exclusions");
    let allowed = exclusions
        .iter()
        .map(|row| {
            (
                row["path"].as_str().unwrap().to_owned(),
                row["value"].as_str().unwrap().to_owned(),
            )
        })
        .collect::<BTreeSet<_>>();
    let rows = inventory::discover(Path::new("native/solstone-tmux"));
    let mut failures = Vec::new();
    for row in &rows {
        if row["scope"] != "source" || row["kind"] == "path" {
            continue;
        }
        let context = row["context"].as_str().unwrap();
        if context.contains("attribute:")
            || context.contains("method:expect")
            || context.contains("macro:unreachable")
            || context.contains("macro:panic")
            || context.contains("macro:debug_assert")
        {
            continue;
        }
        let path = row["path"].as_str().unwrap();
        let value = row["value"].as_str().unwrap();
        if allowed.contains(&(path.to_owned(), value.to_owned())) {
            continue;
        }
        // These are programming-contract failures in the formatter, rather
        // than command output. An ordinary command's language error is FTL.
        if path.ends_with("/i18n.rs")
            && ["fn:bundle", "fn:try_render", "fn:text", "fn:pad_terminal"]
                .iter()
                .any(|name| context.contains(name))
        {
            continue;
        }
        let name = row["macro"]["name"].as_str().unwrap_or_default();
        let format_index = usize::from(matches!(name, "write" | "writeln"));
        let source = std::fs::read(path).expect("source");
        let raw = std::str::from_utf8(
            &source[row["start"].as_u64().unwrap() as usize..row["end"].as_u64().unwrap() as usize],
        )
        .expect("literal boundary");
        let output_literal = matches!(name, "println" | "eprintln" | "write" | "writeln")
            && row["macro"]["arguments"][format_index].as_str() == Some(raw);
        if value.chars().any(char::is_alphabetic)
            && (value.chars().any(char::is_whitespace)
                || output_literal
                || context.contains("literal-output:method"))
        {
            failures.push(json!({"path":path,"line":row["line"],"literal":value}));
        }
    }
    (failures, rows.len())
}
fn main() {
    let mut args = std::env::args().skip(1);
    let mut negative = false;
    let mut out = std::path::PathBuf::from("target/i18n-lint");
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--negative-control" => negative = true,
            "--out" => out = args.next().expect("output directory").into(),
            _ => panic!("unknown tool argument"),
        }
    }
    std::fs::create_dir_all(&out).expect("metrics directory");
    let (clean, count) = failures();
    if !clean.is_empty() {
        println!(
            "hardcoded visible literal: {}",
            serde_json::to_string_pretty(&clean).unwrap()
        );
        std::process::exit(1);
    }
    if negative {
        let path = Path::new("native/solstone-tmux/src/i18n_lint_negative.rs");
        assert!(!path.exists());
        let mut evidence = Vec::new();
        for literal in ["A newly added owner-facing diagnostic.", "hello"] {
            std::fs::write(path,format!("// SPDX-License-Identifier: AGPL-3.0-only\n// Copyright (c) 2026 sol pbc\n\nfn owner_output() {{ eprintln!({literal:?}); }}\n")).unwrap();
            let (found, _) = failures();
            std::fs::remove_file(path).unwrap();
            assert_eq!(found.len(), 1, "negative control must be the only failure");
            assert_eq!(found[0]["literal"], literal);
            evidence.push(json!({"literal":literal,"failures":found,"expected_exit_code":1}));
        }
        std::fs::write(
            out.join("lint-negative-control.json"),
            serde_json::to_string_pretty(&evidence).unwrap(),
        )
        .unwrap();
        assert!(failures().0.is_empty());
    }
    std::fs::write(
        out.join("lint.json"),
        serde_json::to_string_pretty(
            &json!({"hardcoded_visible_literals":0,"source_and_test_ast_rows":count}),
        )
        .unwrap(),
    )
    .unwrap();
    println!("owner literal lint passed across {count} AST rows");
}
