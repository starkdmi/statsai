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

Credential-bearing requests require HTTPS, except for local development using
an explicit numeric loopback host such as `127.0.0.1` or `[::1]`. Plaintext
hostnames, including `localhost`, are rejected.

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
Collectors require the acknowledgement version to match the submitted batch
version exactly; a v1 acknowledgement cannot successfully acknowledge a v2
batch.

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

### Canonical identity and chunk invariants

HTTP chunking is a transport concern and must not change canonical hosted
state. Account aliases are persisted per user and device in
`sync_entity_owners`. Every later assignment, subscription, and summary chunk
resolves its device-local account ID through that mapping, even when the chunk
contains no account records. When a newly observed alias matches historical
rows from that device, the backend repairs their indexed account ID and JSON
payload in the same D1 transaction. The repair first discovers affected daily
months and period rows, then rebuilds their monthly rollups and the all-time
dashboard snapshot in that transaction. Targeted month rebuilds switch to one
bulk rebuild for wide histories, and all lookup/materialization statements are
included in the sync D1 query budget. The preliminary budget check includes
only work known before reconciliation; actual alias-repair and impact-analysis
queries are added to the exact estimate after aliases have been resolved.

Provider user IDs are stronger identity evidence than email addresses. Email
may connect records only when it does not bridge two different non-empty
provider user IDs. Ambiguous email-only identities remain separate.

Each accepted batch stores a SHA-256 digest of its normalized payload. Retrying
the same batch ID and payload returns the duplicate acknowledgement; reusing
the ID with different content returns `batch_id_payload_conflict`. The receipt
insert is the first statement in the same atomic D1 batch as all mutations, so
a competing request cannot change mirrored rows before losing the receipt
claim. Historical receipts created before digest support retain their previous
retry behavior.

`GET /api/sync/status` returns mirror counts for the authenticated device,
computed from active ownership records. User-wide canonical counts remain in
the consistency diagnostics and are not compared with a single device's local
store.

Subscription rows are retained as evidence. Subscription API and dashboard
context reads project that evidence into entitlements: verified provider or
local-auth evidence wins over manual evidence for each canonical account's
connected billing-window cluster, while disconnected periods remain distinct.
Different provider subscription IDs are never merged. Interval observations
are parsed once and clustered with sorted sweeps, keeping projection work
O(n log n) even when stored evidence spans many sync batches.

### Referential-integrity rollout

`SYNC_REQUIRE_CANONICAL_ACCOUNTS=1` rejects any non-null child account
reference that cannot be resolved to an account already stored or included in
the same batch. It is disabled by default for the additive deployment. Enable
it only after:

1. applying all D1 migrations and recording a Time Travel bookmark;
2. completing a full sync from every active device so historical aliases are
   repaired;
3. verifying that no non-null account references are absent from
   `provider_accounts` across assignments, subscriptions, daily rollups, and
   period summaries;
4. confirming shadow dashboard totals and per-device mirror counts;
5. changing the production variable to `1` and deploying the Worker.

The preflight orphan query must return zero for every row:

```sql
SELECT 'source_account_assignments' AS relation, COUNT(*) AS orphan_count
FROM source_account_assignments child
LEFT JOIN provider_accounts parent
  ON parent.user_id = child.user_id
 AND parent.provider_account_id = child.provider_account_id
WHERE child.provider_account_id IS NOT NULL
  AND parent.provider_account_id IS NULL
UNION ALL
SELECT 'subscriptions', COUNT(*)
FROM subscriptions child
LEFT JOIN provider_accounts parent
  ON parent.user_id = child.user_id
 AND parent.provider_account_id = child.provider_account_id
WHERE child.provider_account_id IS NOT NULL
  AND parent.provider_account_id IS NULL
UNION ALL
SELECT 'daily_rollups', COUNT(*)
FROM daily_rollups child
LEFT JOIN provider_accounts parent
  ON parent.user_id = child.user_id
 AND parent.provider_account_id = child.provider_account_id
WHERE child.provider_account_id IS NOT NULL
  AND parent.provider_account_id IS NULL
UNION ALL
SELECT 'period_summaries', COUNT(*)
FROM period_summaries child
LEFT JOIN provider_accounts parent
  ON parent.user_id = child.user_id
 AND parent.provider_account_id = child.provider_account_id
WHERE child.provider_account_id IS NOT NULL
  AND parent.provider_account_id IS NULL;
```

Once enabled, a child chunk that arrives before its account metadata is
rejected without storing partial data. The client may send the account chunk
and safely retry the original child batch ID.

## Open Decisions

- Whether the first backend stores sanitized event payloads as JSON blobs first,
  then promotes indexed columns later.
- Whether periodic sync should use a launch agent/service, an app daemon, or both.
