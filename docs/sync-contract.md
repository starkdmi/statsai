# Sync Contract

`sync_batch.v1` is the legacy usage-only backend contract for `statsai`.
`sync_batch.v2` extends it with hosted task bucket snapshots and task
verification uploads. It also supports an optional `authoritative_snapshot`
marker for deletion reconciliation.
`sync_batch.v3` adds privacy-safe numeric code-change metrics and makes their
acknowledgement counts and authoritative snapshot IDs part of the versioned
contract. Versions 1 and 2 never treat those new fields as authoritative.
The collector owns local scanning, normalization, idempotent local storage, and
privacy scrubbing. The backend owns authentication, validation, deduplication,
rollups, and user-facing queries. The production path sends sanitized batches to
a Cloudflare Worker backed by D1 and Better Auth device tokens.

Local Git collection is deliberately bounded: it inspects the current `HEAD`
and local branches, includes only commits from the last 90 days whose committer
email matches the repository's configured `user.email`, and ignores
remote-tracking-only history. Repositories without a configured Git email report
Git coverage as unavailable instead of attributing commits speculatively.
Stored Git scans are retained only while a current project or trace references
their repository. If an actively referenced repository temporarily fails to
scan, its last successful snapshot is retained with partial Git coverage;
unreferenced repository scans are retired from derived metrics and local cache.

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
- `TaskSpan.source_record_id`
- `TaskSpan.session_id`
- `TaskSpan.thread_id`
- `Subscription.notes`
- code source text, diffs, file paths, tool arguments, and commit messages

Code-change counting uses an explicit source-file extension allowlist.
Documentation, configuration, manifests, lockfiles, generated output, and
unknown text formats are excluded. Mutation records that claim a whole-file
deletion without exposing the deleted content reduce trace coverage instead of
emitting a zero-line edit.
Trace edit identities include the mutation section ordinal, so identical edits
within one tool call remain distinct while repeated scans remain idempotent.
Applied agent edits are published as one metric per day, project, and
repository rather than one per individual edit, so an archive holding hundreds
of thousands of edits still syncs a payload proportional to observed days.
Archive collection reconciles its stored file keys before rebuilding metrics;
when a previously imported archive file disappears, its trace edits and coverage
are removed while the retained conversation archive remains available locally.
Reconciliation is skipped when the archive root itself is unreachable, because
an unmounted volume or a renamed home directory produces the same empty file
list as an emptied archive and must not be read as a deletion. Removing a source
with `--delete-data` drops that source's reconstructed edits and coverage and
rebuilds the metrics immediately, so the authoritative snapshot retires the
hosted rows instead of continuing to republish them.

An edit is placed in a repository only when its path stays inside that
repository. A tool call naming an absolute path elsewhere, or escaping the
project with `..`, is still counted as an applied agent edit but carries no
`repository_hash`.

A shell tool call reduces trace coverage only when its command could reach a
file; confidently read-only inspection such as `ls`, `git status`, or
`cargo test` leaves coverage intact. A program is read-only only in the forms
that cannot write: `find -delete`, `sort -o`, and `env <program>` are judged by
what they would actually do. Interpreters are never classified read-only, since
`sed`, `awk`, and `perl` all reach the filesystem from inside their own program
text and no flag inspection can prove one of them safe. A patch hunk that arrives before any
file header is dropped and reported as unmeasured rather than charged to the
next file in the patch. A project path that is not inside a Git
repository is likewise not a failed scan: agents also run in scratch
directories, and those must not degrade Git coverage. Commits and trace records
dated beyond the sync target's future skew are left unmeasured, so a
clock-skewed record degrades coverage instead of failing every later sync: a
skipped commit degrades its repository's Git coverage, and a skipped trace edit
degrades the trace coverage stamped on every metric that run publishes.

Authenticated HTTP preflight returns a 32-byte, user-scoped code-change identity
key. The collector uses that key to HMAC the local repository identity and raw
commit object ID into the uploaded `ccm_...` metric ID. Devices paired to the
same user therefore converge on one committed metric ID, while raw commit hashes
remain local and observers cannot enumerate public commits from the uploaded ID.
Repository identity canonicalizes common SSH and HTTPS remote forms before
deriving that ID and does not mix device-local refs or shallow-clone roots into
a remote-backed identity. Root commits identify only repositories without an
origin. A repository that has neither an origin nor a first commit has no shared
identity to derive, so it is keyed by its local path until a commit supplies a
root; without that, every such repository would collide on one identity and
merge unrelated work.

