# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) 2026 sol pbc

import ast
import builtins
import os
from pathlib import Path
import socket
import urllib.request

from solstone_tmux import pointer


EXPECTED = (
    "solstone-tmux 1.0.0 is native-only; install it from "
    "https://github.com/solpbc/solstone-tmux/releases\n"
)


def test_pointer_is_exact_and_has_no_observer_side_effects(monkeypatch, capsys):
    source = Path(pointer.__file__).read_text(encoding="utf-8")
    tree = ast.parse(source)
    imports = [
        alias.name
        for node in ast.walk(tree)
        if isinstance(node, ast.Import)
        for alias in node.names
    ]
    package_imports = [
        node for node in ast.walk(tree) if isinstance(node, ast.ImportFrom)
    ]

    assert imports == ["sys"]
    assert package_imports == []

    def reject_access(*_args, **_kwargs):
        raise AssertionError("pointer attempted external access")

    with monkeypatch.context() as guarded:
        guarded.setattr(builtins, "open", reject_access)
        guarded.setattr(os, "open", reject_access)
        guarded.setattr(os, "listdir", reject_access)
        guarded.setattr(os, "scandir", reject_access)
        guarded.setattr(os, "stat", reject_access)
        guarded.setattr(socket, "socket", reject_access)
        guarded.setattr(socket, "create_connection", reject_access)
        guarded.setattr(urllib.request, "urlopen", reject_access)

        exit_code = pointer.main()

    captured = capsys.readouterr()
    assert exit_code == 1
    assert captured.out == ""
    assert captured.err == EXPECTED
