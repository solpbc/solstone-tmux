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

It prints one line, `version=<VERSION>`. There is no directory listing, so fetch
each file by name.

**Verify first.** Download the package you will install, `SHA256SUMS`, and
`SHA256SUMS.minisig`. Take one package for your system; the architecture table
is below.

```sh
base="https://updates.solstone.app/solstone-tmux/release/<VERSION>"
curl -fLO "$base/solstone-tmux_<VERSION>_<deb-name>.deb"            # Debian / Ubuntu
curl -fLO "$base/solstone-tmux-<VERSION>-1.<rpm-name>.rpm"          # Fedora / RHEL
curl -fLO "$base/solstone-tmux-<VERSION>-<tar-name>-linux.tar.gz"   # Linux tarball
curl -fLO "$base/solstone-tmux-<VERSION>-aarch64-macos.pkg"         # Apple silicon
curl -fLO "$base/solstone-tmux-<VERSION>-aarch64-macos.tar.gz"      # Apple silicon, archive
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
awk -v package='<downloaded-package>' '$2 == package' SHA256SUMS | sha256sum -c -
# macOS
awk -v package='<downloaded-package>' '$2 == package' SHA256SUMS | shasum -a 256 -c -
```

The complete release has 13 files: 11 packages and target records,
`SHA256SUMS`, and its detached signature. Minisign authenticates `SHA256SUMS`;
the next command verifies the downloaded package against one of its 11 entries.
Replace `<downloaded-package>` with its exact filename. You run these checks;
`apt` and `dnf` do not. If either command refuses the files, stop. Then install
one of the packages below.

## Linux

Choose one format. The deb and RPM packages install
`/usr/bin/solstone-tmux`; the tarball installs
`/usr/local/bin/solstone-tmux`.

`<VERSION>` below is the version you downloaded. The three formats do not share
one architecture name, so each command below uses the matching column:

| System | `<tar-name>` | `<deb-name>` | `<rpm-name>` |
| --- | --- | --- | --- |
| Linux x86_64 | `x86_64` | `amd64` | `x86_64` |
| Linux aarch64 | `aarch64` | `arm64` | `aarch64` |

### Tarball

Install tmux with your system package manager, then:

```sh
tar -xzf solstone-tmux-<VERSION>-<tar-name>-linux.tar.gz
sudo install -m 0755 solstone-tmux /usr/local/bin/solstone-tmux
/usr/local/bin/solstone-tmux --version
```

### deb

`apt` does not check our minisign signature. Complete the verify-first step
above before running:

```sh
sudo apt install ./solstone-tmux_<VERSION>_<deb-name>.deb
/usr/bin/solstone-tmux --version
```

The package declares its tmux dependency.

### RPM

`dnf` does not check our minisign signature. Complete the verify-first step
above before running:

```sh
sudo dnf install ./solstone-tmux-<VERSION>-1.<rpm-name>.rpm
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
sudo installer -pkg solstone-tmux-<VERSION>-aarch64-macos.pkg -target /
/usr/local/bin/solstone-tmux --version
```

Install tmux before activation. The tarball contains a Developer-ID-signed
binary, but only the pkg is notarized and stapled.

## Pair and activate

`setup` and `install-service` are separate steps:

1. `setup` reads one private-link pairing link from standard input and stores
   the new pairing:

   ```sh
   solstone-tmux setup < pairing-link.txt
   ```

2. `install-service` activates solstone-tmux as the current user's service:

   ```sh
   solstone-tmux install-service
   solstone-tmux status
   ```

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

These commands leave settings and cached segments in place.
