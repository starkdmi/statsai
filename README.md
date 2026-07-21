# statsai

Local-first AI usage statistics for Codex, Claude Code, OpenCode, and Grok
Build, with room for future local/provider sources.

Status: early implementation. The public API is not stable yet.

## Scope

`statsai` is a Rust-first utility package, not a hosted dashboard. It provides:

- a CLI for scanning, reporting, source config, account mapping, subscription mapping, export, sync, and daemon mode
- a Rust SDK facade for embedding
- local provider adapters for direct trusted source reads
- a local SQLite store
- abstract sync sinks for stdout/file/HTTP
- a loopback-only daemon API for local widgets and toolbar integrations

The first adapters target Claude Code JSONL usage roots, Codex session logs,
OpenCode's local SQLite database, and Grok Build local session summaries.
External aggregate reports, manually transcribed screenshots, or provider
`/usage` summaries are supported as reported summary imports. They stay separate
from trusted raw local events so reports can show direct usage and
imported/manual gaps without double-counting.

## Workspace

- `crates/statsai-core`: normalized types, stable IDs, schema models, privacy metadata
- `crates/statsai-adapters`: local adapters for Claude Code, Codex, OpenCode,
  and Grok Build
- `crates/statsai-store`: SQLite persistence
- `crates/statsai-sync`: pluggable sync sink trait plus stdout/file/HTTP sinks
- `crates/statsai-daemon`: localhost API
- `crates/statsai-sdk`: Rust SDK facade
- `crates/statsai`: `statsai` binary
- `crates/statsai-menubar`: macOS menu bar app (`StatsAI.app`)

## Install (macOS)

```sh
brew install starkdmi/tap/statsai
```

Or use the GitHub release installer script:

```sh
curl -LsSf https://github.com/starkdmi/statsai/releases/latest/download/statsai-installer.sh | sh
```

Or install the published crate:

```sh
cargo install statsai
# or
cargo binstall statsai
```

GitHub Releases also ship `statsai-universal-apple-darwin.tar.xz` and
`StatsAI.app.zip`.

## Local Checks

Use the same Rust checks locally that GitHub runs in CI:

```sh
./scripts/rust-ci.sh full
```

To install repo-local git hooks that run `fmt` + `clippy` before commit and the
full Rust CI suite before push:

```sh
./scripts/install-git-hooks.sh
```

For an exceptional bypass, set `STATSAI_SKIP_LOCAL_CI=1` for that one command.

## CLI Examples

```sh
cargo run -p statsai -- scan --provider codex --preview
cargo run -p statsai -- scan --provider opencode --preview
cargo run -p statsai -- scan --provider grok-build --preview
cargo run -p statsai -- source add --provider codex --path "$HOME/.codex-work"
cargo run -p statsai -- source disable --source-id src_123
cargo run -p statsai -- source enable --source-id src_123
cargo run -p statsai -- source remove --source-id src_123
cargo run -p statsai -- source remove --source-id src_123 --delete-data
cargo run -p statsai -- source connect --path "$HOME/.codex-work" --email work@example.com --label work --started-at 2026-05-01
cargo run -p statsai -- source history --path "$HOME/.codex-work"
cargo run -p statsai -- source disconnect --path "$HOME/.codex-work" --email work@example.com --ended-at 2026-06-01
cargo run -p statsai -- subscription add --provider claude --email personal@example.com --plan Pro --price 20 --started-at 2026-05-15 --paid-at 2026-05-15
cargo run -p statsai -- subscription change --provider codex --email work@example.com --plan Pro --price 200 --started-at 2026-06-01
cargo run -p statsai -- import summary --path ./reported_usage_summaries.json --dry-run --verbose
cargo run -p statsai -- report weekly
cargo run -p statsai -- report monthly --subscriptions
cargo run -p statsai -- task list
cargo run -p statsai -- task show work_123 --include-evidence
cargo run -p statsai -- task verify accept work_123
cargo run -p statsai -- task benchmark
cargo run -p statsai -- task export --level span --format jsonl
cargo run -p statsai -- conversation collect --provider codex --verbose
cargo run -p statsai -- conversation list
cargo run -p statsai -- conversation search 'database AND migration'
cargo run -p statsai -- conversation show conv_123
cargo run -p statsai -- conversation export conv_123 --format json
cargo run -p statsai -- conversation stats
cargo run -p statsai -- sync --sink file --output ./statsai-sync-batch.json
cargo run -p statsai -- sync --sink http --since-last
cargo run -p statsai -- sync --sink http --endpoint http://127.0.0.1:8787/api/sync/batches
cargo run -p statsai -- sync --sink http --endpoint http://127.0.0.1:8787/api/sync/batches --since-last
cargo run -p statsai -- sync --status
cargo run -p statsai -- auth login
cargo run -p statsai -- auth status
cargo run -p statsai -- sync --sink http --verify
cargo run -p statsai -- schema sync-batch
```

