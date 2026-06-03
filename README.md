# statsai

Local-first AI usage statistics for Codex, Claude Code, and future local/provider sources.

Status: early implementation. The public API is not stable yet.

## Scope

`statsai` is a Rust-first utility package, not a hosted dashboard. It provides:

- a CLI for scanning, reporting, source config, account mapping, subscription mapping, export, sync, and daemon mode
- a Rust SDK facade for embedding
- local provider adapters for direct trusted source reads
- a local SQLite store
- abstract sync sinks for stdout/file/HTTP
- a loopback-only daemon API for local widgets and toolbar integrations

The first adapters target Claude Code JSONL usage roots and Codex session logs. External aggregate reports, manually transcribed screenshots, or provider `/usage` summaries are supported as reported summary imports. They stay separate from trusted raw local events so reports can show direct usage and imported/manual gaps without double-counting.

## Workspace

- `crates/statsai-core`: normalized types, stable IDs, schema models, privacy metadata
- `crates/statsai-adapters`: Claude Code and Codex local adapters
- `crates/statsai-store`: SQLite persistence
- `crates/statsai-sync`: pluggable sync sink trait plus stdout/file/HTTP sinks
- `crates/statsai-daemon`: localhost API
- `crates/statsai-sdk`: Rust SDK facade
- `crates/statsai-cli`: `statsai` binary

## CLI Examples

```sh
cargo run -p statsai-cli -- scan --provider codex --preview
cargo run -p statsai-cli -- source add --provider codex --path "$HOME/.codex-work"
cargo run -p statsai-cli -- source disable --source-id src_123
cargo run -p statsai-cli -- source enable --source-id src_123
cargo run -p statsai-cli -- source remove --source-id src_123
cargo run -p statsai-cli -- source remove --source-id src_123 --delete-data
cargo run -p statsai-cli -- source connect --path "$HOME/.codex-work" --email work@example.com --label work --started-at 2026-05-01
cargo run -p statsai-cli -- source history --path "$HOME/.codex-work"
cargo run -p statsai-cli -- source disconnect --path "$HOME/.codex-work" --email work@example.com --ended-at 2026-06-01
cargo run -p statsai-cli -- subscription add --provider claude --email personal@example.com --plan Pro --price 20 --started-at 2026-05-15 --paid-at 2026-05-15
cargo run -p statsai-cli -- subscription change --provider codex --email work@example.com --plan Pro --price 200 --started-at 2026-06-01
cargo run -p statsai-cli -- import summary --path ./reported_usage_summaries.json --dry-run --verbose
cargo run -p statsai-cli -- report weekly
cargo run -p statsai-cli -- report monthly --subscriptions
cargo run -p statsai-cli -- sync --sink file --output ./statsai-sync-batch.json
cargo run -p statsai-cli -- sync --sink http --since-last
cargo run -p statsai-cli -- sync --sink http --endpoint http://127.0.0.1:8787/api/sync/batches
cargo run -p statsai-cli -- sync --sink http --endpoint http://127.0.0.1:8787/api/sync/batches --since-last
cargo run -p statsai-cli -- sync --status
cargo run -p statsai-cli -- auth login
cargo run -p statsai-cli -- auth status
cargo run -p statsai-cli -- sync --sink http --verify
cargo run -p statsai-cli -- schema sync-batch
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

`report weekly`, `report monthly`, and `report all-time` group stored usage by
provider and account. Add `--subscriptions` to include per-subscription-period
value rows matched by account identity and event date. Text output is human-
readable; `--json --verbose` includes source IDs, local path labels, token
split totals, and `estimated_cost_usd` for SDKs or scripts.

`import summary` accepts a single `reported_usage_summary_input.v1` object or an array of them. Use it for user-reported or external aggregate evidence when raw local history is gone or incomplete. Imported summaries are idempotent and are shown under `summary reports (not added to event totals)` with their source paths, summary kind, token split, and the gap versus direct local events.

## Design Notes

Trusted direct reads use `source_kind = local_adapter`. Default, configured, env, and discovered paths are distinguished by `location_origin`, not by trust tier.

Local paths are hashed in source identity fields. `path_label` exists for local configuration and scanning ergonomics; exported privacy policy will tighten before the API is declared stable.

Estimated cost is API-equivalent, not a subscription invoice. It uses known provider pricing for recognized models and remains `unknown` when a local log does not prove the billable model.

The backend-facing sync contract starts at `sync_batch.v1`. See `docs/sync-contract.md` for the current ingestion boundary, privacy defaults, and a minimal fixture.

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
cargo run -p statsai-cli -- auth login
```

After login:

```sh
cargo run -p statsai-cli -- auth status
cargo run -p statsai-cli -- sync --sink http --since-last
```

HTTP sync automatically uses the stored device access token unless
`--auth-token` or `STATSAI_SYNC_TOKEN` is supplied. Access tokens are
short-lived and refreshed from the stored device refresh token as needed.
The collector sends sanitized daily rollups plus metadata to the backend. Raw
events stay local by default.

Source management helpers:

```sh
cargo run -p statsai-cli -- source list
cargo run -p statsai-cli -- source connect --path "$HOME/.codex-work" --email work@example.com --started-at 2026-05-01
cargo run -p statsai-cli -- source history --path "$HOME/.codex-work"
cargo run -p statsai-cli -- source disconnect --path "$HOME/.codex-work" --email work@example.com --ended-at 2026-06-01
cargo run -p statsai-cli -- source disable --source-id src_123
cargo run -p statsai-cli -- source enable --source-id src_123
cargo run -p statsai-cli -- source remove --source-id src_123
cargo run -p statsai-cli -- source remove --source-id src_123 --delete-data
```

`source remove` deletes the source configuration. Add `--delete-data` to also
remove local events, summaries, rollups, and scan-cache entries tied to that
source from SQLite.

Maintainer notes:

- release/distribution plan: [docs/release-distribution-plan.md](docs/release-distribution-plan.md)
- auth UX and headless login research: [docs/auth-login-ux-research.md](docs/auth-login-ux-research.md)

### Local Backend Development

Run any compatible sync service locally and point the CLI at it:

```sh
export STATSAI_API_URL="http://127.0.0.1:8787"
export STATSAI_WEB_URL="http://127.0.0.1:3000"
cargo run -p statsai-cli -- auth login
cargo run -p statsai-cli -- sync --sink http --endpoint http://127.0.0.1:8787/api/sync/batches
```
