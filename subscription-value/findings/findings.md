# Findings (n=800 baseline)

Quality audit of the collected subscription-value corpus. Counts come
from the collector summary (`data/summary.json`, generated
2026-09-12T22:44:00Z). Observation rows themselves are not in this
commit.

## Coverage

| Provider | *n* | Share |
|---|---:|---:|
| anthropic | 360 | 45.0% |
| openai | 202 | 25.2% |
| cursor | 132 | 16.5% |
| xai | 106 | 13.2% |
| **total** | **800** | **100%** |

- High confidence (`confidence >= 80`): **308** (38.5%)
- Analysis-kept subset: **491** (61.4%)

## Quality audit

Rates are on the full 800-observation baseline.

| Dimension | Rate | Count (of 800) |
|---|---:|---:|
| primary | 96% | 768 |
| plan_known | 71.5% | 572 |
| raw_tokens | 32.75% | 262 |
| confidence ≥ 70 | 60.5% | 484 |
| confidence ≥ 85 | 26.62% | 213 |

Almost all rows are primary reports. Plan identity is known for a
majority. Raw token figures are the scarce field: only about one in
three observations includes them. Confidence is bimodal enough that
the analysis-kept cut (491) sits near the `confidence >= 70` band
(484), while the stricter `>= 85` slice is about a quarter of the
corpus.

## Collection frontiers

- `collection_status`: `discovery_frontiers_largely_exhausted`
- `viral_quartet_status`: `exhausted_no_attributed_primary`

Further harvest is expected to add few new primary, attributable
observations. The viral-quartet pass did not yield an attributed
primary source.

## External reference points

Not corpus rows. Copied from the collector summary for later
comparison work:

| Source | Statistic |
|---|---|
| Viberank | *n*=1116; p50 1479; p90 7685; mean 3570 |
| Tokenmaxxing top 100 | p50 9689; p90 37596 |
| SemiAnalysis | Pro 20x $14,000; Claude Max 20x $8,000 |

## Next

Ingest the four checksummed JSONL shards (`HANDOFF.md`) before any
row-level analysis. Until those files verify against
`data/manifest.json`, treat this document as audit metadata only.
