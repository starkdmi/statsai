# Observed Tools, MCP, and Skills Analytics

## Summary

- Collect activity automatically during normal `statsai scan`/daemon runs for Codex, Claude Code, OpenCode, and Grok Build; conversation archive collection remains optional.
- Read local records only—no OpenAI, Anthropic, OpenCode, or xAI account/API calls. Hosted data appears only through the existing StatsAI sync after explicit opt-in.
- Mirror the call-oriented approach used by [OpenAI Personal Analytics](https://help.openai.com/en/articles/20001478) and [OpenCode stats](https://github.com/anomalyco/opencode/blob/dev/packages/opencode/src/cli/cmd/stats.ts): rank observed calls/activations while keeping model tokens and cost separate.
- Add the experience at the bottom of `/dashboard/stats/`. Plugin identity is parent metadata for skills/MCPs, not a separate overlapping counter.
- Exclude Claude-style context-footprint history: `/context` reports what is loaded into the current context window, and that breakdown is not reliably persisted in local history. [Claude Code context documentation](https://code.claude.com/docs/en/debug-your-config)

## Data and Metric Contracts

- Add `ActivityInvocationV1` with a mutually exclusive `kind` of `tool`, `mcp`, or `skill`. Store a deterministic invocation ID, provider/source/account, UTC timestamp, exact display name, tool family, optional MCP server/tool, optional owning plugin, outcome, observed duration, detection evidence, and source-file hash.
- Use versioned tool families: `file-read`, `file-write`, `code-search`, `shell`, `web`, `browser`, `computer`, `agent`, `planning`, `media`, and `other`. Classification uses explicit aliases only; unknown names remain `other`.
- Never persist or sync arguments, results, commands, prompts, file paths, raw session/turn/call IDs, or assistant-prose inferences.
- Report only exact derived metrics:
  - observed calls/skill uses, daily trend, share, active days, and last used;
  - succeeded, failed, and unknown outcomes;
  - success rate as `succeeded / (succeeded + failed)`;
  - average duration from explicit duration fields or matched lifecycle timestamps for the same call ID, with sample count;
  - direct cost only when the local record explicitly reports a per-call USD amount. Missing cost is `null`/“not reported,” never zero and never calculated from model tokens or public price tables.
- Attach detection coverage per provider and kind:
  - Codex: native item-completion tool/MCP records are complete for that format; legacy wrapper records and catalog-resolved `SKILL.md` loads are partial.
  - Claude Code: structured tool, MCP naming, and native Skill events are complete; outcome/duration coverage remains sample-based.
  - OpenCode: structured tools and native `skill` parts are complete; MCP classification is partial unless configuration or record metadata identifies the server.
  - Grok Build: structured tools are complete for supported log formats, MCP is explicit-record-only/partial, and skills are unavailable until Grok emits an explicit skill record.
- Count Codex skills only when an exact `SKILL.md` under a discovered skill catalog is loaded, or an explicit skill-reading tool identifies it. Random files named `SKILL.md`, catalog availability, and prose mentions do not count; UI labels this evidence as an observed load rather than claiming server-equivalent Skill calls.

## Collector, Store, and Sync

- Extend adapter scans and daemon persistence with activity records, bump each provider parser revision for a one-time historical backfill, and reconcile changed/deleted files transactionally with usage records.
- Store normalized invocations locally and materialize UTC daily activity rollups. Re-resolve provider-account ownership by invocation timestamp when assignments change, retiring obsolete rollup IDs.
- Add `statsai activity status [--provider …] [--kind tool|mcp|skill] [--json]`, returning compact totals, top entities, date range, sample coverage, parser coverage, and hosted-sync state. Scan preview/status also reports discovered activity without writing.
- Add independent sync preference `include_activity`, defaulting to `false`, with `sync --include-activity` and `--exclude-activity`. Enabling marks all activity rollups dirty for historical backfill; disabling schedules deletion of the device’s hosted activity on the next successful sync.
- Introduce:
  - `activity_rollup.v1`: one row per device/source/account/day/entity, with readable names, counts, outcome totals, duration samples/sum, optional provider-reported direct micro-USD, and first/last observation times.
  - `activity_coverage.v1`: one row per source/day/kind with coverage level, evidence kind, and parser revision, including zero-call days where the source format was observed.
  - `sync_batch.v5`/`sync_ack.v5`, adding activity collections, counts, and `activity_rollup_ids`/`activity_coverage_ids` to authoritative snapshots.
- Exact skill, plugin, MCP-server, and custom-tool names sync only when activity is enabled. Project metadata is not part of activity v1. Turning activity off sends an authoritative empty activity set so the backend prunes readable names; sync v1–v4 never treat absent activity fields as authoritative.
- Deploy D1/API support for v5 before releasing the collector. Preserve v1–v4 ingestion compatibility, chunking/retry semantics, device ownership, remote reset, privacy inspection, and loopback diagnostic behavior.

## Hosted API and Stats UI

- Add authenticated `GET /api/dashboard/activity?range=<7d|30d|90d|all>&account=<selector>`.
- Return `dashboard_activity.v1` with:
  - totals for observed tool calls, MCP calls, and skill uses;
  - daily three-series activity;
  - tool-family totals;
  - up to 100 ranked rows per kind plus an exact “Other” aggregate;
  - row fields for provider, family/exact name, MCP server or plugin parent, calls, share, active days, last use, outcome counts/coverage, duration average/sample count, nullable direct cost, cost coverage, and detection evidence;
  - provider/kind coverage summaries.
- Store only bounded aggregate fields in D1, validate names and identifiers strictly, use indexed range/account/provider/entity queries, and add the endpoint to D1 query-budget checks.
- Append an “Agent activity” section after Model Summary on `/dashboard/stats/`:
  - cards for Tools, MCP calls, Skill uses, and reported direct cost;
  - a daily Tools/MCP/Skills chart;
  - Tools, MCPs, and Skills tabs with sortable ranked tables;
  - tool family alongside provider-exact name, `server · tool` for MCPs, and `plugin · skill` where ownership is known;
  - shared account/provider/time filters, top-ten collapse with “Show all,” coverage badges, partial-data explanations, and an opt-in empty state showing `statsai sync --sink http --include-activity`.
- If no source reports direct per-call cost, hide monetary ranking and show “Direct cost is not separately reported”; existing model cost above remains unchanged.
- Keep activity out of public profiles, projects, team analytics, leaderboards, and task views in v1.

## Test and Rollout Plan

- Adapter fixtures cover every provider, parallel calls, native skills, catalog skill loads, MCP parsing, legacy formats, missing results, failures, duration samples, duplicates, malformed records, and unsupported coverage.
- Store tests cover idempotent rescans, automatic backfill, cache revision upgrades, source-file removal, account reassignment, preview no-writes, daemon parity, and rollup conservation.
- Privacy tests prove no arguments, outputs, commands, paths, session IDs, or call IDs enter activity sync payloads.
- Sync/API tests cover v1–v5 compatibility, enable/backfill, disable/prune, chunk ACK counts, retry idempotency, authoritative fragments, device ownership, remote reset, validation bounds, account/provider/range filters, nullable cost, and query budgets.
- UI tests cover hook normalization, tabs, grouping, sorting, shared filters, “Other,” empty/error/loading states, partial coverage, unknown outcomes/durations, and missing versus explicit-zero cost.
- Verify with relevant Rust workspace tests and checks, `apps/api` unit/type/D1-budget tests, and `ui` test/lint/build.
- Implement on top of the current v4 quota branch without overwriting its existing dirty quota/daemon changes or the API’s unrelated untracked documentation files.
