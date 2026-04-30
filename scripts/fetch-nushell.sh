#!/usr/bin/env bash
# Download the nushell binary for the host (or override) target triple and
# place it at crates/athen-app/binaries/nu-<triple>(.exe). Tauri picks it up
# via the `externalBin` entry in tauri.conf.json and ships it as a sidecar.
#
# Usage:
#   scripts/fetch-nushell.sh                # host triple
#   TARGET_TRIPLE=aarch64-apple-darwin scripts/fetch-nushell.sh
#   NU_VERSION=0.99.1 scripts/fetch-nushell.sh

set -euo pipefail

NU_VERSION="${NU_VERSION:-0.99.1}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
DEST_DIR="${REPO_ROOT}/crates/athen-app/binaries"

TRIPLE="${TARGET_TRIPLE:-$(rustc -vV 2>/dev/null | sed -n 's/host: //p')}"
if [[ -z "${TRIPLE}" ]]; then
    echo "fetch-nushell: cannot determine target triple; set TARGET_TRIPLE" >&2
    exit 1
fi

case "${TRIPLE}" in
    *-pc-windows-*) ARCHIVE_EXT="zip"; BIN_EXT=".exe" ;;
    *)              ARCHIVE_EXT="tar.gz"; BIN_EXT="" ;;
esac

ARCHIVE="nu-${NU_VERSION}-${TRIPLE}.${ARCHIVE_EXT}"
URL="https://github.com/nushell/nushell/releases/download/${NU_VERSION}/${ARCHIVE}"
DEST_BIN="${DEST_DIR}/nu-${TRIPLE}${BIN_EXT}"

if [[ -x "${DEST_BIN}" ]]; then
    echo "fetch-nushell: ${DEST_BIN} already present, skipping"
    exit 0
fi

mkdir -p "${DEST_DIR}"
TMPDIR="$(mktemp -d)"
trap 'rm -rf "${TMPDIR}"' EXIT

echo "fetch-nushell: downloading ${URL}"
curl -fL --retry 3 "${URL}" -o "${TMPDIR}/${ARCHIVE}"

if [[ "${ARCHIVE_EXT}" == "zip" ]]; then
    unzip -q "${TMPDIR}/${ARCHIVE}" -d "${TMPDIR}"
else
    tar -xzf "${TMPDIR}/${ARCHIVE}" -C "${TMPDIR}"
fi

SRC_BIN="$(find "${TMPDIR}" -type f -name "nu${BIN_EXT}" -perm -u+x 2>/dev/null | head -n1 || true)"
if [[ -z "${SRC_BIN}" ]]; then
    SRC_BIN="$(find "${TMPDIR}" -type f -name "nu${BIN_EXT}" | head -n1 || true)"
fi
if [[ -z "${SRC_BIN}" ]]; then
    echo "fetch-nushell: could not locate nu${BIN_EXT} inside ${ARCHIVE}" >&2
    exit 1
fi

cp "${SRC_BIN}" "${DEST_BIN}"
chmod +x "${DEST_BIN}"

echo "fetch-nushell: wrote ${DEST_BIN}"