The primary model is:

- add a source path with `source add --provider ... --path ...`
- connect that source to a canonical account with `source connect --email ... --started-at ...`
- register time-bounded subscription periods with `subscription add --started-at ...`

`account list` is read-only. Canonical accounts are created implicitly the first
time you use `--email`, `--provider-user-id`, or `--provider-account-id` on a
source connection or subscription command. Labels like `personal` or `work` are
display metadata only, not account identity.

`scan --preview` reads configured and default local sources without writing to SQLite. It reports normalized usage events, not raw log rows, and shows the token split when the provider logs expose it:

```text
codex account=work path=~/.codex-work usage_events=123 input=1,000,000 cached=800,000 output=20,000 total=1,030,000 est_cost=$1.23
```

`scan` persists normalized events idempotently. Re-running it refreshes existing rows when adapter metadata improves, so new token split or estimated cost fields can be backfilled without duplicating events.
Normal scans now keep a lightweight per-source file signature cache in SQLite and skip unchanged JSONL/stat summary files, so repeat scans usually only parse the currently active log files. The diagnostics line includes `cached=` for files skipped as unchanged.
Use `scan --no-cache` for a one-off forced reread without deleting existing data first, or `scan --replace` for a destructive source rebuild.

Default local discovery currently checks:

- Claude Code: `~/.config/claude` and `~/.claude`
- Codex: `~/.codex`
- OpenCode: `~/.local/share/opencode`
- Grok Build: `~/.grok`

Use `source add --provider ... --path ...` for custom roots. OpenCode and Grok
Build also support environment overrides for automation: `OPENCODE_DATA_DIRS`
and `GROK_DATA_DIRS` / `GROK_HOME`.

`report weekly`, `report monthly`, and `report all-time` group stored usage by
provider and account. Add `--subscriptions` to include per-subscription-period
value rows matched by account identity and event date. Text output is human-
readable; `--json --verbose` includes source IDs, local path labels, token
split totals, and `estimated_cost_usd` for SDKs or scripts.

`import summary` accepts a single `reported_usage_summary_input.v1` object or an array of them. Use it for user-reported or external aggregate evidence when raw local history is gone or incomplete. Imported summaries are idempotent and are shown under `summary reports (not added to event totals)` with their source paths, summary kind, token split, and the gap versus direct local events.

## Local Task Collection

`scan` also extracts local task spans and rebuilds derived work items in SQLite.
A normal review loop is:

1. `scan`
2. `task list`
3. `task show <work_item_id> --include-evidence`
4. `task verify ...`
5. `task benchmark`

`task list` hides `rejected_meta` items by default. Use
`task list --status rejected_meta` to inspect work the collector currently
treats as meta, system, or noise.

`task benchmark` is a local evaluation loop. It only becomes a real shipping
gate after you have recorded verified ground truth with `task verify ...`.

See:

- `docs/task-collection.md`
- `docs/task-benchmarking.md`

## Local Conversation Archive

`conversation collect` imports durable, provider-independent conversations into
the local SQLite store. Collection is additive: removing or compacting a source
record does not delete a conversation that StatsAI already archived. Repeated
collection uses a separate file-signature cache and only parses changed source
files unless `--no-cache` is supplied. Referenced local artifact metadata is
also tracked, so creating or modifying an artifact recollects its source file.
Candidates are streamed and committed independently, keeping large JSONL files
memory-bounded and allowing an interrupted collection to resume from the last
completed file.

The archive keeps complete visible user and assistant messages, readable
reasoning text and summaries, and embedded binary artifacts such as images.
Opaque encrypted reasoning is ignored. Large tool arguments and results are
bounded while retaining their original byte count and content hash. External
artifacts that cannot be read locally leave an explicit partial-archive marker.
Embedded and explicit local artifacts are limited to 64 MiB each; local paths
must resolve to regular files.

Conversation content is local-only and is not included in `sync` payloads.
JSON exports include binary artifacts as base64; SQLite stores their decoded
bytes as BLOBs.

`privacy filter` can build a separate local pseudonymized dataset from complete
archive conversations. OpenAI Privacy Filter and Kingfisher scan free text;
typed project metadata and tool-call identifiers are pseudonymized directly,
while binary payloads and raw archive identifiers are excluded. Matching tool
calls and results retain a shared pseudonymous identifier. Statistical detectors
can miss sensitive content, so this derived data is pseudonymized rather than
anonymous. It is not uploaded. Filtering runs only when explicitly requested
with `privacy filter`; inspect it with `privacy status`, `privacy show`, and
`privacy export`.

