# Changelog

All notable changes to solstone-tmux will be documented in this file.

Format adapted from [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [1.0.4] - 2026-08-28

### Changed

- tmux syncing now works through your paired-device connection to your journal.

### Fixed

- if your paired journal is temporarily unavailable when solstone-tmux starts, it now keeps trying to reconnect instead of stopping.
- pairing now works with the current link from your journal, including when it is reachable only remotely.
- finished tmux work now stays on this machine until your journal can confirm it has every file.
- larger completed tmux work can now reach your journal.

## [1.0.3] - 2026-08-28

### Changed
- tmux syncing now works through your paired-device connection to your journal.

### Fixed
- the current pairing link from your journal now works.
- finished tmux work now stays on this machine until your journal can confirm it has every file.
- larger completed tmux work can now reach your journal.

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
