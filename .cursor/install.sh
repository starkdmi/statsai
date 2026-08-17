#!/usr/bin/env bash
# Idempotent Cloud Agent bootstrap for the StatsAI Rust workspace.
# Safe to run repeatedly and against a cached/partially-prepared VM.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

# System library required by the keyring -> dbus-secret-service -> libdbus-sys
# dependency chain (local credential storage). pkg-config resolves dbus-1.pc.
if ! pkg-config --exists dbus-1 2>/dev/null; then
  sudo apt-get update
  sudo DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
    pkg-config libdbus-1-dev
fi

# Install the exact Rust toolchain pinned by rust-toolchain.toml, including the
# rustfmt and clippy components the CI script (scripts/rust-ci.sh) requires.
if command -v rustup >/dev/null 2>&1; then
  channel="$(sed -n 's/^channel = "\([^"]*\)"$/\1/p' rust-toolchain.toml)"
  if [[ -n "$channel" ]]; then
    rustup toolchain install "$channel" \
      --component rustfmt --component clippy --profile minimal
  fi
fi

# Warm the dependency and build caches so the workspace is ready to build/test.
cargo fetch --locked
cargo build --workspace