See `docs/conversation-archive.md` for the archive model, retention guarantees,
and completeness behavior.

## Design Notes

Trusted direct reads use `source_kind = local_adapter`. Default, configured, env, and discovered paths are distinguished by `location_origin`, not by trust tier.

Local paths are hashed in source identity fields. Source and parse-evidence path labels are stripped from sync payloads, while project location path labels are retained for owner-facing dashboard display and manual project linking.

Estimated cost is API-equivalent, not a subscription invoice. It uses known provider pricing for recognized models and remains `unknown` when a local log does not prove the billable model.

The backend-facing sync contract starts at `sync_batch.v1`. See `docs/sync-contract.md` for the current ingestion boundary, privacy defaults, and a minimal fixture.

## Loopback Daemon API

The daemon binds to loopback and keeps `/health` available for local service
checks. Every other route requires `Authorization: Bearer <token>`. The daemon
creates a cryptographically random per-install token at
`~/.statsai/daemon-token`; the file is readable only by the current user on
Unix platforms. Browser-originated requests are rejected, and sync writes must
use `Content-Type: application/json` with a body no larger than 8 MiB.

For example:

```sh
curl -H "Authorization: Bearer $(cat ~/.statsai/daemon-token)" \
  http://127.0.0.1:8765/accounts
```

## Hosted Sync

The collector now targets a Cloudflare-hosted backend, but that backend and its
UI are intentionally out of scope for this public CLI repo. This repo contains
the collector, local store, sync contract, and device-pairing client behavior.

`auth login` opens the web app configured by `STATSAI_WEB_URL`, asks the
signed-in user to authorize the local device, and stores a backend-scoped
device session under `~/.statsai/`. The CLI now defaults to the hosted
production pair `https://api.statsai.dev` + `https://statsai.dev`. Set
`STATSAI_API_URL` / `STATSAI_WEB_URL` only when you want to target a different
backend, such as local development or a self-hosted deployment:

```sh
export STATSAI_API_URL="http://127.0.0.1:8787"
export STATSAI_WEB_URL="http://127.0.0.1:3000"
cargo run -p statsai -- auth login
```

For terminals where automatic browser launch is undesirable, use
`auth login --no-open`. This still expects the approving browser to reach the
CLI's local `127.0.0.1` callback. For SSH sessions and servers where the
browser is on a different device, use:

```sh
cargo run -p statsai -- auth login --headless --device-name "Mini server"
```

The headless flow prints a short user code and approval URL, then polls until
the browser-side approval mints the same backend-scoped device session used by
normal login.

After login:

```sh
cargo run -p statsai -- auth status
cargo run -p statsai -- sync --sink http --since-last
```

HTTP sync automatically uses the stored device access token unless
`--auth-token` or `STATSAI_SYNC_TOKEN` is supplied. Access tokens are
short-lived and refreshed from the stored device refresh token as needed.
The collector sends sanitized daily rollups plus metadata to the backend. Raw
events stay local by default.

Source management helpers:

```sh
cargo run -p statsai -- source list
cargo run -p statsai -- source connect --path "$HOME/.codex-work" --email work@example.com --started-at 2026-05-01
cargo run -p statsai -- source history --path "$HOME/.codex-work"
cargo run -p statsai -- source disconnect --path "$HOME/.codex-work" --email work@example.com --ended-at 2026-06-01
cargo run -p statsai -- source disable --source-id src_123
cargo run -p statsai -- source enable --source-id src_123
cargo run -p statsai -- source remove --source-id src_123
cargo run -p statsai -- source remove --source-id src_123 --delete-data
```

`source remove` deletes the source configuration. Add `--delete-data` to also
remove local events, summaries, rollups, and scan-cache entries tied to that
source from SQLite.

### Local Backend Development

Run any compatible sync service locally and point the CLI at it:

```sh
export STATSAI_API_URL="http://127.0.0.1:8787"
export STATSAI_WEB_URL="http://127.0.0.1:3000"
cargo run -p statsai -- auth login
cargo run -p statsai -- sync --sink http --endpoint http://127.0.0.1:8787/api/sync/batches
```

Credential-bearing authentication and sync requests require HTTPS except when
the URL host is an explicit numeric loopback address such as `127.0.0.1` or
`[::1]`. Plaintext `localhost` URLs are rejected to avoid hostname-resolution
ambiguity.
