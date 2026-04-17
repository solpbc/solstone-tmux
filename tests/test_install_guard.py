# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) 2026 sol pbc

from pathlib import Path

import pytest

from solstone_tmux import install_guard


def _patch_paths(monkeypatch, tmp_path):
    marker_path = tmp_path / ".config" / "solstone-tmux" / ".install-source"
    pipx_bin_path = tmp_path / ".local" / "bin" / "solstone-tmux"
    monkeypatch.setattr(install_guard, "MARKER_PATH", marker_path)
    monkeypatch.setattr(install_guard, "PIPX_BIN_PATH", pipx_bin_path)
    return marker_path, pipx_bin_path


def test_absent_when_nothing_exists(tmp_path, monkeypatch):
    marker_path, pipx_bin_path = _patch_paths(monkeypatch, tmp_path)

    state, stale_path = install_guard.detect_state(tmp_path / "repo")

    assert marker_path.exists() is False
    assert pipx_bin_path.exists() is False
    assert state == install_guard.State.ABSENT
    assert stale_path is None


def test_owned_when_marker_and_bin_present(tmp_path, monkeypatch):
    marker_path, pipx_bin_path = _patch_paths(monkeypatch, tmp_path)
    repo_root = tmp_path / "repo"
    repo_root.mkdir()
    pipx_bin_path.parent.mkdir(parents=True)
    pipx_bin_path.touch()
    marker_path.parent.mkdir(parents=True)
    marker_path.write_text(f"{repo_root.resolve()}\n", encoding="utf-8")

    state, stale_path = install_guard.detect_state(repo_root)

    assert state == install_guard.State.OWNED
    assert stale_path is None


def test_partial_owned_when_marker_present_but_no_bin(tmp_path, monkeypatch):
    marker_path, _pipx_bin_path = _patch_paths(monkeypatch, tmp_path)
    repo_root = tmp_path / "repo"
    repo_root.mkdir()
    marker_path.parent.mkdir(parents=True)
    marker_path.write_text(f"{repo_root.resolve()}\n", encoding="utf-8")

    state, stale_path = install_guard.detect_state(repo_root)

    assert state == install_guard.State.PARTIAL_OWNED
    assert stale_path is None


def test_cross_repo_when_marker_points_elsewhere(tmp_path, monkeypatch):
    marker_path, pipx_bin_path = _patch_paths(monkeypatch, tmp_path)
    repo_root = tmp_path / "repo"
    other_repo = tmp_path / "other-repo"
    repo_root.mkdir()
    other_repo.mkdir()
    pipx_bin_path.parent.mkdir(parents=True)
    pipx_bin_path.touch()
    marker_path.parent.mkdir(parents=True)
    marker_path.write_text(f"{other_repo.resolve()}\n", encoding="utf-8")

    state, stale_path = install_guard.detect_state(repo_root)

    assert state == install_guard.State.CROSS_REPO
    assert stale_path == other_repo.resolve()


def test_cross_repo_when_marker_points_to_deleted_path(tmp_path, monkeypatch):
    marker_path, pipx_bin_path = _patch_paths(monkeypatch, tmp_path)
    repo_root = tmp_path / "repo"
    deleted_repo = tmp_path / "gone" / "repo"
    repo_root.mkdir()
    pipx_bin_path.parent.mkdir(parents=True)
    pipx_bin_path.touch()
    marker_path.parent.mkdir(parents=True)
    marker_path.write_text(f"{deleted_repo}\n", encoding="utf-8")

    state, stale_path = install_guard.detect_state(repo_root)

    assert state == install_guard.State.CROSS_REPO
    assert stale_path == deleted_repo.resolve()


def test_unknown_when_no_marker_but_bin_present(tmp_path, monkeypatch):
    _marker_path, pipx_bin_path = _patch_paths(monkeypatch, tmp_path)
    pipx_bin_path.parent.mkdir(parents=True)
    pipx_bin_path.touch()

    state, stale_path = install_guard.detect_state(tmp_path / "repo")

    assert state == install_guard.State.UNKNOWN
    assert stale_path is None


def test_malformed_multiline(tmp_path, monkeypatch):
    marker_path, _pipx_bin_path = _patch_paths(monkeypatch, tmp_path)
    marker_path.parent.mkdir(parents=True)
    marker_path.write_text("/tmp/one\n/tmp/two\n", encoding="utf-8")

    state, stale_path = install_guard.detect_state(tmp_path / "repo")

    assert state == install_guard.State.MALFORMED
    assert stale_path is None


