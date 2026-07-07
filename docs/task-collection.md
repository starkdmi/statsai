# Task Collection and Verification

`statsai` can extract local task spans during `scan`, rebuild derived work items
in SQLite, and let you verify or correct those work items from the CLI. Older
usage-only clients still emit `sync_batch.v1`, while hosted task sync uses
`sync_batch.v2` task bucket snapshots and task verification uploads.

## Normal Loop

Run the local workflow in this order:

1. Collect or refresh spans with `scan`.
2. Review derived work items with `task list`.
3. Inspect evidence for anything ambiguous with `task show --include-evidence`.
4. Record manual constraints with `task verify ...`.
5. Re-run `task benchmark` as your verified set grows.

Example:

```sh
cargo run -p statsai -- scan
cargo run -p statsai -- task list
cargo run -p statsai -- task show work_123 --include-evidence
cargo run -p statsai -- task verify rename work_123 --title "Implement local task collection"
cargo run -p statsai -- task benchmark
```

If you only want to preview what `scan` would rebuild, use:

```sh
cargo run -p statsai -- scan --preview
```

## Commands

### `task list`

```sh
cargo run -p statsai -- task list
cargo run -p statsai -- task list --provider codex
cargo run -p statsai -- task list --status needs_review
cargo run -p statsai -- task list --status rejected_meta
cargo run -p statsai -- task list --json
```

`task list` shows derived work items, not raw spans. By default it hides
`rejected_meta` items. Use `--status rejected_meta` when you want to inspect
items the grouper currently treats as meta, system, or noise.

### `task show`

```sh
cargo run -p statsai -- task show work_123
cargo run -p statsai -- task show work_123 --include-evidence
cargo run -p statsai -- task show work_123 --include-evidence --json
```

With `--include-evidence`, `task show` includes:

- member spans
- repo, branch, session, and thread anchors when present
- summary previews
- relevant manual verification records

Use it before accepting, rejecting, splitting, merging, or renaming a work
item.

### `task verify`

```sh
cargo run -p statsai -- task verify accept work_123
cargo run -p statsai -- task verify reject work_123 --reason noise
cargo run -p statsai -- task verify split work_123 --after-span span_456
cargo run -p statsai -- task verify merge work_123 work_124 --title "Unified task"
cargo run -p statsai -- task verify rename work_123 --title "Investigate sync regressions"
```

Verification commands rebuild the affected project bucket immediately and return
the stored verification plus the rebuild count.

Use each subcommand like this:

- `accept`: mark the current grouping as verified.
- `reject`: mark the item as `meta`, `system`, or `noise`.
- `split`: break one work item into two after a member span.
- `merge`: combine two work items from the same project bucket.
- `rename`: replace the canonical title while preserving membership.

### `task stats`

```sh
cargo run -p statsai -- task stats
cargo run -p statsai -- task stats --json
```

`task stats` summarizes the current local store:

- total spans
- total work items
- verified percentage
- no-git percentage
- cross-provider percentage
- rejected-meta percentage
- average spans per work item

### `task export`

```sh
cargo run -p statsai -- task export --level work-item --format json
cargo run -p statsai -- task export --level work-item --format jsonl
cargo run -p statsai -- task export --level span --format json
```

Supported combinations are:

- `--level work-item --format json`
- `--level work-item --format jsonl`
- `--level span --format json`
- `--level span --format jsonl`

### `task rebuild`

```sh
cargo run -p statsai -- task rebuild --all
cargo run -p statsai -- task rebuild --provider codex
cargo run -p statsai -- task rebuild --source-id src_123
```

Normal `scan` already rebuilds affected project buckets. `task rebuild` is
mainly for store repair, selective debugging, or after changes that require a
full derived-work-item refresh.

## Statuses

Work items use four statuses:

- `auto`: the grouper is confident and no manual override exists.
- `needs_review`: the item looks plausible but the evidence is mixed or weak.
- `verified`: a manual `accept`, `rename`, `split`, or `merge` has established local ground truth.
- `rejected_meta`: the item was classified or manually marked as meta/system/noise.

`needs_review` commonly appears when the item has one or more review reasons
such as:

- `no_usage_evidence`
- `zero_token_usage`
- `no_git_anchor`
- `generic_title`
- `weak_title`
- `low_specificity_title`
- `cross_provider_merge`
- `low_signal_exchange`
- `multi_day_no_anchor`

## Hosted Sync Boundaries

The task collector keeps bounded derived text such as titles, summaries, todo
excerpts, and workspace anchors. It does not duplicate full transcripts. Hosted
sync can upload those bounded task snapshots and hosted verification actions to
your private dashboard. Prompts, model responses, raw provider log lines, parse
line numbers, and other full-transcript evidence stay local.
