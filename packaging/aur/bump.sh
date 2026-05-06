#!/usr/bin/env bash
# Bump the athen-bin AUR package to a new release.
#
# Usage:
#   ./packaging/aur/bump.sh                  # uses version from tauri.conf.json
#   ./packaging/aur/bump.sh 0.1.2            # explicit version
#   ./packaging/aur/bump.sh 0.1.2 ~/aur-athen-bin   # also copy + commit + push to AUR clone
#
# Run from the repo root.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
PKG_DIR="$REPO_ROOT/packaging/aur/athen-bin"
GH_REPO="albiol2004/Athen"

VERSION="${1:-}"
AUR_CLONE="${2:-}"

if [[ -z "$VERSION" ]]; then
    VERSION=$(grep -oP '"version":\s*"\K[^"]+' "$REPO_ROOT/crates/athen-app/tauri.conf.json")
fi

DEB_URL="https://github.com/$GH_REPO/releases/download/v$VERSION/Athen_${VERSION}_amd64.deb"
LICENSE_URL="https://github.com/$GH_REPO/raw/v$VERSION/LICENSE"

echo "==> Bumping athen-bin to $VERSION"
echo "    deb:     $DEB_URL"
echo "    license: $LICENSE_URL"

echo "==> Fetching hashes"
DEB_SHA=$(curl -sL --fail "$DEB_URL" | sha256sum | cut -d' ' -f1)
LIC_SHA=$(curl -sL --fail "$LICENSE_URL" | sha256sum | cut -d' ' -f1)
[[ ${#DEB_SHA} -eq 64 && ${#LIC_SHA} -eq 64 ]] || { echo "hash fetch failed — is the release published?"; exit 1; }
echo "    deb sha256:     $DEB_SHA"
echo "    license sha256: $LIC_SHA"

echo "==> Updating PKGBUILD"
sed -i \
    -e "s/^pkgver=.*/pkgver=$VERSION/" \
    -e "s/^pkgrel=.*/pkgrel=1/" \
    "$PKG_DIR/PKGBUILD"

# Replace the two sha256 lines (deb first, license second).
python3 - "$PKG_DIR/PKGBUILD" "$DEB_SHA" "$LIC_SHA" <<'PY'
import re, sys
path, deb, lic = sys.argv[1:]
text = open(path).read()
new = re.sub(
    r"sha256sums=\([^)]*\)",
    f"sha256sums=(\n    '{deb}'\n    '{lic}'\n)",
    text,
    count=1,
    flags=re.DOTALL,
)
open(path, "w").write(new)
PY

echo "==> Regenerating .SRCINFO via Arch container"
podman run --rm -v "$PKG_DIR:/pkg:Z" -w /pkg archlinux:latest bash -c '
    pacman -Sy --noconfirm pacman-contrib sudo >/dev/null 2>&1
    useradd -m b
    sudo -u b makepkg --printsrcinfo > .SRCINFO
'
podman unshare chown -R 0:0 "$PKG_DIR"

echo "==> Done. Files updated:"
echo "    $PKG_DIR/PKGBUILD"
echo "    $PKG_DIR/.SRCINFO"

if [[ -n "$AUR_CLONE" ]]; then
    [[ -d "$AUR_CLONE/.git" ]] || { echo "$AUR_CLONE is not a git repo"; exit 1; }
    echo "==> Pushing to AUR clone at $AUR_CLONE"
    cp "$PKG_DIR/PKGBUILD" "$PKG_DIR/.SRCINFO" "$AUR_CLONE/"
    git -C "$AUR_CLONE" add PKGBUILD .SRCINFO
    git -C "$AUR_CLONE" commit -m "upgpkg: athen-bin $VERSION-1"
    git -C "$AUR_CLONE" push origin master
    echo "==> Live at https://aur.archlinux.org/packages/athen-bin"
else
    cat <<EOF
==> Next:
    cp $PKG_DIR/{PKGBUILD,.SRCINFO} ~/aur-athen-bin/
    cd ~/aur-athen-bin
    git add PKGBUILD .SRCINFO
    git commit -m "upgpkg: athen-bin $VERSION-1"
    git push origin master

(or re-run with the clone path:
    ./packaging/aur/bump.sh $VERSION ~/aur-athen-bin)
EOF
fi
