# Observation schema (`statsai_subscription_value.observation.v1`)

One JSON object per JSONL line. Nested map of the v1 record; committed
shards are authoritative for field presence and types. This note does
not invent example rows.

```
observation
├── identity
│   ├── id                    string   stable row id (`claude-NNN`, `codex-NNN`, `cursor-NNN`, `grok-NNN`)
│   └── schema_version        string   `statsai_subscription_value.observation.v1`
├── provider                  string   `anthropic` | `openai` | `cursor` | `xai`
├── source
│   ├── primary               bool     first-party report vs attributed/repost
│   ├── url                   string   public source locator when known
│   └── captured_at           string   ISO-8601 capture time when known
├── plan
│   ├── plan_known            bool     subscription plan identified
│   ├── name                  string   plan label when known
│   └── price_usd             number   stated subscription price when known
├── usage
│   ├── raw_tokens            bool     original token counts present (not inferred-only)
│   ├── tokens                object   input/output/cache/total when reported
│   ├── window                object   usage period start/end when reported
│   └── api_equivalent_usd    number   stated or derived API-equivalent value
├── quality
│   ├── confidence            number   0–100 collector confidence
│   └── analysis_kept         bool     retained for the analysis subset
└── notes                     string   free-text collector notes when present
```

## Quality dimensions used in the n=800 audit

| Field | Meaning |
|---|---|
| `source.primary` | Observation is a primary report (`true` for 96% of baseline). |
| `plan.plan_known` | Plan identity is known (`true` for 71.5%). |
| `usage.raw_tokens` | Raw token figures are present (`true` for 32.75%). |
| `quality.confidence` | Collector score; ≥70 on 60.5%, ≥85 on 26.62%. |

High-confidence rollup in `data/summary.json` uses `confidence >= 80`
(308 of 800). Analysis-kept count is 491.

## File encoding

- UTF-8 JSONL, one observation per line, no wrapping array.
- Four shards of 200 rows; see `data/manifest.json`.
- IDs are unique across the corpus (`unique_observation_ids` = 800).
