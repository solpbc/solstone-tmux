# solstone-tmux

Standalone tmux terminal observer for [solstone](https://solpbc.org). Captures tmux terminal sessions to a local cache and syncs them to a solstone server.

## Install

Packages are not yet on PyPI. Install from source:

```bash
git clone https://github.com/solpbc/solstone-tmux.git
cd solstone-tmux
pipx install .
```

## Setup

### 1. Register an observer on the solstone server

```bash
sol observer create solstone-tmux
```

This prints the server URL and API key. You'll need both for the next step.

### 2. Write the config

Create `~/.local/share/solstone-tmux/config/config.json`:

```json
{
  "server_url": "http://localhost:8000",
  "key": "<api-key-from-sol-observer-create>",
  "stream": "<hostname>.tmux",
  "capture_interval": 5,
  "segment_interval": 300
}
```

Set `stream` to `<your-hostname>.tmux` (e.g., `fedora.tmux`, `archon.tmux`). This matches the stream naming convention used by the built-in observers.

Alternatively, `solstone-tmux setup` runs an interactive wizard that prompts for the server URL and auto-registers.

### 3. Install the systemd service

```bash
solstone-tmux install-service
```

This writes the unit file to `~/.config/systemd/user/solstone-tmux.service`, enables it, and starts it.

### 4. Verify

```bash
systemctl --user status solstone-tmux
solstone-tmux status
sol observer list  # should show the observer as "connected"
```

## Manual run

```bash
solstone-tmux run         # foreground, ctrl-c to stop
solstone-tmux run -v      # verbose/debug logging
```

## How it works

- Polls all active tmux sessions every 5 seconds for content changes
- Accumulates captures in 5-minute segments under `~/.local/share/solstone-tmux/captures/`
- Background sync service uploads completed segments to the solstone ingest API
- Captures offline — syncs when server becomes available
- Recovers incomplete segments on startup after crashes

## Commands

| Command | What it does |
|---------|-------------|
| `solstone-tmux run` | Start capture + sync (default if no subcommand) |
| `solstone-tmux setup` | Interactive config wizard |
| `solstone-tmux install-service` | Install and start systemd user service |
| `solstone-tmux status` | Show capture state, sync state, cache size |

## License

AGPL-3.0-only. Copyright (c) 2026 sol pbc.
