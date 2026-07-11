#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF' >&2
usage: scripts/rust-ci.sh <fmt|clippy|test|fast|full>

  fmt     Run cargo fmt in check mode.
  clippy  Run cargo clippy with warnings denied.
  test    Run the Rust test suite used in CI.
  fast    Run fmt + clippy.
  full    Run fmt + clippy + test.
EOF
  exit 64
}

repo_root=$(git rev-parse --show-toplevel 2>/dev/null || pwd)
cd "$repo_root"

required_rust_version=$(sed -n 's/^channel = "\([^"]*\)"$/\1/p' rust-toolchain.toml)
if [[ -z "$required_rust_version" ]]; then
  echo "rust-ci: missing Rust toolchain channel in rust-toolchain.toml" >&2
  exit 1
fi

if command -v rustup >/dev/null 2>&1; then
  if ! active_rust_version=$(rustup run "$required_rust_version" rustc --version 2>/dev/null | awk '{print $2}'); then
    cat >&2 <<EOF
rust-ci: Rust $required_rust_version is not installed in rustup.
Run: rustup toolchain install $required_rust_version --component rustfmt --component clippy
EOF
    exit 1
  fi
  cargo_command=(rustup run "$required_rust_version" cargo)
else
  active_rust_version=$(rustc --version | awk '{print $2}')
  cargo_command=(cargo)
fi

if [[ "$active_rust_version" != "$required_rust_version" ]]; then
  cat >&2 <<EOF
rust-ci: Rust $required_rust_version is required; found $active_rust_version.
Install rustup and run: rustup toolchain install $required_rust_version --component rustfmt --component clippy
EOF
  exit 1
fi

run_fmt() {
  "${cargo_command[@]}" fmt --all --check
}

run_clippy() {
  "${cargo_command[@]}" clippy --workspace --all-targets -- -D warnings
}

run_tests() {
  "${cargo_command[@]}" test --workspace
  "${cargo_command[@]}" test -p statsai-daemon --features watch
}

case "${1:-full}" in
  fmt)
    run_fmt
    ;;
  clippy)
    run_clippy
    ;;
  test)
    run_tests
    ;;
  fast)
    run_fmt
    run_clippy
    ;;
  full)
    run_fmt
    run_clippy
    run_tests
    ;;
  *)
    usage
    ;;
esac
