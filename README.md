# ai-stats

Local-first AI usage statistics for Codex, Claude Code, and future local/provider sources.

Status: early implementation. The public API is not stable yet.

## Scope

`ai-stats` is a Rust-first utility package, not a hosted dashboard. It provides:

- a CLI for scanning, reporting, source config, account mapping, subscription mapping, export, sync, and daemon mode
- a Rust SDK facade for embedding
- local provider adapters for direct trusted source reads
- a local SQLite store
- abstract sync sinks for stdout/file today and Firebase/Supabase/HTTP later
- a loopback-only daemon API for local widgets and toolbar integrations

The first adapters target Claude Code JSONL usage roots and Codex session logs. External aggregate reports, manually transcribed screenshots, or provider `/usage` summaries are supported as reported summary imports. They stay separate from trusted raw local events so reports can show direct usage and imported/manual gaps without double-counting.

## Workspace

- `crates/ai-stats-core`: normalized types, stable IDs, schema models, privacy metadata
- `crates/ai-stats-adapters`: Claude Code and Codex local adapters
- `crates/ai-stats-store`: SQLite persistence
- `crates/ai-stats-sync`: pluggable sync sink trait plus stdout/file sinks
- `crates/ai-stats-daemon`: localhost API
- `crates/ai-stats-sdk`: Rust SDK facade
- `crates/ai-stats-cli`: `ai-stats` binary

## CLI Examples

```sh
cargo run -p ai-stats-cli -- scan --provider codex --preview
cargo run -p ai-stats-cli -- source add --provider codex --path "$HOME/.codex-work" --account work
cargo run -p ai-stats-cli -- source disable --source-id src_123
cargo run -p ai-stats-cli -- source enable --source-id src_123
cargo run -p ai-stats-cli -- source remove --source-id src_123
cargo run -p ai-stats-cli -- source remove --source-id src_123 --delete-data
cargo run -p ai-stats-cli -- account resolve --provider codex
cargo run -p ai-stats-cli -- subscription add --provider claude --account personal --plan Pro --price 20 --paid-at 2026-05-15
cargo run -p ai-stats-cli -- import summary --path ./reported_usage_summaries.json --dry-run --verbose
cargo run -p ai-stats-cli -- report weekly
cargo run -p ai-stats-cli -- sync --sink file --output ./ai-stats-sync-batch.json
cargo run -p ai-stats-cli -- sync --sink http --endpoint http://127.0.0.1:3000/v1/sync/batches
cargo run -p ai-stats-cli -- sync --sink http --endpoint http://127.0.0.1:3000/v1/sync/batches --since-last
cargo run -p ai-stats-cli -- sync --status
cargo run -p ai-stats-cli -- auth login
cargo run -p ai-stats-cli -- auth status
cargo run -p ai-stats-cli -- sync --sink firestore --since-last
cargo run -p ai-stats-cli -- sync --sink firestore --since-last --firestore-mode stats
cargo run -p ai-stats-cli -- sync --sink firestore --verify
cargo run -p ai-stats-cli -- schema sync-batch
```

`scan --preview` reads configured and default local sources without writing to SQLite. It reports normalized usage events, not raw log rows, and shows the token split when the provider logs expose it:

```text
codex account=work path=~/.codex-work usage_events=123 input=1,000,000 cached=800,000 output=20,000 total=1,030,000 est_cost=$1.23
```

`scan` persists normalized events idempotently. Re-running it refreshes existing rows when adapter metadata improves, so new token split or estimated cost fields can be backfilled without duplicating events.
Normal scans now keep a lightweight per-source file signature cache in SQLite and skip unchanged JSONL/stat summary files, so repeat scans usually only parse the currently active log files. The diagnostics line includes `cached=` for files skipped as unchanged.
Use `scan --no-cache` for a one-off forced reread without deleting existing data first, or `scan --replace` for a destructive source rebuild.

`report weekly`, `report monthly`, and `report all-time` group stored usage by provider and account. Text output is human-readable; `--json --verbose` includes source IDs, local path labels, token split totals, and `estimated_cost_usd` for SDKs or scripts.

