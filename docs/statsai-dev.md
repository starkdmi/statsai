# Exact-SHA development workflow

`statsai-dev` selects immutable StatsAI development artifacts and launches them
against one reusable, isolated database. It is intentionally a CLI-only Apple
Silicon workflow in v1; it never installs a development daemon or rewires the
menu bar application.

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

The prod environment is allowed only when the production database schema **and**
applied pricing ruleset exactly match the versions supported by the selected
build; it prints a warning when it proceeds. Missing, older, or newer production
pricing metadata is refused. A development build is never allowed to migrate or
reprice production data, so a schema-changing or pricing-changing PR can only be
tested under `env dev` against the isolated clone.

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
