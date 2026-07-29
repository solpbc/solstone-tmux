#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
native_root="$repo_root/native/solstone-tmux-observer"
crate_roots=(
    "$native_root/src/lib.rs"
    "$native_root/src/main.rs"
)
expected_spdx='// SPDX-License-Identifier: AGPL-3.0-only'
expected_copyright='// Copyright (c) 2026 sol pbc'
failed=false

mapfile -d '' rust_files < <(find "$native_root" -type f -name '*.rs' -print0 | sort -z)
if ((${#rust_files[@]} == 0)); then
    echo "$native_root: no Rust source files found" >&2
    exit 1
fi

for file in "${rust_files[@]}"; do
    first_line="$(sed -n '1p' "$file")"
    second_line="$(sed -n '2p' "$file")"
    if [[ "$first_line" != "$expected_spdx" ]]; then
        echo "$file:1: missing exact SPDX header" >&2
        failed=true
    fi
    if [[ "$second_line" != "$expected_copyright" ]]; then
        echo "$file:2: missing exact copyright header" >&2
        failed=true
    fi
done

for crate_root in "${crate_roots[@]}"; do
    if [[ "$(sed -n '4p' "$crate_root")" != '#![forbid(unsafe_code)]' ]]; then
        echo "$crate_root:4: missing #![forbid(unsafe_code)] before crate items" >&2
        failed=true
    fi
done

if unsafe_hits="$(rg -n '\bunsafe\b' "$native_root" --glob '*.rs')"; then
    printf '%s\n' "$unsafe_hits" >&2
    echo "$native_root: bare unsafe token is forbidden" >&2
    failed=true
fi

mapfile -t targets < <("$repo_root/scripts/rust-targets.sh")
deny_file="$repo_root/deny.toml"
in_graph=false
in_graph_targets=false
saw_graph=false
saw_graph_targets=false
saw_graph_targets_end=false
declare -A seen_graph_targets=()
graph_targets=()
line_number=0

while IFS= read -r line || [[ -n "$line" ]]; do
    ((line_number += 1))
    if [[ "$line" =~ ^\[[^]]+\]$ ]]; then
        if $in_graph_targets; then
            echo "$deny_file:$line_number: graph targets array is unterminated" >&2
            failed=true
            in_graph_targets=false
        fi
        if [[ "$line" == "[graph]" ]]; then
            if $saw_graph; then
                echo "$deny_file:$line_number: duplicate graph section" >&2
                failed=true
            fi
            saw_graph=true
            in_graph=true
        else
            in_graph=false
        fi
        continue
    fi

    if $in_graph && [[ "$line" == "targets = [" ]]; then
        if $saw_graph_targets; then
            echo "$deny_file:$line_number: duplicate graph targets array" >&2
            failed=true
        fi
        saw_graph_targets=true
        in_graph_targets=true
        continue
    fi

    if $in_graph_targets && [[ "$line" == "]" ]]; then
        in_graph_targets=false
        saw_graph_targets_end=true
        continue
    fi

    if $in_graph_targets; then
        if [[ "$line" =~ ^[[:space:]]{4}\"([^\"]+)\",$ ]]; then
            graph_target="${BASH_REMATCH[1]}"
            if [[ -n "${seen_graph_targets[$graph_target]:-}" ]]; then
                echo "$deny_file:$line_number: duplicate graph target" >&2
                failed=true
            else
                seen_graph_targets["$graph_target"]=1
                graph_targets+=("$graph_target")
            fi
        else
            echo "$deny_file:$line_number: expected one quoted graph target per line" >&2
            failed=true
        fi
    fi
done < "$deny_file"

if ! $saw_graph || ! $saw_graph_targets || ! $saw_graph_targets_end || $in_graph_targets; then
    echo "$deny_file: graph targets are missing, empty, malformed, or unterminated" >&2
    failed=true
elif ((${#graph_targets[@]} == 0)); then
    echo "$deny_file: graph targets are empty" >&2
    failed=true
elif ((${#graph_targets[@]} != ${#targets[@]})); then
    echo "$deny_file: graph targets do not match rust-toolchain.toml" >&2
    failed=true
else
    for index in "${!targets[@]}"; do
        if [[ "${graph_targets[$index]}" != "${targets[$index]}" ]]; then
            echo "$deny_file: graph target order does not match rust-toolchain.toml" >&2
            failed=true
            break
        fi
    done
fi

drift_files=(
    "$repo_root/Makefile"
    "$repo_root/AGENTS.md"
)
while IFS= read -r -d '' script; do
    drift_files+=("$script")
done < <(find "$repo_root/scripts" -maxdepth 1 -type f -print0)

for target in "${targets[@]}"; do
    if drift_hits="$(rg -nF "$target" "${drift_files[@]}")"; then
        printf '%s\n' "$drift_hits" >&2
        echo "target drift: target literals belong only in rust-toolchain.toml" >&2
        failed=true
    fi
done

if $failed; then
    exit 1
fi

echo "Rust repository guards passed."
