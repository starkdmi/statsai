# Sync Contract

`sync_batch.v1` is the legacy usage-only backend contract for `statsai`.
`sync_batch.v2` extends it with hosted task bucket snapshots and task
verification uploads. It also supports an optional `authoritative_snapshot`
marker for deletion reconciliation.
`sync_batch.v3` adds privacy-safe numeric code-change metrics and makes their
acknowledgement counts and authoritative snapshot IDs part of the versioned
contract. Versions 1 and 2 never treat those new fields as authoritative.
`sync_batch.v4` adds provider-neutral quota-cycle contributions and makes their
acknowledgement counts and authoritative snapshot IDs part of the versioned
contract. Versions 1–3 never treat those new fields as authoritative.
`sync_batch.v5` adds privacy-safe account-plan observations and aggregate
evidence-quality summaries, and makes their acknowledgement counts and
authoritative snapshot IDs part of the versioned contract. Versions 1–4 never
treat those collections or their snapshot IDs as authoritative. Because a v4
acknowledgement carries no counter for them, a collector must not place them in
a v4 batch even though a v4 backend would ignore the unknown keys.
The collector owns local scanning, normalization, idempotent local storage, and
privacy scrubbing. The backend owns authentication, validation, deduplication,
rollups, and user-facing queries. The production path sends sanitized batches to
a Cloudflare Worker backed by D1 and Better Auth device tokens.

Local Git collection is deliberately bounded: it inspects the current `HEAD`
and local branches, includes only commits from the last 90 days whose committer
email matches an identity the repository has been scanned under, and ignores
remote-tracking-only history. A repository no committer identity has ever been
known for is not scanned at all, rather than attributing commits speculatively
or reporting an empty scan as authoritative.
Stored Git scans are retained only while a current project or trace references
their repository. If an actively referenced repository temporarily fails to
scan, its last successful snapshot is retained with partial Git coverage;
unreferenced repository scans are retired from derived metrics and local cache,
including the aged metrics that the rolling window can no longer rebuild.

A repository is known by two names, its identity hash and its worktree root, and
either can change while the repository stays exactly where it was: adding an
origin remote re-keys it, and moving a worktree relocates it. Each stored
repository is therefore claimed by the scan that is the same repository, and its
aged metrics are rewritten onto the hash it goes by now, so retirement only ever
compares against hashes still in use.

Exempting a renamed repository from retirement is not sufficient, because a
re-keyed repository leaves aged metrics under a hash it has stopped using: once
that scan row is gone, no later refresh has any record of the old hash, so those
rows match nothing, survive every retirement decision, and are republished
indefinitely. Lineage is established by a shared worktree root, or failing that
by a shared commit — commit hashes are globally unique, so an overlap proves two
scans are the same repository even when both of its names changed at once.

Remembered committer identities are looked up under both names for the same
reason. A re-keyed repository is still found by its root, and a relocated one is
still found by its hash; either lookup alone would hand the scan an empty set in
the case the other covers, and the scan would then drop in-window commits made
under an earlier address and retire them remotely. The root is matched exactly
rather than by prefix, so a repository nested inside another never inherits its
parent's identities.

Removing a source's data rebuilds metrics whether or not any reconstructed edits
went with it, because committed churn is discovered from the project paths
carried by usage rather than from traces.

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

Account-plan sync sends only the canonical account reference, provider, raw and
display plan names, observation time, explicit provider bounds, evidence kind,
confidence, and aggregate coverage/conflict counts. Email, provider user ID,
conversation and turn IDs, artifact paths, tokens, and raw provenance remain local.

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
with `--delete-data` drops that source's reconstructed edits, coverage, identity
observations, plan observations, and conversation bindings, then rebuilds the
derived metrics immediately. The next authoritative snapshot therefore retires
hosted rows instead of continuing to republish deleted source evidence.

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
degrades the trace coverage stamped on every metric that run publishes. A trace
edit carrying no timestamp at all is skipped the same way, because metrics are
published per day and an edit belonging to no day cannot be carried by one.

Commits are attributed by committer email, and a repository remembers every
identity it has been scanned under rather than only the address `user.email`
holds right now. Reconfiguring that address does not rewrite the commits already
in the object database, so matching on the current value alone would report an
authoritative scan of zero commits, delete measured commits from the local store
and retire them remotely through the authoritative snapshot. The remembered
addresses are stored blinded, local to the device, and never synced.

A scan therefore fails only when no identity has ever been known for the
repository, which is an unanswerable question rather than an answer of "none". A
repository seen for the first time in that state stays unmeasured; one that
already knows an identity keeps measuring under it, so a temporarily missing
`user.email` costs no coverage at all.

Identities are never forgotten while the repository is still on record, so
configuring a colleague's address once claims it for that repository until the
repository itself is retired. Over-counting inside a repository the user already
works in is preferred to deleting history the user did write, and retiring a
repository once nothing references it drops its remembered identities along with
its commit rows.

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

When a local pricing ruleset advances, StatsAI reprices persisted normalized
events and refreshes the affected `sync_rollups` in place. Changed rollups are
marked dirty so a later incremental HTTP sync publishes the corrected values
without `--rebuild-rollups` or `--full`. Provider-reported cost fields are
never replaced by estimates.

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
device ownership or reconcile deletions. It rejects batches carrying
`quota_cycle_contributions` for the same reason: a local store keeps quota
observations and derives its own cycles from them, so acknowledging another
device's cycles would tell the sender they had been stored when they had not.
`/api/sync/batches` is the production contract. A compatible backend should:

