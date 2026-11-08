# Changelog

All notable changes to solstone-tmux will be documented in this file.

Format adapted from [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Fixed

- `solstone-tmux status` now shows the version the service is running. if you installed an update and the service is still on the old version, it says so and tells you to run `solstone-tmux install-service`.
- when your journal turns away a segment, solstone-tmux now says why. `status` shows how many segments your journal has turned away, with its reason for the newest one, and the log notes each one when it happens. a segment your journal turns away no longer makes `status` read offline, or stops older segments from syncing behind it. it stays on this computer and is tried again later.

## [2.0.27] - 2026-11-05

### Fixed

- if this computer slept while solstone-tmux was waiting to retry, it could take up to five minutes after waking to check your journal again. this release resolves that.

## [2.0.26] - 2026-11-04

### Added

- every download now includes solstone-tmux's license and `RUST_DEPENDENCY_NOTICES.txt`, which reproduces the licenses of the open source code it's built from.

### Fixed

- after this computer wakes, `solstone-tmux status` no longer says connected from before it slept. connected now means your journal answered within about the last minute. just after solstone-tmux starts or this computer wakes, it checks with your journal within a few seconds. if your journal doesn't answer, the status says connecting, then offline once 30 seconds have passed. a retry after a failed attempt no longer flips the status to syncing and back.
- the ☼ in your status line now stays yellow a moment longer each time it lights, so it no longer flickers.
- if you run solstone-tmux in the foreground, the steps `solstone-tmux confirm` gives after a no, a cancel or wrong mark words no longer send you to the service commands. they now say to stop it, run setup again with a fresh pairing link from your journal, then start it again. for the service, `solstone-tmux uninstall-service` and `solstone-tmux install-service` still do the stopping and starting.

## [2.0.25] - 2026-11-03

### Fixed

- this is a security fix. someone who got you to use a pairing link they had swapped could pair this computer with something that isn't your journal, while showing you your own journal's mark, so the check would have looked right. what solstone-tmux took in from your terminal would then have gone to them instead of your journal. setup now refuses a link when what answers claims to be a different journal from the one the link points to, before it asks you anything or saves anything. a swapped link that doesn't make that claim still reaches the mark question, showing its own mark, so if it doesn't match your journal's, answer no. if you paired before this update, install it, restart solstone-tmux (`solstone-tmux install-service` does that for the service), then run `solstone-tmux status`. if it says it's waiting for you to confirm your journal's mark, run `solstone-tmux confirm`. otherwise, compare the mark it shows with the one in your journal's network app. if they don't match, this computer isn't paired with your journal, and what solstone-tmux takes in keeps going to whatever it's paired with until you pair again: stop solstone-tmux (`solstone-tmux uninstall-service` for the service), run setup again with a fresh pairing link from your journal, which replaces the old pairing, then start it again (`solstone-tmux install-service` for the service). if `confirm` says it couldn't verify, type cancel, then do the same three steps with a fresh pairing link.
- on a computer that's already paired, setup with the wrong --mark words, or a no at the mark question, now says your current pairing stays in use, instead of saying this computer isn't paired.
- setup now asks for the pairing link at a terminal, and goes on when you press enter.

## [2.0.24] - 2026-11-02

### Added

- you can now have solstone-tmux take in a tmux session while nobody is using it, such as one where an agent works on its own, so it goes into your journal too. list the session's name in `config.json`, for example `"output_sessions": ["agent-*"]`, and restart the service. a listed session is then taken in whenever output changes in its current window. anything that appears while nobody is using it is saved without the cursor, so your journal doesn't read it as something you typed. sessions you don't list work as before. thanks @dvanduzer for the idea.

### Fixed

- the ☼ that solstone-tmux adds to your tmux status bar no longer cuts off the end of your session name. tmux gives the left side of the status bar 10 columns by default (`status-left-length`), and the ☼ took one of them, so a session name that fit before could lose its last letter. now solstone-tmux widens the left side by one column while it runs and puts it back when it stops. tmux's own limit still applies to long names: `tmux set -g status-left-length 20` raises it. thanks @dvanduzer for the report.

## [2.0.23] - 2026-11-01

### Fixed

- `solstone-tmux install-service` now restarts the service when it's already running. before, running it after an update left the old version running until you restarted the service yourself. thanks @dvanduzer for the report.

## [2.0.22] - 2026-10-30

### Fixed

- solstone-tmux no longer stops when it can't update its status indicator in tmux. before, a failed update made it exit with code 1, and the service started it again. now it keeps running and writes one warning that says why the update failed, and each line in the service log starts with the time it was written. thanks @dvanduzer for the report.
- solstone-tmux no longer stops when tmux is slow to answer. before, if tmux took longer than five seconds to say which sessions were in use, or failed to say, solstone-tmux exited with code 1 and the service started it again. now it skips that one read, writes one warning, and reads again at the next interval. thanks @dvanduzer for the report.

## [2.0.21] - 2026-10-27

### Fixed

- on macos, the service now keeps a log at `~/Library/Logs/solstone-tmux.log`, so when solstone-tmux stops on an error, the reason is there to read. before, what it printed went nowhere. to add the log to a setup you already have, run `solstone-tmux install-service` again after updating. thanks @dvanduzer for the report.
- `solstone-tmux status` no longer reads `update_needed` while your journal is working. when your journal answered without naming its version, solstone-tmux took that as a sign something needed an update, even though your terminal text kept going into your journal. an answer from your journal that stops partway now shows `offline`, as your journal not answering, instead of `update_needed`.
- `solstone-tmux status`, `about`, `--help` and `--version` no longer end with an error when what they print goes to a command that stops reading early, like `head`.

## [2.0.20] - 2026-10-26

### Fixed

- on macos, `solstone-tmux setup` and `solstone-tmux confirm` now ask about your journal's mark in your terminal. before, they stopped without asking, and pairing didn't finish unless you gave the mark's two words with `--mark`.
- when `solstone-tmux setup` or `solstone-tmux confirm` asks about your journal's mark and you type an answer other than the ones it asks for, it now asks again. before, it stopped there and pairing didn't finish.

## [2.0.19] - 2026-10-25

### Added

- terminal screens that go into your journal now include cursor position and pane modes.

### Changed

- pane titles, working directories and executable names now go into your journal by default. before this release they were off unless you turned them on. if you set any of them to `false` in config.json, it stays off. executable names do not include command arguments.

## [2.0.18] - 2026-10-21

### Fixed

- security fix: if you use the relay, solstone-tmux no longer accepts relay certificates issued through emSign certificate authorities whose private keys are now public. before, someone between your computer and the relay could have posed as the relay and seen your computer's relay token. what goes to your journal travels inside a second encrypted connection that ends at your journal, so they could not read it.

## [2.0.17] - 2026-10-20

### Added

- solstone-tmux can now include a pane's title, working directory, and executable with the terminal text that goes into your journal. those settings are off unless you set `pane_title`, `working_directory`, or `executable` to true in config.json. restart the service after you change them.

### Fixed

- if tmux isn't running when solstone-tmux starts (say, right after you install the service or after a reboot), solstone-tmux now waits for it and adds its status indicator once you start tmux. before, it stopped and started again every few seconds until tmux was running. closing your last tmux session no longer stops it either.
- on linux, `solstone-tmux status` now shows `restarting` while the service is about to start again after stopping unexpectedly. before, it showed `inactive`.

## [2.0.16] - 2026-10-18

### Changed

- setup's messages now say "pairing link", the same words your journal uses.

### Fixed

- the install guide now shows where to make a pairing link in your journal and how to use it with solstone-tmux.

## [2.0.15] - 2026-10-09

### Fixed

- with journal 2.0.38 or later, solstone-tmux now saves your journal's current addresses each time it connects, so once it has connected after your journal's address changes, it keeps using the new one without pairing again.

## [2.0.14] - 2026-10-05

### Fixed

- if you pair solstone-tmux with a different journal, terminal text still waiting on this computer goes to the new journal on its first sync after you confirm its mark. before, if your old journal had turned some of it away, solstone-tmux could hold it back from the new one for up to a day.

## [2.0.13] - 2026-10-04

### Fixed

- if solstone-tmux is paused (say, with ctrl-z) and later resumed, the stretch of terminal text open when it paused no longer shows up in your journal as lasting the whole pause.

## [2.0.12] - 2026-10-03

### Added

- `solstone-tmux about` prints a copyable block with your app and journal versions. reports carry the same block.

## [2.0.11] - 2026-10-02

### Fixed

- when you pair through your journal's private network and choose "that doesn't match", solstone-tmux now uses that connection to remove this device from your journal. before, removal could time out, leaving the device listed.

## [2.0.10] - 2026-10-01

### Fixed

- this is a security fix: setup now asks you to confirm your journal's mark, and nothing waiting on this computer goes to the journal until you confirm it's your journal. before, setup finished pairing first and only printed the mark, so anything waiting could go before you had checked it, even to a journal that wasn't yours. if setup can't ask you (run from a script, say), it now needs the mark's two words, shown in your journal's network app, with --mark. if you paired before this update, you won't be asked.
- if you change time zones while solstone-tmux is running, new terminal text goes into your journal under your new local time within a few minutes. before, it kept the time zone it started with until it restarted.
- when the clocks go back an hour, solstone-tmux keeps going through the repeated hour. before, it could stop until it restarted.

## [2.0.9] - 2026-09-30

### Added

- solstone-tmux now includes your computer's time zone with the terminal text that goes into your journal.

## [2.0.8] - 2026-09-28

### Fixed

- security fix: when you paired this computer while your journal's private network was on, even over your own wifi, solstone-tmux sent your pairing link's one-time secret through the relay before checking it had reached your journal, so someone who could sit where the relay sits could have read the secret and used it to pair their own device. it now checks first that it's talking to your journal. please install this update before you pair again. if you paired this way, open your journal's network app and remove any device you don't recognize, including under "nothing added yet".
- after the clocks change for daylight saving time, your terminal text now goes into your journal under the right hour and day right away. before, solstone-tmux kept the old offset until it restarted, and text from that stretch stays an hour off. if it can't tell your time zone, it still uses UTC and now prints a warning when it starts, saying how to fix it.

## [2.0.7] - 2026-09-25

### Changed

- solstone-tmux now removes terminal text from this computer as soon as your journal confirms it has it, or that you removed it from your journal, instead of keeping it as long as the `cache_retention_days` setting said (seven days unless you changed it). anything an earlier version kept goes on the first sync after this update, once your journal has confirmed it. the setting no longer does anything, and a config file that still has it keeps working.

### Fixed

- if you renamed this computer, terminal text from before the rename that hadn't reached your journal yet now gets there. before, it stayed on this computer.

## [2.0.6] - 2026-09-24

### Fixed

- a correction to the 2.0.5 notes: they said a pairing link could point at any address you can reach your journal at. a pairing link that reaches your journal directly has to use an IPv4 address, private or public. a hostname or an IPv6 address isn't accepted there.
- solstone-tmux no longer offers your journal the same terminal text again every minute, or asks it every minute about days your journal already holds.

## [2.0.5] - 2026-09-23

### Changed

- a pairing link can now point at any address you can reach your journal at, not only one on your local network. pairing over the internet works the same way as pairing at home: the link still carries your journal's fingerprint, and this computer still checks it before trusting anything.

## [2.0.4] - 2026-09-18

### Added

- `solstone-tmux setup` now confirms pairing and shows your journal's mark: two colors and two words, so you can check it matches what your journal shows.

## [2.0.3] - 2026-09-16

### Fixed

- setup and runtime errors now name solstone-tmux and your private network directly.
- `solstone-tmux status` could show `revoked` when your journal was down, restarting, or a single request failed for a reason unrelated to pairing. it now shows `offline` in those cases. a computer that's no longer paired still shows `revoked` right away, and if this computer's connection keeps being refused after it reaches your journal, it shows `revoked` once that has gone on for about 30 minutes; time your journal is simply unreachable doesn't count.

## [2.0.2] - 2026-09-15

### Fixed

- fixed a TLS security issue by rejecting handshake messages received at the wrong encryption level.

## [2.0.1] - 2026-09-13

### Added

- `solstone-tmux status` now prints help and problem-report links, with the app version, system version, and current sync state filled in.

### Changed

- release install instructions now start at `updates.solstone.app`, with packages and their verification files at the same sol pbc release origin.

## [2.0.0] - 2026-09-08

### Added

- the current pairing link now works when your journal is available only remotely, and you can keep the same pairing after the first connection.
- after connecting, the paired-device entry in the solstone app now shows this computer's name, operating system, and solstone-tmux version.
- `solstone-tmux status` now shows the current or last known version of your paired journal, and sync progress now updates throughout larger backlogs.

### Changed

- tmux work now appears under a new name in your journal based on the paired computer and source. earlier history stays where it is.

### Fixed

- finished tmux work could leave this machine before your journal confirmed every file. it now stays until that confirmation, and larger completed work can reach your journal.
- if tmux or your journal was not ready when solstone-tmux started, syncing could stop. it now keeps trying, including after a linux reboot.

## [1.0.2] - 2026-08-08

### Changed
- syncing works through larger local backlogs without repeatedly reading work
  your journal already has. changes made locally before delivery are still
  noticed.

### Fixed
- an unexpectedly large reply from your journal could make syncing keep growing
  in memory. that attempt now stops cleanly and reports the reason.
- stopping solstone-tmux no longer has to wait for a large backlog to finish
  syncing. any local cleanup already in progress finishes before shutdown
  completes.
- the tmux sun no longer flickers during routine sync checks. it stays steady
  when your journal already has the finished work, and shows one continuous
  interval while new work is delivered.

## [1.0.1] - 2026-08-03

### Changed
- syncing no longer re-sends what your journal already holds. everything
  still in the local cache used to be sent again and again, for as long as
  your retention setting kept it. nothing changed about what goes into your
  journal, or how long it stays on this machine.

### Fixed
- a reply from your journal that got cut short used to count as a complete
  one. syncing now treats those as unfinished and tries again.
- a large reply from your journal could be cut short before all of it
  arrived. those now come through intact.

## [1.0.0] - 2026-07-29

### Changed
- solstone-tmux is now one native application that experiences tmux sessions
  along with you, keeps observations local while your journal is unavailable,
  and syncs when the connection returns.
- on Linux, the first native run automatically adopts the previous stream,
  intervals, retention, and status-indicator settings and continues using the
  existing cache in place. pairing is fresh; previous credentials are not
  carried over.
- native packages support Linux on x86_64 and aarch64 and macOS on Apple
  silicon. Intel macOS, 32-bit systems, and Windows are not supported.

## [0.3.1] - 2026-06-15

### Changed
- a tmux observer running on a machine that reaches your journal over your private link now streams cleanly to that journal. it sends its own handle so the journal can tell which stream it is, kept separate from the machine's link identity. an observer on the same machine as your journal is unaffected.

## [0.3.0] - 2026-06-14

### Changed
- setup no longer asks for a journal url — the observer connects to your
  journal automatically. if your journal runs on another machine you reach
  directly, set its address with `solstone-tmux setup --server-url <url>`.

## [0.2.0] - 2026-06-13

### Added
- the observer now sets itself up with your journal automatically on first run.
  when you run setup it registers itself and remembers the connection, so
  there's no separate manual key step. if your observer machine can't reach
  your solstone host, you can still paste a key from your journal during setup.

### Changed
- the observer keeps observing your tmux sessions locally even when your
  journal is briefly unreachable, then syncs once the connection is back.

## [0.1.0] - 2026-05-19

### Added
- initial release of solstone-tmux, a standalone tmux observer for solstone.
- experiences your tmux sessions along with you, accumulating observations to a local cache and syncing them to your journal.
