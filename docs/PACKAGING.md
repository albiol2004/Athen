# Packaging & Distribution

How Athen reaches users, and why each channel was chosen. **Read when:**
shipping a new release, debugging an updater issue, or adding a new
distribution channel.

## Channels

| Channel | Format | Self-update | Per-release effort | Status |
|---------|--------|-------------|--------------------|--------|
| GitHub Releases | `.AppImage`, `.dmg`, `.msi`, `.exe`, `.deb`, `.rpm` | AppImage / macOS / Windows only | Automatic via `release.yml` | Live |
| AUR | `athen-bin` | No (pacman) | `packaging/aur/bump.sh` (~5s) | Live |
| Fedora COPR | source-built RPM | No (dnf) | Webhook-driven | Live |

## Decisions

### AUR ships `-bin`, not source

We publish [`athen-bin`](https://aur.archlinux.org/packages/athen-bin) — a
repackage of the upstream `.deb` — rather than a from-source PKGBUILD.

**Why:** a from-source build of athen-app on a user's machine takes 10-15
minutes of Rust compilation. The `-bin` install is seconds. AUR convention
allows both; for an end-user app, the friction of source builds isn't worth
the purity. The `packaging/aur/athen/` PKGBUILD is kept around for users who
want it, but is not what we publish.

The `bump.sh` script automates the per-release loop: it fetches the new
`.deb` and `LICENSE` hashes, rewrites the PKGBUILD, regenerates `.SRCINFO`
via a podman Arch container, and (optionally) commits + pushes to the AUR
git remote. Two minutes per release becomes five seconds.

### COPR builds from source with network enabled

The COPR project at `albiol2004/athen` builds from
`packaging/copr/athen.spec`, fetching the GitHub source tarball and
compiling against Fedora's mock infrastructure across Fedora 40/41/42/rawhide.

**Why source-build instead of repackaging the upstream `.rpm`:**
COPR's value is that it produces canonical Fedora-packaged RPMs with
proper deps, `dist`-tagged release strings, and per-chroot rebuilds. A
binary repackage would lose all of that and is harder to get accepted into
official Fedora repos later.

**Why network-enabled builds:** Athen pulls hundreds of cargo crates;
vendoring would mean shipping a 200-500 MB tarball per release. For an
alpha-stage app the engineering cost of vendoring isn't worth it. When
the project stabilizes (and especially before any push for inclusion in
official Fedora repos), we'll switch to a vendored Source1 tarball with
`cargo build --offline`. Setting documented in `packaging/copr/README.md`.

**Auto-build on every release:** the COPR project is wired to a GitHub
webhook, so every published release tag triggers rebuilds for every
selected chroot. Zero per-release packaging work.

### Fedora .rpm / .deb preferred over AppImage

On Fedora 44+ (Mesa 26 with RADV), the bundled wayland libs in AppImage
collide with Mesa's EGL stack, causing crashes (`EGL_BAD_PARAMETER`). Ship
`.rpm` + `.deb` bundles as primary artifacts; AppImage is best-effort fallback
for users on other distros. The `.rpm` is built by COPR; GitHub Releases hosts
all three formats for maximum portability.

### Auto-updater is split by install kind

`tauri-plugin-updater` only supports updating the binary it can swap in
place — AppImage on Linux, `.app` bundle on macOS, MSI/NSIS on Windows.
**It cannot update system-package installs** (rpm, deb, AUR), because
those binaries live at `/usr/bin/athen-app` and are owned by the package
manager.

The fix in `crates/athen-app/src/commands.rs::detect_installer_kind` (line 7239):

- Linux + `$APPIMAGE` set → `"appimage"` → in-app updater swaps the binary.
- Linux without `$APPIMAGE` → `"system"` → updater UI shows "Open download
  page" linking to the GitHub release; user upgrades through their package
  manager.
- macOS / Windows → `"appimage"` (i.e., self-updatable) — DMG-installed
  `.app` bundles and MSI/NSIS installs both work.

Frontend gates the install button on `installer_kind` returned from
`check_for_update` (line 7260). Backend `install_update` also gates as
defense-in-depth (line 7320) — returns a friendly error pointing at the release page
if anything bypasses the frontend gate.

### Versioning rules

The version that reaches users is `crates/athen-app/Cargo.toml` +
`crates/athen-app/tauri.conf.json` (kept identical). Both are read at
compile time and define what the About dialog shows and what the
auto-updater compares against `latest.json`.

**Always bump both before tagging a release.** A tagged release with the
old Cargo version causes an updater loop: the binary keeps reporting the
old version while the manifest says newer. The other workspace crates
(`athen-core`, etc.) are internal libraries and their version fields have
no user-visible effect; bumping them is purely cosmetic.

## Files

```
packaging/
├── aur/
│   ├── athen-bin/          # The published AUR PKGBUILD (binary repackage of .deb)
│   ├── athen/              # Optional source-build PKGBUILD (not published)
│   └── bump.sh             # Per-release version bumper for athen-bin
└── copr/
    ├── athen.spec          # COPR source-build spec
    ├── bump.sh             # Per-release spec bumper
    └── README.md           # COPR project setup instructions
```

## Per-release checklist

After cutting a new tag:

```bash
# 1. Bump versions before tagging (NEVER tag without this — see "Versioning rules")
#    crates/athen-app/Cargo.toml      version = "0.2.X"
#    crates/athen-app/tauri.conf.json "version": "0.2.X"
git commit -am "v0.2.X"
git tag v0.2.X && git push origin v0.2.X

# 2. Wait for release.yml to finish, then publish the draft
#    release.yml builds: .deb, .rpm, .AppImage (Linux); .dmg, .app.tar.gz (macOS); .msi, .exe (Windows)
#    publish-manifest auto-generates latest.json from signed artifacts
gh release edit v0.2.X --draft=false

# 3. Bump AUR (one command — pushes to AUR remote)
./packaging/aur/bump.sh 0.2.X ~/aur-athen-bin

# 4. COPR auto-rebuilds via webhook. No manual step required.
#    If the webhook isn't wired yet:
#    ./packaging/copr/bump.sh 0.2.X && git commit -am "copr: $(date)" && git push
#    copr-cli build albiol2004/athen packaging/copr/athen.spec
```
