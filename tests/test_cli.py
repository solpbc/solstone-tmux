# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) 2026 sol pbc

import argparse
from unittest.mock import patch

from solstone_tmux.cli import cmd_install_service, cmd_setup
from solstone_tmux.config import Config, load_config, save_config


class TestSetup:
    class FakeClient:
        instances = []

        def __init__(self, config):
            self.server_url = config.server_url
            TestSetup.FakeClient.instances.append(self)

        def ensure_registered(self, config):
            return False

    def _wire_setup(self, tmp_path, monkeypatch):
        self.FakeClient.instances = []
        monkeypatch.setattr(
            "solstone_tmux.cli.load_config", lambda: load_config(tmp_path)
        )
        monkeypatch.setattr("solstone_tmux.cli.save_config", save_config)
        monkeypatch.setattr("solstone_tmux.upload.UploadClient", self.FakeClient)
        monkeypatch.setattr(
            "solstone_tmux.cli.stream_name", lambda **kw: "testhost.tmux"
        )

    def test_setup_defaults_to_localhost(self, tmp_path, monkeypatch, capsys):
        self._wire_setup(tmp_path, monkeypatch)
        args = argparse.Namespace(server_url=None)

        assert cmd_setup(args) == 0

        config = load_config(tmp_path)
        assert config.server_url == "http://localhost:5015"
        assert self.FakeClient.instances[0].server_url == "http://localhost:5015"
        captured = capsys.readouterr()
        assert "journal URL is required" not in captured.out
        assert "journal URL is required" not in captured.err

    def test_setup_server_url_override(self, tmp_path, monkeypatch):
        self._wire_setup(tmp_path, monkeypatch)
        args = argparse.Namespace(server_url="http://192.168.1.50:5015")

        assert cmd_setup(args) == 0

        config = load_config(tmp_path)
        assert config.server_url == "http://192.168.1.50:5015"
        assert self.FakeClient.instances[0].server_url == "http://192.168.1.50:5015"

    def test_setup_preserves_existing_url(self, tmp_path, monkeypatch):
        config = Config(base_dir=tmp_path, server_url="http://192.168.1.50:5015")
        save_config(config)
        self._wire_setup(tmp_path, monkeypatch)
        args = argparse.Namespace(server_url=None)

        assert cmd_setup(args) == 0

        config = load_config(tmp_path)
        assert config.server_url == "http://192.168.1.50:5015"


