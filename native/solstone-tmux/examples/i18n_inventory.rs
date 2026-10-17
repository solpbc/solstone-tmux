// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Deterministic discovery across all Rust source and test files.
#[path = "support/inventory.rs"]
mod inventory;
fn main() {
    let root = std::path::PathBuf::from(
        std::env::args()
            .nth(1)
            .unwrap_or_else(|| "native/solstone-tmux".into()),
    );
    println!(
        "{}",
        serde_json::to_string_pretty(&inventory::discover(&root)).unwrap()
    );
}
