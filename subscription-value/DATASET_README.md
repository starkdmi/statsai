# StatsAI subscription-value corpus

Machine-readable public-report corpus for subscription-value research.
Each observation is one attributed usage report for an AI coding
subscription (Claude, Codex, Cursor, or Grok).

This commit lands the directory layout, checksummed shard manifest,
rollup summary, and quality notes. **Observation rows are not invented
here.** The 800 JSONL rows arrive as four gzip+base64 shards in
follow-up messages and must match `data/manifest.json`.

## Versions

| Field | Value |
|---|---|
| Corpus | `1.0.0` |
| Schema | `statsai_subscription_value.observation.v1` |
| Baseline *n* | 800 |
| Unique IDs | 800 |
| Shard size target | 200 rows |

Manifest generated at `2026-09-13T08:35:52Z`.
Summary generated at `2026-09-12T22:44:00Z`.

## Layout

```
subscription-value/
  DATASET_README.md
  HANDOFF.md
  STATUS.json
  collection_state.json
  data/
    manifest.json
    summary.json
    observations.jsonl          # pointer note only; not the corpus
    observations/
      .gitkeep
      observations-0001.jsonl   # pending
      observations-0002.jsonl   # pending
      observations-0003.jsonl   # pending
      observations-0004.jsonl   # pending
    sources.jsonl               # referenced by manifest; not in this commit
    leads.jsonl                 # referenced by manifest; not in this commit
  schema/
    observation.schema.md
  findings/
    findings.md
```

Paths in `data/manifest.json` are relative to this directory.

## Provider mix (n=800)

| Provider | Observations | ID prefix |
|---|---:|---|
| anthropic | 360 | `claude-` |
| openai | 202 | `codex-` |
| cursor | 132 | `cursor-` |
| xai | 106 | `grok-` |

High-confidence subset (`confidence >= 80`): 308.
Analysis-kept subset: 491.

Collection status: `discovery_frontiers_largely_exhausted`.
Viral-quartet status: `exhausted_no_attributed_primary`.

## Shards

| File | Rows | Bytes | First ID | Last ID |
|---|---:|---:|---|---|
| `data/observations/observations-0001.jsonl` | 200 | 411376 | `claude-001` | `claude-200` |
| `data/observations/observations-0002.jsonl` | 200 | 458624 | `claude-201` | `codex-040` |
| `data/observations/observations-0003.jsonl` | 200 | 457572 | `codex-041` | `cursor-038` |
| `data/observations/observations-0004.jsonl` | 200 | 499151 | `cursor-039` | `grok-106` |

SHA-256 checksums live in `data/manifest.json`. Verify after decode:

```sh
cd subscription-value
python3 - <<'PY'
import hashlib, json, pathlib
manifest = json.loads(pathlib.Path("data/manifest.json").read_text())
ok = True
for shard in manifest["shards"]:
    path = pathlib.Path(shard["file"])
    data = path.read_bytes()
    digest = hashlib.sha256(data).hexdigest()
    rows = sum(1 for line in data.splitlines() if line.strip())
    checks = [
        (path.exists(), "exists"),
        (len(data) == shard["bytes"], f"bytes {len(data)}=={shard['bytes']}"),
        (digest == shard["sha256"], "sha256"),
        (rows == shard["rows"], f"rows {rows}=={shard['rows']}"),
    ]
    print(path, *("OK" if passed else f"FAIL:{label}" for passed, label in checks))
    ok = ok and all(passed for passed, _ in checks)
raise SystemExit(0 if ok else 1)
PY
```

## Quality snapshot

See `findings/findings.md` and `data/summary.json`. Headline rates on
the full 800-observation baseline:

- primary: 96%
- plan_known: 71.5%
- raw_tokens: 32.75%
- confidence ≥ 70: 60.5%
- confidence ≥ 85: 26.62%

## External benchmarks (summary only)

Copied from `data/summary.json` for convenience; not observation rows.

- Viberank (*n*=1116): p50 1479, p90 7685, mean 3570
- Tokenmaxxing top 100: p50 9689, p90 37596
- SemiAnalysis: Pro 20x $14,000; Claude Max 20x $8,000

## Schema

Brief nested field map: `schema/observation.schema.md`.
The committed shards are the ground truth for field presence.

## Status

`STATUS.json` is `layout_ready_awaiting_shards` until all four shards
are decoded, checksum-verified, and committed. Follow-up ingest
instructions are in `HANDOFF.md`.
