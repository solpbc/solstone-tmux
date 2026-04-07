# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) 2026 sol pbc

"""Stream identity for observer segments.

Extracted from solstone's think/streams.py — only the pure naming functions
needed by standalone observers.

Naming convention (separator is '.'):
    Local tmux:   {hostname}.tmux   e.g. "archon.tmux"
    Observer:     {observer_name}   e.g. "laptop"
"""

from __future__ import annotations

import re

_STREAM_NAME_RE = re.compile(r"^[a-z0-9][a-z0-9._-]*$")


def _strip_hostname(name: str) -> str:
    """Strip domain suffix from a hostname, keeping only the first label.

    Dots in stream names are reserved for qualifiers (e.g., '.tmux').
    Hostnames like 'ja1r.local' or '192.168.1.1' must be reduced to a
    dot-free base name.

    Examples: 'ja1r.local' -> 'ja1r', '192.168.1.1' -> '192-168-1-1',
    'archon' -> 'archon', 'my.host.example.com' -> 'my'
    """
    name = name.strip()
    if not name:
        return name
    parts = name.split(".")
    if all(p.isdigit() for p in parts if p):
        return "-".join(p for p in parts if p)
    return parts[0]


def stream_name(
    *,
    host: str | None = None,
    observer: str | None = None,
    qualifier: str | None = None,
) -> str:
    """Derive canonical stream name from source characteristics.

    Parameters
    ----------
    host : str, optional
        Local hostname (e.g., "archon").
    observer : str, optional
        Observer name (e.g., "laptop").
    qualifier : str, optional
        Sub-stream qualifier (e.g., "tmux"). Appended with dot separator.

    Returns
    -------
    str
        Canonical stream name.

    Raises
    ------
    ValueError
        If no source is provided, or the resulting name is invalid.
    """
    if host:
        base = _strip_hostname(host)
    elif observer:
        base = _strip_hostname(observer)
    else:
        raise ValueError("stream_name requires host or observer")

    name = base.lower().strip()
    name = re.sub(r"[\s/\\]+", "-", name)

    if qualifier:
        qualifier = qualifier.lower().strip()
        qualifier = re.sub(r"[\s/\\]+", "-", qualifier)
        name = f"{name}.{qualifier}"

    if not name or ".." in name:
        raise ValueError(f"Invalid stream name: {name!r}")
    if not _STREAM_NAME_RE.match(name):
        raise ValueError(f"Invalid stream name: {name!r}")

    return name
