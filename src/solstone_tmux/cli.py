# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) 2026 sol pbc

"""CLI entry point for solstone-tmux.

Subcommands:
    run             Start capture loop + sync service (default)
    setup           Interactive configuration
    install-service Write systemd user unit, enable, start
    status          Show capture and sync state
"""

from __future__ import annotations

import argparse
import asyncio
import json
import logging
import shutil
import socket
import subprocess
import sys
from pathlib import Path

from .config import load_config, save_config
from .streams import stream_name


def _setup_logging(verbose: bool = False) -> None:
    level = logging.DEBUG if verbose else logging.INFO
    logging.basicConfig(
        level=level,
        format="%(asctime)s [%(levelname)s] %(name)s: %(message)s",
        datefmt="%H:%M:%S",
    )


def cmd_run(args: argparse.Namespace) -> int:
    """Start the capture loop + sync service."""
    from .observer import async_run
    from .recovery import recover_incomplete_segments

    config = load_config()
    config.ensure_dirs()

    if not config.stream:
        try:
            config.stream = stream_name(host=socket.gethostname(), qualifier="tmux")
        except ValueError as e:
            print(f"Error: {e}", file=sys.stderr)
            return 1

    if args.interval:
        config.segment_interval = args.interval

    # Crash recovery before starting
    recovered = recover_incomplete_segments(config.captures_dir)
    if recovered:
        print(f"Recovered {recovered} incomplete segment(s)")

    try:
        return asyncio.run(async_run(config))
    except KeyboardInterrupt:
        return 0


def cmd_setup(args: argparse.Namespace) -> int:
    """Interactive setup — configure server URL and register."""
    from .upload import UploadClient

    config = load_config()

    # Prompt for server URL
    default_url = config.server_url or ""
    url = input(f"Solstone server URL [{default_url}]: ").strip()
    if url:
        config.server_url = url
    elif not config.server_url:
        print("Error: server URL is required", file=sys.stderr)
        return 1

    # Derive stream name
    if not config.stream:
        try:
            config.stream = stream_name(host=socket.gethostname(), qualifier="tmux")
        except ValueError as e:
            print(f"Error deriving stream name: {e}", file=sys.stderr)
            return 1
    print(f"Stream: {config.stream}")

    # Save config before registration (so URL is persisted)
    config.ensure_dirs()
    save_config(config)

    # Auto-register — try sol CLI first (no server needed), fall back to HTTP
    if not config.key:
        sol = shutil.which("sol")
        if sol:
            print("Registering via sol CLI...")
            try:
                result = subprocess.run(
                    [sol, "observer", "--json", "create", config.stream],
                    capture_output=True,
                    text=True,
                    timeout=10,
                )
                if result.returncode == 0:
                    data = json.loads(result.stdout)
                    config.key = data["key"]
                    save_config(config)
                    print(f"Registered (key: {config.key[:8]}...)")
                else:
                    print("CLI registration failed, trying HTTP...")
            except (subprocess.TimeoutExpired, json.JSONDecodeError, KeyError, OSError):
                print("CLI registration failed, trying HTTP...")

        if not config.key:
            print("Registering with server...")
            client = UploadClient(config)
            if client.ensure_registered(config):
                config = load_config()
                print(f"Registered (key: {config.key[:8]}...)")
            else:
                print(
                    "Warning: registration failed. Run setup again when server is available."
                )
    else:
        print(f"Already registered (key: {config.key[:8]}...)")

    print(f"\nConfig saved to {config.config_path}")
    print(f"Captures will go to {config.captures_dir}")
    print(
        "\nRun 'solstone-tmux run' to start, or 'solstone-tmux install-service' for systemd."
    )
    return 0


