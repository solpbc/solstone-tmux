# solstone-tmux

Standalone tmux terminal observer for [solstone](https://solpbc.org). Experiences your tmux sessions along with you, accumulating observations to a local cache and syncing them to your journal.

## Install

solstone (the journal) must already be installed and running on the host this observer reports to. If it isn't, start here: https://solstone.app/install.

On the machine that will host the observer:

```bash
pipx install solstone-tmux
solstone-tmux install-service
solstone-tmux setup
```

`setup` registers the observer against your journal over the local `http://localhost:5015` link, so there's no URL to type. If this machine reaches your solstone host directly instead, run `solstone-tmux setup --server-url <journal-url>`.

### From source

For working on solstone-tmux itself:

```bash
git clone https://github.com/solpbc/solstone-tmux.git
cd solstone-tmux
make install-service
```

This installs the `solstone-tmux` command via pipx and writes the systemd user unit. The manual setup steps below cover registration and config.

## Manual setup

If `solstone-tmux setup` isn't a good fit (no interactive shell, scripted provisioning, custom config layout), work through these steps by hand.

### 1. Register an observer with your journal

```bash
sol observer create solstone-tmux
```

This prints the journal URL and API key. You'll need both for the next step.

### 2. Write the config

Create `~/.local/share/solstone-tmux/config/config.json`:

```json
{
  "server_url": "http://localhost:5015",
  "key": "<api-key-from-sol-observer-create>",
  "stream": "<hostname>.tmux",
  "capture_interval": 5,
  "segment_interval": 300
}
```

Set `stream` to `<your-hostname>.tmux` (e.g., `fedora.tmux`, `archon.tmux`). This matches the stream naming convention used by the built-in observers.

Alternatively, `solstone-tmux setup` registers against your journal automatically — over the local `http://localhost:5015` link by default, or pass `--server-url` for a direct-to-remote journal.

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
- Accumulates observations in 5-minute segments under `~/.local/share/solstone-tmux/captures/`
- Background sync service uploads completed segments to your journal
- Works offline — syncs when your journal is reachable
- Recovers incomplete segments on startup after crashes

## Commands

| Command | What it does |
|---------|-------------|
| `solstone-tmux run` | Start capture + sync (default if no subcommand) |
| `solstone-tmux setup` | Interactive config wizard |
| `solstone-tmux install-service` | Install and start systemd user service |
| `solstone-tmux status` | Show observer state, sync state, cache size |

## License

AGPL-3.0-only. Copyright (c) 2026 sol pbc.
