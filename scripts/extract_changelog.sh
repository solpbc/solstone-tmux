#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) 2026 sol pbc

set -euo pipefail

usage() {
    cat <<'EOF'
Usage: scripts/extract_changelog.sh VERSION CHANGELOG_PATH
EOF
}

if [[ $# -eq 1 && ( "$1" == "--help" || "$1" == "-h" ) ]]; then
    usage
    exit 0
fi

if [[ $# -ne 2 ]]; then
    usage >&2
    exit 2
fi

VERSION="$1"
CHANGELOG="$2"
[[ -r "$CHANGELOG" ]] || { echo "changelog not readable: $CHANGELOG" >&2; exit 1; }

# Use a flag-based awk scanner, NOT a sed range.
# A sed range like /^## \[VERSION\]/,/^## \[/ self-closes on the header itself
# (the start-pattern line also matches the end-pattern), so it prints only that
# single line. Verified broken upstream on the 0.3.4 cut; this awk form is the fix.
# Use awk -v rather than shell-interpolating VERSION into the awk program.
OUTPUT="$(awk -v v="$VERSION" '
    $0 ~ "^## \\[" v "\\]" { seen = 1 }
    seen && /^## \[/ && $0 !~ "^## \\[" v "\\]" { exit }
    seen { print }
' "$CHANGELOG")"

if [[ -z "$OUTPUT" ]]; then
    echo "no CHANGELOG block found for version $VERSION in $CHANGELOG" >&2
    exit 1
fi

printf '%s\n' "$OUTPUT"
