# Quota window projection contract

StatsAI stores provider quota observations locally and reconstructs reset windows without adding
them to `SyncBatch.v3`. The standalone hosted-facing contract is available from the CLI:

```sh
statsai schema quota-window-projection
```

`quota_window_sync_projection.v1` is a device contribution. Its `projection_id` is deterministic
for the device, provider account, limit identity, duration, and cluster anchor point. It is not a
logical account-window identifier. Projections omit source paths, local source/account row
identities, provider payloads, usage tokens, and cost.

Only attributed windows lasting at least 10,080 minutes are emitted by:

```sh
statsai quota export --level sync-windows --format jsonl
```

The command warns when locally observed weekly history is not fully attributed.

Before attribution, local window reconstruction is partitioned by stable source ID so evidence
from separate local provider installations cannot be blended. That source partition is local-only:
once evidence is attributed, account identity becomes the reconstruction scope, and no source ID is
included in the sync projection.

## Server merge semantics

1. Group contributions by stable provider account, provider, limit identity, and duration.
2. Cluster reset epochs whose raw reset values are within five minutes. This tolerance covers
   provider recomputation drift; it is not a device clock-skew correction.
3. Deduplicate change points by `point_fingerprint`.
4. Preserve the chronological percentage sequence. Do not sum percentages and do not replace the
   sequence with a maximum observed percentage.
5. Retain the logical window while at least one device projection contributes it.
6. Join the merged interval to the existing server-deduplicated usage rollup for the provider
   account. Never derive or sum token/cost values from quota projections.

Adding this projection to a future hosted wire payload is a separate capability and versioning
decision; this contract does not change `SyncBatch.v3`.
