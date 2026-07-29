# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) 2026 sol pbc

from pathlib import Path
import os
import subprocess
import textwrap


REPO_ROOT = Path(__file__).resolve().parent.parent
EXTRACTOR = REPO_ROOT / "scripts" / "extract_changelog.sh"
RELEASE_SCRIPT = REPO_ROOT / "scripts" / "release.sh"


def _run(version, changelog_path):
    return subprocess.run(
        ["bash", str(EXTRACTOR), version, str(changelog_path)],
        capture_output=True,
        text=True,
    )


def test_extract_two_block_returns_only_target_block(tmp_path):
    changelog = tmp_path / "CHANGELOG.md"
    changelog.write_text(
        textwrap.dedent(
            """\
            # Changelog

            ## [0.2.0] - 2026-05-19

            ### Added
            - newer owner-facing note.

            ## [0.1.0] - 2026-05-18

            ### Added
            - older owner-facing note.
            """
        ),
        encoding="utf-8",
    )

    result = _run("0.2.0", changelog)

    assert result.returncode == 0
    assert result.stdout.startswith("## [0.2.0]")
    assert "## [0.1.0]" not in result.stdout
    assert "- newer owner-facing note." in result.stdout


def test_extract_one_block_bootstrap(tmp_path):
    changelog = tmp_path / "CHANGELOG.md"
    changelog.write_text(
        textwrap.dedent(
            """\
            # Changelog

            ## [0.1.0] - 2026-05-19

            ### Added
            - first owner-facing note.
            - second owner-facing note.
            """
        ),
        encoding="utf-8",
    )

    result = _run("0.1.0", changelog)

    assert result.returncode == 0
    assert result.stdout.startswith("## [0.1.0]")
    assert "- first owner-facing note." in result.stdout
    assert "- second owner-facing note." in result.stdout


def test_extract_missing_version_errors(tmp_path):
    changelog = tmp_path / "CHANGELOG.md"
    changelog.write_text(
        textwrap.dedent(
            """\
            # Changelog

            ## [0.1.0] - 2026-05-19

            ### Added
            - first owner-facing note.
            """
        ),
        encoding="utf-8",
    )

    result = _run("9.9.9", changelog)

    assert result.returncode != 0
    assert "9.9.9" in result.stderr


def test_release_help_prints_usage():
    result = subprocess.run(
        ["bash", str(RELEASE_SCRIPT), "--help"],
        capture_output=True,
        text=True,
    )

    assert result.returncode == 0
    assert "Usage: scripts/release.sh [--test]" in result.stdout


def test_production_path_omits_repository_url(tmp_path, monkeypatch):
    bin_dir = tmp_path / "bin"
    bin_dir.mkdir()
    project_dir = tmp_path / "project"
    project_dir.mkdir()
    (project_dir / "pyproject.toml").write_text(
        '[project]\nversion = "0.1.0"\n',
        encoding="utf-8",
    )
    (project_dir / "CHANGELOG.md").write_text(
        "# Changelog\n\n## [0.1.0] - 2026-05-19\n\n- release note.\n",
        encoding="utf-8",
    )

    shims = {
        "uv": """\
            #!/usr/bin/env bash
            printf '%s\n' "$*" >> "$(dirname "$0")/uv.log"
            if [[ "${1:-}" == "build" ]]; then
                mkdir -p dist
                touch dist/solstone_tmux-0.1.0.tar.gz
                touch dist/solstone_tmux-0.1.0-py3-none-any.whl
            fi
            exit 0
        """,
        "uvx": """\
            #!/usr/bin/env bash
            printf '%s\n' "$*" >> "$(dirname "$0")/uvx.log"
            exit 0
        """,
        "git": """\
            #!/usr/bin/env bash
            printf '%s\n' "$*" >> "$(dirname "$0")/git.log"
            case "${1:-}" in
                diff|status|tag|push) exit 0 ;;
                rev-parse) echo "stub-head"; exit 0 ;;
                *) exit 0 ;;
            esac
        """,
        "gh": """\
            #!/usr/bin/env bash
            printf '%s\n' "$*" >> "$(dirname "$0")/gh.log"
            exit 0
        """,
    }

    for name, content in shims.items():
        path = bin_dir / name
        path.write_text(textwrap.dedent(content), encoding="utf-8")
        path.chmod(0o755)

    env = os.environ.copy()
    env["PATH"] = f"{bin_dir}:{env['PATH']}"
    env["PYPI_TOKEN"] = "stub"
    env["HOME"] = str(tmp_path)
    env.pop("TESTPYPI_TOKEN", None)
    monkeypatch.setenv("PATH", env["PATH"])

    result = subprocess.run(
        ["bash", str(RELEASE_SCRIPT)],
        cwd=project_dir,
        env=env,
        capture_output=True,
        text=True,
    )

    assert result.returncode == 0, result.stderr
    uvx_log = (bin_dir / "uvx.log").read_text(encoding="utf-8")
    upload_lines = [
        line for line in uvx_log.splitlines() if line.startswith("twine upload")
    ]
    assert upload_lines
    upload_line = upload_lines[0]
    assert "--repository-url" not in upload_line
    assert "dist/solstone_tmux-0.1.0.tar.gz" in upload_line
    assert "dist/solstone_tmux-0.1.0-py3-none-any.whl" in upload_line
