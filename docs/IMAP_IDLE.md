# IMAP IDLE & `imap` Crate Migration

**Status: Deferred design doc. Not implemented.** Synthesized 2026-05-22 from parallel research.

Athen currently polls IMAP every 60s with the sync `imap = "2.4.1"` crate (`athen-sentidos/src/email.rs`). This document captures the migration plan for when one of two triggers fires:

1. **Rust promotes the `SEMICOLON_IN_EXPRESSIONS_FROM_MACROS` lint to a hard error.** Today it's `warn`-by-default and has been since Rust 1.56 (Aug 2021). Edition 2024 did **not** flip it. Tracking issue [rust-lang/rust#79813](https://github.com/rust-lang/rust/issues/79813) sits on `S-tracking-needs-to-bake` with no committed edition / release. Probably 3+ years out, possibly never — but if it lands, our build breaks until we move off `imap-proto 0.10.x`.

2. **We want sub-second new-mail latency** (IMAP IDLE / push). Today's 60s poll gives ~30s avg latency on new mail; this only matters if we want "Telegram ping within 2s of email arriving" UX rather than "agent eventually triages." Agent thinking time (10–30s for risk eval + LLM) dominates the polling delay, so this is a polish move, not a correctness fix.

The two triggers share the same migration target, which is why they share a doc.

## Why polling stays anyway

This is the most important framing point: **IDLE doesn't replace polling, it adds a state on top of it.** You keep the polling code as the fallback for:

- Servers that don't advertise the `IDLE` capability (mostly small / self-hosted).
- Transient socket errors mid-idle (NAT timeouts, server-side disconnects, auth-token expiry on long-held connections).
- The 29-minute RFC-2177 cap, which forces a `DONE` + re-`IDLE` cycle.
- Provider-specific quirks (Gmail vs. Outlook vs. Fastmail all behave slightly differently on the IDLE handshake).

So the deliverable is `Polling | Idling | ReIdling | Backoff` running **alongside** today's polling, not replacing it.

## The crate choice — `imap 3.0.0-alpha.15` vs. `async-imap 0.11.2`

### `imap 3.0.0-alpha.15` (sync, Feb 2025)