class TestInstallServicePath:
    """Tests for PATH capture in cmd_install_service."""

    def _run_install(self, tmp_path, monkeypatch, binary_path, env_path=None):
        """Helper: run cmd_install_service with mocked deps, return unit file content."""
        monkeypatch.setattr(
            "solstone_tmux.cli.shutil.which", lambda _name: str(binary_path)
        )
        monkeypatch.setattr(
            "solstone_tmux.cli.Path.home", staticmethod(lambda: tmp_path)
        )

        if env_path is not None:
            monkeypatch.setenv("PATH", env_path)
        else:
            monkeypatch.delenv("PATH", raising=False)

        with patch("solstone_tmux.cli.subprocess.run"):
            cmd_install_service(argparse.Namespace())

        unit_path = tmp_path / ".config" / "systemd" / "user" / "solstone-tmux.service"
        return unit_path.read_text()

    def test_path_contains_environment_line(self, tmp_path, monkeypatch):
        """Generated unit file contains Environment=PATH= in [Service]."""
        binary = tmp_path / "venv" / "bin" / "solstone-tmux"
        binary.parent.mkdir(parents=True)
        binary.touch()

        content = self._run_install(tmp_path, monkeypatch, binary, "/usr/bin:/bin")

        assert "Environment=PATH=" in content

    def test_venv_bin_is_first(self, tmp_path, monkeypatch):
        """Venv bin dir is the first entry in the PATH."""
        binary = tmp_path / "venv" / "bin" / "solstone-tmux"
        binary.parent.mkdir(parents=True)
        binary.touch()

        content = self._run_install(tmp_path, monkeypatch, binary, "/usr/bin:/bin")

        for line in content.splitlines():
            if line.startswith("Environment=PATH="):
                path_value = line.split("=", 2)[2]
                parts = path_value.split(":")
                assert parts[0] == str(binary.resolve().parent)
                break
        else:
            raise AssertionError("No Environment=PATH= line found")

    def test_deduplication(self, tmp_path, monkeypatch):
        """Duplicate PATH entries are removed, first occurrence wins."""
        binary = tmp_path / "venv" / "bin" / "solstone-tmux"
        binary.parent.mkdir(parents=True)
        binary.touch()
        venv_bin = str(binary.resolve().parent)
        env_path = f"{venv_bin}:/usr/bin:/usr/bin:/bin"

        content = self._run_install(tmp_path, monkeypatch, binary, env_path)

        for line in content.splitlines():
            if line.startswith("Environment=PATH="):
                path_value = line.split("=", 2)[2]
                parts = path_value.split(":")
                assert parts == list(dict.fromkeys(parts)), "PATH contains duplicates"
                assert parts.count(venv_bin) == 1
                assert parts.count("/usr/bin") == 1
                break
        else:
            raise AssertionError("No Environment=PATH= line found")

    def test_fallback_when_no_path_env(self, tmp_path, monkeypatch):
        """Falls back to /usr/local/bin:/usr/bin:/bin when PATH not set."""
        binary = tmp_path / "venv" / "bin" / "solstone-tmux"
        binary.parent.mkdir(parents=True)
        binary.touch()

        content = self._run_install(tmp_path, monkeypatch, binary, env_path=None)

        for line in content.splitlines():
            if line.startswith("Environment=PATH="):
                path_value = line.split("=", 2)[2]
                venv_bin = str(binary.resolve().parent)
                expected_start = venv_bin + ":/usr/local/bin:/usr/bin:/bin"
                assert path_value == expected_start
                break
        else:
            raise AssertionError("No Environment=PATH= line found")

    def test_empty_path_components_filtered(self, tmp_path, monkeypatch):
        """Empty PATH components (from double colons) are filtered out."""
        binary = tmp_path / "venv" / "bin" / "solstone-tmux"
        binary.parent.mkdir(parents=True)
        binary.touch()

        content = self._run_install(tmp_path, monkeypatch, binary, "/usr/bin::/bin:")

        for line in content.splitlines():
            if line.startswith("Environment=PATH="):
                path_value = line.split("=", 2)[2]
                parts = path_value.split(":")
                assert "" not in parts, "Empty component in PATH"
                assert not path_value.startswith(":"), "PATH starts with colon"
                assert not path_value.endswith(":"), "PATH ends with colon"
                assert "::" not in path_value, "Double colon in PATH"
                break
        else:
            raise AssertionError("No Environment=PATH= line found")

    def test_no_trailing_or_leading_colons(self, tmp_path, monkeypatch):
        """Generated PATH has no leading or trailing colons."""
        binary = tmp_path / "venv" / "bin" / "solstone-tmux"
        binary.parent.mkdir(parents=True)
        binary.touch()

        content = self._run_install(tmp_path, monkeypatch, binary, "/usr/bin:/bin")

        for line in content.splitlines():
            if line.startswith("Environment=PATH="):
                path_value = line.split("=", 2)[2]
                assert not path_value.startswith(":"), "PATH starts with colon"
                assert not path_value.endswith(":"), "PATH ends with colon"
                break
        else:
            raise AssertionError("No Environment=PATH= line found")

    def test_start_limit_keys_in_unit_section(self, tmp_path, monkeypatch):
        """StartLimit keys are rendered in [Unit], not [Service]."""
        binary = tmp_path / "venv" / "bin" / "solstone-tmux"
        binary.parent.mkdir(parents=True)
        binary.touch()

        content = self._run_install(tmp_path, monkeypatch, binary, "/usr/bin:/bin")
        sections = [chunk.lstrip("[") for chunk in content.split("\n[")]
        unit_chunk = next(chunk for chunk in sections if chunk.startswith("Unit]"))
        service_chunk = next(
            chunk for chunk in sections if chunk.startswith("Service]")
        )

        assert "StartLimitIntervalSec=300" in unit_chunk
        assert "StartLimitBurst=5" in unit_chunk
        assert "StartLimitIntervalSec" not in service_chunk
        assert "StartLimitBurst" not in service_chunk