def test_malformed_relative_path(tmp_path, monkeypatch):
    marker_path, _pipx_bin_path = _patch_paths(monkeypatch, tmp_path)
    marker_path.parent.mkdir(parents=True)
    marker_path.write_text("relative/path\n", encoding="utf-8")

    state, stale_path = install_guard.detect_state(tmp_path / "repo")

    assert state == install_guard.State.MALFORMED
    assert stale_path is None


def test_malformed_empty_file(tmp_path, monkeypatch):
    marker_path, _pipx_bin_path = _patch_paths(monkeypatch, tmp_path)
    marker_path.parent.mkdir(parents=True)
    marker_path.write_text("", encoding="utf-8")

    state, stale_path = install_guard.detect_state(tmp_path / "repo")

    assert state == install_guard.State.MALFORMED
    assert stale_path is None


def test_write_marker_creates_dir_and_writes_resolved_path_newline_terminated(
    tmp_path, monkeypatch
):
    marker_path, _pipx_bin_path = _patch_paths(monkeypatch, tmp_path)
    (tmp_path / "repo").mkdir()
    repo_root = tmp_path / "repo" / ".." / "repo"

    install_guard.write_marker(repo_root)

    assert marker_path.read_text(encoding="utf-8") == f"{repo_root.resolve()}\n"


def test_write_marker_is_atomic(tmp_path, monkeypatch):
    marker_path, _pipx_bin_path = _patch_paths(monkeypatch, tmp_path)
    repo_root = tmp_path / "repo"
    repo_root.mkdir()
    marker_path.parent.mkdir(parents=True)
    marker_path.write_text("old\n", encoding="utf-8")
    rename_calls = []
    original_rename = install_guard.os.rename

    def record_rename(src, dst):
        rename_calls.append((Path(src), Path(dst)))
        original_rename(src, dst)

    monkeypatch.setattr(install_guard.os, "rename", record_rename)

    install_guard.write_marker(repo_root)

    assert marker_path.read_text(encoding="utf-8") == f"{repo_root.resolve()}\n"
    assert rename_calls == [
        (marker_path.with_name(f"{marker_path.name}.tmp"), marker_path)
    ]


def test_remove_marker_idempotent_when_absent(tmp_path, monkeypatch):
    marker_path, _pipx_bin_path = _patch_paths(monkeypatch, tmp_path)

    install_guard.remove_marker()
    install_guard.remove_marker()

    assert marker_path.exists() is False


def test_main_check_prints_state_name_exits_zero(tmp_path, monkeypatch, capsys):
    marker_path, _pipx_bin_path = _patch_paths(monkeypatch, tmp_path)
    repo_root = tmp_path / "repo"
    repo_root.mkdir()
    marker_path.parent.mkdir(parents=True)
    marker_path.write_text(f"{repo_root.resolve()}\n", encoding="utf-8")
    monkeypatch.chdir(repo_root)

    result = install_guard.main(["check"])

    captured = capsys.readouterr()
    assert result == 0
    assert captured.out.strip() == "PARTIAL_OWNED"


def test_main_install_fresh_prints_mode_fresh_install_exits_zero(
    tmp_path, monkeypatch, capsys
):
    _marker_path, _pipx_bin_path = _patch_paths(monkeypatch, tmp_path)
    repo_root = tmp_path / "repo"
    repo_root.mkdir()
    monkeypatch.chdir(repo_root)

    result = install_guard.main(["install"])

    captured = capsys.readouterr()
    assert result == 0
    assert captured.out.strip() == "mode: fresh install"


def test_main_install_owned_prints_mode_upgrade_exits_zero(
    tmp_path, monkeypatch, capsys
):
    marker_path, pipx_bin_path = _patch_paths(monkeypatch, tmp_path)
    repo_root = tmp_path / "repo"
    repo_root.mkdir()
    marker_path.parent.mkdir(parents=True)
    marker_path.write_text(f"{repo_root.resolve()}\n", encoding="utf-8")
    pipx_bin_path.parent.mkdir(parents=True)
    pipx_bin_path.touch()
    monkeypatch.chdir(repo_root)

    result = install_guard.main(["install"])

    captured = capsys.readouterr()
    assert result == 0
    assert captured.out.strip() == "mode: upgrade"


