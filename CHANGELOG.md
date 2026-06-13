# Changelog

All notable changes to solstone-tmux will be documented in this file.

Format adapted from [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

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
