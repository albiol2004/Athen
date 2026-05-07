#!/usr/bin/env bash
# scripts/bump_version.sh — bump every pre-release version source in lockstep.
#
# Usage:
#   scripts/bump_version.sh 0.1.2
#
# Updates (all under the repo):
#   crates/athen-app/Cargo.toml         version = "X.Y.Z"   (the canonical source)
#   crates/athen-app/tauri.conf.json    "version": "X.Y.Z"  (what the binary stamps)
#   packaging/copr/athen.spec           Version, Release reset, new %changelog entry
#   packaging/aur/athen-bin/PKGBUILD    pkgver, pkgrel reset (hashes left stale)
#   packaging/aur/athen/PKGBUILD        pkgver, pkgrel reset (source-build, unpublished)
#
# Refuses to:
#   - Bump to the same version that's already set.
#   - Bump backwards (lower than the current Cargo.toml version).
#
# Does NOT:
#   - git add / commit / tag / push. Inspect the diff and run those yourself.
#   - Refetch AUR sha256 hashes — those depend on the .deb that release.yml
#     uploads after the tag is pushed. Run packaging/aur/bump.sh AFTER the
#     release publishes to refresh hashes, regenerate .SRCINFO, and push
#     the AUR update.
#
# Why both Cargo.toml and tauri.conf.json: cargo stamps Cargo.toml's version
# into the compiled binary; Tauri reads tauri.conf.json for the bundle's
# manifest version (rpm/deb/msi/appimage filenames + about-dialog string).
# If they drift, the auto-updater hits an infinite loop — the binary self-
# reports the old version while latest.json (derived from the git tag)
# claims the new one. See docs/PACKAGING.md "Versioning rules".

set -euo pipefail

MAINTAINER="Alejandro Garcia <contact@alejandrogarcia.blog>"

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

NEW="${1:-}"
if [[ -z "$NEW" ]]; then
    echo "usage: $0 <version>     e.g. $0 0.1.2" >&2
    exit 1
fi
if ! [[ "$NEW" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    echo "error: version must be X.Y.Z (got '$NEW')" >&2
    exit 1
fi

CARGO=crates/athen-app/Cargo.toml
TAURI=crates/athen-app/tauri.conf.json
SPEC=packaging/copr/athen.spec
AUR_BIN=packaging/aur/athen-bin/PKGBUILD
AUR_SRC=packaging/aur/athen/PKGBUILD

CUR=$(grep -m1 -E '^version = "' "$CARGO" | sed -E 's/version = "(.*)"/\1/')
[[ -n "$CUR" ]] || { echo "error: could not read current version from $CARGO" >&2; exit 1; }

if [[ "$NEW" == "$CUR" ]]; then
    echo "already at $CUR — nothing to do" >&2
    exit 1
fi

# `sort -V` orders by semantic version (so 0.1.10 > 0.1.9). If NEW sorts
# below CUR, it's a downgrade.
LOWEST=$(printf '%s\n%s\n' "$CUR" "$NEW" | sort -V | head -1)
if [[ "$LOWEST" == "$NEW" ]]; then
    echo "error: $NEW is older than current $CUR — refusing to bump backwards" >&2
    exit 1
fi

echo "==> bumping $CUR -> $NEW"

# 1. Cargo.toml — only the FIRST `version = "..."` (the package's own,
#    not its dependencies').
sed -i -E "0,/^version = \"$CUR\"/{s/^version = \"$CUR\"/version = \"$NEW\"/}" "$CARGO"

# 2. tauri.conf.json — top-level "version" field.
sed -i -E "s/(\"version\":[[:space:]]*\")$CUR(\")/\1$NEW\2/" "$TAURI"

# 3. COPR spec — Version, Release, + new %changelog entry.
sed -i -E "s/^Version:.*/Version:        $NEW/" "$SPEC"
sed -i -E "s/^Release:.*/Release:        1%{?dist}/" "$SPEC"

DATE=$(LC_ALL=C date "+%a %b %d %Y")
python3 - "$SPEC" "$NEW" "$DATE" "$MAINTAINER" <<'PY'
import sys
spec, version, date, maintainer = sys.argv[1:]
text = open(spec).read()
entry = f"* {date} {maintainer} - {version}-1\n- Release {version}\n\n"
text = text.replace("%changelog\n", f"%changelog\n{entry}", 1)
open(spec, "w").write(text)
PY

# 4. AUR PKGBUILDs — pkgver + reset pkgrel. The athen-bin sha256 hashes and
#    .SRCINFO are deliberately left stale: they need the published .deb,
#    which only exists after release.yml runs. packaging/aur/bump.sh
#    refreshes them post-release.
for f in "$AUR_BIN" "$AUR_SRC"; do
    [[ -f "$f" ]] || continue
    sed -i -E \
        -e "s/^pkgver=.*/pkgver=$NEW/" \
        -e "s/^pkgrel=.*/pkgrel=1/" \
        "$f"
done

# Sanity check — every file we touched should now mention NEW.
fail=0
for f in "$CARGO" "$TAURI" "$SPEC" "$AUR_BIN" "$AUR_SRC"; do
    [[ -f "$f" ]] || continue
    if ! grep -q -F "$NEW" "$f"; then
        echo "error: $f does not contain $NEW after bump" >&2
        fail=1
    fi
done
[[ $fail -eq 0 ]] || exit 1

cat <<EOF

==> updated:
    $CARGO
    $TAURI
    $SPEC                (Version, Release reset, new %changelog entry)
    $AUR_BIN             (pkgver, pkgrel; hashes refreshed by packaging/aur/bump.sh post-release)
    $AUR_SRC             (pkgver, pkgrel)

==> next:
    git diff
    git add -A && git commit -m "v$NEW"
    git tag v$NEW
    git push origin main v$NEW
    # wait for release.yml to publish the .deb + .rpm + etc., then:
    packaging/aur/bump.sh $NEW ~/aur-athen-bin    # refresh AUR sha256, regen .SRCINFO, push to AUR
EOF
