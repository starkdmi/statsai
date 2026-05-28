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

The first adapters target Claude Code JSONL usage roots and Codex session logs. External reports from tools like `ccusage`, screenshots transcribed by a user, or provider `/usage` summaries are supported as reported summary imports. They stay separate from trusted raw local events so reports can show direct usage and imported/manual gaps without double-counting.

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
cargo run -p ai-stats-cli -- account resolve --provider codex
cargo run -p ai-stats-cli -- subscription add --provider claude --account personal --plan Pro --price 20 --paid-at 2026-05-15
cargo run -p ai-stats-cli -- import summary --path ./reported_usage_summaries.json --dry-run --verbose
cargo run -p ai-stats-cli -- report weekly
cargo run -p ai-stats-cli -- sync --sink file --output ./ai-stats-sync-batch.json
cargo run -p ai-stats-cli -- sync --sink http --endpoint http://127.0.0.1:3000/v1/sync/batches
cargo run -p ai-stats-cli -- sync --sink http --endpoint http://127.0.0.1:3000/v1/sync/batches --since-last
cargo run -p ai-stats-cli -- sync --status
cargo run -p ai-stats-cli -- schema sync-batch
```

`scan --preview` reads configured and default local sources without writing to SQLite. It reports normalized usage events, not raw log rows, and shows the token split when the provider logs expose it:

```text
codex account=work path=~/.codex-work usage_events=123 input=1,000,000 cached=800,000 output=20,000 total=1,030,000 est_cost=$1.23
```

`scan` persists normalized events idempotently. Re-running it refreshes existing rows when adapter metadata improves, so new token split or estimated cost fields can be backfilled without duplicating events.

`report weekly`, `report monthly`, and `report all-time` group stored usage by provider and account. Text output is human-readable; `--json --verbose` includes source IDs, local path labels, token split totals, and `estimated_cost_usd` for SDKs or scripts.

`import summary` accepts a single `reported_usage_summary_input.v1` object or an array of them. Use it for user-reported or external aggregate evidence when raw local history is gone or incomplete. Imported summaries are idempotent and are shown under `summary reports (not added to event totals)` with their source paths, summary kind, token split, and the gap versus direct local events.

## Design Notes

Trusted direct reads use `source_kind = local_adapter`. Default, configured, env, and discovered paths are distinguished by `location_origin`, not by trust tier.

Local paths are hashed in source identity fields. `path_label` exists for local configuration and scanning ergonomics; exported privacy policy will tighten before the API is declared stable.

Estimated cost is API-equivalent, not a subscription invoice. It uses known provider pricing for recognized models and remains `unknown` when a local log does not prove the billable model.

The backend-facing sync contract starts at `sync_batch.v1`. See `docs/sync-contract.md` for the current ingestion boundary, privacy defaults, and a minimal fixture.
