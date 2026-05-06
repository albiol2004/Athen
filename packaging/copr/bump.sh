#!/usr/bin/env bash
# Bump the COPR spec to a new version. Run from the repo root.
#
# Usage:
#   ./packaging/copr/bump.sh                # version from tauri.conf.json
#   ./packaging/copr/bump.sh 0.1.2          # explicit version

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
SPEC="$REPO_ROOT/packaging/copr/athen.spec"

VERSION="${1:-}"
if [[ -z "$VERSION" ]]; then
    VERSION=$(grep -oP '"version":\s*"\K[^"]+' "$REPO_ROOT/crates/athen-app/tauri.conf.json")
fi

DATE=$(LC_ALL=C date "+%a %b %d %Y")
MAINTAINER="Alejandro Garcia <albiol2004@gmail.com>"

echo "==> Bumping spec to $VERSION"

# Replace Version: line.
sed -i "s/^Version:.*/Version:        $VERSION/" "$SPEC"
sed -i "s/^Release:.*/Release:        1%{?dist}/" "$SPEC"

# Prepend a changelog entry under %changelog.
python3 - "$SPEC" "$VERSION" "$DATE" "$MAINTAINER" <<'PY'
import sys
spec_path, version, date, maintainer = sys.argv[1:]
with open(spec_path) as f:
    text = f.read()
new_entry = f"* {date} {maintainer} - {version}-1\n- Release {version}\n\n"
text = text.replace("%changelog\n", f"%changelog\n{new_entry}", 1)
with open(spec_path, "w") as f:
    f.write(text)
PY

echo "==> Done. Updated:"
echo "    $SPEC"
echo
echo "==> Next:"
echo "    git add packaging/copr/athen.spec"
echo "    git commit -m \"copr: bump to $VERSION\""
echo "    git push"
echo "    # The COPR webhook will trigger a build automatically once the v$VERSION tag is pushed."