def cmd_install_service(args: argparse.Namespace) -> int:
    """Write systemd user unit file, enable, and start the service."""
    binary = shutil.which("solstone-tmux")
    if not binary:
        print("Error: solstone-tmux not found on PATH", file=sys.stderr)
        print("Install with: pipx install solstone-tmux", file=sys.stderr)
        return 1

    unit_dir = Path.home() / ".config" / "systemd" / "user"
    unit_dir.mkdir(parents=True, exist_ok=True)
    unit_path = unit_dir / "solstone-tmux.service"

    unit_content = f"""\
[Unit]
Description=Solstone Tmux Terminal Observer
After=basic.target

[Service]
Type=simple
ExecStart={binary} run
Restart=on-failure
RestartSec=5
StartLimitIntervalSec=300
StartLimitBurst=5

[Install]
WantedBy=default.target
"""

    unit_path.write_text(unit_content)
    print(f"Wrote {unit_path}")

    # Reload, enable, start
    try:
        subprocess.run(["systemctl", "--user", "daemon-reload"], check=True)
        subprocess.run(
            ["systemctl", "--user", "enable", "--now", "solstone-tmux.service"],
            check=True,
        )
        print("Service enabled and started.")
        subprocess.run(
            ["systemctl", "--user", "status", "solstone-tmux.service"],
            check=False,
        )
    except FileNotFoundError:
        print("Warning: systemctl not found. Enable the service manually.")
    except subprocess.CalledProcessError as e:
        print(f"Warning: systemctl command failed: {e}")

    return 0


def cmd_status(args: argparse.Namespace) -> int:
    """Show capture and sync state."""
    config = load_config()

    print(f"Config: {config.config_path}")
    print(f"Server: {config.server_url or '(not configured)'}")
    print(f"Key:    {config.key[:8] + '...' if config.key else '(not registered)'}")
    print(f"Stream: {config.stream or '(not set)'}")
    print()

    # Cache size
    captures_dir = config.captures_dir
    if captures_dir.exists():
        total_size = 0
        segment_count = 0
        day_count = 0

        for day_dir in sorted(captures_dir.iterdir()):
            if not day_dir.is_dir():
                continue
            day_count += 1
            for stream_dir in day_dir.iterdir():
                if not stream_dir.is_dir():
                    continue
                for seg_dir in stream_dir.iterdir():
                    if not seg_dir.is_dir():
                        continue
                    if seg_dir.name.endswith(".incomplete"):
                        continue
                    if seg_dir.name.endswith(".failed"):
                        continue
                    segment_count += 1
                    for f in seg_dir.iterdir():
                        if f.is_file():
                            total_size += f.stat().st_size

        size_mb = total_size / (1024 * 1024)
        print(f"Cache:  {captures_dir}")
        print(
            f"        {segment_count} segments across {day_count} day(s), {size_mb:.1f} MB"
        )
    else:
        print(f"Cache:  {captures_dir} (not created yet)")

    # Retention policy
    retention = config.cache_retention_days
    if retention < 0:
        print("Retain: forever")
    elif retention == 0:
        print("Retain: delete after sync")
    else:
        print(f"Retain: {retention} day(s)")

    # Synced days
    synced_path = config.state_dir / "synced_days.json"
    if synced_path.exists():
        try:
            with open(synced_path) as f:
                synced = json.load(f)
            print(f"Synced: {len(synced)} day(s) fully synced")
        except (json.JSONDecodeError, OSError):
            pass

    # Systemd status
    try:
        result = subprocess.run(
            ["systemctl", "--user", "is-active", "solstone-tmux.service"],
            capture_output=True,
            text=True,
        )
        state = result.stdout.strip()
        print(f"\nService: {state}")
    except FileNotFoundError:
        pass

    return 0


def main() -> None:
    """CLI entry point."""
    parser = argparse.ArgumentParser(
        prog="solstone-tmux",
        description="Standalone tmux terminal observer for solstone",
    )
    parser.add_argument(
        "-v", "--verbose", action="store_true", help="Enable debug logging"
    )
    subparsers = parser.add_subparsers(dest="command")

    # run
    run_parser = subparsers.add_parser("run", help="Start capture + sync")
    run_parser.add_argument(
        "--interval",
        type=int,
        default=None,
        help="Segment duration in seconds (default: 300)",
    )

    # setup
    subparsers.add_parser("setup", help="Interactive configuration")

    # install-service
    subparsers.add_parser("install-service", help="Install systemd user service")

    # status
    subparsers.add_parser("status", help="Show capture and sync state")

    args = parser.parse_args()
    _setup_logging(args.verbose)

    # Default to run if no subcommand
    command = args.command or "run"

    commands = {
        "run": cmd_run,
        "setup": cmd_setup,
        "install-service": cmd_install_service,
        "status": cmd_status,
    }

    handler = commands.get(command)
    if handler:
        sys.exit(handler(args))
    else:
        parser.print_help()
        sys.exit(1)
