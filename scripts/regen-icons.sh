#!/usr/bin/env bash
# Regenerate the Athen icon set from frontend/assets/logo.svg.
#
# Produces:
#   - crates/athen-app/icons/*  (Tauri bundle icons: PNGs, .icns, .ico, iOS, Android, Windows)
#   - ~/.local/share/icons/hicolor/<size>x<size>/apps/com.athen.app.png  (Linux desktop integration)
#
# Run after editing frontend/assets/logo.svg.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SVG="$REPO_ROOT/frontend/assets/logo.svg"
APP_CRATE="$REPO_ROOT/crates/athen-app"
APP_ID="com.athen.app"

if [[ ! -f "$SVG" ]]; then
    echo "error: $SVG not found" >&2
    exit 1
fi

for cmd in inkscape cargo; do
    if ! command -v "$cmd" >/dev/null 2>&1; then
        echo "error: required command '$cmd' not found in PATH" >&2
        exit 1
    fi
done

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

MASTER="$TMP/athen-logo-1024.png"
echo "rasterizing $SVG -> 1024x1024 master png"
inkscape "$SVG" --export-type=png --export-filename="$MASTER" -w 1024 -h 1024 >/dev/null

echo "running cargo tauri icon (writes into $APP_CRATE/icons/)"
(cd "$APP_CRATE" && cargo tauri icon "$MASTER")

HICOLOR="$HOME/.local/share/icons/hicolor"
echo "installing hicolor PNGs to $HICOLOR"
for SIZE in 16 24 32 48 64 96 128 256 512; do
    DEST_DIR="$HICOLOR/${SIZE}x${SIZE}/apps"
    mkdir -p "$DEST_DIR"
    inkscape "$SVG" --export-type=png \
        --export-filename="$DEST_DIR/${APP_ID}.png" \
        -w "$SIZE" -h "$SIZE" >/dev/null
done

if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -f -t "$HICOLOR" 2>/dev/null || true
fi

echo "done. restart the app for changes to apply."
