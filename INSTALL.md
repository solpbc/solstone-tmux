# Installing solstone-tmux

solstone-tmux takes in what you share from your tmux sessions, and all of it
goes into your journal. The current release is a native application and requires
tmux.

Supported systems are Linux on x86_64 or aarch64 and macOS on Apple silicon.
Intel macOS, 32-bit systems, and Windows are not supported.

Releases are published to `updates.solstone.app`. Read
`https://updates.solstone.app/solstone-tmux/release/latest` for the current
version:

```sh
curl -fsS https://updates.solstone.app/solstone-tmux/release/latest
```

It prints one line, `version=<VERSION>`. Read it into a variable that the
commands below use, so you don't type the version by hand:

```sh
SOLSTONE_VERSION=$(curl -fsS https://updates.solstone.app/solstone-tmux/release/latest | sed -n 's/^version=//p')
echo "installing solstone-tmux $SOLSTONE_VERSION"
```

Run the rest of these steps in the same shell. In a new one, set
`SOLSTONE_VERSION` again first.

Download paths use the bare version, `release/<VERSION>/` with no `v`, even
though the GitHub tag is `v<VERSION>`. There is no directory listing, so fetch
each file by name.

**Verify first.** Download the package you will install, `SHA256SUMS`, and
`SHA256SUMS.minisig`. Take one package for your system; the architecture table
is below.

```sh
base="https://updates.solstone.app/solstone-tmux/release/$SOLSTONE_VERSION"
curl -fLO "$base/solstone-tmux_${SOLSTONE_VERSION}_<deb-name>.deb"         # Debian / Ubuntu
curl -fLO "$base/solstone-tmux-$SOLSTONE_VERSION-1.<rpm-name>.rpm"         # Fedora / RHEL
curl -fLO "$base/solstone-tmux-$SOLSTONE_VERSION-<tar-name>-linux.tar.gz"  # Linux tarball
curl -fLO "$base/solstone-tmux-$SOLSTONE_VERSION-aarch64-macos.pkg"        # Apple silicon
curl -fLO "$base/solstone-tmux-$SOLSTONE_VERSION-aarch64-macos.tar.gz"     # Apple silicon, archive
curl -fLO "$base/SHA256SUMS"
curl -fLO "$base/SHA256SUMS.minisig"
```

Then fetch the published key, authenticate the checksum file, and check the
package against it:

Install minisign if you do not have it: `apt install minisign`,
`dnf install minisign`, or `brew install minisign` on macOS.

```sh
curl -fLo solstone-tmux-release.pub https://updates.solstone.app/solstone-tmux/minisign.pub
minisign -Vm SHA256SUMS -p solstone-tmux-release.pub
# Linux
awk -v package="<downloaded-package>" '$2 == package' SHA256SUMS | sha256sum -c -
# macOS
awk -v package="<downloaded-package>" '$2 == package' SHA256SUMS | shasum -a 256 -c -
```

The complete release has 13 files: 11 packages and target records,
`SHA256SUMS`, and its detached signature. Minisign authenticates `SHA256SUMS`;
the next command verifies the downloaded package against one of its 11 entries.
Replace `<downloaded-package>` with its exact filename, such as
`solstone-tmux-$SOLSTONE_VERSION-aarch64-macos.pkg`. You run these checks;
`apt` and `dnf` do not. If either command refuses the files, stop. Then install
one of the packages below.

## Linux

Choose one format. The deb and RPM packages install
`/usr/bin/solstone-tmux`; the tarball installs
`/usr/local/bin/solstone-tmux`.

The commands below use `$SOLSTONE_VERSION` from the first step. The three
formats do not share one architecture name, so each command below uses the
matching column:

| System | `<tar-name>` | `<deb-name>` | `<rpm-name>` |
| --- | --- | --- | --- |
| Linux x86_64 | `x86_64` | `amd64` | `x86_64` |
| Linux aarch64 | `aarch64` | `arm64` | `aarch64` |

### Tarball

Install tmux with your system package manager, then:

```sh
tar -xzf "solstone-tmux-$SOLSTONE_VERSION-<tar-name>-linux.tar.gz"
sudo install -m 0755 solstone-tmux /usr/local/bin/solstone-tmux
/usr/local/bin/solstone-tmux --version
```

### deb

`apt` does not check our minisign signature. Complete the verify-first step
above before running:

```sh
sudo apt install "./solstone-tmux_${SOLSTONE_VERSION}_<deb-name>.deb"
/usr/bin/solstone-tmux --version
```

The package declares its tmux dependency.

### RPM

`dnf` does not check our minisign signature. Complete the verify-first step
above before running:

```sh
sudo dnf install "./solstone-tmux-$SOLSTONE_VERSION-1.<rpm-name>.rpm"
/usr/bin/solstone-tmux --version
```

The package declares its tmux dependency.

If this is a new installation, continue with [Pair and activate](#pair-and-activate).

## macOS

The notarized pkg supports Apple silicon and installs
`/usr/local/bin/solstone-tmux`:

The macOS installer does not run minisign for you. Complete the verify-first
step above before running:

```sh
sudo installer -pkg "solstone-tmux-$SOLSTONE_VERSION-aarch64-macos.pkg" -target /
/usr/local/bin/solstone-tmux --version
```

Install tmux before activation. The tarball contains a Developer-ID-signed
binary, but only the pkg is notarized and stapled.

## Pair and activate

In your journal, open **network** and choose **pair a device**. Copy the pairing link into a plain-text file named `pairing-link.txt` on the computer running tmux. Use it while the dialog is open; the link expires after five minutes and works once. Keep it private.

`setup` and `install-service` are separate steps:

1. `setup` reads one pairing link from standard input. Pairing is
   not complete until you confirm your journal's mark, either interactively or
   with `--mark "word word"`. If needed, `confirm` can verify the mark later:

   ```sh
   solstone-tmux setup < pairing-link.txt
   ```

2. `install-service` activates solstone-tmux as the current user's service:

   ```sh
   solstone-tmux install-service
   solstone-tmux status
   ```

   The service keeps a log of warnings and errors. On Linux, read it with
   `journalctl --user -u solstone-tmux`. On macOS, it is
   `~/Library/Logs/solstone-tmux.log`.

Run these through the format-specific absolute path during a Linux cutover if
command lookup has not yet been refreshed. Service installation records the
exact executable invoked; it does not assume an install prefix.

To run without activating a service:

```sh
solstone-tmux run
```

## Status indicator

By default, solstone-tmux owns a small tmux status indicator while it runs:

- yellow means solstone-tmux is running and sync is connected;
- grey means solstone-tmux is running and sync is unavailable;
- absent means solstone-tmux is not running.

Set `"status_indicator": false` in the native `config.json` to leave tmux
options untouched.

## Pane context

Pane titles, working directories and executable names are on by default. Set
`"pane_title": false`, `"working_directory": false` or `"executable": false` in
`config.json` to turn each off. An existing explicit `false` stays off after an
update; an omitted setting uses the new default. Executable names do not include
command arguments. Titles and paths can be sensitive. Restart the service after
changing these settings.

## Uninstall

On Linux, remove the owned user service first:

```sh
solstone-tmux uninstall-service
```

Then remove the installed format:

```sh
sudo apt remove solstone-tmux
```

or:

```sh
sudo dnf remove solstone-tmux
```

For a tar installation, remove `/usr/local/bin/solstone-tmux` after
`uninstall-service` succeeds.

On macOS:

```sh
/usr/local/bin/solstone-tmux uninstall-service
sudo rm /usr/local/bin/solstone-tmux
```

These commands leave settings, cached segments and the macOS log in place.
