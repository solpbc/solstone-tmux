#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
native_root="$repo_root/native/solstone-tmux"
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

legacy_identity_tokens=(
    "solstone-tmux-"'observer'
    "solstone_tmux_"'observer'
    "com.solstone.tmux-"'observer'
)

# These surfaces intentionally retain transitional history until P6. The final-tree
# guard removes the AGENTS.md alias/Python carve-outs, and the design file is
# deleted in commit 15.
identity_carveout() {
    case "$1" in
        AGENTS.md | CLAUDE.md | docs/design/native-1.0.0-cutover.md | src/solstone_tmux/* | tests/*)
            return 0
            ;;
    esac
    return 1
}

while IFS= read -r -d '' tracked_path; do
    case "$tracked_path" in
        target/* | .venv/* | .git/*)
            continue
            ;;
    esac
    if identity_carveout "$tracked_path"; then
        continue
    fi
    for legacy_token in "${legacy_identity_tokens[@]}"; do
        if [[ "$tracked_path" == *"$legacy_token"* ]]; then
            echo "$tracked_path: legacy native identity is forbidden in tracked paths" >&2
            failed=true
        fi
        tracked_file="$repo_root/$tracked_path"
        if [[ -f "$tracked_file" ]] && identity_hits="$(rg -a -nF "$legacy_token" "$tracked_file")"; then
            printf '%s\n' "$identity_hits" >&2
            echo "$tracked_path: legacy native identity is forbidden in tracked contents" >&2
            failed=true
        fi
    done
done < <(git -C "$repo_root" ls-files -z)

mapfile -d '' native_manifests < <(
    find "$repo_root/native" -mindepth 2 -maxdepth 2 -type f -name Cargo.toml -print0
)
if ((${#native_manifests[@]} != 1)) || [[ "${native_manifests[0]:-}" != "$native_root/Cargo.toml" ]]; then
    echo "$repo_root/native: expected exactly one workspace crate at native/solstone-tmux" >&2
    failed=true
fi

mapfile -t workspace_member_lines < <(sed -n '/^members = /p' "$repo_root/Cargo.toml")
if ((${#workspace_member_lines[@]} != 1)) ||
    [[ "${workspace_member_lines[0]}" != 'members = ["native/solstone-tmux"]' ]]; then
    echo "$repo_root/Cargo.toml: expected exactly one solstone-tmux workspace member" >&2
    failed=true
fi

package_name="$(
    awk '
        $0 == "[package]" { in_package = 1; next }
        in_package && /^\[/ { exit }
        in_package && /^name = "/ {
            value = $0
            sub(/^name = "/, "", value)
            sub(/"$/, "", value)
            print value
            exit
        }
    ' "$native_root/Cargo.toml"
)"
mapfile -t declared_bins < <(
    awk '
        $0 == "[[bin]]" { in_bin = 1; next }
        in_bin && /^\[/ { in_bin = 0 }
        in_bin && /^name = "/ {
            value = $0
            sub(/^name = "/, "", value)
            sub(/"$/, "", value)
            print value
        }
    ' "$native_root/Cargo.toml"
)
if [[ "$package_name" != "solstone-tmux" ]]; then
    echo "$native_root/Cargo.toml: package must be named solstone-tmux" >&2
    failed=true
fi
if ((${#declared_bins[@]} != 1)) || [[ "${declared_bins[0]}" != "solstone-tmux" ]]; then
    echo "$native_root/Cargo.toml: expected exactly one binary named solstone-tmux" >&2
    failed=true
fi

systemd_identity_count="$(
    awk '$0 == "pub const UNIT_NAME: &str = \"solstone-tmux.service\";" { count += 1 }
        END { print count + 0 }' "$native_root/src/service/systemd.rs"
)"
launchd_identity_count="$(
    awk '$0 == "pub const LABEL: &str = \"com.solstone.tmux\";" { count += 1 }
        END { print count + 0 }' "$native_root/src/service/launchd.rs"
)"
if [[ "$systemd_identity_count" != "1" ]]; then
    echo "$native_root/src/service/systemd.rs: expected exact solstone-tmux.service identity" >&2
    failed=true
fi
if [[ "$launchd_identity_count" != "1" ]]; then
    echo "$native_root/src/service/launchd.rs: expected exact com.solstone.tmux identity" >&2
    failed=true
fi

if $failed; then
    exit 1
fi

echo "Rust repository guards passed."
