# Localization experiment

This is an isolated extraction experiment. The embedded catalogs have English
and a synthetic `en-XA` locale; they contain no production translations and use
no translation services.

`en-US.ftl` and `en-XA.ftl` contain restricted Fluent. Each message has its own
JSON sidecar in `meta/`, with purpose, surfaces, named argument types, an optional
length hint, risk class, screenshot references, and the source revision.
`plan.json` preserves the initial assignments and mechanical edits;
`judgments.json` records the candidate batches. `exclusions.json` explains the
original source literals kept outside presentation. Test fixtures and assertion
messages are inventoried separately by the discovery tool.

Logical IDs use `surface.feature.element.state` components. Fluent does not
accept dots in message identifiers, so the catalog lowers each dot to a hyphen.
The generated Rust enum is the application API. Preserve assigned IDs when
wording changes; do not rediscover IDs from the edited prose.

The formatter rejects missing, duplicate, unexpected, and incorrectly typed
arguments. Most extracted arguments are explicitly typed as preformatted text
to preserve existing Rust `Display` and debug rendering. Relative-age counts are
numbers and use Fluent plural selection. Semantic numeric typing for all other
counts, durations, and exit statuses remains unfinished. There are no active
date messages. Message references, attributes, multiple selects, and functions
other than `NUMBER` or `DATETIME` are outside the restricted contract; terms
resolve within each locale file.

CLI locale selection is `--lang en-US|en-XA` before the command, followed by the
first nonempty `LC_ALL`, `LC_MESSAGES`, or `LANG`. Unsupported environment locales fall back
to English. `en-XA` accents human prose, adds about 35% of its alphabetic
character count as expansion, and brackets messages. Immutable terms preserve
commands, flags, answer tokens, and example mark words. Dynamic values remain
unchanged. This is a layout stress fixture, not a translation or an RTL proof.
Fluent isolation is disabled to preserve the original English output bytes; RTL
isolation needs a separate design.

Pairing outcomes retain message IDs until output. Accepted input still uses
`yes/y/no/n/continue/cancel` and typed parser codes. The alias-table prototype is
compiled only in tests and cannot enable additional runtime acceptance. The
mark's words and colors are protocol data. `--mark` remains explicit mark
verification and cannot skip the ceremony.

Protocol fields, serialization, diagnostic codes, CLI status fields, support
report payloads, and service artifacts remain canonical. Human About rendering
is separate from its canonical report value. Service and host-fact commands run
under `LC_ALL=C` and `LANG=C`. Three machine parser exceptions still inspect English
service-manager/sysctl messages; locale pinning contains them but does not
eliminate that dependency. Tmux keeps its text locale and explicitly uses `-u`
to preserve Unicode even with a `C` parent locale.

Changing human help or diagnostics can break scripts or owner agents that parse
English. Stable status/protocol fields reduce that risk; they do not establish a
policy for existing human-output consumers. Transitive library and OS error
messages are passed as arguments and are not all localized.

From the repository root:

```sh
cargo run --locked --example i18n_inventory
cargo run --locked --example i18n_lint -- --negative-control
cargo run --locked --example i18n_corpus
make i18n-check
make ci
```

Discovery uses `syn` on every Rust source and test file and descends into nested
macro arguments. The lint heuristically checks prose literals and one-word
labels written directly by output macros. Its negative controls use a new source file. The original Rust
renderer captured the English corpus before application; `i18n_corpus` reproduces
it after application. The corpus contains synthetic text arguments plus actual
help, usage-error, and age/plural paths. It does not instantiate every error
variant or exercise every transitive error. Three deliberate pairing wording
goldens remain alongside the corpus. Some older substring assertions still need
semantic migration; `.text()` expectations are rendering checks, not ID checks.

`pad_terminal` demonstrates Unicode terminal-cell padding for CJK and combining
marks. Existing path and wire byte limits remain byte limits. No existing
multi-column terminal layout needed conversion.

The one-time discovery/classification/generation/application driver is kept
outside this Rust-only repository. The in-repository tools reproduce discovery,
lint, negative controls, and the English corpus. Sidecar descriptions are
purpose-oriented generated context, with additional care for pairing, help, and
About; they still need translator review. Empty screenshot lists and null length
hints mean evidence was not collected, not that a message has unlimited space.

The old last-seen fragment remains cataloged only for the original-template
comparison. Production uses a whole journal-and-age template. Count catalog
entries, original strings, AST occurrences, and assertions separately.
