#!/bin/sh
set -eu

KINGFISHER_REVISION=8fa4f142bcd32664ac0feb16fc8aabc67637660d
KINGFISHER_VERSION=1.106.0
SOURCE=${1:?usage: build.sh KINGFISHER_CHECKOUT [BUILD_DIRECTORY]}
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
BUILD_DIR=${2:-"$SCRIPT_DIR/build/kingfisher-$KINGFISHER_REVISION"}

ACTUAL_REVISION=$(git -C "$SOURCE" rev-parse HEAD)
if [ "$ACTUAL_REVISION" != "$KINGFISHER_REVISION" ]; then
  printf '%s\n' "expected Kingfisher $KINGFISHER_REVISION, found $ACTUAL_REVISION" >&2
  exit 1
fi
if [ -e "$BUILD_DIR" ]; then
  printf '%s\n' "build directory already exists: $BUILD_DIR" >&2
  exit 1
fi

mkdir -p "$BUILD_DIR"
git -C "$SOURCE" archive "$KINGFISHER_REVISION" | tar -x -C "$BUILD_DIR"
patch -d "$BUILD_DIR" -p1 < "$SCRIPT_DIR/kingfisher-fallible-scan.patch"
cp "$SCRIPT_DIR/kingfisher-scanner.Cargo.toml" \
  "$BUILD_DIR/crates/kingfisher-scanner/Cargo.toml"
mkdir -p "$BUILD_DIR/crates/statsai-kingfisher"
cp "$SCRIPT_DIR/Cargo.toml" "$BUILD_DIR/crates/statsai-kingfisher/Cargo.toml"
cp -R "$SCRIPT_DIR/src" "$BUILD_DIR/crates/statsai-kingfisher/src"
cp "$SCRIPT_DIR/workspace.Cargo.toml" "$BUILD_DIR/Cargo.toml"

STATSAI_KINGFISHER_VERSION=$KINGFISHER_VERSION \
STATSAI_KINGFISHER_REVISION=$KINGFISHER_REVISION \
  cargo build --manifest-path "$BUILD_DIR/Cargo.toml" -p statsai-kingfisher --release
printf '%s\n' "$BUILD_DIR/target/release/statsai-kingfisher"
