#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) 2026 sol pbc

set -euo pipefail

usage() {
    cat <<'EOF'
Usage: scripts/release.sh [--test]

Options:
  --test      Publish to TestPyPI.
  -h, --help  Show this help.
EOF
}

TARGET="pypi"
TOKEN_VAR="PYPI_TOKEN"
REPOSITORY_ARGS=()

while [[ $# -gt 0 ]]; do
    case "$1" in
        --test)
            TARGET="testpypi"
            TOKEN_VAR="TESTPYPI_TOKEN"
            REPOSITORY_ARGS=(--repository-url https://test.pypi.org/legacy/)
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "unknown argument: $1" >&2
            usage >&2
            exit 2
            ;;
    esac
done

if [[ -z "${!TOKEN_VAR:-}" ]]; then
    echo "set \$${TOKEN_VAR} before re-running" >&2
    exit 1
fi
TOKEN="${!TOKEN_VAR}"

if ! git diff --quiet HEAD; then
    echo "working tree dirty (modified tracked files); commit before releasing" >&2
    exit 1
fi
if [[ -n "$(git status --porcelain)" ]]; then
    echo "working tree dirty (untracked or staged changes); commit/ignore before releasing" >&2
    exit 1
fi

VERSION="$(awk -F'"' '/^version[[:space:]]*=/ {print $2; exit}' pyproject.toml)"
[[ -n "$VERSION" ]] || { echo "could not parse version from pyproject.toml" >&2; exit 1; }

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
EXTRACTOR="$SCRIPT_DIR/extract_changelog.sh"
if [[ ! -x "$EXTRACTOR" ]]; then
    echo "extractor missing or not executable: $EXTRACTOR" >&2
    exit 1
fi

echo "==> [1/4] cleaning + building sdist + wheel"
rm -rf dist/
uv build

echo "==> [2/4] twine check"
uvx twine check dist/*

echo "==> [3/4] uploading to $TARGET"
TWINE_USERNAME=__token__ TWINE_PASSWORD="$TOKEN" \
    uvx twine upload "${REPOSITORY_ARGS[@]}" dist/*

if [[ "$TARGET" == "testpypi" ]]; then
    echo "TestPyPI upload complete — skipping tag + GitHub Release"
    exit 0
fi

echo "==> [4/4] tagging + GitHub Release"

# Order matters: tag -> push -> extract notes -> gh release create.
# If git push fails we abort BEFORE creating any release.
# If extraction fails we also abort BEFORE creating any release.
# If gh release create fails after the PyPI upload, print the manual recovery.
git tag -a "v$VERSION" -m "solstone-tmux $VERSION"

if ! git push origin "v$VERSION"; then
    echo "git push origin v$VERSION failed (exit $?); aborting before GitHub Release" >&2
    exit 1
fi

NOTES_FILE="$(mktemp -t solstone-tmux-notes.XXXXXX)"
trap 'rm -f "$NOTES_FILE"' EXIT

if ! "$EXTRACTOR" "$VERSION" CHANGELOG.md > "$NOTES_FILE"; then
    echo "could not extract CHANGELOG block for $VERSION; aborting before GitHub Release" >&2
    exit 1
fi

SDIST="dist/solstone_tmux-${VERSION}.tar.gz"
WHEEL="dist/solstone_tmux-${VERSION}-py3-none-any.whl"

if ! gh release create "v$VERSION" "$SDIST" "$WHEEL" \
    --title "solstone-tmux $VERSION" \
    --notes-file "$NOTES_FILE"; then
    echo "" >&2
    echo "PyPI version $VERSION is published and immutable; GitHub Release failed — re-run manually with: gh release create v$VERSION dist/solstone_tmux-${VERSION}.tar.gz dist/solstone_tmux-${VERSION}-py3-none-any.whl --title \"solstone-tmux $VERSION\" --notes-file $NOTES_FILE" >&2
    # Keep the notes file around for the manual re-run — clear the trap.
    trap - EXIT
    exit 1
fi

echo "published solstone-tmux $VERSION to $TARGET"
