# How the Local Conversation Archive Works

The conversation archive protects useful AI history from provider retention,
compaction, and format changes. It is a local canonical copy, separate from the
hosted sync contract.

## Canonical Records

Provider adapters normalize history into conversations, ordered items, and
content parts. Items distinguish messages, tool calls, tool results, readable
reasoning summaries, and artifacts. Content parts distinguish searchable text
from images, audio, and files.

Stable conversation and item IDs use provider-native identifiers when they are
available. They do not include the StatsAI device ID, which keeps the data model
compatible with future multi-device deduplication.

## Retention

Collection is additive. A later provider scan does not delete records merely
because the source no longer contains them. Re-reading the same source is
idempotent, while an updated native record replaces its canonical content.
Authoritative item updates remove content parts that the native record removed.
Partial or truncated rescans retain richer previously materialized parts.

Only an explicit future archive-purge operation should delete archived
conversation data. Existing source-removal and hosted-sync operations do not
upload or reconcile archive records.

## Content Policy

StatsAI stores:

- complete visible user, assistant, developer, and system message text;
- readable reasoning text and summaries;
- embedded base64 images, audio, and files as decoded SQLite BLOBs, up to 64
  MiB per artifact;
- explicit local file artifacts from provider-normalized user attachment
  blocks, up to 64 MiB per artifact;
- tool names, call IDs, statuses, arguments, and result previews;
- timestamps, models, usage evidence, project context, and source provenance.

StatsAI does not store opaque encrypted reasoning payloads because they cannot
be read or restored as conversation content. Tool-call text is bounded at 32
KiB. Tool-result text keeps a bounded head and tail up to 64 KiB. Truncated
parts retain the original byte count and SHA-256 hash.

Network artifacts are not fetched during local collection. Their URI is kept,
the conversation is marked `partial`, and `missing_content_count` is
incremented. This makes incomplete archives visible instead of silently
claiming complete recovery.

Local paths in assistant content, reasoning, tool calls, and tool results are
never opened. They remain unresolved external references, preventing generated
or tool-controlled content from copying unrelated files into the archive.

Local artifact paths must resolve to regular files. Files larger than 64 MiB,
special files, and artifacts that change beyond the limit while being read are
left as explicit missing references so collection cannot block or consume
unbounded memory. The archive cache tracks local artifact metadata separately
from provider records, allowing later file creation or modification to repair
the archived conversation without `--no-cache`.

Collection parses Codex JSONL records incrementally and commits each source
candidate in its own transaction. Interrupting a long first import therefore
retains completed files, and a later run resumes from the remaining candidates.
Parser or security-policy revisions invalidate the archive import cache when an
authoritative rescan is required to reconcile previously stored content.

## Storage and Search

Text remains ordinary SQLite text so it can be inspected and indexed directly.
Binary payloads are stored as BLOBs rather than base64, avoiding base64 storage
overhead. JSON boundaries encode those BLOBs as base64 for lossless export.

An external-content FTS5 table indexes only text. The search index is derived
from canonical content and can be rebuilt without changing the archive.

## Completeness States

- `complete`: every recognized visible content part was copied locally.
- `partial`: at least one referenced or malformed content part could not be
  copied.
- `metadata_only`: the provider exposed conversation metadata but no readable
  items.

Use `statsai conversation stats` to inspect archived text and binary sizes and
the number of missing content parts. Use `statsai conversation show <id> --json`
or `conversation export <id> --format json` when exact artifact payloads are
required.

## Privacy-Filtered Dataset

The exact archive remains canonical and unchanged. `statsai privacy filter`
creates a separate, versioned local dataset containing pseudonymized text and
selected conversation metadata. It is derived data that can be rebuilt after a
policy or archive change.

The filter uses OpenAI Privacy Filter and Kingfisher locally for free-text
detection. Typed project, repository, branch, path, and tool-call identifiers
are pseudonymized directly. Matching tool calls and results retain the same
pseudonymous identifier, including inside provider JSON, without removing or
renaming payload fields. Raw archive identifiers are omitted by schema.
Non-secret entities receive stable installation-local pseudonyms, and every
detected secret becomes `[SECRET]`. Completed output is scanned again before it
can be stored. A partial archive, detector failure, residual finding, stale
input, or missing pseudonym key makes the conversation non-exportable.

A successful scan means the configured detectors reported no remaining
findings. It does not prove that free text contains no missed sensitive content;
the dataset remains pseudonymized rather than anonymous and must be reviewed
against representative data before external use.

Filtered payloads retain provider, model, role, item kind, usage, ordering,
UTC day-level dates, attachment names and external URIs, and filtered text.
They omit binary bytes, binary hashes, source/native IDs, raw archive IDs, and
exact timestamps. The result is pseudonymized, not anonymous: conversational
content can still identify a person or project through context.

Filtering is explicit and never runs during collection or in the daemon:

```text
statsai privacy setup --mlx-server <path> --mlx-model <path> --kingfisher-helper <path>
statsai privacy status [--json]
statsai privacy filter [--provider <provider>] [--conversation <id>] [--preview]
statsai privacy show <conversation-id> [--json]
statsai privacy export --format jsonl --output <path> [--provider <provider>]
```

StatsAI runs MLX through one exported `4 x 1024` fixed trace and splits long
fields into overlapping 1024-token chunks. This prevents a persistent helper
from accumulating active graphs for several trace shapes. Setup defaults to a
4096 padded-token budget, a 4 GiB MLX allocator limit, and a 256 MiB allocator
cache limit; allocator limits are not hard process-memory caps.

`--preview` writes no key, pseudonym mapping, finding, or filtered payload.
JSONL export writes a manifest followed by conversations ordered by UTC day and
pseudonymous dataset key. This feature performs no backup, sync, or upload.