def test_main_install_partial_owned_prints_mode_upgrade_repair_exits_zero(
    tmp_path, monkeypatch, capsys
):
    marker_path, _pipx_bin_path = _patch_paths(monkeypatch, tmp_path)
    repo_root = tmp_path / "repo"
    repo_root.mkdir()
    marker_path.parent.mkdir(parents=True)
    marker_path.write_text(f"{repo_root.resolve()}\n", encoding="utf-8")
    monkeypatch.chdir(repo_root)

    result = install_guard.main(["install"])

    captured = capsys.readouterr()
    assert result == 0
    assert captured.out.strip() == "mode: upgrade (repair)"


def test_main_install_cross_repo_prints_error_exits_two(tmp_path, monkeypatch, capsys):
    marker_path, pipx_bin_path = _patch_paths(monkeypatch, tmp_path)
    repo_root = tmp_path / "repo"
    other_repo = tmp_path / "other-repo"
    repo_root.mkdir()
    other_repo.mkdir()
    marker_path.parent.mkdir(parents=True)
    marker_path.write_text(f"{other_repo.resolve()}\n", encoding="utf-8")
    pipx_bin_path.parent.mkdir(parents=True)
    pipx_bin_path.touch()
    monkeypatch.chdir(repo_root)

    result = install_guard.main(["install"])

    captured = capsys.readouterr()
    assert result == 2
    assert captured.out == ""
    assert "mode: aborted" in captured.err
    assert str(other_repo.resolve()) in captured.err


def test_main_install_unknown_exits_two(tmp_path, monkeypatch, capsys):
    _marker_path, pipx_bin_path = _patch_paths(monkeypatch, tmp_path)
    repo_root = tmp_path / "repo"
    repo_root.mkdir()
    pipx_bin_path.parent.mkdir(parents=True)
    pipx_bin_path.touch()
    monkeypatch.chdir(repo_root)

    result = install_guard.main(["install"])

    captured = capsys.readouterr()
    assert result == 2
    assert "pre-hygiene install" in captured.err


def test_main_install_malformed_exits_two(tmp_path, monkeypatch, capsys):
    marker_path, _pipx_bin_path = _patch_paths(monkeypatch, tmp_path)
    repo_root = tmp_path / "repo"
    repo_root.mkdir()
    marker_path.parent.mkdir(parents=True)
    marker_path.write_text("relative/path\n", encoding="utf-8")
    monkeypatch.chdir(repo_root)

    result = install_guard.main(["install"])

    captured = capsys.readouterr()
    assert result == 2
    assert "malformed marker" in captured.err


def test_main_uninstall_absent_prints_no_artifacts_exits_zero(
    tmp_path, monkeypatch, capsys
):
    _marker_path, _pipx_bin_path = _patch_paths(monkeypatch, tmp_path)
    repo_root = tmp_path / "repo"
    repo_root.mkdir()
    monkeypatch.chdir(repo_root)

    result = install_guard.main(["uninstall"])

    captured = capsys.readouterr()
    assert result == 0
    assert captured.out.strip() == "no artifacts to remove"


def test_main_uninstall_owned_exits_zero_silent(tmp_path, monkeypatch, capsys):
    marker_path, pipx_bin_path = _patch_paths(monkeypatch, tmp_path)
    repo_root = tmp_path / "repo"
    repo_root.mkdir()
    marker_path.parent.mkdir(parents=True)
    marker_path.write_text(f"{repo_root.resolve()}\n", encoding="utf-8")
    pipx_bin_path.parent.mkdir(parents=True)
    pipx_bin_path.touch()
    monkeypatch.chdir(repo_root)

    result = install_guard.main(["uninstall"])

    captured = capsys.readouterr()
    assert result == 0
    assert captured.out == ""
    assert captured.err == ""


def test_main_uninstall_cross_repo_exits_two(tmp_path, monkeypatch, capsys):
    marker_path, pipx_bin_path = _patch_paths(monkeypatch, tmp_path)
    repo_root = tmp_path / "repo"
    other_repo = tmp_path / "other-repo"
    repo_root.mkdir()
    other_repo.mkdir()
    marker_path.parent.mkdir(parents=True)
    marker_path.write_text(f"{other_repo.resolve()}\n", encoding="utf-8")
    pipx_bin_path.parent.mkdir(parents=True)
    pipx_bin_path.touch()
    monkeypatch.chdir(repo_root)

    result = install_guard.main(["uninstall"])

    captured = capsys.readouterr()
    assert result == 2
    assert str(other_repo.resolve()) in captured.err


def test_main_write_marker_subcommand_requires_repo_root_arg():
    with pytest.raises(SystemExit) as excinfo:
        install_guard.main(["write-marker"])

    assert excinfo.value.code == 2
