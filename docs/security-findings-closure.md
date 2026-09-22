# Security findings closure

This record covers the local security findings that shipped in the
`statsai` crates and the follow-up review items on that work. Fixes stay
inside published crate sources so `cargo install` and binary releases
receive them. CLI commands and wire formats are unchanged.

## Historical audit coverage

The original audit inspected the then-current sources and their
dependencies. It did not compile the vendored FSEvents backend on macOS,
did not send raw sockets at the daemon or OAuth listeners, and did not
run the Linux secret-service fixture against a real `gnome-keyring`.
Those gaps hid:

- macOS-only type errors in `crates/statsai-daemon/src/watch/fsevent/darwin.rs`
- `EqualReader` allocating from a client-declared `Content-Length` in `Drop`
- a sequential-writer deadlock on HTTP/2.0 `505` responses
- symlink retargets that never emit `ROOT_CHANGED` on the canonical root
- a process-wide keyring fixture that treated `SetAlias` success as
  persistence

Linux CI still cannot typecheck `darwin.rs`. macOS CI must clippy both
`aarch64-apple-darwin` and `x86_64-apple-darwin` with `--features watch`.
A live FSEvents retarget test is compiled only on macOS.

`cargo package --verify` against crates.io still resolves sibling crates
to the published `0.5.0` API, so packaging checks inspect tarball
contents and rebuild from extracted sources rather than relying on
`--verify`.

## Finding-by-finding status

### F1 — Bundled SQLite and defensive mode

**Issue.** Older bundled SQLite missed the FTS5 floor, and connections
did not enable defensive mode before migrations.

**Fix.** Workspace `rusqlite` is `0.40.2` (`libsqlite3-sys` `0.38.2`,
bundled SQLite ≥ 3.53.2). Production helpers call
`SQLITE_DBCONFIG_DEFENSIVE` before queries.

**Evidence.**

- `statsai-store::sqlite_security::bundled_sqlite_meets_the_fts5_fix_floor`
- `defensive_mode_rejects_writable_schema_and_keeps_wal`
- `existing_database_reopens_without_rewriting_the_schema`
- `fts_insert_update_delete_and_search_work_under_defensive_mode`
- `snapshot_reads_schema_and_clone_stays_compatible_with_defensive_mode`

### F2 — HTTP request limits

**Issue.** The daemon and OAuth callback used unbounded tiny_http
defaults: header size, connection slots, pipelining, and slow clients.

**Fix.** `statsai-daemon` vendors a patched tiny_http. Limits are applied
before handlers: 8 KiB per line, 32 KiB of headers, 100 headers, 32
active connections, 32 queued requests, one outstanding request per
connection, a 5-second header deadline, and 30-second body and write
deadlines. Excess connections close immediately.

**Evidence.** `crates/statsai-daemon/src/http/tests.rs` and
`oauth_callback_keeps_state_checks_and_header_limits`, plus
`daemon_run_serves_health_and_rejects_oversized_headers`.

### F2 follow-up P1 — Client-sized allocations on request destruction

**Issue.** `EqualReader::drop` allocated `vec![0; remaining_to_read]`
from `Content-Length`. An unauthenticated `/health` or a rejected OAuth
callback can reach that destructor with an unread body. Header limits
and the 8 MiB sync cap do not bound that allocation.

**Fix.** Drop never sizes a buffer from the declared remainder. It
either shuts the cloned connection down or drains through an 8 KiB
stack buffer. Rejected and abandoned-body connections close instead of
being drained in the handler.

**Evidence.**

- `drop_never_allocates_the_declared_remainder` (`usize::MAX / 2`)
- `an_unread_enormous_body_closes_promptly_without_growing_the_discard_buffer`
  (`Content-Length: 4000000000`, RSS delta under 64 MiB on Linux, then
  `/recovered`)
- `daemon_run_serves_health_and_rejects_oversized_headers` huge-body
  path
- `oauth_callback_keeps_state_checks_and_header_limits` huge-body path
  against the callback listener

### F2 follow-up P1 — Unsupported HTTP version deadlock

**Issue.** `GET / HTTP/2.0` built a `Request` that owned the first
sequential writer, then tried to write `505` with a second writer. The
second writer waits for the first to drop; that drop cannot happen until
the write returns. Socket deadlines do not interrupt the channel wait.
Occupying all 32 slots prevented recovery.

**Fix.** Versions newer than HTTP/1.1 are rejected before `Request` is
constructed. The `505` uses the next sequential writer under the write
deadline, then the connection is shut down.

