# Pricing file

StatsAI estimates the API-equivalent cost of usage from published per-token
prices. Those prices live in one JSON file,
[`crates/statsai-pricing/pricing.json`](../crates/statsai-pricing/pricing.json),
not in Rust code. Every binary embeds the file it was built with. Clients also
fetch the copy on `main` once a day and keep it in their SQLite store, so a
price change reaches users without a release.

This page is for whoever edits prices, and for users who want to know where
their cost estimates come from.

## Why prices are a data file

Prices change more often than StatsAI ships. Promotions start and end, and
price cuts land between releases. When prices were Rust constants, every change
waited for a release, and users saw corrected costs only after they upgraded.

Now a price change is a pull request that edits `pricing.json`. Once it merges
to `main`, clients pick it up within a day and reprice their stored estimates.

## Where the file lives

- In the repository: `crates/statsai-pricing/pricing.json`. It sits inside the
  crate so the crate can embed it with `include_str!` and the published crate
  ships it.
- Clients fetch it from
  `https://raw.githubusercontent.com/starkdmi/statsai/main/crates/statsai-pricing/pricing.json`.
  `STATSAI_PRICING_URL` replaces that URL, for testing. A file adopted from
  it stays in the store for good, because versions never go down, so point
  such a test at a scratch store with `--store`. `statsai-dev env prod`
  removes the variable.

## Format 1

A short example, trimmed from the real file:

```json
{
  "format": 1,
  "version": 7,
  "catalog": "official:2026-11-18",
  "models": {
    "gpt-5.6-sol": {
      "note": "https://developers.openai.com/api/docs/pricing",
      "prices": [
        { "from": null, "input": 5, "cache_write": 6.25, "cache_read": 0.5, "output": 30 },
        { "from": "2026-08-21", "input": 4, "cache_write": 5, "cache_read": 0.4, "output": 20, "note": "promotion" }
      ],
      "long_context": { "above": 272000, "input": 2, "output": 1.5 },
      "fast": { "multiplier": 2 }
    },
    "gpt-5.6-luna": {
      "prices": [
        { "from": null, "input": 1, "cache_write": 1.25, "cache_read": 0.1, "output": 6 },
        { "from": "2026-07-30", "input": 0.2, "cache_write": 0.25, "cache_read": 0.02, "output": 1.2, "note": "price cut" }
      ]
    },
    "gpt-5.4": {
      "prices": [
        { "from": null, "input": 2.5, "cache_write": 2.5, "cache_read": 0.25, "output": 15 }
      ]
    }
  },
  "equivalents": {
    "codex-auto-review": [
      { "from": null, "model": "gpt-5.4" },
      { "from": "2026-07-30", "model": "gpt-5.6-luna" }
    ]
  },
  "names": {
    "claude-opus-5-5": ["claude-opus-5-5-thinking", "claude-opus-5.5"]
  },
  "patterns": [
    { "contains": ["claude-fable-5-2", "claude-fable-5.2"], "model": "claude-fable-5-2", "whole_version": true }
  ],
  "reversed": {
    "claude-5.5-opus": "claude-opus-5-5"
  }
}
```

### Top-level fields

