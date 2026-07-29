# solstone-tmux

solstone-tmux is a standalone observer for
[solstone](https://solpbc.org). It experiences tmux sessions along with you,
keeps observations locally while your journal is unavailable, and syncs them
when the connection returns.

Version 1.0.0 is one native executable with tmux as its runtime prerequisite.
Supported systems are Linux on x86_64 or aarch64 and macOS on Apple silicon.
Intel macOS, 32-bit systems, and Windows are not supported.

## Install

Download the native release from
[GitHub Releases](https://github.com/solpbc/solstone-tmux/releases):

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

`setup` reads one private-link pairing link from standard input.
`install-service` activates the current user's systemd or launchd service.

On first native run, Linux adopts only the previous stream, intervals,
retention, and status-indicator settings and continues using the existing cache
in place. Previous credentials are not copied; pairing is fresh.

## Commands

| Command | Purpose |
| --- | --- |
| `solstone-tmux run` | Run in the foreground; this is the default command |
| `solstone-tmux setup` | Pair through one standard-input private link |
| `solstone-tmux status` | Report service and sync health |
| `solstone-tmux install-service` | Install and activate the user service |
| `solstone-tmux uninstall-service` | Remove the owned user service |
| `solstone-tmux --help` | Show command usage |
| `solstone-tmux --version` | Show version and source identity |

## How it works

- Experiences active tmux panes every five seconds and writes changed
  observations into five-minute segments.
- Keeps segments under
  `~/.local/share/solstone-tmux/captures/` and recovers incomplete work after a
  restart.
- Syncs sequentially and retains local data until journal custody is proven.
- Keeps local observation running when pairing or sync is unavailable.
- Emits diagnostics that exclude pane content and tmux session names.

## License

AGPL-3.0-only. Copyright (c) 2026 sol pbc.