`import summary` accepts a single `reported_usage_summary_input.v1` object or an array of them. Use it for user-reported or external aggregate evidence when raw local history is gone or incomplete. Imported summaries are idempotent and are shown under `summary reports (not added to event totals)` with their source paths, summary kind, token split, and the gap versus direct local events.

## Design Notes

Trusted direct reads use `source_kind = local_adapter`. Default, configured, env, and discovered paths are distinguished by `location_origin`, not by trust tier.

Local paths are hashed in source identity fields. `path_label` exists for local configuration and scanning ergonomics; exported privacy policy will tighten before the API is declared stable.

Estimated cost is API-equivalent, not a subscription invoice. It uses known provider pricing for recognized models and remains `unknown` when a local log does not prove the billable model.

The backend-facing sync contract starts at `sync_batch.v1`. See `docs/sync-contract.md` for the current ingestion boundary, privacy defaults, and a minimal fixture.

## Firebase Backend

This repository includes Firebase scaffold files for production sync:

- `web/login/`: hosted Firebase Auth login page for the CLI loopback flow
- `firestore.rules`: user-scoped read/write rules for direct CLI sync
- `functions/`: optional HTTPS ingestion scaffold for a later server-side sync gateway
- `firebase.json`, `.firebaserc`: Firebase CLI project config

Production login opens the hosted Firebase Auth page in the browser and stores
Firebase credentials locally in `~/.ai-stats/auth.json`:

```sh
cargo run -p ai-stats-cli -- auth login
cargo run -p ai-stats-cli -- auth status
cargo run -p ai-stats-cli -- sync --sink firestore --since-last
cargo run -p ai-stats-cli -- sync --sink firestore --verify
```

After login, Firestore and HTTP sync automatically use the stored Firebase ID
token unless `--auth-token` or `AI_STATS_SYNC_TOKEN` is supplied. The
`--client-id "$GOOGLE_CLIENT_ID"` login path is still available as an escape
hatch for a custom desktop OAuth client.

Firestore sync sends writes through Firestore Commit API batches and prints
progress by sub-batch/chunk. In the default `stats` mode, the CLI now maintains
local daily rollup summaries and syncs only the dirty rollups instead of
rescanning all raw events on every run. Unchanged source/account/subscription
metadata is skipped, and an empty sync no longer writes bookkeeping docs.

Default Firestore mode is `stats`, which uploads cached daily summary documents
for production sync. Hosted `--firestore-mode full` is disabled by default as a
guardrail because direct per-event writes are too expensive for the intended
production flow. Use `--firestore-mode full` only with the emulator or by
explicitly setting `AI_STATS_ENABLE_HOSTED_FIRESTORE_FULL=1` for future/debug
experiments.

If you already have a populated local store from older builds, run one full
stats sync once to bootstrap the local rollup cache:

```sh
cargo run -p ai-stats-cli -- sync --sink firestore --firestore-mode stats
```

Source management helpers:

```sh
cargo run -p ai-stats-cli -- source list
cargo run -p ai-stats-cli -- source disable --source-id src_123
cargo run -p ai-stats-cli -- source enable --source-id src_123
cargo run -p ai-stats-cli -- source remove --source-id src_123
cargo run -p ai-stats-cli -- source remove --source-id src_123 --delete-data
```

`source remove` deletes the source configuration. Add `--delete-data` to also
remove local events, summaries, rollups, and scan-cache entries tied to that
source from SQLite.

`sync --verify` resolves the active Firebase target, shows local sync state,
and fetches a small remote snapshot (`devices`, `syncBatches`, `sources`,
`accounts`, `subscriptions`, `events`, `summaries`) so you can confirm what the
backend sees without relying on the Firebase console UI.

### Local Firebase Tests

Use the Firestore emulator for local integration tests:

```sh
export FIRESTORE_EMULATOR_HOST="127.0.0.1:8080"
export AI_STATS_FIRESTORE_TEST_UID="local-dev-user"
firebase emulators:start --only firestore,auth
cargo run -p ai-stats-cli -- sync --sink firestore --firestore-mode stats --since-last
```