**Evidence.**

- `unsupported_http_version_releases_connection_slots` (max_active=2,
  four `HTTP/2.0` responses, four immediate disconnects, then
  `/after-version`)
- `daemon_run_serves_health_and_rejects_oversized_headers` `HTTP/2.0`
  `505` followed by a successful `/health`

The response write window starts on the first write, not when headers
are parsed, so a queued request can still reply after handler delay.
`write_deadline_starts_when_the_response_is_written` sleeps past a
150 ms limit before `respond` and still succeeds.

### F2 follow-up P2 — Unbounded chunk-size lines

**Issue.** Chunked request bodies used `chunked_transfer::Decoder`, which
buffers the chunk-size line and extensions in an uncapped `Vec`. The 8
MiB sync cap counts decoded payload only, so an authenticated
`Transfer-Encoding: chunked` request can spend megabytes and seconds on
framing (`2` plus padding, then `{}`) and still reach JSON validation.
The 30-second body deadline is not a memory bound.

**Fix.** The vendored server decodes chunks locally. Size digits are
folded into a `u64` one byte at a time. Extensions are skipped with a
counter. Metadata (size line plus extensions, excluding CRLF) is capped
at 8 KiB, matching header lines. Overflowing sizes and malformed framing
fail as `InvalidInput` and shut the connection down. Abandoned chunked
bodies close on `Drop` unless the last-chunk trailer was consumed.
Response encoding still uses the upstream `Encoder`.

**Evidence.**

- `chunk_size_at_the_metadata_cap_is_accepted` /
  `oversized_chunk_size_line_is_rejected` /
  `oversized_chunk_extension_is_rejected` /
  `unfinished_chunk_size_line_is_rejected` /
  `numeric_overflow_is_rejected`
- `oversized_chunk_metadata_is_rejected_and_the_server_recovers`
- `unfinished_chunk_metadata_hits_the_absolute_deadline`
- `overflowing_chunk_size_is_rejected_and_the_server_recovers`
- `valid_chunked_requests_decode_the_payload`
- `daemon_run_rejects_oversized_chunk_metadata_without_json_validation`
  (valid `{}` reaches `invalid batch`; a padded size line does not;
  then `/health`)

### F3 — Bounded JSON decoding

**Issue.** `into_json()` could decompress or parse unbounded response
bodies, then advance sync cursors or replace credentials.

**Fix.** `read_json_limited` / `read_encoded_json_limited` read at most
`limit + 1` decompressed bytes. Health is 64 KiB, auth is 1 MiB, other
JSON is 10 MiB.

**Evidence.** `statsai-core::json_limit` tests for exact limits,
overflow, gzip expansion, chunked bodies, misleading lengths, and
malformed JSON. Auth refresh and sync HTTP paths keep persisted state
when parsing fails.

### F4–F7 — macOS watching

**Issue.** notify 8.2.0’s FSEvents backend could panic on unknown flags,
drop non-UTF-8 paths, mishandle null CoreFoundation releases, lose
configured-to-canonical mappings, and fail to coalesce overflow into a
retryable rescan.

**Fix.** `statsai-daemon` embeds a patched backend. Raw event bytes are
kept, unknown flags request a rescan, unsupported registrations return
errors, null releases are skipped, and configured paths stay mapped
across deletion. Pending paths cap at 4,096 and collapse to one rescan.
Failed scans restore that notice.

**Evidence.** `watch/fsevent/logic.rs` and `watch/pending.rs` tests, plus
`cargo clippy -p statsai-daemon --features watch` in `scripts/rust-ci.sh`.

### F4–F7 follow-up P1 — macOS compilation

**Issue.** `FsEventWatcher` and `poll_restart` were `pub(super)` inside
`fsevent`, so `watch` could not name them. The backend called notify’s
private `recursive_mode.is_recursive()`, captured `stream.0` inside a
`move` closure (bypassing `CFSendWrapper: Send`), and imported unused
`std::ptr`. Linux CI cannot see these errors.

**Fix.** Both items are `pub(in crate::watch)`. Recursion uses
`matches!(recursive_mode, RecursiveMode::Recursive)`. The FSEvents
thread consumes the wrapper through `into_inner()` after the move.
`std::ptr` and the unused `PathBuf` import are gone. `poll_restart`
errors convert from `notify::Error` to `anyhow::Error`. The unused
`refresh_targets` wrapper is gone; production and tests call
`refresh_changed_targets`.

