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
native_candidate_workflow="$repo_root/.github/workflows/native-candidate.yml"
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

workflow_runners=()
workflow_targets=()
workflow_target_lines=()
if [[ ! -f "$native_candidate_workflow" ]]; then
    echo "$native_candidate_workflow: native candidate workflow is missing" >&2
    failed=true
else
    while IFS='|' read -r runner target target_line; do
        workflow_runners+=("$runner")
        workflow_targets+=("$target")
        workflow_target_lines+=("$target_line")
    done < <(
        awk '
            /^          - runner: / {
                runner = $0
                sub(/^          - runner: /, "", runner)
                next
            }
            /^            rust_target: / {
                target = $0
                sub(/^            rust_target: /, "", target)
                if (runner != "") {
                    print runner "|" target "|" NR
                    runner = ""
                }
            }
        ' "$native_candidate_workflow"
    )

    expected_workflow_runners=("ubuntu-22.04" "ubuntu-22.04-arm")
    if ((${#workflow_runners[@]} != 2)); then
        echo "$native_candidate_workflow: expected exactly two native Linux runner/target pairs" >&2
        failed=true
    elif ((${#targets[@]} < 2)); then
        echo "$native_candidate_workflow: configured Rust target authority has fewer than two Linux targets" >&2
        failed=true
    else
        for index in 0 1; do
            if [[ "${workflow_runners[$index]}" != "${expected_workflow_runners[$index]}" ||
                "${workflow_targets[$index]}" != "${targets[$index]}" ]]; then
                echo "$native_candidate_workflow: Linux runner/target matrix is not the ordered rust-toolchain.toml replica" >&2
                failed=true
                break
            fi
        done
    fi

    permissions_count="$(rg -c '^permissions:$' "$native_candidate_workflow" || true)"
    contents_read_count="$(rg -c '^  contents: read$' "$native_candidate_workflow" || true)"
    if [[ "$permissions_count" != "1" || "$contents_read_count" != "1" ]]; then
        echo "$native_candidate_workflow: workflow permissions must be exactly contents: read" >&2
        failed=true
    fi
    if permission_writes="$(rg -n '^[[:space:]]*[A-Za-z0-9_-]+:[[:space:]]*write(-all)?[[:space:]]*$|^[[:space:]]*permissions:[[:space:]]*write-all[[:space:]]*$' "$native_candidate_workflow")"; then
        printf '%s\n' "$permission_writes" >&2
        echo "$native_candidate_workflow: workflow write permission is forbidden" >&2
        failed=true
    fi

    uses_count=0
    while IFS= read -r uses_line; do
        ((uses_count += 1))
        if [[ ! "$uses_line" =~ uses:[[:space:]]+[^[:space:]@]+@[0-9a-f]{40}[[:space:]]*$ ]]; then
            echo "$native_candidate_workflow: every action must be pinned by a full commit SHA" >&2
            failed=true
        fi
    done < <(rg '^[[:space:]]*uses:' "$native_candidate_workflow" || true)
    if ((uses_count == 0)); then
        echo "$native_candidate_workflow: expected at least one pinned action" >&2
        failed=true
    fi

    expected_fedora_image='  FEDORA_IMAGE: "registry.fedoraproject.org/fedora@sha256:e78cd1a688cd079c23864f289a89a49a3f4ad66d817864e325e1d058310ee95c"'
    fedora_count="$(awk -v expected="$expected_fedora_image" '$0 == expected { count += 1 } END { print count + 0 }' "$native_candidate_workflow")"
    if [[ "$fedora_count" != "1" ]] ||
        rg -qi 'FEDORA_IMAGE:.*(placeholder|replace|todo)' "$native_candidate_workflow"; then
        echo "$native_candidate_workflow: Fedora image must use the resolved non-placeholder digest" >&2
        failed=true
    fi

    if release_writes="$(rg -ni 'gh[[:space:]]+release|git[[:space:]]+(tag|push)|cargo[[:space:]]+publish|npm[[:space:]]+publish|twine[[:space:]]+upload|upload-release-asset|release-action' "$native_candidate_workflow")"; then
        printf '%s\n' "$release_writes" >&2
        echo "$native_candidate_workflow: candidate lanes may not tag, publish, or mutate releases" >&2
        failed=true
    fi
    # The workflow must scan the literal minisign canary as candidate data; that
    # one exact line is not signing access.
    minisign_canary_line='            MINISIGN_SECRET_CANARY_DO_NOT_SHIP'
    if [[ "$(rg -cFx "$minisign_canary_line" "$native_candidate_workflow" || true)" != "1" ]]; then
        echo "$native_candidate_workflow: expected one exact minisign canary scan entry" >&2
        failed=true
    fi
    signing_access="$(
        rg -ni 'minisign|codesign|notarytool|signing[_ -]?key|secrets\.' \
            "$native_candidate_workflow" |
            while IFS=: read -r signing_line signing_text; do
                [[ "$signing_text" == "$minisign_canary_line" ]] ||
                    printf '%s:%s\n' "$signing_line" "$signing_text"
            done
    )"
    if [[ -n "$signing_access" ]]; then
        printf '%s\n' "$signing_access" >&2
        echo "$native_candidate_workflow: candidate lanes may not access signing material" >&2
        failed=true
    fi
    if rg -n '^[[:space:]]+(push|pull_request|schedule):' "$native_candidate_workflow" >/dev/null ||
        [[ "$(rg -c '^  workflow_dispatch:$' "$native_candidate_workflow" || true)" != "1" ]]; then
        echo "$native_candidate_workflow: native candidates must be manually dispatched only" >&2
        failed=true
    fi
    package_version="$(
        awk '
            $0 == "[package]" { in_package = 1; next }
            in_package && /^\[/ { exit }
            in_package && /^version = "/ {
                value = $0
                sub(/^version = "/, "", value)
                sub(/"$/, "", value)
                print value
                exit
            }
        ' "$native_root/Cargo.toml"
    )"
    if [[ -z "$package_version" ]] ||
        rg -nF "$package_version" "$native_candidate_workflow" >/dev/null; then
        echo "$native_candidate_workflow: package version must be derived from Cargo metadata" >&2
        failed=true
    fi
fi

drift_files=(
    "$repo_root/Makefile"
    "$repo_root/AGENTS.md"
)
while IFS= read -r -d '' script; do
    drift_files+=("$script")
done < <(find "$repo_root/scripts" -maxdepth 1 -type f -print0)
while IFS= read -r -d '' workflow_file; do
    drift_files+=("$workflow_file")
done < <(find "$repo_root/.github" -type f -print0 2>/dev/null || true)

for target in "${targets[@]}"; do
    for drift_file in "${drift_files[@]}"; do
        while IFS=: read -r drift_line_number drift_text; do
            [[ -n "$drift_line_number" ]] || continue
            matrix_replica=false
            if [[ "$drift_file" == "$native_candidate_workflow" ]]; then
                for matrix_line in "${workflow_target_lines[@]}"; do
                    if [[ "$drift_line_number" == "$matrix_line" ]]; then
                        matrix_replica=true
                        break
                    fi
                done
            fi
            if ! $matrix_replica; then
                echo "$drift_file:$drift_line_number:$drift_text" >&2
                echo "target drift: target literals belong only in rust-toolchain.toml or the verified workflow matrix" >&2
                failed=true
            fi
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
        "$(rg -io '(^|[^[:alnum:]_])(uv|pipx)([^[:alnum:]_]|$)' <<<"$retirement_block" | wc -l)" != "2" ]]; then
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

# SPL is supplied only by the two revision-pinned Git dependencies. No copied
# core or transport tree may live in this repository.
expected_spl_revision='742bc9dc789c5a75658844849a04d75033aeb6e3'
expected_spl_source="https://github.com/solpbc/spl-rust"
for spl_package in spl-core spl-transport; do
    expected_dependency="$spl_package = { git = \"$expected_spl_source\", rev = \"$expected_spl_revision\" }"
    if [[ "$(rg -cFx "$expected_dependency" "$native_root/Cargo.toml" || true)" != "1" ]]; then
        echo "$native_root/Cargo.toml: $spl_package must use the exact pinned Git revision" >&2
        failed=true
    fi
done
expected_lock_source="source = \"git+$expected_spl_source?rev=$expected_spl_revision#$expected_spl_revision\""
if [[ "$(rg -cFx "$expected_lock_source" "$repo_root/Cargo.lock" || true)" != "2" ]]; then
    echo "$repo_root/Cargo.lock: SPL packages must resolve only from the exact pinned Git revision" >&2
    failed=true
fi
while IFS= read -r -d '' tracked_path; do
    if [[ "$tracked_path" =~ (^|/)(spl-core|spl-transport|spl_core|spl_transport)(/|$) ]]; then
        echo "$tracked_path: copied in-tree SPL implementation is forbidden" >&2
        failed=true
    fi
done < <(git -C "$repo_root" ls-files -z)

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
