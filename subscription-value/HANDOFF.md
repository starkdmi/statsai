# Handoff: shard ingest

This PR commits layout, manifest, summary, and docs only.
**READY_FOR_SHARDS.**

Do not invent, reconstruct, or synthesize observation rows. The parent
run cannot pipe the ~1.8 MB corpus through chat. Follow-ups will deliver
`observations-0001` through `observations-0004` as gzip+base64.

## Expected payloads

Decode each follow-up to exactly one file under `data/observations/`.
Paths, sizes, checksums, and ID bounds are authoritative in
`data/manifest.json`.

| Deliverable | Output path | Rows | Bytes | SHA-256 | ID range |
|---|---|---:|---:|---|---|
| observations-0001 | `data/observations/observations-0001.jsonl` | 200 | 411376 | `b6e45899f0dd7152dabbb87fa267e0b0384328fb693a189f7df9aa41e1730c16` | `claude-001` … `claude-200` |
| observations-0002 | `data/observations/observations-0002.jsonl` | 200 | 458624 | `09d6b229601ef194c33812e0bfa453838557efc8f560b8159404021a8e56f538` | `claude-201` … `codex-040` |
| observations-0003 | `data/observations/observations-0003.jsonl` | 200 | 457572 | `d9be4be859181a9786acba2bbe0f4daf9c7ebb934f49fea5fff0c2c70be61c94` | `codex-041` … `cursor-038` |
| observations-0004 | `data/observations/observations-0004.jsonl` | 200 | 499151 | `87bc7198f2960c20ac25c7453ae2414a459f77270db97755526ebff71be9349e` | `cursor-039` … `grok-106` |

`data/observations.jsonl` stays a pointer note. Do not concatenate shards
into that path.

`data/sources.jsonl` and `data/leads.jsonl` are named in the manifest
`files` map but are not part of this layout commit.

## Ingest steps

1. Receive one gzip+base64 payload per shard.
2. Decode and gunzip to the output path above. Example:

   ```sh
   python3 - <<'PY'
   import base64, gzip, pathlib, sys
   src, dest = sys.argv[1], sys.argv[2]
   raw = pathlib.Path(src).read_text().strip()
   pathlib.Path(dest).write_bytes(gzip.decompress(base64.b64decode(raw)))
   PY
   payload.b64 data/observations/observations-0001.jsonl
   ```

3. Verify `sha256`, byte length, row count, `first_id`, and `last_id`
   against `data/manifest.json`. Reject the shard on any mismatch.
   Do not patch bytes to force a checksum.
4. After all four shards verify, set `STATUS.json` to
   `shards_committed: true`, `committed_observations: 800`,
   `verified_shards: 4`, `pending_shards: []`, and
   `phase: shards_verified`. Update `collection_state.json`
   `shard_ingest` the same way.
5. Commit the four JSONL files (keep `.gitkeep` or drop it) and update
   the open PR. Do not merge.

## Stop conditions

- Any SHA-256, size, row-count, or ID-bound mismatch: stop and report.
- Missing or truncated base64: stop and request a resend of that shard.
- Temptation to invent rows to “fill” 800: stop. The corpus is invalid
  without the original collector output.
