# Security policy

## Reporting a vulnerability

**Please don't open a public issue for security problems.**

If you've found a vulnerability in Athen, report it privately via one of:

- **GitHub Security Advisories** — preferred. Open a private advisory at
  <https://github.com/albiol2004/Athen/security/advisories/new>. This
  gives us a private channel and lets you collaborate on a fix and an
  embargoed disclosure.
- **Email** — `contact@alejandrogarcia.blog` with `[Athen security]` in the subject.

Please include:

- The Athen version (or commit SHA) you observed it on.
- Your platform (Linux distro, macOS version, Windows version).
- A clear description of the issue, ideally with a minimal reproduction.
- Your assessment of impact (data exposure, sandbox escape, RCE, etc.).
- Whether you've shared this with anyone else.

## What to expect

- An acknowledgement within **3 business days**.
- An initial triage and severity assessment within **7 days**.
- Coordinated disclosure: we'll work with you on a fix, agree on a
  disclosure timeline (typically up to 90 days), and credit you publicly
  once the fix ships unless you'd rather stay anonymous.

## Scope

In scope:

- Sandbox escapes (shell tools running outside their allowed write set,
  reading paths they shouldn't).
- Risk system bypasses (Danger/Critical commands executing without the
  expected gate).
- Credential leaks (API keys ending up in logs, IPC, or persisted state
  in cleartext where they shouldn't be).
- Memory safety bugs (`unsafe` misuse, panics that bring down the daemon
  in production).
- Sense pipeline issues that could let a malicious email/message coerce
  the agent into actions outside the user's intent.

Out of scope (please don't report these):

- Issues in the user's own LLM provider (Anthropic, DeepSeek, etc.).
- Bugs in dependencies — please report those upstream.
- Social-engineering scenarios that require the user to run a malicious
  binary they downloaded themselves.

## Known accepted advisories

Advisories the project is aware of and has consciously chosen not to act on.
The corresponding suppression for tooling lives in `deny.toml`.

- **RUSTSEC-2024-0429** — `glib::VariantStrIter::impl_get` unsoundness
  (affected: glib `>=0.15.0, <0.20.0`; patched: `>=0.20.0`). Athen pulls
  glib 0.18.5 transitively via `tauri 2.10.3 -> gtk 0.18.2`. No workspace
  crate calls `VariantStrIter` or `g_variant_get_child` directly; the
  unsound path is only reachable through gtk-rs internals. Tauri 2.x has
  not bumped its gtk-rs pin and no 0.18.x backport of the upstream fix
  exists on crates.io. Will be revisited once Tauri moves to gtk-rs
  `>= 0.19` (which pulls glib `>= 0.20`).

Thanks for helping keep Athen safe.