| Field | Type | Meaning |
|---|---|---|
| `format` | integer | Structure version of the file. See [`format` and `version`](#format-and-version). |
| `version` | integer, 1 to 1,000,000 | Goes up on every change to the file. |
| `catalog` | string | Written into each estimate's `pricing_version`. Non-empty, at most 100 characters, contains `:`, for example `official:2026-11-18`. |
| `models` | object | Prices, keyed by lowercase catalog model name. |
| `equivalents` | object | Names priced as another model, by date. |
| `names` | object | Exact spellings that normalize to a canonical name. |
| `patterns` | array | Ordered substring rules for names with no exact match. |
| `reversed` | object | Names Cursor writes with the family after the version. |

Unknown fields are rejected anywhere in the file. `note` is allowed on a model,
a price period, an equivalent period and a pattern. Code ignores it; use it for
sources and explanations.

### `models`

Each key is a lowercase catalog name, such as `claude-opus-5-5` or `gpt-5.5`.
Two models with the same rates are still two entries.

| Field | Required | Meaning |
|---|---|---|
| `prices` | yes | List of price periods, oldest first. |
| `source` | no | Pricing-source prefix: `cursor_model_pricing` or `xai_api_pricing`. Lowercase letters, digits and `_`. Absent means `{provider}_api_pricing`. Estimates record `{source}:{observed name}`. |
| `long_context` | no | A higher tier for long prompts. |
| `fast` | no | Fast-mode pricing. |
| `note` | no | Free text. |

A **price period** has `from` and four rates, in USD per million tokens:

| Field | Meaning |
|---|---|
| `from` | `null` in the first period, which applies since forever. A `YYYY-MM-DD` UTC date in every later period. Dates strictly increase. |
| `input` | Uncached input. |
| `cache_write` | Cache writes. Where a provider bills cache writes as input, this equals `input`. |
| `cache_read` | Cache reads (cached input). |
| `output` | Output, including reasoning. |
| `note` | Optional free text. |

An event is priced with the last period whose `from` is on or before the
event's UTC date.

**`long_context`** is `{ "above": N, "input": x, "output": y }` or
`{ "at_least": N, "input": x, "output": y }`, with exactly one of `above`
(prompt tokens greater than N) and `at_least` (prompt tokens N or more). Prompt
tokens are input plus cache writes plus cache reads. The tier applies only when
a record is one known request, because an aggregate cannot be split by prompt
size. `input` multiplies the input, cache-write and cache-read rates; `output`
multiplies the output rate.

**`fast`** has exactly one of:

- `multiplier`: fast rates are the standard rates of the period in effect that
  day, all four times this number.
- `prices`: fast rates as their own list of periods, with the same rules as
  `prices`.

It may also have:

- `max_prompt: N`: fast mode does not serve one known request whose prompt is
  over N tokens, so that request is priced at standard rates.
- `long_context`: replaces the model's `long_context` while fast rates apply.
  Without it, the model's `long_context` applies on top of the fast rates.

### `equivalents`

Maps an observed name to periods of `{ "from", "model" }`, with the same date
rules as prices. Events in a period are priced as that `model`: its rates, fast
mode, long-context tier and source. The observed name stays on the event; only
the price comes from the target. Every target must be a key in `models`, and a
name cannot be in both `models` and `equivalents`.

### `names`

The exact-match table of the name normalizer. Each key is a canonical name; its
value lists other spellings of it. A lowercased name equal to a key maps to
itself, and a name equal to a listed spelling maps to its key. A spelling
appears once in the whole table and is never also a key. A key does not need a
`models` entry.

### `patterns`

Ordered substring rules for names with no exact match, such as dated snapshot
ids and names a proxy wraps. The first rule with any matching `contains`
string wins, so order matters: `claude-fable-5-2` must come before
`claude-fable-5`. With `whole_version: true`, the match must not be followed by
an ASCII digit, so `claude-fable-5-2` does not match the Fable 5 snapshot
`claude-fable-5-20260610`. The default is a plain substring match.

### `reversed`

Cursor writes some Claude names with the version before the family, for example
`claude-4.5-sonnet-thinking`. The code reads `claude-<version>-<family>` from
the start of the name, drops the rest, and looks that key up here. Keys start
with `claude-`.

A name is normalized in this order: an exact match in `names`, then the first
matching rule in `patterns`, then `reversed`. A name that none of them matches
stays as observed, lowercased.

### Limits

- Names (in every section): non-empty, at most 128 bytes, lowercase ASCII, no
  whitespace.
- Rates: finite, from 0 to 1000 USD per million tokens.
- Multipliers (`fast.multiplier`, `long_context.input` and `output`): finite,
  greater than 0 and at most 20.
- Token thresholds (`above`, `at_least`, `max_prompt`): from 1 to 100,000,000.
- `version`: from 1 to 1,000,000. CI also fails a change that raises it by
  more than 10 (see [Editing the file](#editing-the-file)).
- The whole file: at most 1 MiB.

## `format` and `version`

`version` goes up on every change to the file, and never goes down. Clients
compare versions to decide whether a file is newer than the one they have.

`format` changes only when the structure changes: a new field, or a rule older
code cannot follow. A binary reads every format up to the one it was built for.
A new format therefore needs a release, and clients on older releases stop
taking new files from `main` until they upgrade. Raise `format` only together
with the code that reads it.

A binary refuses a store whose stored file has a format it cannot read, and
asks you to upgrade, whether or not the refresh is on. Commands that use
price-derived data stop with that message. So does `statsai daemon --watch`
started on such a store: it exits, and a LaunchAgent keeps restarting it into
the same message. A watch daemon already running when such a file is stored
keeps running, says why once, and skips its scan passes. In practice only
upgrading StatsAI clears this, since a newer file from `main` is in the new
format too. A running watch daemon restarts when its executable is replaced,
and scans every watched source again when it starts.

A binary never refuses a store because its stored file or its estimates have a
newer `version`. While the refresh is on it uses a newer stored file. It does
not reprice a store whose applied version is higher than its own. When a scan,
sync, import or watch pass ends in such a store and the binary can read no file
at the applied version, it lowers the applied version to its own. After that,
a binary with a higher version reprices every estimate to its version,
including estimates priced with a newer one (see
[How clients refresh](#how-clients-refresh)).

### Releases before pricing files

Releases before pricing files (0.7.3 and earlier) compare the store's
`pricing.applied_ruleset_version` with the ruleset version compiled into them
(5 in 0.7.3), and refuse a store priced with a higher one:
`database pricing ruleset version 7 is newer than this StatsAI binary supports (5)`.
Releases with pricing files embed version 7 or higher. The first time one of
them runs a command that uses price-derived data on a store, it reprices the
store to its version, whether or not the refresh is on. From then on, every
older release that shares the store stops there: an older CLI on your `PATH`,
an older StatsAI for Mac and the CLI bundled with it, and an older watch daemon
the next time it starts. An older daemon that is already running checks prices
only when it starts, so it keeps writing estimates at its own prices that
nothing reprices until the next version. Turning the refresh off in the newer
binary does not prevent this, because its embedded file is already newer than
any older release's ruleset. Upgrade every binary that shares the store (the
CLI, the watch daemon or its LaunchAgent, and the desktop app) together, and
restart the daemon.

## Editing the file

- **Change a price by adding a period.** Never edit or delete a period that
  has started. Usage priced under it must keep its rate. A period dated in the
  future can still be corrected until its day arrives. A new period can start
  in the past, for a change found late; clients reprice the usage it covers,
  with the exceptions under [How clients refresh](#how-clients-refresh). An
  aggregate summary whose period spans the new period's `from` date then has
  an unknown cost, because its tokens cannot be split between the two rates.
- **`from` is a UTC day.** The new rate applies to events on that UTC date and
  later.
- **The first period has `from: null`.**
- **To end a promotion, add a period with the regular rate**, dated the day the
  promotion ends. Do not delete the promotional period.
- **Raise `version` on every change**, even to a note or to whitespace. Never
  lower or reuse it: a client ignores a file whose version is not higher than
  its own, so the change would not reach anyone. If another pull request raised
  `version` first, update your branch and raise it again. Two open pull
  requests that raise it to the same number can each pass CI; whichever merges
  second must be updated and raise it again (see below).
- **Raise `version` by one.** CI fails a change that raises it by more than
  10, such as `80` typed for `8`, and clients reject a file whose `version` is
  above 1,000,000. A version set too high cannot be taken back: every client
  that adopts that file ignores each later file with a lower version, and
  only editing its store by hand would recover it.
- **Set `catalog` to the date of the change**, for example
  `official:2026-11-24`.
- **Add a model** by adding a `models` entry, plus whatever `names`,
  `patterns` or `reversed` entries the normalizer needs to reach it from the
  spellings tools report. Use `equivalents` for a name that should be priced
  as another model.
- **Never remove a model or an `equivalents` entry.** Stored events still
  name it.
- **Put sources in `note`**, with the provider's pricing URL.

`long_context`, `fast.multiplier`, `fast.max_prompt`, `fast.long_context` and
`source` have no dates. Changing one changes the price of all past usage, so
change them only to fix a mistake.

The check described below enforces most of these rules.

## What CI checks

- The `statsai-pricing` tests parse the file with the same validation every
  client runs: structure, unknown fields, limits, period order and name rules.
- The **Pricing file** job runs
  [`scripts/check-pricing-file.sh`](../scripts/check-pricing-file.sh). On a
  pull request it compares the file in the merge commit GitHub built with that
  commit's first parent, the base tip the pull request merges onto, so a
  re-run after `main` moved still checks only the pull request's own change.
  On every push to `main` it compares the new tip with the one the push
  replaced. It fails when:
  - the file changed and `version` did not go up;
  - `version` went up by more than 10, or is above 1,000,000;
  - `format` went down;
  - a `models` or `equivalents` entry was removed;
  - a period whose `from` is `null` or on or before today's UTC date was
    changed or removed. This covers `prices`, `fast.prices` and `equivalents`.

  The same job first runs
  [`scripts/test-check-pricing-file.sh`](../scripts/test-check-pricing-file.sh),
  which runs the check on edited copies of `pricing.json`, one for each rule
  above and for edits it must accept, such as one to a period that has not
  started, and also against a git ref in a scratch repository, as the job
  runs it. It fails when the check's exit status or message is not the one
  expected.

`main` does not require pull requests to be up to date before they merge. Two
pull requests that both raise `version` from 7 to 8 each pass against the base
they saw, and when they set the same `catalog` they can merge without a
conflict. `main` then holds a second, different file at version 8, which
clients that already took the first one never adopt. The push check fails
`main` as soon as that happens. Fix it at once with a pull request that raises
`version` again; before you merge a pricing pull request, update it if another
one merged since its checks ran.

Run the same check locally before you push:

```sh
scripts/check-pricing-file.sh origin/main
```

A pricing pull request changes only `pricing.json`, so the tests must pass
whatever periods it adds. A test must not assume that a model has no period
after the ones it has today: one that prices at the current time, or on a day
after the newest period, fails as soon as a new period starts. Take expected
rates from the embedded table (`embedded_pricing().pricing_for_model_on(model,
day)`), price on a past day inside an existing period, or build the test's own
table. No CI job simulates such an edit; the simulation used to check the
suite (append a period to every model and run the tests) is not part of CI.

## How clients refresh

- `statsai scan` and `statsai sync` (every mode) check, once the store is
  open, whether the refresh is on and due. Only then do they start a short
  background process, `statsai pricing refresh --background` (a hidden mode of
  `statsai pricing refresh`), with its own connection to the store. The
  command never waits for it, so a scan never waits on the network, and the
  process finishes its attempt even when the command exits first. It prints
  nothing, always exits successfully, and never reprices: the next command
  that reads prices uses a file it stored. It runs the same `statsai` binary
  with the same environment, has no standard input or output, so a program
  that reads the command's output is not held open by it, and runs in its own
  process group (detached from the console on Windows), so a Ctrl-C that stops
  the command does not stop it. It inherits no open file from the command, so
  it holds neither the scan lock nor the command's store connection, and it
  never starts another process.
- The watch daemon (`statsai daemon --watch`) starts a refresher thread that
  checks every hour whether a refresh is due.
- A refresh is due at most once a day per store: when the last check is at
  least 24 hours old and no attempt started in the last 10 minutes. A check or
  attempt time more than that far in the future, left by a clock that was set
  wrong, counts as absent. The schedule lives in the store's metadata, so every
  process sharing a store shares it:
  - `pricing.refresh.attempt_at` is written before the request, which keeps
    other processes from fetching at the same time. The write is an atomic
    compare-and-set from the value read when the refresh was found due, so of
    several processes that find it due at once, exactly one claims the attempt
    and fetches; the others skip it. `statsai pricing refresh` claims
    unconditionally, since it ignores the schedule;
  - `pricing.refresh.checked_at` is written after every completed attempt,
    whatever its result;
  - `pricing.refresh.etag` holds the `ETag` of the last file that was stored
    or was not newer.
- The request is a `GET` with `User-Agent: statsai/<version>` and, when an
  `ETag` is stored, `If-None-Match`, so an unchanged file costs a `304` and no
  body. Once the host name resolves, it times out after 10 seconds overall,
  connecting and reading included; the name lookup itself is bounded only by
  the system resolver. It reads at most 1 MiB. Any status other than `200` and `304` is a
  failure. Only `https` URLs are fetched; `STATSAI_PRICING_URL` may also be a
  plain `http` URL to `127.0.0.1` or `localhost`, for testing. A failed attempt
  counts as the day's check; `statsai pricing refresh` tries again at once.
- A fetched file is adopted only when it parses and passes validation (so no
  rate is negative or out of bounds), its `format` is one the binary reads, and
  its `version` is higher than any copy the process already has.
- An adopted file is stored in the SQLite store under the `pricing.file`
  metadata key. Only a valid file of a known format with a higher `version`
  than the stored copy replaces it. A stored copy that is invalid in a format
  the binary reads is ignored with a warning, and the next file stored
  replaces it.
- While the refresh is on, every command uses the newest copy it can read:
  the file embedded in the binary or the stored copy, whichever has the higher
  `version`. When a binary reprices the store with an embedded file newer than
  the stored copy, it also stores its embedded file, so other binaries sharing
  the store use it.
- The next command that uses price-derived data reprices stored estimates with
  the newer version, and so does the watch daemon before its next scan pass.
  Changed rollups are marked dirty, so the next incremental
  `statsai sync --sink http` uploads the corrected costs. Neither `--full` nor
  `--rebuild-rollups` is needed. Provider-reported costs are never replaced.
- Estimates that adapters such as Grok Build and OpenCode sum from per-request
  records are the exception. Repricing keeps them, since the stored aggregate
  cannot reproduce them, so they keep the older rates until their sources are
  scanned again; `statsai scan --no-cache` re-reads them all.
- The store records one applied version for all its estimates, in
  `pricing.applied_ruleset_version` (its catalog in
  `pricing.applied_catalog_version`): the oldest version any of them may be
  priced with. `statsai pricing status` shows it as `store's applied pricing`.
  A process that prices with a higher version reprices the store when it opens
  it; a process at that version or a lower one does not.
- Repricing goes backward only after a lowering (below). A binary whose copy
  has a lower version than the store's applied version leaves the stored
  estimates as they are. But once a process has lowered the applied version, a
  process whose version is above the lowered one reprices every estimate to its
  own version, including estimates priced with a newer version.
- A scan, sync, import or watch pass can write estimates priced with an older
  version than the store's applied version: another process repriced the store
  while it ran, or the store was already ahead of every file the process can
  read. So when such a command or pass finishes, whether or not it wrote
  anything, and finds the store's applied version higher than the version it
  priced with:
  - when it can read a file at the applied version (the store's copy, or the
    file built into the binary), it reprices the whole store again with that
    file and logs the repricing report;
  - otherwise, for example with the refresh off when the stored copy is newer
    than the applied version, it lowers the applied version and catalog to the
    ones it priced with, reprices nothing, and logs
    `lowered the store's applied pricing version from N to M`. The next process
    that prices with a newer file, one with the refresh on or a newer binary,
    then reprices every estimate, those this command wrote included. A process
    at the lowered version finds the store current and reprices nothing.
- Commands that open the store without repricing it, such as
  `statsai quota` and `statsai status`, use the newest copy they can read for
  any estimate they make.
- The background refresh of `scan` and `sync` is silent, whatever its result;
  `statsai pricing status` shows the stored file and when the last check ran.
  The daemon logs
  `daemon: pricing: stored pricing file version N from GitHub; it applies at the next scan pass`,
  and logs a refused file or a failed request, at most once per daily check.

## Turning the refresh off

Either of these turns it off:

- `statsai pricing auto-refresh off`, a setting kept in the store.
  `statsai pricing auto-refresh on` turns it back on.
- `STATSAI_PRICING_REFRESH=off` in the environment. `0`, `false` and `no` work
  too, in any case.

With the refresh off, StatsAI never fetches the file and prices with the file
embedded in the binary. The one exception is a store whose estimates were
already repriced with a newer stored copy: StatsAI keeps pricing with that copy,
so the store does not mix two versions and no price goes back. A store priced
with a newer version it holds no copy of, or whose copy is newer still and not
yet applied, keeps its estimates, and new ones use the embedded file. When a
scan, sync, import or watch pass finishes in such a store, whether or not it
wrote anything, it lowers the store's applied version to the embedded file's,
without repricing anything. The store then reads as current to every process
that prices with that file, and the first process with a newer file, such as
one with the refresh on, reprices all of it to its version: the estimates
written with the embedded file, and also any priced with a version newer than
its own.

This also holds for a watch daemon that is running when the setting changes:
it checks the setting before every pass, and its prices stay the store's.

Tests and `cargo run` started inside this repository run with the refresh off:
[`.cargo/config.toml`](../.cargo/config.toml) sets
`STATSAI_PRICING_REFRESH=off`. A value set in your shell, such as `on`, takes
precedence, except in tests that start the `statsai` binary: they set
`STATSAI_PRICING_REFRESH=off` on it themselves, so a test never fetches from
GitHub. The tests of the background refresh set it to `on` on the binary they
start, with `STATSAI_PRICING_URL` pointing at a server on `127.0.0.1`. The
desktop golden captures in statsai-ui also run with it off, so their costs come
from the embedded file.

## Commands

```sh
statsai pricing status              # the pricing files, the refresh state and the last check
statsai pricing status --json
statsai pricing refresh             # fetch now, ignoring the daily schedule
statsai pricing auto-refresh off    # or on
```

`statsai pricing status` reports:

- the embedded file's version, format and catalog;
- the stored copy's version, format and catalog, or that there is none, that
  it is unreadable and ignored, or that its format is one this binary cannot
  read;
- the version and catalog the store's estimates were last repriced with;
- the version price-derived commands use, and whether it is the embedded file
  or the stored copy, or that they refuse the store because of an unknown
  format;
- whether the refresh is on, and if not, whether the store's setting or
  `STATSAI_PRICING_REFRESH` turned it off;
- when the last check completed and the last attempt started;
- whether an `ETag` is stored, and the URL the refresh fetches.

It works on a store whose stored copy has an unknown format.

`statsai pricing refresh` fetches the file at once, even when a check ran in
the last day. When the refresh is off it refuses and exits non-zero, naming
the cause: run `statsai pricing auto-refresh on` for the setting, or unset
`STATSAI_PRICING_REFRESH` for the variable. When it stores a newer file, it
reprices the store immediately and prints the repricing report. When the file
is unchanged or not newer, it brings the store's estimates to the newest
version it can use and prints `Pricing is current at version N`. A refused file
or a failed request prints the reason and exits non-zero.

`statsai pricing auto-refresh on|off` sets the store's `pricing.refresh`
setting and says what it means. When `STATSAI_PRICING_REFRESH` turns the
refresh off for the process, it says that the variable overrides the setting.

Under `statsai-dev env prod`, `statsai-dev pricing refresh` and
`statsai-dev pricing auto-refresh` are refused, because they change
production's pricing state. They run under `env dev` and `env local`.

`statsai store supported-pricing-format` prints the newest format the binary
reads. `statsai store supported-pricing-ruleset-version` prints the `version`
of the embedded file. Neither opens a store.

## What stays in Rust

Some pricing rules are mechanics, not data. Changing them needs a code change
and a release:

- How tiers and multipliers combine, the one-known-request rule for
  long-context tiers, and integer micro-USD arithmetic.
- Cursor-hosted Grok 4.7: its own tier above 256k prompt tokens (2x, or 1.5x
  on fast rates), priced under `cursor_model_pricing`, and never xAI's 200k
  tier.
- 1-hour cache writes for `claude-*` models, billed at 2x the input rate.
- Name handling around the tables: the `-fast` suffix, the `anthropic/` and
  `openai/` prefixes, and the parser behind `reversed`.

When a price change needs one of these to change, or needs a field format 1
does not have, it ships with a release.
