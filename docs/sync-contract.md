# Sync Contract

`sync_batch.v1` is the legacy usage-only backend contract for `statsai`.
`sync_batch.v2` extends it with hosted task bucket snapshots and task
verification uploads. It also supports an optional `authoritative_snapshot`
marker for deletion reconciliation.
The collector owns local scanning, normalization, idempotent local storage, and
privacy scrubbing. The backend owns authentication, validation, deduplication,
rollups, and user-facing queries. The production path sends sanitized batches to
a Cloudflare Worker backed by D1 and Better Auth device tokens.

## Producer

The CLI produces a sync batch with:

```sh
cargo run -p statsai -- sync --sink stdout
cargo run -p statsai -- sync --sink http --endpoint http://127.0.0.1:8787/api/sync/batches
cargo run -p statsai -- sync --sink http --since-last
cargo run -p statsai -- sync --sink http --verify
```

The JSON Schema is available with:

```sh
cargo run -p statsai -- schema sync-batch
```

## Privacy Defaults

The current production sync path strips record-level local evidence before sending:

- `SourceLocation.path_label`
- `ProviderAccount.plan_name`
- `UsageEvent.source.source_record_id`
- `UsageEvent.parse_evidence.source_line_number`
- `UsageEvent.parse_evidence.source_record_id`
- `UsageSummary.source.source_record_id`
- `UsageSummary.parse_evidence.source_line_number`
- `UsageSummary.parse_evidence.source_record_id`
- `Subscription.notes`

`ProjectInfo.path_label` is retained for owner-facing project location displays,
manual project linking, and hosted task review. Hashed path, source, event, and
summary identifiers remain so the server can deduplicate records and keep
stable location identity.

Canonical provider account identity may now sync through
`ProviderAccount.provider_user_id` and `ProviderAccount.email`. Hosted task
snapshots can also include bounded task titles, summary previews, todo excerpts,
repo labels, branch labels, path labels, and task verification actions. The
backend uses identity plus project metadata to route those hosted task records.

User-defined aliases are still retained in `ProviderAccount.account_label` for
display, but they are no longer the primary account key.

## Local HTTP Endpoint

For local end-to-end development, run any compatible HTTP service and point the
CLI at it. The CLI now defaults to the hosted production URLs, so export the
localhost pair explicitly when you want a local session that stays separate from
hosted sync:

```sh
export STATSAI_API_URL="http://127.0.0.1:8787"
export STATSAI_WEB_URL="http://127.0.0.1:3000"
cargo run -p statsai -- auth login
cargo run -p statsai -- sync --sink http --endpoint http://127.0.0.1:8787/api/sync/batches
cargo run -p statsai -- sync --sink http --endpoint http://127.0.0.1:8787/api/sync/batches --since-last
cargo run -p statsai -- sync --sink http --verify
cargo run -p statsai -- sync --status
```

The daemon still supports `/v1/sync/batches` for loopback-only diagnostics, but
rejects batches containing `authoritative_snapshot` because it does not stage
device ownership or reconcile deletions. `/api/sync/batches` is the production
contract. A compatible backend should:

- require an authenticated device access token
- accept `Authorization: Bearer <device_access_token>` from stored auth, `--auth-token`, or `STATSAI_SYNC_TOKEN`
- validate the request body against `sync_batch.v1` and `sync_batch.v2`
- reject unsupported `schema_version` values
- deduplicate sources, accounts, source-account assignments, subscriptions, and summaries by their IDs when server-side deduplication is needed
- treat collector IDs as stable client-provided IDs, not database primary keys exposed to users
- compute daily, monthly, and dashboard rollups server-side from accepted summaries
- atomically replace accepted task bucket snapshots per `(user, device, project_bucket)`
- treat the ordered `authoritative_snapshot` fragments sharing one `snapshot_id`
  as the complete set of metadata and summary IDs owned by the authenticated
  device; each fragment carries zero-based `part_index` and a common
  `part_count`, with at most 200 IDs across its ID arrays
- stage snapshot ownership without pruning until the final in-order fragment;
  then apply ownership and deletion reconciliation atomically, pruning a hosted
  entity only when no device still owns its canonical row
- reject missing or out-of-order snapshot fragments; send them only after all
  data chunks in the same logical full sync have succeeded, while batches
  without snapshot fragments remain incremental
- project hosted task verifications onto the latest bucket snapshot when serving task reads
- return accepted, updated, duplicate, and rejected counts

## Response Shapes

```json
{
  "schema_version": "sync_ack.v1",
  "batch_id": "batch_1710000000000",
  "accepted": {
    "sources": 1,
    "accounts": 1,
    "source_account_assignments": 1,
    "subscriptions": 0,
    "events": 1,
    "summaries": 0
  },
  "duplicates": {
    "sources": 0,
    "accounts": 0,
    "source_account_assignments": 0,
    "events": 0,
    "summaries": 0,
    "subscriptions": 0
  },
  "rejected": []
}
```

`sync_batch.v2` returns `sync_ack.v2`, which adds `task_buckets` and
`task_verifications` counters under both `accepted` and `duplicates`.

The current loopback daemon returns this shape and reports duplicate events
when the existing store already has the semantic event. Source, account,
source-account assignment, subscription, and summary upserts are currently
reported as accepted writes.

## Local Sync State

After a successful sync, the collector records local sync state keyed by sink
and target. The state stores the last successful batch, event cursor, summary
cursor, and failure count. Passing `--since-last` sends only events and
summaries after the recorded cursor for that sink target while still including
the current source, account, source-account assignment, and subscription
metadata.

Full HTTP rollup syncs send their authoritative snapshot as the final logical
chunk. The marker lists all current source, provider-account,
source-account-assignment, subscription, and summary IDs, including empty lists.
The backend tracks ownership per authenticated device and keeps device-local IDs
separate from server-canonical IDs so account alias reconciliation cannot delete
the canonical row. Incremental and legacy batches omit the marker and never
prune absent records.

The HTTP sink parses `sync_ack.v1` and `sync_ack.v2` before updating local
state. File and stdout sinks update state after their local write succeeds.

## Cloudflare Production Backend

The production backend uses Better Auth on Cloudflare Workers plus D1. The CLI
opens the web app configured by `STATSAI_WEB_URL`, pairs the local device
through a loopback callback, stores a device refresh token in a backend-scoped
local auth file, and sends sync batches to the Worker API:

```text
POST /api/sync/batches
```

D1 stores app-owned tables for devices, device tokens, sources, provider
accounts, source-account assignments, subscriptions, daily rollups, monthly
rollups, dashboard snapshots, and sync batch metadata. Better Auth owns its
auth/session/account tables in the same D1 database. That backend lives
outside this public CLI repo.

```sh
cargo run -p statsai -- auth login
cargo run -p statsai -- auth status
cargo run -p statsai -- sync --sink http --since-last
```

Auth token precedence for sync is:

```text
--auth-token > STATSAI_SYNC_TOKEN > stored Cloudflare device access token
```

The Worker rejects raw event cloud sync by default and accepts sanitized daily
summary rollups plus metadata, along with hosted task snapshots and hosted task
verification actions for `sync_batch.v2`. The collector now prepares those
daily rollups before HTTP sync, so a normal Cloudflare sync can populate the
dashboard without shipping raw events. Repeated batches are idempotent by
stable IDs.
The dashboard reads compact API responses backed by D1 rollups instead of
scanning all synced records in the browser.

## Open Decisions

- Whether the first backend stores sanitized event payloads as JSON blobs first,
  then promotes indexed columns later.
- Whether periodic sync should use a launch agent/service, an app daemon, or both.
