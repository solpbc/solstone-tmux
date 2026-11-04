#!/usr/bin/env bash
set -euo pipefail
umask 077

if (($# != 2)); then
    echo "usage: verify-rpm-payload.sh <rpm> <source-executable>" >&2
    exit 2
fi
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
doc_directory="./usr/share/doc/solstone-tmux"
scratch_root=$(mktemp -d)
trap 'rm -rf -- "$scratch_root"' EXIT
rpm2cpio "$1" > "$scratch_root/payload.cpio"
cpio --quiet --list < "$scratch_root/payload.cpio" > "$scratch_root/members"
LC_ALL=C sort "$scratch_root/members" > "$scratch_root/sorted-members"
printf '%s\n' \
    './usr/bin/solstone-tmux' \
    "$doc_directory" \
    "$doc_directory/LICENSE" \
    "$doc_directory/RUST_DEPENDENCY_NOTICES.txt" |
    LC_ALL=C sort > "$scratch_root/expected-members"
if ! cmp -s -- "$scratch_root/expected-members" "$scratch_root/sorted-members"; then
    echo "RPM payload must contain exactly the product executable, LICENSE and RUST_DEPENDENCY_NOTICES.txt" >&2
    exit 1
fi
verify_member() {
    local member="$1" source="$2"
    cpio --quiet --extract --to-stdout "$member" < "$scratch_root/payload.cpio" > "$scratch_root/member"
    if ! cmp -- "$source" "$scratch_root/member"; then
        echo "RPM payload $member differs from $source" >&2
        exit 1
    fi
}
verify_member './usr/bin/solstone-tmux' "$2"
verify_member "$doc_directory/LICENSE" "$repo_root/LICENSE"
verify_member "$doc_directory/RUST_DEPENDENCY_NOTICES.txt" "$repo_root/RUST_DEPENDENCY_NOTICES.txt"
