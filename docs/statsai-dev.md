# Exact-SHA development workflow

`statsai-dev` selects immutable StatsAI development artifacts and launches them
against the database its environment selects: a reusable, isolated APFS clone
under `local` and `dev`, and the production database under `prod`. Isolation is
therefore a property of the environment, not of the launcher — see
[Forward StatsAI commands](#forward-statsai-commands). It is intentionally a
CLI-only Apple Silicon workflow in v1; it never installs a development daemon or
rewires the menu bar application.

## Install the launcher

From a checkout:

```sh
cargo install --path crates/statsai-dev
```

Or directly from GitHub:

```sh
cargo install --git https://github.com/starkdmi/statsai statsai-dev
```

The repository is public, but downloading GitHub Actions artifacts normally
requires GitHub authentication. The launcher reads, in order,
`STATSAI_DEV_GITHUB_TOKEN`, `GH_TOKEN`, `GITHUB_TOKEN`, or the result of
`gh auth token`. The simplest setup is:

```sh
gh auth login
```

The token needs read access to repository Actions artifacts. It is never stored
by `statsai-dev`.

## Create the isolated database

Refresh once before forwarding ordinary StatsAI commands:

```sh
statsai-dev data refresh
statsai-dev data status
```

The refresh takes a SQLite writer lock, checkpoints WAL through SQLite, and
uses APFS `fclonefileat` to publish a consistent clone at:

```text
~/.cache/statsai-dev/data/statsai.sqlite
```

The production source remains:

```text
~/.statsai/statsai.sqlite
```

Source and destination must be on the same APFS volume. There is no ordinary
byte-copy fallback: if copy-on-write cloning is unavailable, refresh fails
without replacing the existing development database. The initial clone shares
disk blocks with production, while later writes to either file allocate their
own changed blocks.

The database is independent of build selection. Moving between PRs, `main`,
and exact SHAs keeps the same evolving development data. To reset that state or
remove it entirely:

```sh
statsai-dev data refresh
statsai-dev data clean
```

`data clean` does not retain historical snapshots.

`data refresh` writes the clone whatever environment is selected. Under `env
prod` the clone is therefore *not* the store forwarded commands open, and the
refresh says so. Carrying the previous clone's sync cursors also means opening
the clone, which migrates it to the schema `statsai-dev` itself links — so the
schema the clone ends up at can be ahead of the one it was copied with, and the
refresh reports both.

## Select an exact build

```sh
statsai-dev use pr 18
statsai-dev use main
statsai-dev use 46f56a8b34021d35f6e0937958131dd796f3df48
```

For a PR or `main`, the launcher first resolves the current full commit SHA.
It then accepts only a completed successful `dev-build` workflow run whose
`head_sha` is that exact value. A failed current `main` build never falls back
to an earlier successful commit. Across separate runs, the newest successful
artifact-bearing run wins. For a rerun, only its current attempt is eligible:
GitHub no longer exposes artifacts from superseded attempts, so a failed rerun
does not fall back to an inaccessible earlier attempt.

By default the command waits while the exact run is queued or in progress:

```sh
statsai-dev use pr 18
```

Waiting is bounded to one hour. Polling backs off, uses a slower schedule when
no GitHub token is available, honors GitHub's `Retry-After` and rate-limit reset
headers, and retries a limited number of transient transport or server errors.
If the deadline is reached, the command fails without substituting another
commit; run it again to continue waiting for the same or newly resolved head.

To inspect the current build state without waiting:

```sh
statsai-dev use pr 18 --no-wait
```

`--no-wait` exits with status 2 when the exact artifact is not ready. If the PR
advances after resolution, the launcher finishes the originally resolved SHA
and reports the newer head; it never changes commits halfway through an
install.

Each download is checked before selection:

1. the ZIP contains only `statsai`, `build.json`, and `SHA256SUMS`;
2. paths, duplicate entries, and symlinks are rejected;
3. the SHA-256 checksum matches;
4. `build.json.repository` is `starkdmi/statsai`;
5. `build.json.sha` is the exact resolved SHA;
6. workflow run ID and attempt match the downloaded run;
7. the supported store schema version and pricing ruleset version are recorded
   in the manifest (`build.json` schema 2);
8. the target and Mach-O header are ARM64 macOS.

Selection is an atomic state-file replacement. The cache retains the current
and previous extracted builds; downloaded ZIP data is discarded. Remove every
obsolete build while keeping the current selection with:

```sh
statsai-dev clean
```

This never removes the development database.

## Select the backend

```sh
statsai-dev env local
statsai-dev env dev
statsai-dev env prod
```

The profiles set the existing `STATSAI_API_URL` and `STATSAI_WEB_URL` variables
for the launched process, and select the store (see
[Forward StatsAI commands](#forward-statsai-commands)):

| Profile | API | Web | Store |
|---|---|---|---|
| `local` | `http://127.0.0.1:8787` | `http://127.0.0.1:3000` | dev clone |
| `dev` | `https://dev-api.statsai.dev` | `https://dev.statsai.dev` | dev clone |
| `prod` | StatsAI production defaults | StatsAI production defaults | production |

Selection and environment changes compose:

```sh
statsai-dev use pr 18 --env dev
statsai-dev use main --env prod
```

Environment changes never refresh or replace the development database. Auth
records remain namespaced by backend URL, as they are in the stable CLI.

## Forward StatsAI commands

No `run` keyword is needed:

```sh
statsai-dev scan
statsai-dev report monthly
statsai-dev sync
statsai-dev doctor
statsai-dev auth login
```

Every normal command is executed as the selected binary with the selected URL
profile and an injected `--store`. **The environment selects the store**:

| environment | backend | injected `--store` |
| --- | --- | --- |
| `local`, `dev` | local / dev API | `~/.cache/statsai-dev/data/statsai.sqlite` |
| `prod` | production API | `~/.statsai/statsai.sqlite` |

So `statsai-dev env prod` gives you the real CLI — production backend against
production data — and `statsai-dev env dev` gives you a PR build against a
throwaway clone. Forwarded `--store` options are rejected.

The two stores carry the same device id, so the server keys that device's
`last_batch_id` to whichever store synced last. Crossing a backend with the other
store therefore leaves the local sync pointer unreachable and promotes the next
`sync` to a full-history upload of the whole account. Binding the store to the
environment makes those two pairings unreachable.

`--prod-data` used to select the database independently of the backend and has
been removed; `statsai-dev env prod` replaces it.

A `prod` selection stored before this change meant "production backend, isolated
clone". It is reset to `dev` on first read, with a note, so the old selection is
never silently reinterpreted as permission to open the production database; run
`statsai-dev env prod` to opt in to the new meaning.

The prod environment is allowed only when the production database schema **and**
applied pricing ruleset exactly match the versions supported by the selected
build; it prints a warning when it proceeds. Missing, older, or newer production
pricing metadata is refused. A development build never migrates or reprices
production data as a side effect of a forwarded command, so a schema-changing or
pricing-changing PR can only be tested under `env dev` against the isolated
clone.

## Upgrade production to a merged build

Released `statsai` reaches Homebrew and GitHub releases on its own cadence, so
production can sit on an older schema than `main` for as long as that takes —
and the check above then refuses `env prod` outright. `data upgrade-prod` is the
deliberate way across:

```sh
statsai-dev use main
statsai-dev data upgrade-prod
```

It refuses unless GitHub reports the selected commit as `main`'s head
(`identical` on `compare/main...<sha>`). A PR head's migrations are a proposal
that can still be revised or dropped, and a production database stamped with a
version that never shipped has no supported way back — so PR builds are never
eligible, however green their CI is.

Merely being *contained* in main is not enough either. A schema-changing commit
that main later reverted stays an ancestor of main forever, so accepting
`behind` would keep accepting a build whose migration main deliberately removed.
Only main's head describes what main ships now, so if main has advanced, re-run
`statsai-dev use main` and try again.

The check runs twice: once up front, and again after the confirmation prompt,
immediately before the migration. The prompt waits as long as you do, and main
can advance — or revert this very migration — while it waits.

It also refuses to move backwards: a build behind production's schema or pricing
ruleset has nothing to offer it. A build level with production reports that
there is nothing to do.

Before migrating, it clones production to
`~/.statsai/backups/statsai-schema<N>-<timestamp>.sqlite`. The clone is
copy-on-write, so backing up a multi-gigabyte database costs almost no time and
almost no space. The migration itself runs as the selected build's `statsai
store migrate`, which applies the schema and the compiled pricing ruleset and
nothing else.

Afterwards it reads production back. A migration that exits zero without leaving
production at the schema and pricing ruleset the build declares fails the command
and names the backup, so a `--yes` caller that restarts the daemon or resumes
syncing on success never does so over a half-finished upgrade.

Nothing else may hold the database. Both the upgrade and the restore refuse when

- the `dev.statsai.daemon` LaunchAgent is loaded,
- anything answers on `127.0.0.1:8765`, or
- any other process has `~/.statsai/statsai.sqlite` or its `-wal`/`-shm`/`-journal`
  open, as reported by `lsof`,

and they re-check immediately before touching the database. The third check is
the one that matters most: `statsai daemon --api` accepts any loopback address,
so a daemon started by hand can hold the store while the first two say nothing,
and it is also the only check that does not depend on which binary started the
daemon. A daemon from an older release keeps its own long-lived connection, and
left running across an upgrade it writes events priced by its older catalog while
the metadata now says the database is current — so nothing reprices them
afterwards. A restore would meanwhile replace the file underneath that
connection.

A probe that cannot answer counts as a refusal, not an all-clear: `lsof` failing
to run, and `launchctl` or `id` failing so that the LaunchAgent's state is
unknown, both stop the command. That last one matters because a `KeepAlive`
daemon between restarts holds neither the port nor the database, so during that
window the LaunchAgent is the only signal that it is coming back. `--yes` skips
the confirmation prompt, not any of these checks.

```sh
statsai service uninstall   # or: launchctl bootout gui/$(id -u)/dev.statsai.daemon
```

To roll back:

```sh
statsai-dev data restore-prod            # newest backup
statsai-dev data restore-prod <path>     # a specific one
```

Restoring is a clone, not a `mv`. A database is more than its main file: moving
one over `~/.statsai/statsai.sqlite` would leave production's own `-wal` and
`-shm` beside it, and SQLite would replay frames written *after* the backup into
the database just restored. `restore-prod` checkpoints the backup, publishes a
sidecar-free copy atomically, and displaces the destination's sidecars as part of
that swap. It also backs up the database it replaces first, so a rollback is
itself reversible.

Ordinary isolated `statsai-dev` stores are opened by the selected exact-SHA
`statsai` binary. That binary applies its own pricing ruleset automatically
before scan, report, sync, snapshot, and other price-derived commands. Status,
doctor, quota, and conversation do not trigger a reprice. A pricing catalog
change does not require a raw rescan; a later incremental sync publishes
corrected dirty rollups.

Development daemon commands and mutating service commands are blocked:

```text
statsai-dev daemon ...
statsai-dev service install
statsai-dev service uninstall
```

`statsai-dev service status` remains read-only and is allowed. Parallel
development daemon support requires a separate label, endpoint, token, store,
and log layout and is intentionally outside v1.

## Inspect state

```sh
statsai-dev status
```

Status reports the selected source and workflow attempt, backend URLs,
production and development schema versions, clone timestamp and logical size,
and whether a selected PR or `main` has advanced.

Persistent state is deliberately small:

```text
~/.local/state/statsai-dev/state.json
~/.cache/statsai-dev/builds/<sha>/
~/.cache/statsai-dev/data/statsai.sqlite
```

`XDG_STATE_HOME` and `XDG_CACHE_HOME` are honored. For isolated test harnesses,
`STATSAI_DEV_STATE_DIR`, `STATSAI_DEV_CACHE_DIR`, and
`STATSAI_DEV_PROD_STORE` can override the resolved paths.

## Artifact workflow

`.github/workflows/dev-build.yml` runs for `main` pushes and PR `opened`,
`synchronize`, and `reopened` events. PR checkout explicitly uses
`github.event.pull_request.head.sha`, never GitHub's synthetic merge ref. Each
artifact is named `statsai-dev-<full-sha>`, targets only
`aarch64-apple-darwin`, and is retained for seven days.

PR and `main` concurrency groups cancel superseded in-progress builds. A build
that completed successfully remains immutable and addressable by its full SHA.
The workflow smoke-runs the produced artifact and its clean-home auth-status
path before upload; release packaging, installers, universal binaries, and
Homebrew artifacts are deliberately not part of this ephemeral workflow.
