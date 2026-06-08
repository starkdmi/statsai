# starkdmi/homebrew-tap

Homebrew tap for StatsAI. Formulae are published automatically by the
`publish-homebrew-formula` job in `.github/workflows/release.yml` when
`HOMEBREW_TAP_TOKEN` is configured on the `starkdmi/statsai` repository.

## One-time setup

1. Create `https://github.com/starkdmi/homebrew-tap` (initialize with a README).
2. Create a GitHub personal access token with `repo` scope.
3. Add the token as `HOMEBREW_TAP_TOKEN` in `starkdmi/statsai` repository secrets.

## Install

```sh
brew install starkdmi/tap/statsai
```

Per-arch bottles are built by cargo-dist. For a single universal macOS binary,
download `statsai-universal-apple-darwin.tar.xz` from GitHub Releases instead.