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

# --- optional: expose adapter fixtures as runnable provider sources -----
# The VM has no real AI-tool history, so `statsai scan` finds nothing.
# This writes a helper the agent can source when it wants end-to-end data.
fixtures="$repo_root/crates/statsai-adapters/tests/fixtures"
if [[ -d "$fixtures" ]]; then
  cat >"$repo_root/.cursor/use-fixtures.sh" <<EOF
#!/usr/bin/env bash
# Point StatsAI at the sanitized adapter fixtures instead of real history.
# Usage:  source .cursor/use-fixtures.sh
export CLAUDE_CONFIG_DIR="$fixtures/claude/basic"
export CODEX_HOME="$fixtures/codex/basic"
echo "claude + codex fixtures active; register the others with:"
echo "  statsai source add --provider grok     --path $fixtures/grok/basic"
echo "  statsai source add --provider opencode --path $fixtures/opencode/sqlite-v2"
EOF
  chmod +x "$repo_root/.cursor/use-fixtures.sh"
fi
