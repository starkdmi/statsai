# Quota cycle contribution contract

StatsAI stores provider quota observations locally and reconstructs reset
windows before sync. Hosted quota history travels inside `sync_batch.v4` as
`quota_cycle_contribution.v1` records. The standalone
`quota_window_sync_projection.v1` export remains available for diagnostics and
is not part of hosted sync.

```sh
statsai schema sync-batch
statsai schema quota-window-projection
```

A contribution is a deterministic device record for one attributed cycle. Its
`contribution_id` is derived from the device, provider account, limit identity,
nominal duration, and the first reconstructed change-point fingerprint. It is
not a logical account-cycle identifier. The authenticated device identity on
the wire is supplied by the backend token; contributions themselves do not
carry a `device_id`.

Each hosted contribution contains only:

- the deterministic contribution ID
- provider and provider-account ID
- optional provider limit ID and nominal duration
- the representative reset timestamp
- UTC daily envelopes with timestamped first, last, minimum, and maximum percentages
- exact usage slices for partial UTC days at cycle and observed schedule-transition boundaries

Usage slices contain token-category totals and estimated API-equivalent cost in
integer micro-USD. Contributions omit raw events, event hashes, paths, source
IDs, quota payloads, plans, credits, provider slots, fingerprints, and sample
counts.

Codex currently emits only attributed weekly windows (`window_minutes = 10080`).
Five-hour, monthly, and unattributed cycles stay local. Reset timestamps may
shift, and consecutive cycles need not be exactly seven days apart.

Complete UTC days are not copied into the contribution. The backend joins
existing daily summaries for those days. Boundary slices exist only to avoid
inaccurate proration at cycle and schedule-transition edges. Usage observed on
different devices is additive; hosted sync does not deduplicate usage events
across devices.

Only attributed cycles are synced. Before attribution, local reconstruction
is partitioned by stable source ID so evidence from separate local provider
installations cannot be blended.

## Server merge semantics

1. Group contributions by canonical provider account, provider, limit identity,
   and nominal duration.
2. Cluster reset candidates only when their total spread is at most five
   minutes. This tolerance covers provider recomputation drift; it is not a
   device clock-skew correction.
3. Use the median reset as the representative reset.
4. Merge daily envelopes using earliest first, latest last, lowest minimum, and
   highest maximum observations, and derive contributing-device counts.
5. Never synthesize a zero-percent start. A device first observed at 80% joins
   the existing cycle identified by its reset; another device may supply earlier
   evidence.
6. Preserve cycles observed only by another device.
7. For overlapping schedules, use the first observation of the newer schedule as
   the transition boundary, clamped to the overlap. Do not assign usage to both
   cycles.
8. Combine full UTC-day summaries with exact boundary slices. If required
   boundary evidence is absent, return the available value as partial rather
   than prorating it.

A logical cycle remains while any device still contributes it. Stale device
contributions are retired through the same authoritative-snapshot ownership
used by summaries and code-change metrics.
