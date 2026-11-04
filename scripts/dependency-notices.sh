#!/usr/bin/env bash
# Generates RUST_DEPENDENCY_NOTICES.txt from the locked dependency graph, or checks
# that the committed file is exactly what generation produces now.
#
# The population is every external crate in the normal and build closure of
# solstone-tmux, for every target in rust-toolchain.toml, as Cargo resolves features
# for that build. Each crate contributes the license files in its own source
# directory, the license files at the root of its repository when it comes from
# git, and any committed files under licensing/overrides/<name>/<version>/, which
# carry license texts a crate's own source does not. Every crate needs at least
# one. Identical texts appear once, listing every crate that uses them.
set -euo pipefail

usage() {
    echo "usage: dependency-notices.sh generate|check" >&2
    exit 2
}

(($# == 1)) || usage
mode="$1"
case "$mode" in
    generate | check) ;;
    *) usage ;;
esac

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
notices_path="$repo_root/RUST_DEPENDENCY_NOTICES.txt"
overrides_root="$repo_root/licensing/overrides"
root_package="solstone-tmux"
export LC_ALL=C

work_root="$(mktemp -d)"
trap 'rm -rf -- "$work_root"' EXIT

cargo metadata --locked --offline --format-version 1 \
    --manifest-path "$repo_root/Cargo.toml" >"$work_root/metadata.json"
jq -r '
    .packages[]
    | select(.source != null)
    | [.name, .version, (.manifest_path | sub("/Cargo\\.toml$"; "")), .source]
    | @tsv
' "$work_root/metadata.json" | sort -u >"$work_root/external.tsv"

mapfile -t targets < <("$repo_root/scripts/rust-targets.sh")
((${#targets[@]} > 0)) || {
    echo "no Rust targets are configured" >&2
    exit 1
}
: >"$work_root/population.tsv"
for target in "${targets[@]}"; do
    cargo tree --locked --offline \
        --manifest-path "$repo_root/Cargo.toml" \
        -p "$root_package" \
        -e normal,build \
        --target "$target" \
        --prefix none \
        --format '{p}' >"$work_root/tree.txt"
    # A local path crate prints "(/path)"; it is this repository's own code.
    awk '$3 ~ /^\(\// { next } { sub(/^v/, "", $2); print $1 "\t" $2 }' \
        "$work_root/tree.txt" >>"$work_root/population.tsv"
done
sort -u -o "$work_root/population.tsv" "$work_root/population.tsv"
[[ -s "$work_root/population.tsv" ]] || {
    echo "the dependency closure of $root_package has no external crates" >&2
    exit 1
}

# Join each crate to its source directory. A crate that resolves to zero or two
# directories is an error, never a silent skip.
awk -F '\t' '
    NR == FNR { count[$1 "\t" $2]++; dir[$1 "\t" $2] = $3 "\t" $4; next }
    {
        key = $1 "\t" $2
        if (count[key] != 1) {
            printf "cannot resolve the source directory of %s %s\n", $1, $2 > "/dev/stderr"
            failed = 1
            next
        }
        print key "\t" dir[key]
    }
    END { exit failed }
' "$work_root/external.tsv" "$work_root/population.tsv" >"$work_root/crates.tsv"

is_license_file() {
    local lower
    lower="$(tr '[:upper:]' '[:lower:]' <<<"$(basename -- "$1")")"
    case "$lower" in
        unlicense*) return 1 ;;
        license* | licence* | copying* | notice* | copyright) return 0 ;;
        *) return 1 ;;
    esac
}

# add_member <name> <version> <label> <file>
add_member() {
    local digest
    digest="$(git hash-object --no-filters -- "$4")"
    cp -- "$4" "$work_root/bodies/$digest"
    printf '%s\t%s\t%s\t%s\n' "$1" "$2" "$3" "$digest" >>"$work_root/members.tsv"
}

mkdir -- "$work_root/bodies"
: >"$work_root/members.tsv"
missing=0
while IFS=$'\t' read -r name version crate_dir source; do
    found=0
    while IFS= read -r -d '' file; do
        is_license_file "$file" || continue
        add_member "$name" "$version" "${file#"$crate_dir"/}" "$file"
        found=1
    done < <(find "$crate_dir" -type f -print0)
    if [[ "$source" == git+* ]]; then
        # Cargo marks the root of each git checkout with .cargo-ok.
        repository_root="$crate_dir"
        while [[ "$repository_root" != / && ! -e "$repository_root/.cargo-ok" ]]; do
            repository_root="$(dirname -- "$repository_root")"
        done
        if [[ "$repository_root" != / && "$repository_root" != "$crate_dir" ]]; then
            while IFS= read -r -d '' file; do
                is_license_file "$file" || continue
                add_member "$name" "$version" "$(basename -- "$file") (repository root)" "$file"
                found=1
            done < <(find "$repository_root" -maxdepth 1 -type f -print0)
        fi
    fi
    override_dir="$overrides_root/$name/$version"
    if [[ -d "$override_dir" ]]; then
        while IFS= read -r -d '' file; do
            add_member "$name" "$version" "$(basename -- "$file")" "$file"
            found=1
        done < <(find "$override_dir" -type f -print0)
    fi
    if ((found == 0)); then
        echo "no license file for $name $version; add its license text under licensing/overrides/$name/$version/" >&2
        missing=1
    fi
done <"$work_root/crates.tsv"
((missing == 0)) || exit 1
sort -t $'\t' -k1,1 -k2,2 -k3,3 -o "$work_root/members.tsv" "$work_root/members.tsv"

rule="================================================================================"
{
    printf '%s\n' "Rust dependency notices"
    printf '\n'
    printf '%s\n' "this file reproduces the license texts of the crates solstone-tmux is built"
    printf '%s\n' "from. solstone-tmux's own code is AGPL-3.0-only; see LICENSE."
    # Each distinct text appears once, in the order its first crate sorts, under
    # the list of every crate file that carries it.
    awk -F '\t' '!seen[$4]++ { print $4 }' "$work_root/members.tsv" |
        while IFS= read -r digest; do
            printf '\n%s\n' "$rule"
            awk -F '\t' -v digest="$digest" '$4 == digest { print $1 " " $2 ": " $3 }' \
                "$work_root/members.tsv"
            printf '%s\n\n' "$rule"
            cat -- "$work_root/bodies/$digest"
            if [[ -n "$(tail -c 1 -- "$work_root/bodies/$digest")" ]]; then
                printf '\n'
            fi
        done
} >"$work_root/notices.txt"

if [[ "$mode" == generate ]]; then
    cp -- "$work_root/notices.txt" "$notices_path"
    echo "wrote $(wc -l <"$work_root/crates.tsv" | tr -d ' ') crates to RUST_DEPENDENCY_NOTICES.txt"
    exit 0
fi

if [[ ! -f "$notices_path" ]] || ! cmp -s -- "$work_root/notices.txt" "$notices_path"; then
    echo "RUST_DEPENDENCY_NOTICES.txt does not match the locked dependency graph." >&2
    echo "Run 'make notices' and commit the result." >&2
    exit 1
fi
echo "RUST_DEPENDENCY_NOTICES.txt matches the locked dependency graph ($(wc -l <"$work_root/crates.tsv" | tr -d ' ') crates)."
