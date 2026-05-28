# Sync Contract

`sync_batch.v1` is the first backend-facing contract for `ai-stats`.
The collector owns local scanning, normalization, idempotent local storage, and
privacy scrubbing. The backend owns authentication, validation, deduplication,
rollups, and user-facing queries.

## Producer

The CLI produces a sync batch with:

```sh
cargo run -p ai-stats-cli -- sync --sink stdout
cargo run -p ai-stats-cli -- sync --sink http --endpoint http://127.0.0.1:3000/v1/sync/batches
```

The JSON Schema is available with:

```sh
cargo run -p ai-stats-cli -- schema sync-batch
```

## Privacy Defaults

The current sync path strips record-level local evidence before sending:

- `SourceLocation.path_label`
- `SourceLocation.account_hint`
- `ProviderAccount.account_label`
- `ProviderAccount.plan_name`
- `UsageEvent.source.source_record_id`
- `UsageEvent.parse_evidence.source_line_number`
- `UsageEvent.parse_evidence.source_record_id`
- `UsageSummary.source.source_record_id`
- `UsageSummary.parse_evidence.source_line_number`
- `UsageSummary.parse_evidence.source_record_id`
- `Subscription.notes`

Hashed path, account, source, event, and summary identifiers remain so the
server can deduplicate records without seeing local file names or raw account
labels.

## Server Responsibilities

A minimal backend ingestion endpoint should accept:

```text
POST /v1/sync/batches
```

For local end-to-end development, the daemon implements this endpoint on its
loopback server:

```sh
cargo run -p ai-stats-cli -- daemon --api 127.0.0.1:8765
cargo run -p ai-stats-cli -- sync --sink http --endpoint http://127.0.0.1:8765/v1/sync/batches
cargo run -p ai-stats-cli -- sync --sink http --endpoint http://127.0.0.1:8765/v1/sync/batches --since-last
cargo run -p ai-stats-cli -- sync --status
```

The server should:

- require an authenticated user or device token
- accept `Authorization: Bearer <token>` when the collector is configured with `--auth-token` or `AI_STATS_SYNC_TOKEN`
- validate the request body against `sync_batch.v1`
- reject unsupported `schema_version` values
- deduplicate sources, accounts, subscriptions, events, and summaries by their IDs
- treat collector IDs as stable client-provided IDs, not database primary keys exposed to users
- compute daily and monthly rollups server-side from accepted events
- return accepted, updated, duplicate, and rejected counts

## MVP Response Shape

```json
{
  "schema_version": "sync_ack.v1",
  "batch_id": "batch_1710000000000",
  "accepted": {
    "sources": 1,
    "accounts": 1,
    "subscriptions": 0,
    "events": 1,
    "summaries": 0
  },
  "duplicates": {
    "events": 0,
    "summaries": 0
  },
  "rejected": []
}
```

The current loopback daemon returns this shape and reports duplicate events
when the existing store already has the semantic event. Source, account,
subscription, and summary upserts are currently reported as accepted writes.

## Local Sync State

After a successful sync, the collector records local sync state keyed by sink
and target. The state stores the last successful batch, event cursor, summary
cursor, and failure count. Passing `--since-last` sends only events and
summaries after the recorded cursor for that sink target while still including
the current source, account, and subscription metadata.

The HTTP sink parses `sync_ack.v1` before updating local state. File and stdout
sinks update state after their local write succeeds.

## Open Decisions

- Whether the first backend stores sanitized event payloads as JSON blobs first,
  then promotes indexed columns later.
- Whether periodic sync should send only new records after local sync state is
  added, or continue sending full idempotent batches for the first backend MVP.
- Whether auth starts with device tokens or OAuth device flow.
