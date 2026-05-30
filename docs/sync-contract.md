# Sync Contract

`sync_batch.v1` is the first backend-facing contract for `ai-stats`.
The collector owns local scanning, normalization, idempotent local storage, and
privacy scrubbing. The backend owns authentication, validation, deduplication,
rollups, and user-facing queries. The MVP Firebase path writes directly to
Firestore under the authenticated user's namespace; a server-side ingestion
gateway can be added later if deduplication or trusted rollups become necessary.

## Producer

The CLI produces a sync batch with:

```sh
cargo run -p ai-stats-cli -- sync --sink stdout
cargo run -p ai-stats-cli -- sync --sink firestore --since-last
cargo run -p ai-stats-cli -- sync --sink firestore --verify
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

## Local HTTP Endpoint

An optional backend ingestion endpoint can accept:

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

Such a server should:

- require an authenticated Firebase user token
- accept `Authorization: Bearer <firebase_id_token>` from stored auth, `--auth-token`, or `AI_STATS_SYNC_TOKEN`
- validate the request body against `sync_batch.v1`
- reject unsupported `schema_version` values
- deduplicate sources, accounts, subscriptions, events, and summaries by their IDs when server-side deduplication is needed
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

## Firebase Production Backend

The production MVP uses Firebase Auth plus direct Firestore writes. The CLI
opens the hosted login page, stores Firebase credentials locally, refreshes ID
tokens as needed, and writes synced data inside the authenticated user
namespace:

```text
users/{uid}
users/{uid}/devices/{deviceId}
users/{uid}/syncBatches/{batchId}
users/{uid}/sources/{sourceId}
users/{uid}/accounts/{providerAccountId}
users/{uid}/subscriptions/{subscriptionId}
users/{uid}/events/{eventId}
users/{uid}/summaries/{summaryId}
```

CLI login uses the hosted Firebase web app by default:

```sh
cargo run -p ai-stats-cli -- auth login
cargo run -p ai-stats-cli -- auth status
cargo run -p ai-stats-cli -- sync --sink firestore --since-last
cargo run -p ai-stats-cli -- sync --sink firestore --verify
```

The fallback `--client-id "$GOOGLE_CLIENT_ID"` login mode supports a custom
Google OAuth Desktop client when needed. Firestore sync accepts
`--firebase-project` and defaults to the configured `ai-stats-fire` project.
Default Firestore sync mode is `stats`, which converts local events into daily
rollup summaries before upload to reduce write volume. The CLI now persists
those rollups locally and syncs only dirty rollups in `stats` mode instead of
recomputing all summaries from raw events on every run. Hosted
`--firestore-mode full` is gated off by default; use it only with the emulator
or by explicitly setting `AI_STATS_ENABLE_HOSTED_FIRESTORE_FULL=1` for
future/debug raw-event experiments.
Use `--rebuild-rollups` when you intentionally want to rebuild local rollups
from stored events and force a full hosted rewrite, for example after changing
the rollup schema.
These daily rollup summaries keep top-level usage/cost totals and also include
`models[]` per-model breakdown entries so a frontend can chart model-level
token usage without depending on remote raw events.
Large syncs are split into sub-batches (`--firestore-records-per-batch`) and
each sub-batch uses Firestore commit chunks (`--firestore-commit-writes`).
Successful sub-batches update local sync state immediately, so `--since-last`
resumes from the last completed cursor after quota or network failures.
Unchanged source/account/subscription metadata is skipped, and if nothing has
changed locally the CLI performs no hosted Firestore writes.

For local tests, set `FIRESTORE_EMULATOR_HOST=127.0.0.1:8080` and run against
the Firebase emulator suite instead of production. Optionally set
`AI_STATS_FIRESTORE_TEST_UID` to force a stable local UID without login.

`sync --verify` performs a lightweight inspection of the resolved Firestore
target. It reports the local sync cursor/hash state, current dirty rollup
counts, and a small remote snapshot of the user's `devices`, `syncBatches`,
`sources`, `accounts`, `subscriptions`, `events`, and `summaries`
subcollections.

Auth token precedence for sync is:

```text
--auth-token > AI_STATS_SYNC_TOKEN > stored Firebase auth
```

Firestore rules allow authenticated users to read only their own namespace.
Client writes are allowed only under selected user-owned collections
(`devices`, `syncBatches`, `sources`, `accounts`, `subscriptions`, `summaries`).
Hosted rules deny writes to `users/{uid}/events/*` to prevent accidental
high-volume per-event sync on no-cost quota, and the CLI now blocks hosted
direct `full` mode unless explicitly overridden.
The HTTPS function remains optional scaffold for a later server-side gateway.

## Open Decisions

- Whether the first backend stores sanitized event payloads as JSON blobs first,
  then promotes indexed columns later.
- Whether periodic sync should use a launch agent/service, an app daemon, or both.
