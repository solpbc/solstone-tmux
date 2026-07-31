#!/usr/bin/env bash
set -euo pipefail

if (($# > 1)); then
    echo "scripts/spl-pin.sh: expected zero arguments or one repository root" >&2
    exit 1
fi

if (($# == 1)); then
    if ! repo_root="$(cd "$1" 2>/dev/null && pwd)"; then
        echo "$1: spl-core and spl-transport pin check requires an existing repository root" >&2
        exit 1
    fi
else
    script_dir="${BASH_SOURCE[0]%/*}"
    [[ "$script_dir" != "${BASH_SOURCE[0]}" ]] || script_dir=.
    repo_root="$(cd "$script_dir/.." && pwd)"
fi

workspace_file="$repo_root/Cargo.toml"
native_file="$repo_root/native/solstone-tmux/Cargo.toml"
lock_file="$repo_root/Cargo.lock"
deny_file="$repo_root/deny.toml"
spl_packages=(spl-core spl-transport)
failed=false

fail() {
    printf '%s\n' "$1" >&2
    failed=true
}

deny_valid=false
approved_source=
if [[ ! -f "$deny_file" ]]; then
    fail "$deny_file: spl-core and spl-transport require exactly one approved Git source in [sources].allow-git; set allow-git to an array containing one quoted Git URL"
else
    sources_count=0
    allow_git_count=0
    in_sources=false
    deny_malformed=false
    parsed_source=

    while IFS= read -r line || [[ -n "$line" ]]; do
        if [[ "$line" =~ ^[[:space:]]*\[sources\][[:space:]]*$ ]]; then
            ((sources_count += 1))
            in_sources=true
            continue
        fi
        if [[ "$line" =~ ^[[:space:]]*\[[^]]+\][[:space:]]*$ ]]; then
            in_sources=false
            continue
        fi
        if $in_sources && [[ "$line" =~ ^[[:space:]]*allow-git[[:space:]]*= ]]; then
            ((allow_git_count += 1))
            if [[ "$line" =~ ^[[:space:]]*allow-git[[:space:]]*=[[:space:]]*\[[[:space:]]*\"([^\"]+)\"[[:space:]]*,?[[:space:]]*\][[:space:]]*$ ]]; then
                parsed_source="${BASH_REMATCH[1]}"
            else
                deny_malformed=true
            fi
        fi
    done < "$deny_file"

    if ((sources_count == 1 && allow_git_count == 1)) && ! $deny_malformed && [[ -n "$parsed_source" ]]; then
        approved_source="$parsed_source"
        deny_valid=true
    else
        fail "$deny_file: spl-core and spl-transport require exactly one approved Git source in [sources].allow-git; set allow-git to an array containing one quoted Git URL"
    fi
fi

if $deny_valid; then
    approved_source_hint="$approved_source"
else
    approved_source_hint='<approved Git source>'
fi

declare -A workspace_count=([spl-core]=0 [spl-transport]=0)
declare -A workspace_shape=([spl-core]=false [spl-transport]=false)
declare -A workspace_git_count=([spl-core]=0 [spl-transport]=0)
declare -A workspace_git=([spl-core]="" [spl-transport]="")
declare -A workspace_selector_count=([spl-core]=0 [spl-transport]=0)
declare -A workspace_selector=([spl-core]="" [spl-transport]="")
declare -A workspace_selector_value=([spl-core]="" [spl-transport]="")
declare -A workspace_package_valid=([spl-core]=false [spl-transport]=false)
workspace_authority_valid=false

if [[ ! -f "$workspace_file" ]]; then
    fail "$workspace_file: spl-core and spl-transport pin check requires this file; restore Cargo.toml"
else
    in_workspace_dependencies=false
    while IFS= read -r line || [[ -n "$line" ]]; do
        if [[ "$line" =~ ^[[:space:]]*\[workspace\.dependencies\][[:space:]]*$ ]]; then
            in_workspace_dependencies=true
            continue
        fi
        if [[ "$line" =~ ^[[:space:]]*\[[^]]+\][[:space:]]*$ ]]; then
            in_workspace_dependencies=false
            continue
        fi
        $in_workspace_dependencies || continue
        if [[ ! "$line" =~ ^[[:space:]]*(spl-core|spl-transport)[[:space:]]*=(.*)$ ]]; then
            continue
        fi

        package="${BASH_REMATCH[1]}"
        entry="${BASH_REMATCH[2]}"
        ((workspace_count["$package"] += 1))
        entry_shape=true
        entry_git_count=0
        entry_git=
        entry_selector_count=0
        entry_selector=
        entry_selector_value=

        if [[ "$entry" =~ ^[[:space:]]*\"([^\"]*)\"[[:space:]]*$ ]]; then
            entry_selector_count=1
            entry_selector=version
            entry_selector_value="${BASH_REMATCH[1]}"
        elif [[ "$entry" =~ ^[[:space:]]*\{(.*)\}[[:space:]]*$ ]]; then
            entry_body="${BASH_REMATCH[1]}"
            IFS=',' read -r -a entry_fields <<< "$entry_body"
            if ((${#entry_fields[@]} == 0)); then
                entry_shape=false
            fi
            for field in "${entry_fields[@]}"; do
                if [[ ! "$field" =~ ^[[:space:]]*([[:alnum:]_-]+)[[:space:]]*=[[:space:]]*\"([^\"]*)\"[[:space:]]*$ ]]; then
                    entry_shape=false
                    continue
                fi
                key="${BASH_REMATCH[1]}"
                value="${BASH_REMATCH[2]}"
                case "$key" in
                    git)
                        ((entry_git_count += 1))
                        entry_git="$value"
                        ;;
                    rev | branch | tag | version | path)
                        ((entry_selector_count += 1))
                        entry_selector="$key"
                        entry_selector_value="$value"
                        ;;
                    *)
                        entry_shape=false
                        ;;
                esac
            done
            if ((entry_git_count > 1)); then
                entry_shape=false
            fi
        else
            entry_shape=false
        fi

        workspace_shape["$package"]="$entry_shape"
        workspace_git_count["$package"]="$entry_git_count"
        workspace_git["$package"]="$entry_git"
        workspace_selector_count["$package"]="$entry_selector_count"
        workspace_selector["$package"]="$entry_selector"
        workspace_selector_value["$package"]="$entry_selector_value"
    done < "$workspace_file"

    for package in "${spl_packages[@]}"; do
        package_valid=true
        if ((workspace_count["$package"] == 0)); then
            fail "$workspace_file: $package is missing from [workspace.dependencies]; add $package = { git = \"$approved_source_hint\", rev = \"<40-character lowercase hex>\" }"
            package_valid=false
        elif ((workspace_count["$package"] > 1)); then
            fail "$workspace_file: $package appears more than once in [workspace.dependencies]; keep exactly one Git revision declaration"
            package_valid=false
        elif [[ "${workspace_shape[$package]}" != true ]]; then
            fail "$workspace_file: $package must declare only git and rev in [workspace.dependencies]; replace it with $package = { git = \"$approved_source_hint\", rev = \"<40-character lowercase hex>\" }"
            package_valid=false
        elif ((workspace_selector_count["$package"] != 1)) || [[ "${workspace_selector[$package]}" != rev ]]; then
            fail "$workspace_file: $package must select a revision, not a branch, tag, version, or path; pin it with rev = \"<40-character lowercase hex>\" in [workspace.dependencies]"
            package_valid=false
        elif $deny_valid && { ((workspace_git_count["$package"] != 1)) || [[ "${workspace_git[$package]}" != "$approved_source" ]]; }; then
            fail "$workspace_file: $package must use the Git source approved by deny.toml; set git = \"$approved_source\" in [workspace.dependencies]"
            package_valid=false
        elif [[ ! "${workspace_selector_value[$package]}" =~ ^[0-9a-f]{40}$ ]]; then
            fail "$workspace_file: $package revision must be exactly 40 lowercase hexadecimal characters; set rev = \"<40-character lowercase hex>\" in [workspace.dependencies]"
            package_valid=false
        elif ! $deny_valid || ((workspace_git_count["$package"] != 1)); then
            package_valid=false
        fi
        workspace_package_valid["$package"]="$package_valid"
    done

    if [[ "${workspace_package_valid[spl-core]}" == true && "${workspace_package_valid[spl-transport]}" == true ]]; then
        if [[ "${workspace_selector_value[spl-core]}" != "${workspace_selector_value[spl-transport]}" ]]; then
            fail "$workspace_file: spl-core and spl-transport must use the same revision in [workspace.dependencies]; set both rev values to one 40-character lowercase hexadecimal revision"
        else
            workspace_authority_valid=true
        fi
    fi
fi

declare -A native_count=([spl-core]=0 [spl-transport]=0)
declare -A native_exact=([spl-core]=false [spl-transport]=false)
if [[ ! -f "$native_file" ]]; then
    fail "$native_file: spl-core and spl-transport pin check requires this file; restore native/solstone-tmux/Cargo.toml"
else
    in_native_dependencies=false
    while IFS= read -r line || [[ -n "$line" ]]; do
        if [[ "$line" =~ ^[[:space:]]*\[dependencies\][[:space:]]*$ ]]; then
            in_native_dependencies=true
            continue
        fi
        if [[ "$line" =~ ^[[:space:]]*\[[^]]+\][[:space:]]*$ ]]; then
            in_native_dependencies=false
            continue
        fi
        $in_native_dependencies || continue
        if [[ "$line" =~ ^[[:space:]]*(spl-core|spl-transport)([[:space:]]*=|\.) ]]; then
            package="${BASH_REMATCH[1]}"
            ((native_count["$package"] += 1))
            if [[ "$line" == "$package = { workspace = true }" ]]; then
                native_exact["$package"]=true
            fi
        fi
    done < "$native_file"

    for package in "${spl_packages[@]}"; do
        if ((native_count["$package"] == 0)); then
            fail "$native_file: $package is missing from [dependencies]; add $package = { workspace = true }"
        elif ((native_count["$package"] > 1)); then
            fail "$native_file: $package appears more than once in [dependencies]; keep exactly one $package = { workspace = true } entry"
        elif [[ "${native_exact[$package]}" != true ]]; then
            fail "$native_file: $package must inherit the workspace declaration; replace the entry with $package = { workspace = true }"
        fi
    done
fi

lock_present=true
if [[ ! -f "$lock_file" ]]; then
    fail "$lock_file: spl-core and spl-transport pin check requires this file; restore Cargo.lock"
    lock_present=false
fi

if $lock_present && $workspace_authority_valid; then
    declare -A lock_block_count=([spl-core]=0 [spl-transport]=0)
    declare -A lock_source_count=([spl-core]=0 [spl-transport]=0)
    declare -A lock_source=([spl-core]="" [spl-transport]="")
    lock_in_block=false
    lock_name=
    current_source_count=0
    current_source=

    finish_lock_block() {
        if ! $lock_in_block; then
            return 0
        fi
        case "$lock_name" in
            spl-core | spl-transport)
                ((lock_block_count["$lock_name"] += 1))
                if ((lock_block_count["$lock_name"] == 1)); then
                    lock_source_count["$lock_name"]="$current_source_count"
                    lock_source["$lock_name"]="$current_source"
                fi
                ;;
        esac
    }

    while IFS= read -r line || [[ -n "$line" ]]; do
        if [[ "$line" =~ ^[[:space:]]*\[\[package\]\][[:space:]]*$ ]]; then
            finish_lock_block
            lock_in_block=true
            lock_name=
            current_source_count=0
            current_source=
            continue
        fi
        if [[ "$line" =~ ^[[:space:]]*\[ ]]; then
            finish_lock_block
            lock_in_block=false
            continue
        fi
        $lock_in_block || continue
        if [[ "$line" =~ ^[[:space:]]*name[[:space:]]*=[[:space:]]*\"([^\"]+)\"[[:space:]]*$ ]]; then
            lock_name="${BASH_REMATCH[1]}"
        elif [[ "$line" =~ ^[[:space:]]*source[[:space:]]*=[[:space:]]*\"([^\"]+)\"[[:space:]]*$ ]]; then
            ((current_source_count += 1))
            current_source="${BASH_REMATCH[1]}"
        fi
    done < "$lock_file"
    finish_lock_block

    common_revision="${workspace_selector_value[spl-core]}"
    expected_lock_source="git+$approved_source?rev=$common_revision#$common_revision"
    approved_lock_prefix="git+$approved_source"
    for package in "${spl_packages[@]}"; do
        if ((lock_block_count["$package"] == 0)); then
            fail "$lock_file: $package is missing from Cargo.lock; regenerate the lockfile from the workspace declaration"
        elif ((lock_block_count["$package"] > 1)); then
            fail "$lock_file: $package has multiple package resolutions; remove alternate resolutions by regenerating the lockfile from the workspace declaration"
        elif ((lock_source_count["$package"] == 0)); then
            fail "$lock_file: $package resolves without the workspace Git source; remove local routing and regenerate the lockfile from the workspace declaration"
        elif ((lock_source_count["$package"] > 1)); then
            fail "$lock_file: $package has multiple source declarations in its package block; regenerate the lockfile from the workspace declaration"
        elif [[ "${lock_source[$package]}" == "$expected_lock_source" ]]; then
            :
        elif [[ "${lock_source[$package]}" == "$approved_lock_prefix"* ]]; then
            fail "$lock_file: $package resolves at a revision other than the workspace declaration; regenerate the lockfile at the declared revision"
        else
            fail "$lock_file: $package resolves from a source other than the workspace declaration; regenerate the lockfile from the declared Git source"
        fi
    done
fi

scan_patch_tables() {
    local manifest="$1"
    local in_patch=false
    local line package
    declare -A reported=([spl-core]=false [spl-transport]=false)

    [[ -f "$manifest" ]] || return
    while IFS= read -r line || [[ -n "$line" ]]; do
        if [[ "$line" =~ ^[[:space:]]*\[patch(\.|[[:space:]]*\]) ]]; then
            in_patch=true
            if [[ "$line" =~ [.]\"?(spl-core|spl-transport)\"?[[:space:]]*\][[:space:]]*$ ]]; then
                package="${BASH_REMATCH[1]}"
                if [[ "${reported[$package]}" != true ]]; then
                    fail "$manifest: $package must not be routed through [patch]; remove the $package patch entry and use its [workspace.dependencies] declaration"
                    reported["$package"]=true
                fi
            fi
            continue
        fi
        if [[ "$line" =~ ^[[:space:]]*\[[^]]+\][[:space:]]*$ ]]; then
            in_patch=false
            continue
        fi
        $in_patch || continue
        if [[ "$line" =~ ^[[:space:]]*\"(spl-core|spl-transport)\"[[:space:]]*= ]] ||
            [[ "$line" =~ ^[[:space:]]*(spl-core|spl-transport)[[:space:]]*= ]]; then
            package="${BASH_REMATCH[1]}"
            if [[ "${reported[$package]}" != true ]]; then
                fail "$manifest: $package must not be routed through [patch]; remove the $package patch entry and use its [workspace.dependencies] declaration"
                reported["$package"]=true
            fi
        fi
    done < "$manifest"
}

scan_patch_tables "$workspace_file"
scan_patch_tables "$native_file"

while IFS= read -r -d '' tracked_path; do
    if [[ "$tracked_path" =~ (^|/)(spl-core|spl-transport|spl_core|spl_transport)(/|$) ]]; then
        echo "$tracked_path: copied in-tree SPL implementation is forbidden" >&2
        failed=true
    fi
done < <(git -C "$repo_root" ls-files -z)

if $failed; then
    exit 1
fi
