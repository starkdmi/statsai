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
- whether local reconstruction saw another cycle in the same scope overlapping
  this one (`has_schedule_overlap`)
- UTC daily envelopes with timestamped first, last, minimum, and maximum percentages
- exact usage slices for partial UTC days at cycle and observed schedule-transition boundaries

Usage slices contain token-category totals and estimated API-equivalent cost in
integer micro-USD. Contributions omit raw events, event hashes, paths, source
IDs, quota payloads, plans, credits, provider slots, fingerprints, and sample
counts.

Codex currently emits only attributed weekly windows (`window_minutes = 10080`).
Five-hour, monthly, and unattributed cycles stay local. Reset timestamps may
shift, and consecutive cycles need not be exactly seven days apart.

A Codex weekly cycle starts lazily at the first request after the previous
reset, not at the previous reset itself. An idle stretch therefore leaves a gap
between cycles, while consecutive cycles are never closer than the nominal
duration unless the earlier one was reset ahead of schedule — banked by the user
or granted server-side. Overlapping schedules are consequently expected, and
`has_schedule_overlap` records that a single device reconstructed the overlap
from its own observations.

A reset is a handover: once a window resets, the provider reports the new
schedule and never returns to the old one. Local reconstruction therefore
discards any schedule that another schedule brackets — reported before it
appeared and again after it vanished — once that other schedule has passed
everything the bracketed one ever claimed. During the July 2026 tier migration
Codex answered scattered turns with a blank snapshot, near-zero usage against a
fresh weekly reset, interleaved with the live schedule for up to 34 hours; each
such run would otherwise become a cycle that takes days away from the cycle
actually running. A genuine early reset leads from the moment it starts and the
schedule it replaced stops being reported, so it can never be bracketed.

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
4. Drop daily envelopes whose first or last observation is more than one hour
   after that contribution's representative reset. Provider recomputation lag of
   about 47 minutes is genuine; replay stamps weeks later are not. Already-stored
   rows are filtered on read, and ingest strips the envelopes rather than
   rejecting the batch, so devices need not re-sync.
5. Merge remaining daily envelopes using earliest first, latest last, lowest
   minimum, and highest maximum observations, and derive contributing-device
   counts.
6. Never synthesize a zero-percent start. A device first observed at 80% joins
   the existing cycle identified by its reset; another device may supply earlier
   evidence.
7. Preserve cycles observed only by another device.
8. For overlapping schedules, use the first observation of the newer schedule as
   the transition boundary, clamped to the overlap. Do not assign usage to both
   cycles. When a device contributed to both cycles and flagged
   `has_schedule_overlap` on each, the overlap is an expected early reset and
   both cycles stay complete. An overlap no single device reconstructed is
   reported as a conflict, because it can only arise from cross-device records
   that failed to cluster or from two provider accounts resolving to one
   identity.
9. Combine full UTC-day summaries with exact boundary slices. If required
   boundary evidence is absent, return the available value as partial rather
   than prorating it.

A logical cycle remains while any device still contributes it. Stale device
contributions are retired through the same authoritative-snapshot ownership
used by summaries and code-change metrics.
