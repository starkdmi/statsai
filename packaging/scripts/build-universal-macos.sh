#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
DIST_DIR="${ROOT}/target/distrib"
ARM_ARCHIVE="${DIST_DIR}/statsai-aarch64-apple-darwin.tar.xz"
X64_ARCHIVE="${DIST_DIR}/statsai-x86_64-apple-darwin.tar.xz"
WORK_DIR="${DIST_DIR}/universal-build"
OUT_DIR="${WORK_DIR}/statsai-universal-apple-darwin"
OUT_ARCHIVE="${DIST_DIR}/statsai-universal-apple-darwin.tar.xz"

if [[ ! -f "${ARM_ARCHIVE}" || ! -f "${X64_ARCHIVE}" ]]; then
  echo "missing per-arch macOS archives; expected:" >&2
  echo "  ${ARM_ARCHIVE}" >&2
  echo "  ${X64_ARCHIVE}" >&2
  exit 1
fi

rm -rf "${WORK_DIR}"
mkdir -p "${OUT_DIR}"

tar -xJf "${ARM_ARCHIVE}" -C "${WORK_DIR}"
tar -xJf "${X64_ARCHIVE}" -C "${WORK_DIR}"

ARM_BIN="$(find "${WORK_DIR}" -path '*aarch64-apple-darwin*/statsai' -type f | head -n1)"
X64_BIN="$(find "${WORK_DIR}" -path '*x86_64-apple-darwin*/statsai' -type f | head -n1)"

if [[ -z "${ARM_BIN}" || -z "${X64_BIN}" ]]; then
  echo "failed to locate statsai binaries in extracted archives" >&2
  exit 1
fi

lipo -create -output "${OUT_DIR}/statsai" "${ARM_BIN}" "${X64_BIN}"
chmod +x "${OUT_DIR}/statsai"

if [[ -f "${ROOT}/README.md" ]]; then
  cp "${ROOT}/README.md" "${OUT_DIR}/"
fi
if [[ -f "${ROOT}/LICENSE" ]]; then
  cp "${ROOT}/LICENSE" "${OUT_DIR}/"
fi

tar -cJf "${OUT_ARCHIVE}" -C "${WORK_DIR}" "$(basename "${OUT_DIR}")"
shasum -a 256 "${OUT_ARCHIVE}" | awk '{print $1}' > "${OUT_ARCHIVE}.sha256"
echo "built ${OUT_ARCHIVE}"