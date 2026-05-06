# Athen — COPR packaging

[Fedora COPR](https://copr.fedorainfracloud.org/) builds & hosts RPMs from this spec.
After setup, Fedora/RHEL users install with:

```bash
sudo dnf copr enable albiol2004/athen
sudo dnf install athen
```

Updates flow naturally through `dnf upgrade`.

## One-time setup

1. Sign in to https://copr.fedorainfracloud.org with your Fedora Account (FAS).
2. Create a new project named `athen`. Select chroots:
   - `fedora-40-x86_64`
   - `fedora-41-x86_64`
   - `fedora-42-x86_64`
   - `fedora-rawhide-x86_64`
   - (optional) `epel-9-x86_64` for RHEL/Alma/Rocky
3. Under **Packages → New package → SCM**:
   - Clone URL: `https://github.com/albiol2004/Athen.git`
   - Subdirectory: `packaging/copr`
   - Spec file: `athen.spec`
   - Build method: `make_srpm` → leave default (uses `rpkg`)
4. Save.

## Auto-build on every release (the magic step)

In your COPR project: **Settings → Integrations → Webhooks** — copy the URL.

Then in this repo: **Settings → Webhooks → Add webhook**, paste the URL, content
type `application/json`, trigger only on `Releases`. Every published release will
fire a build for every selected chroot — no manual work per version.

For full automation including version bumping, see `bump.sh` in this directory.

## Manual one-off build (if you don't wire the webhook)

```bash
# from the repo root
copr-cli build albiol2004/athen \
    --chroot fedora-42-x86_64 \
    packaging/copr/athen.spec
```

## Updating the spec for a new version

`bump.sh <new-version>` rewrites `Version:` and prepends a changelog entry.
Then commit + push; the webhook (or your manual `copr-cli build`) takes over.