- require an authenticated device access token
- accept `Authorization: Bearer <device_access_token>` from stored auth, `--auth-token`, or `STATSAI_SYNC_TOKEN`
- validate the request body against `sync_batch.v1` through `sync_batch.v5`
- reject unsupported `schema_version` values
- deduplicate sources, accounts, source-account assignments, subscriptions, summaries, and equivalent account-plan evidence when server-side deduplication is needed
- treat collector IDs as stable client-provided IDs, not database primary keys exposed to users
- compute daily, monthly, and dashboard rollups server-side from accepted summaries
- atomically replace accepted task bucket snapshots per `(user, device, project_bucket)`
- treat the ordered `authoritative_snapshot` fragments sharing one `snapshot_id`
  as the complete set of metadata and summary IDs owned by the authenticated
  device; v3 fragments also carry code-change metric IDs; v4 fragments also
  carry quota-cycle contribution IDs; v5 fragments also carry account-plan
  observation and evidence-summary IDs; each fragment carries
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

A chunk that fails is answered by one of two remedies, and never the other's. A
rejected size (HTTP 413) is a decision about that batch and is answered by
splitting it into smaller batches. A transient infrastructure failure (HTTP 500,
502, 503, or 504) is the *absence* of a decision — the batch was neither
accepted nor rejected, only the answer was lost — and is answered by resending
the identical chunk after a doubling backoff, three times before the run gives
up. Resending is safe because ingest records the batch ID in the same
transaction that applies the payload: a resent chunk either applies exactly once
or is acknowledged as a duplicate. Statuses are read without requiring a JSON
body, since these failures come from the infrastructure in front of the worker
and answer in plain text.

HTTP 429 is deliberately not resent on that schedule: it carries the endpoint's
own `Retry-After`, and retrying sooner would work against the limit it asked
for. Any other 4xx is a decision that repeating cannot change, so it fails the
run immediately.

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
`sync_batch.v4` returns `sync_ack.v4`, which additionally adds the
`quota_cycle_contributions` counter.
`sync_batch.v5` returns `sync_ack.v5`, which additionally adds the
`account_plan_observations` and `account_evidence_summaries` counters.
Collectors require the acknowledgement version to match the submitted batch
version exactly; a v1 acknowledgement cannot successfully acknowledge a v2
batch, a v2 acknowledgement cannot acknowledge a v3 batch, a v3
acknowledgement cannot acknowledge a v4 batch, and a v4 acknowledgement cannot
acknowledge a v5 batch.

The current loopback daemon returns this shape and reports duplicate events
when the existing store already has the semantic event. Source, account,
source-account assignment, subscription, and summary upserts are currently
reported as accepted writes. Its `quota_cycle_contributions` counter is
therefore always zero: a batch carrying any is refused outright rather than
acknowledged, since the daemon has nowhere to store them.

## Evolving the Contract

Two kinds of addition behave very differently, and the difference decides the
release order.

**A new top-level collection is backward compatible.** Batch collections carry
`#[serde(default, skip_serializing_if = "Vec::is_empty")]`, and the backend
reads named fields without a top-level allowlist, so a deployment that predates
the collection ignores it rather than failing. That is not silent data loss:
collectors require `accepted + duplicates == submitted` for every collection
they send, so an endpoint that ignores one reports zero against a non-zero
submission and the sync fails loudly with a count mismatch. Local sync state
stays dirty and the records are resent. Adding a collection therefore needs no
schema-version bump.

**A new field on an existing record is not.** Every synced record is checked
against a closed set of permitted keys, and an unrecognized key refuses the
whole batch with `400 invalid_sync_batch`. This is deliberate: an unexpected
key may carry a path, address, or message that this contract excludes, and the
refusal guarantees none of it is stored. The consequence is a hard release
order:

> **Deploy the backend before releasing a collector that adds a record field.**
> Until it is deployed, upgraded collectors cannot sync anything — not just the
> affected collection.

To keep the two causes distinguishable, the refusal names the offending field:

```json
{
  "error": "invalid_sync_batch",
  "rejected": [
    {
      "kind": "quota_cycle_contributions",
      "id": "quota_cycle_0f2c…",
      "reason": "unknown_field:has_schedule_overlap"
    }
  ]
}
```

Nested records report a dotted path, such as
`unknown_field:boundary_slices.working_directory`. Only the field name is
returned; its value is never echoed. Collectors render this as
`endpoint does not recognize \`<field>\` on <collection>` rather than an opaque
HTTP error.

Malformed values inside a *known* field remain a plain `400` with no `rejected`
detail — that is a client defect, not version skew.

## Local Sync State

After a successful sync, the collector records local sync state keyed by sink
and target. The state stores the last successful batch, event cursor, summary
cursor, and failure count. Passing `--since-last` sends only events and
summaries after the recorded cursor for that sink target while still including
the current source, account, source-account assignment, and subscription
metadata.

Full HTTP rollup syncs send their authoritative snapshot as the final logical
chunk. The marker lists all current source, provider-account,
source-account-assignment, subscription, summary, code-change metric,
quota-cycle contribution, account-plan-observation, and evidence-summary IDs,
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

The HTTP sink parses `sync_ack.v1` through `sync_ack.v5` before
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
verification actions for `sync_batch.v2` and later, plus code-change
metrics for `sync_batch.v3` and later, plus quota-cycle contributions for
`sync_batch.v4` and later, plus privacy-safe account-plan evidence for
`sync_batch.v5`. The collector now prepares those
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
contains no account records. Account-plan observations and evidence summaries
use the same mapping. When a newly observed alias matches historical
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
