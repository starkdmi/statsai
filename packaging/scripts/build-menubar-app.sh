#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
DIST_DIR="${ROOT}/target/distrib"
UNIVERSAL_ARCHIVE="${DIST_DIR}/statsai-universal-apple-darwin.tar.xz"
OUT_ZIP="${DIST_DIR}/StatsAI.app.zip"
ARM_TARGET="aarch64-apple-darwin"
X64_TARGET="x86_64-apple-darwin"

cd "${ROOT}"

if ! command -v cargo-bundle >/dev/null 2>&1; then
  cargo install cargo-bundle --locked
fi

has_target() {
  rustup target list --installed | grep -qx "$1"
}

BUILD_UNIVERSAL=false
if has_target "${ARM_TARGET}" && has_target "${X64_TARGET}"; then
  BUILD_UNIVERSAL=true
  echo "building universal StatsAI.app (menubar + embedded statsai CLI)..."
else
  if ! has_target "${ARM_TARGET}"; then
    echo "missing required Rust target: ${ARM_TARGET}" >&2
    echo "install it with: rustup target add ${ARM_TARGET}" >&2
    exit 1
  fi
  echo "building arm64-only StatsAI.app (${X64_TARGET} not installed)..."
  echo "for a universal release zip, run: rustup target add ${X64_TARGET}"
fi

cargo build --release -p statsai-menubar --target "${ARM_TARGET}"
if [[ "${BUILD_UNIVERSAL}" == "true" ]]; then
  cargo build --release -p statsai-menubar --target "${X64_TARGET}"
fi

# Generate bundle metadata (Info.plist, icon, LSUIElement) from the arm64 build.
cargo bundle --release -p statsai-menubar --target "${ARM_TARGET}"

APP_DIR="${ROOT}/target/${ARM_TARGET}/release/bundle/osx/StatsAI.app"
if [[ ! -d "${APP_DIR}" ]]; then
  echo "expected app bundle at ${APP_DIR}" >&2
  exit 1
fi

MENUBAR_ARM="${ROOT}/target/${ARM_TARGET}/release/statsai-menubar"
MENUBAR_UNI="${APP_DIR}/Contents/MacOS/statsai-menubar"
if [[ "${BUILD_UNIVERSAL}" == "true" ]]; then
  MENUBAR_X64="${ROOT}/target/${X64_TARGET}/release/statsai-menubar"
  lipo -create -output "${MENUBAR_UNI}" "${MENUBAR_ARM}" "${MENUBAR_X64}"
else
  cp "${MENUBAR_ARM}" "${MENUBAR_UNI}"
fi
chmod +x "${MENUBAR_UNI}"

CLI_BIN="${APP_DIR}/Contents/MacOS/statsai"
if [[ -f "${UNIVERSAL_ARCHIVE}" ]]; then
  TMP="${DIST_DIR}/menubar-cli"
  rm -rf "${TMP}"
  mkdir -p "${TMP}"
  tar -xJf "${UNIVERSAL_ARCHIVE}" -C "${TMP}"
  cp "${TMP}/statsai-universal-apple-darwin/statsai" "${CLI_BIN}"
else
  cargo build --release -p statsai --target "${ARM_TARGET}"
  if [[ "${BUILD_UNIVERSAL}" == "true" ]]; then
    cargo build --release -p statsai --target "${X64_TARGET}"
    lipo -create -output "${CLI_BIN}" \
      "${ROOT}/target/${ARM_TARGET}/release/statsai" \
      "${ROOT}/target/${X64_TARGET}/release/statsai"
  else
    cp "${ROOT}/target/${ARM_TARGET}/release/statsai" "${CLI_BIN}"
  fi
fi
chmod +x "${CLI_BIN}"

rm -f "${OUT_ZIP}"
ditto -c -k --sequesterRsrc --keepParent "${APP_DIR}" "${OUT_ZIP}"
shasum -a 256 "${OUT_ZIP}" | awk '{print $1}' > "${OUT_ZIP}.sha256"
echo "built ${OUT_ZIP}"