Attribution is withheld entirely, at every confidence level, from an edit that
changes too few distinct lines to single out a commit. A lone `}` or blank line
reaches a perfect overlap against whichever commit happens to touch the file,
which would credit an agent for human-written work. Such an edit still counts as
an applied agent edit, and its commit still counts as committed churn; only the
link between them is withheld. When project metadata is excluded, both `project_id` and the otherwise
identifying `repository_hash` are omitted; the blinded metric ID still provides
cross-device deduplication.
The CLI uses the key only while preparing the HTTP sync and does not persist it
in the local SQLite store or print it in the normal sync-status response.
Endpoints that do not expose the preflight capability get randomly generated
`ccm_...` IDs instead. Those IDs stay stable for a given store, but two devices
mint different IDs for the same commit, so a backend cannot deduplicate them;
the CLI warns when it uploads committed metrics without a blinding key.

`repository_hash` is a digest of a canonicalized remote URL, not a secret: an
observer holding a list of candidate remotes can confirm which repository a hash
belongs to. It is therefore treated as project-identifying metadata and is
omitted, together with `project_id`, unless project sync is enabled.

Committed metrics for days older than the 90-day Git observation window are
retained as already-materialized history rather than rebuilt. They stay in the
authoritative snapshot so a rescan never retires them, and only rescanned days
are recomputed. Trace-matched metrics are not carried forward: an attribution
claim depends on both a commit and the archived trace it was matched against,
and once the commit ages out that claim can no longer be reverified, corrected,
or retired when the trace behind it is deleted. Attribution is therefore a
rolling window over the churn totals, which stay complete. A carried-forward
committed metric is re-keyed to the account-scoped blinded ID as soon as one is
available, so commits that materialized before hosted login still deduplicate
across devices. Merge commits are excluded from committed totals: a merge diff
replays the churn its branch already reported
through the branch's own commits. Conflict resolutions carried only by a merge
commit are consequently not counted.

`ProjectInfo.path_label` is retained for owner-facing project location displays,
manual project linking, and hosted task review. Hashed path, source, event, and
summary identifiers remain so the server can deduplicate records and keep
stable location identity.

Canonical provider account identity may now sync through
`ProviderAccount.provider_user_id` and `ProviderAccount.email`. Hosted task
snapshots can also include bounded task titles, summary previews, todo excerpts,
repo labels, branch labels, path labels, and task verification actions. The
backend uses identity plus project metadata to route those hosted task records.
Cost payloads may include `provider_reported_micro_usd`,
`estimated_api_equivalent_micro_usd`, or task-level
`estimated_cost_micro_usd`. Receivers prefer these integer micro-USD values and
fall back to legacy rounded-cent fields when they are absent.

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
- validate the request body against `sync_batch.v1`, `sync_batch.v2`, and `sync_batch.v3`
- reject unsupported `schema_version` values
- deduplicate sources, accounts, source-account assignments, subscriptions, and summaries by their IDs when server-side deduplication is needed
- treat collector IDs as stable client-provided IDs, not database primary keys exposed to users
- compute daily, monthly, and dashboard rollups server-side from accepted summaries
- atomically replace accepted task bucket snapshots per `(user, device, project_bucket)`
- treat the ordered `authoritative_snapshot` fragments sharing one `snapshot_id`
  as the complete set of metadata and summary IDs owned by the authenticated
  device; v3 fragments also carry code-change metric IDs; each fragment carries
  zero-based `part_index` and a common
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
`sync_batch.v3` returns `sync_ack.v3`, which additionally adds the
`code_change_metrics` counter.
Collectors require the acknowledgement version to match the submitted batch
version exactly; a v1 acknowledgement cannot successfully acknowledge a v2
batch, and a v2 acknowledgement cannot acknowledge a v3 batch.

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
source-account-assignment, subscription, summary, and code-change metric IDs,
including empty lists.
The backend tracks ownership per authenticated device and keeps device-local IDs
separate from server-canonical IDs so account alias reconciliation cannot delete
the canonical row. Incremental and legacy batches omit the marker and never
prune absent records.

When a device completes its first authoritative snapshot against a database that
predates ownership tracking, the backend also reconciles legacy summary rows
whose stored `device_id` matches that device. Rows represented by the completed
snapshot are retained through their active ownership mapping; omitted unowned
rows are pruned. Legacy rows from other devices, and canonical rows still owned
by any device, are preserved.

The HTTP sink parses `sync_ack.v1`, `sync_ack.v2`, and `sync_ack.v3` before
updating local state. File and stdout sinks update state after their local write
succeeds.

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
verification actions for `sync_batch.v2` and `sync_batch.v3`, plus code-change
metrics for `sync_batch.v3`. The collector now prepares those
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
