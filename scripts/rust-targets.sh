#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
toolchain_file="$repo_root/rust-toolchain.toml"

if [[ ! -f "$toolchain_file" ]]; then
    echo "$toolchain_file: missing target source of truth" >&2
    exit 1
fi

in_targets=false
saw_targets=false
saw_end=false
declare -A seen=()
targets=()
line_number=0

while IFS= read -r line || [[ -n "$line" ]]; do
    ((line_number += 1))
    if [[ "$line" == "targets = [" ]]; then
        if $saw_targets; then
            echo "$toolchain_file:$line_number: duplicate targets array" >&2
            exit 1
        fi
        in_targets=true
        saw_targets=true
        continue
    fi

    if $in_targets && [[ "$line" == "]" ]]; then
        in_targets=false
        saw_end=true
        continue
    fi

    if $in_targets; then
        if [[ "$line" =~ ^[[:space:]]{4}\"([^\"]+)\",$ ]]; then
            target="${BASH_REMATCH[1]}"
            if [[ -n "${seen[$target]:-}" ]]; then
                echo "$toolchain_file:$line_number: duplicate target '$target'" >&2
                exit 1
            fi
            seen["$target"]=1
            targets+=("$target")
        else
            echo "$toolchain_file:$line_number: expected one quoted target per line" >&2
            exit 1
        fi
    fi
done < "$toolchain_file"

if ! $saw_targets || ! $saw_end || $in_targets || ((${#targets[@]} == 0)); then
    echo "$toolchain_file: targets array is missing, empty, or unterminated" >&2
    exit 1
fi

printf '%s\n' "${targets[@]}"
