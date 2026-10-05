#!/usr/bin/env bash
set -euo pipefail
umask 077

if (($# != 2)); then
    echo "usage: verify-rpm-payload.sh <rpm> <source-executable>" >&2
    exit 2
fi
scratch_root=$(mktemp -d)
trap 'rm -rf -- "$scratch_root"' EXIT
rpm2cpio "$1" > "$scratch_root/payload.cpio"
cpio --quiet --list < "$scratch_root/payload.cpio" > "$scratch_root/members"
mapfile -t members < "$scratch_root/members"
if ((${#members[@]} != 1)) || [[ "${members[0]}" != './usr/bin/solstone-tmux' ]]; then
    echo "RPM payload must contain exactly the product executable" >&2
    exit 1
fi
cpio --quiet --extract --to-stdout './usr/bin/solstone-tmux' < "$scratch_root/payload.cpio" > "$scratch_root/solstone-tmux"
if ! cmp -- "$2" "$scratch_root/solstone-tmux"; then
    echo "RPM payload executable differs from the source executable" >&2
    exit 1
fi
