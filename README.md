# solstone-tmux

solstone-tmux adds your terminal to [solstone](https://solpbc.org). It takes in
what you share from your tmux sessions, and all of it goes into your journal.
While your journal is unavailable, what it takes in waits on this device and
syncs when the connection returns.

The current release is one native executable with tmux as its runtime
prerequisite.
Supported systems are Linux on x86_64 or aarch64 and macOS on Apple silicon.
Intel macOS, 32-bit systems, and Windows are not supported.

## Install

Download the native release from `updates.solstone.app`. Read
`https://updates.solstone.app/solstone-tmux/release/latest` for the current
version; `INSTALL.md` carries the exact URLs and filenames. The formats are:

- Linux tarball: install `solstone-tmux` at
  `/usr/local/bin/solstone-tmux`.
- Linux deb or RPM: the package installs
  `/usr/bin/solstone-tmux` and declares its tmux dependency.
- macOS: install the notarized pkg at
  `/usr/local/bin/solstone-tmux`.

See [INSTALL.md](INSTALL.md) for format-specific commands, the one-time Linux
service cutover, verification, and uninstall instructions.

Pairing and service activation are separate:

```sh
solstone-tmux setup < pairing-link.txt
solstone-tmux install-service
solstone-tmux status
```

`setup` reads one private network pairing link from standard input.
`install-service` activates the current user's systemd or launchd service.

## Commands

| Command | Purpose |
| --- | --- |
| `solstone-tmux run` | Run in the foreground; this is the default command |
| `solstone-tmux setup` | Pair through one private network link on standard input |
| `solstone-tmux status` | Report service and sync health |
| `solstone-tmux install-service` | Install and activate the user service |
| `solstone-tmux uninstall-service` | Remove the owned user service |
| `solstone-tmux --help` | Show command usage |
| `solstone-tmux --version` | Show version and source identity |

## How it works

- Takes in what you share from active tmux panes every five seconds, and all of
  it goes into your journal in five-minute segments.
- Segments wait under `~/.local/share/solstone-tmux/captures/`, and incomplete
  work is recovered after a restart.
- Syncs one segment at a time, and local data is released only after the journal
  has proven custody.
- Keeps taking in what you share when pairing or sync is unavailable.
- Emits diagnostics that exclude pane content and tmux session names.

## License

AGPL-3.0-only. Copyright (c) 2026 sol pbc.