**Evidence.** macOS `clippy` for `aarch64-apple-darwin` and
`x86_64-apple-darwin` with `--features watch` (`scripts/rust-ci.sh` and
the macOS CI job). Linux-only checks still cannot typecheck `darwin.rs`.
The ARM64 development build compiles `statsai-daemon` on macOS and
catches the same errors.

### F4–F7 follow-up P2 — Symlink retargeting

**Issue.** Registrations use the canonical target. Replacing the
configured symlink elsewhere need not emit `ROOT_CHANGED` on that
target. Periodic reconciliation only compared configured paths, so an
unchanged path did not re-register. An earlier test called
`refresh_targets` directly; that wrapper is gone.

**Fix.** `poll_restart` (the 250 ms watch-loop hook) compares canonical
targets via `refresh_changed_targets`. A change rebuilds FSEvents
registrations and emits a full rescan. Deleted mappings and scan retry
state are left in place. Missing paths keep the last canonical.

**Evidence.**

- `a_symlink_retarget_is_detected_by_comparing_canonical_paths` (atomic
  rename, no `ROOT_CHANGED`)
- `live_watcher_detects_an_atomic_symlink_retarget_and_new_writes`
  (macOS CI: real `FsEventWatcher`, `poll_restart` only, then a write
  under the new target)

### F8 — Linux keyring encryption

**Issue.** The secret-service backend could negotiate `plain`.

**Fix.** `keyring` `3.6.3` enables `crypto-rust` with `sync-secret-service`.
Apple and Windows native backends are unchanged.

**Evidence.** `linux_secret_service_negotiates_an_encrypted_session`
asserts `dh-ietf1024-sha256-aes128-cbc-pkcs7` and rejects `plain`.

### F8 follow-up P2 — Isolated keyring fixture

**Issue.** CI failed with `NoStorageAccess(NoResult)`. The fixture
trusted `SetAlias` without checking that a new connection resolved
`default`. GNOME persists the alias in
`$XDG_DATA_HOME/keyrings/default`; a missing directory fails silently,
and a later client reloads `login`. Process-wide `DBUS_SESSION_BUS_ADDRESS`
and `XDG_*` mutations raced with parallel tests.

**Fix.** The parent creates `data/keyrings` mode `0700`, starts dbus and
gnome-keyring in a child environment, provisions an unlocked collection,
rewrites the alias file to that collection’s name, and checks
`ReadAlias("default")` on a fresh connection. The credential probe runs
in a child process with `STATSAI_KEYRING_CHILD` and the isolated env.
Daemon stderr is retained. Process-wide variables are not changed.

**Evidence.** `linux_secret_service_negotiates_an_encrypted_session` on
Linux with `dbus` and `gnome-keyring` installed.

## Packaging

Patched sources live under `crates/statsai-daemon/src/http/tiny_http` and
`crates/statsai-daemon/src/watch/fsevent`, so they are part of the
published `statsai-daemon` crate. SQLite defensive mode lives in
`statsai-store`. The `crypto-rust` feature lives in `statsai`. Packaging
verification extracts those crates outside the workspace and checks that
the patched files and Cargo features are present, then builds the
extracted `statsai-daemon` / `statsai-store` / `statsai` sources against
the extracted path dependencies.

## Linux verification (this environment)

`scripts/rust-ci.sh full` passed on Linux x86_64 (fmt, clippy including
`statsai-daemon --features watch`, workspace tests, and watch tests).
Covered regressions include:

- huge unread `Content-Length` on the daemon, echo server, and OAuth callback
- HTTP/2.0 `505` slot release, including client disconnect then `/after-version`
- oversized chunk-size lines and extensions (8 KiB cap), unfinished metadata
  hitting the body deadline, numeric overflow, valid chunked `{}`, and daemon
  recovery without `invalid batch`
- canonical symlink retarget comparison (no live FSEvents backend here)
- isolated Linux keyring child process with encrypted `OpenSession`

Packaged `0.5.0` crates were extracted to a separate workspace, patched
sources were grepped in the tarballs, and the same regressions were
re-run against those extracted sources.

This environment cannot typecheck `darwin.rs` or clippy
`aarch64-apple-darwin` / `x86_64-apple-darwin`. macOS CI clippy both
Apple targets with `--features watch`, and
`live_watcher_detects_an_atomic_symlink_retarget_and_new_writes` passed
on `macos-latest`. The ARM64 development build compiles `statsai-daemon`
on macOS.
