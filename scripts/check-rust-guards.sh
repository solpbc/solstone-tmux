#!/usr/bin/env bash
set -euo pipefail

if ! command -v rg >/dev/null 2>&1; then
    echo "required repository guard tool is unavailable: rg" >&2
    exit 1
fi

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

package_model_file="$native_root/tests/support/package_model.rs"
mapfile -t package_model_targets < <(
    awk '
        /pub const fn rust_target/ { in_targets = 1; next }
        in_targets && /^    }/ { exit }
        in_targets && /Self::[A-Za-z0-9_]+ => "/ {
            value = $0
            sub(/^.*=> "/, "", value)
            sub(/",$/, "", value)
            print value
        }
    ' "$package_model_file"
)
if ((${#package_model_targets[@]} != ${#targets[@]})); then
    echo "$package_model_file: target replica does not match rust-toolchain.toml" >&2
    failed=true
else
    for index in "${!targets[@]}"; do
        if [[ "${package_model_targets[$index]}" != "${targets[$index]}" ]]; then
            echo "$package_model_file: target order does not match rust-toolchain.toml" >&2
            failed=true
            break
        fi
    done
fi

github_workflow_root="$repo_root/.github/workflows"
mapfile -d '' github_workflows < <(
    find "$github_workflow_root" -type f -print0 2>/dev/null || true
)
if ((${#github_workflows[@]} != 0)); then
    printf '%s\n' "${github_workflows[@]}" >&2
    echo "$github_workflow_root: GitHub workflows are forbidden; builds and gates run on native release machines" >&2
    failed=true
fi

drift_files=(
    "$repo_root/Makefile"
    "$repo_root/AGENTS.md"
)
while IFS= read -r -d '' script; do
    drift_files+=("$script")
done < <(find "$repo_root/scripts" -maxdepth 1 -type f -print0)

for target in "${targets[@]}"; do
    for drift_file in "${drift_files[@]}"; do
        while IFS=: read -r drift_line_number drift_text; do
            [[ -n "$drift_line_number" ]] || continue
            echo "$drift_file:$drift_line_number:$drift_text" >&2
            echo "target drift: target literals belong only in rust-toolchain.toml" >&2
            failed=true
        done < <(rg -nF "$target" "$drift_file" || true)
    done
done

# These are the only permitted tracked content occurrences of the retired
# identities. Keeping each token whole here makes the ban reviewable; the scan
# below verifies this exact declaration before exempting it.
legacy_identity_tokens=(
    'solstone-tmux-observer'
    'solstone_tmux_observer'
    'com.solstone.tmux-observer'
)

while IFS= read -r -d '' tracked_path; do
    case "$tracked_path" in
        target/* | .venv/* | .git/*)
            continue
            ;;
    esac
    for legacy_token in "${legacy_identity_tokens[@]}"; do
        if [[ "$tracked_path" == *"$legacy_token"* ]]; then
            echo "$tracked_path: legacy native identity is forbidden in tracked paths" >&2
            failed=true
        fi
        tracked_file="$repo_root/$tracked_path"
        if [[ "$tracked_path" == "scripts/check-rust-guards.sh" ]]; then
            expected_declaration="    '$legacy_token'"
            declaration_count=0
            while IFS= read -r guard_line || [[ -n "$guard_line" ]]; do
                if [[ "$guard_line" == *"$legacy_token"* ]]; then
                    ((declaration_count += 1))
                    if [[ "$guard_line" != "$expected_declaration" ]]; then
                        echo "$tracked_path: retired identity exemption is broader than its exact declaration" >&2
                        failed=true
                    fi
                fi
            done <"$tracked_file"
            if ((declaration_count != 1)); then
                echo "$tracked_path: expected one exact retired identity declaration" >&2
                failed=true
            fi
        elif [[ -f "$tracked_file" ]] && identity_hits="$(rg -a -nF "$legacy_token" "$tracked_file")"; then
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

# Retired language runtimes and packaging must not return as tracked product
# surfaces. Missing paths are ignored here so the guard also validates a
# pre-commit working tree whose deletions are not staged yet.
while IFS= read -r -d '' tracked_path; do
    tracked_file="$repo_root/$tracked_path"
    [[ -e "$tracked_file" || -L "$tracked_file" ]] || continue
    case "$tracked_path" in
        *.py | pyproject.toml | setup.py | setup.cfg | requirements*.txt | \
            Pipfile | Pipfile.lock | poetry.lock | tox.ini | pytest.ini | \
            .python-version | uv.lock | MANIFEST.in)
            echo "$tracked_path: retired Python source or packaging configuration is forbidden" >&2
            failed=true
            ;;
        src/solstone_tmux | src/solstone_tmux/* | tests | tests/*)
            echo "$tracked_path: legacy Python source tree is forbidden" >&2
            failed=true
            ;;
    esac
done < <(git -C "$repo_root" ls-files -z)

if [[ -e "$repo_root/src/solstone_tmux" || -e "$repo_root/tests" ]]; then
    echo "$repo_root: legacy Python source directories are forbidden" >&2
    failed=true
fi

# This is a route ban, not a prose token ban. Operational build, test, install,
# workflow, packaging, and release surfaces may not invoke retired tooling.
route_files=("$repo_root/Makefile" "$repo_root/Cargo.toml")
while IFS= read -r -d '' route_path; do
    # This guard names the retired tools in order to enforce the route ban; it
    # is not itself a build, test, install, packaging, workflow, or release route.
    [[ "$route_path" == "scripts/check-rust-guards.sh" ]] && continue
    route_file="$repo_root/$route_path"
    [[ -f "$route_file" ]] && route_files+=("$route_file")
done < <(
    git -C "$repo_root" ls-files -z \
        'scripts/*' '.github/workflows/*' 'packaging/*' \
        'native/*/Cargo.toml'
)
for route_file in "${route_files[@]}"; do
    if route_hits="$(
        rg -n -i \
            '(^|[^[:alnum:]_])(ruff|pytest|uv|pipx|twine|python([0-9.]*)?|venv)([^[:alnum:]_]|$)' \
            "$route_file"
    )"; then
        printf '%s\n' "$route_hits" >&2
        echo "$route_file: retired Python tooling route is forbidden" >&2
        failed=true
    fi
done

# The only uv/pipx prose allowance is the bounded one-time retirement block.
# Each uninstall command appears once inside it and neither token may appear
# elsewhere in INSTALL.md.
install_file="$repo_root/INSTALL.md"
retirement_start='<!-- legacy-python-retirement:start -->'
retirement_end='<!-- legacy-python-retirement:end -->'
if [[ "$(rg -cF "$retirement_start" "$install_file" || true)" != "1" ||
    "$(rg -cF "$retirement_end" "$install_file" || true)" != "1" ]]; then
    echo "$install_file: expected one bounded legacy retirement block" >&2
    failed=true
else
    retirement_block="$(
        awk -v start="$retirement_start" -v end="$retirement_end" '
            $0 == start { inside = 1; next }
            $0 == end { inside = 0; next }
            inside { print }
        ' "$install_file"
    )"
    retirement_outside="$(
        awk -v start="$retirement_start" -v end="$retirement_end" '
            $0 == start { inside = 1; next }
            $0 == end { inside = 0; next }
            !inside { print }
        ' "$install_file"
    )"
    if [[ "$(rg -cF 'uv tool uninstall solstone-tmux' <<<"$retirement_block" || true)" != "1" ||
        "$(rg -cF 'pipx uninstall solstone-tmux' <<<"$retirement_block" || true)" != "1" ||
        "$(rg -io '(^|[^[:alnum:]_])(uv|pipx)([^[:alnum:]_]|$)' <<<"$retirement_block" | awk 'END { print NR }')" != "2" ]]; then
        echo "$install_file: retirement block must contain only the two approved tool references" >&2
        failed=true
    fi
    if rg -i '(^|[^[:alnum:]_])(uv|pipx)([^[:alnum:]_]|$)' \
        <<<"$retirement_outside" >/dev/null; then
        echo "$install_file: retired installer names are allowed only in the retirement block" >&2
        failed=true
    fi
fi

# One legacy migration fixture retains one old endpoint as inert input so tests
# can prove it is ignored. The exact count is enforced before that file is
# exempted; no other tracked surface may depend on the endpoint.
legacy_endpoint_fixture="native/solstone-tmux/tests/data/legacy/config-empty-stream.json"
if [[ "$(rg -cF 'localhost:5015' "$repo_root/$legacy_endpoint_fixture" || true)" != "1" ]]; then
    echo "$legacy_endpoint_fixture: expected exactly one inert localhost endpoint" >&2
    failed=true
fi
while IFS= read -r -d '' tracked_path; do
    case "$tracked_path" in
        "$legacy_endpoint_fixture" | scripts/check-rust-guards.sh)
            continue
            ;;
    esac
    tracked_file="$repo_root/$tracked_path"
    [[ -f "$tracked_file" ]] || continue
    if endpoint_hits="$(rg -a -nF 'localhost:5015' "$tracked_file")"; then
        printf '%s\n' "$endpoint_hits" >&2
        echo "$tracked_path: retired localhost endpoint dependency is forbidden" >&2
        failed=true
    fi
done < <(git -C "$repo_root" ls-files -z)

# SPL dependency authority, inheritance, lock resolution, and copied-tree
# policy are checked together from their repository sources of truth.
if ! "$repo_root/scripts/spl-pin.sh" "$repo_root"; then
    failed=true
fi

# Exactly one declared binary plus the exact platform identities prove there is
# no second service executable, unit, plist, or separate sync daemon.
while IFS= read -r -d '' tracked_path; do
    tracked_file="$repo_root/$tracked_path"
    [[ -e "$tracked_file" || -L "$tracked_file" ]] || continue
    case "$tracked_path" in
        *.service | *.service.in | *.plist | native/solstone-tmux/src/bin/*)
            echo "$tracked_path: standalone service artifact or second binary is forbidden" >&2
            failed=true
            ;;
    esac
done < <(git -C "$repo_root" ls-files -z)

if $failed; then
    exit 1
fi

echo "Rust repository guards passed."
