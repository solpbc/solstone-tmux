# tmux frame format

Each screen JSONL envelope has `frame_id`, `timestamp`, `requests`, `analysis`,
and `content.tmux`. The tmux object contains `session`, `window` (id, index,
name), `windows` (the roster, including active state), and `panes`.

Each pane contains `id`, `index`, `left`, `top`, `width`, `height`, `active`, and
`content` (the ANSI screen text). These additional snapshot fields are optional
for readers of older frames:

| field | type | tmux format | meaning |
|---|---|---|---|
| `cursor_x` | unsigned integer | `#{cursor_x}` | zero-based column in the underlying pane screen |
| `cursor_y` | unsigned integer | `#{cursor_y}` | zero-based row in the underlying pane screen |
| `cursor_visible` | boolean | `#{cursor_flag}` | the pane application's cursor-visibility flag |
| `alternate_on` | boolean | `#{alternate_on}` | alternate-screen buffer is active |
| `pane_in_mode` | boolean | `#{pane_in_mode}` | tmux copy or another pane mode is active |

All five formats exist in [tmux 1.8's upstream source](https://github.com/tmux/tmux/blob/1.8/format.c).
That is the verified format floor; the live fixture was taken on tmux 3.4.
An alternate screen is common in editors, but does not identify every full-screen
application. Copy mode does not turn the underlying cursor into a scrollback
selection cursor: `capture-pane -p -e` still reads the underlying screen.

Metadata is read by `list-panes` immediately before the per-pane `capture-pane`
reads within the capture transaction. These are sequential reads, not an atomic
snapshot; a running application can change between them. The coordinates belong
to that sampled screen, not a later poll. They do not establish what the owner
typed or whether the visible text is an input prompt.

The screen dedupe rule uses window identity and pane text only. Cursor-only and
mode-only changes do not produce a screen frame. The next changed-text frame
includes its own metadata sample. Executable point samples are separate desktop
activity records on each admitted poll; executable names do not include command arguments.

The journal retains these fields. Existing screen-talent and terminal timeline
projections ignore them until a consumer is deliberately added.
