#!/usr/bin/env bash
set -euo pipefail

repo_root=$(git rev-parse --show-toplevel)
cd "$repo_root"

git config core.hooksPath .githooks

cat <<'EOF'
Installed repo-local git hooks.

- pre-commit runs rust fmt + clippy for Rust-related staged changes
- pre-push runs the full local Rust CI suite

Set STATSAI_SKIP_LOCAL_CI=1 to bypass the hooks for an exceptional case.
EOF
