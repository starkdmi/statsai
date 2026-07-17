# StatsAI Kingfisher helper

This helper embeds Kingfisher's in-memory scanner without any credential
validation features. It is built inside the exact Kingfisher v1.106.0 source
workspace so that Kingfisher's pinned vendored Vectorscan implementation is
used. The helper never invokes Kingfisher's CLI, writes input to a temporary
file, performs an update check, or serializes matched secret text.

For a local build, check out
`mongodb/kingfisher@8fa4f142bcd32664ac0feb16fc8aabc67637660d` and run:

```sh
tools/statsai-kingfisher/build.sh /path/to/kingfisher-checkout /tmp/statsai-kingfisher-build
```

The exact pinned Git object is extracted into a minimal virtual workspace
containing only Kingfisher's three library crates and this helper. Working-tree
changes and untracked files in the source checkout cannot affect the build.
The build applies the checked-in `kingfisher-fallible-scan.patch` so Vectorscan
engine failures propagate to the helper instead of becoming an empty finding
list. The original checkout is never modified, and Kingfisher's
network-validation CLI is not resolved or built.