- **Pulls in `imap-proto 0.16.x`** → fixes the future-incompat warning.
- API delta from 2.4.1 → 3.0 (touching our call sites in `email.rs` / `email_test.rs`):
  - `ClientBuilder` replaces the free-function `imap::connect`. Manual-stream `Client::new(stream)` may still exist but is no longer the documented path.
  - `Session::expunge` returns `Result<Deleted>` instead of `Result<Vec<u32>>` (we don't currently call `expunge`).
  - IDLE: all `wait_*` helpers merged into a single `wait_while(callback)` builder.
  - `append_with_*` family replaced by an `AppendCmd` builder (we use `lettre` for outbound, so this is moot for us).
  - `Flag`, `Mailbox`, `UnsolicitedResponse`, `Error` are now `#[non_exhaustive]` — exhaustive matches need a wildcard arm.
  - TLS is enforced by default; the `tls` feature was renamed to `native-tls`, with a new parallel `rustls-tls` feature.
- **Risk:** Has been in alpha since 2022. Only 2 alpha releases in the 18 months before alpha.15 (alpha.14 in Mar 2024, alpha.15 in Feb 2025). No stated stable-release timeline. Nobody serious is shipping on it.
- **Effort:** ~15–25 lines of mechanical rewiring in `email.rs` + `email_test.rs`. Still sync, so `spawn_blocking` shim and existing call shape are preserved. **But IDLE is awkward in sync** — every IDLE'd account burns one OS thread for 24 minutes at a stretch. This is the dealbreaker for scaling past 1–2 accounts.

### `async-imap 0.11.2` (async, Feb 2026)

- Same maintainer as `imap` (Jonas Schievink / `jonhoo`'s collaborators). Recently updated.
- **Pulls in `imap-proto 0.16.x`** → fixes the warning.
- API is async-first: every call is `.await`, sessions are owned across tasks instead of blocking threads.
- **Strictly better for IDLE.** One tokio task per account, `tokio::select!` over the IDLE response stream + a 24-minute re-IDLE timer + a NOOP keepalive timer. No thread burn.
- **Migration cost:** higher — requires undoing the `spawn_blocking` shim in `email.rs` and `email_test.rs`. ~200–400 lines touched, plus the IDLE state machine itself if we ship that at the same time.

### Recommendation

**`async-imap` whenever we move.**

- If the trigger is the lint becoming a hard error and we *don't* want IDLE, doing the sync→async migration still beats picking up a permanently-alpha sync crate.
- If the trigger is IDLE, `async-imap` is the obvious choice — IDLE in sync is a non-starter at >1 account.
- Both triggers point the same way, so there's no scenario where `imap 3.0.0-alpha.15` is the right answer.

## Migration sketch (`async-imap` + IDLE)

Touch list:

1. **`Cargo.toml` (workspace):** swap `imap = "2.4"` → `async-imap = "0.11"`. Add `futures = "0.3"` if not already in tree (for `StreamExt`).
2. **`athen-sentidos/src/email.rs`:**
   - Remove the `spawn_blocking` wrappers around `imap::Client::new` / `Session` calls.
   - Convert `fn` IMAP helpers to `async fn`.
   - Replace `client.login(...)` etc. with their async-imap equivalents (very similar shape, just `.await`).
   - Add the IDLE branch (see state machine below).
3. **`athen-app/src/email_test.rs`:**
   - The `tokio::task::spawn_blocking(move || imap_blocking(...))` wrapper goes away; the test helpers become async directly.
   - Error type changes (`async_imap::error::Error` instead of `imap::Error`); update the static error catalog keys in `email_errors.rs` if any match on type-name strings.
4. **`athen-app/src/email_autodetect.rs`:** no changes — autoconfig probing is HTTP, not IMAP.
5. **`athen-app/src/settings.rs`:** likely no changes — the IMAP code path here is just credential-test invocation.
6. **`athen-app/src/email_gate.rs`:** no changes — gating is event-shape, not connection-shape.

### IDLE state machine

```
                    ┌──────────────┐
                    │   Polling    │◄──────────────────┐
                    │  (60s loop)  │                   │
                    └──────┬───────┘                   │
                           │ capability advertises IDLE│
                           │ AND last poll was clean   │
                           ▼                           │
                    ┌──────────────┐                   │
                    │   Idling     │                   │
                    │  (24m max)   │                   │
                    └──────┬───────┘                   │
                ┌──────────┴──────────┐                │
       EXISTS / │                     │ 24m elapsed    │
       EXPUNGE  │                     │                │
                ▼                     ▼                │
        fetch new mail        ┌──────────────┐         │
        (drop out of IDLE)    │   ReIdling   │         │
                              │ (DONE + IDLE)│         │
                              └──────────────┘         │
                                                       │
       on socket error / auth failure ─────────────────┘
       (with exponential backoff: 30s → 5m cap)
```

Sticky behavior on failure: once an account falls back to polling for any reason, stay in polling for the rest of the process lifetime. Don't auto-promote back to IDLE — too easy to ping-pong on flaky links.

### Scoping the experiment

When (and if) we ship this, scope tightly to bound complexity:

- **First cut: default account only.** Multi-account IDLE is a separate task — scaling concerns + per-account quirks compound.
- **No IDLE for self-hosted / unknown providers.** Whitelist of "we know IDLE works here" providers (Gmail, Outlook, iCloud, Fastmail, Yahoo). Everyone else: polling.
- **Feature-flag the IDLE path.** A settings toggle "Use real-time email (experimental)" — defaults to off — so we can ship the async-imap migration first, prove it's stable, then enable IDLE per-user.

## What NOT to do

- **Don't fork `imap-proto 0.10`** to silence the warning via `[patch.crates-io]`. That's a small fork but you own it forever for a non-issue.
- **Don't suppress the lint** via `#[allow(...)]` — the warning comes from a transitive dep's source, not ours; you'd have to use `--cap-lints` or similar global hacks. Not worth it.
- **Don't migrate to `imap 3.0.0-alpha.X`** — see "Recommendation" above.

## Related

- [Email Setup Wizard](EMAIL_SETUP.md) — the UX shipped 2026-05-12 sits on the polling stack this doc would replace. The wizard itself is provider-agnostic; an IDLE upgrade is invisible to it.
- [Integrations Push](INTEGRATIONS_PUSH.md) — Move #3 (OAuth wave) and Move #2 (email setup) were the higher-priority items. This is post-Move-#3.
