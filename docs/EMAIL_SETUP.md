# Email Setup Wizard

[SHIPPED] **Status: Phases 1–3 shipped 2026-05-12. Phase 4 (polish) deferred.**

Design for the **Settings → Email** panel: a one-screen account-add flow that figures out IMAP/SMTP servers from just an email + password, deep-links the user to their provider's app-password page when needed, and translates auth/connection errors into plain English.

Synthesized 2026-05-12 from five parallel research streams (provider autodetect mechanics, Rust crate audit, credential UX patterns, error catalog, IDLE monitoring). Builds on the existing `athen-sentidos/src/email.rs` polling monitor and `athen-sentidos/src/email_send.rs` SMTP sender — this is an **additive UX layer**, not a rewrite. Credentials now live in `athen-vault` (encrypted keychain) under scope `email:<account_id>`.

## Scope and non-goals

**In scope (Phase 1–3):**
- Account-add wizard in Settings → Email (NOT first-run onboarding; user opts in when they want it).
- Autodetect for the top-10 providers + Thunderbird autoconfig fallback + manual override.
- Test-connection button with LLM-translated error messages and per-provider app-password deep-links.
- Credential storage in `athen-vault` under scope `email:<account_id>` (not the plaintext config row we use today).

**Deferred:**
- OAuth2 (XOAUTH2) for Gmail/Outlook — Move #3 in the integrations push.
- Real IMAP IDLE — current polling works; IDLE upgrade is its own task once we want sub-minute push.
- Multiple accounts per user — first cut is single-account, additive to today's `EmailConfig`.
- JMAP for Fastmail, MS Graph webhooks for Outlook — same line as OAuth.

## UX shape (one screen, two states)

**State A — happy path (90% of users):**

```
┌─ Connect your email ──────────────────────────────┐
│                                                    │
│  Email address                                     │
│  [alice@gmail.com                  ]              │
│                                                    │
│  Password                                          │
│  [••••••••••••                     ]              │
│  ⓘ Gmail needs an app-specific password.          │
│    [Open Google app passwords →]                  │
│                                                    │
│  ▸ Advanced (server settings)                     │
│                                                    │
│             [Cancel]  [Test & Save]               │
└────────────────────────────────────────────────────┘
```

- The app-password hint and button appear **only after the email domain is matched** against the provider table — so users on Fastmail/Yandex/self-hosted don't see Gmail noise.
- The deep-link button opens the provider's app-password page in the system browser. We don't try to scrape or auto-fill — user copies the generated password back into the form.
- "Test & Save" is the only positive action. On success: banner "Connected to Gmail as alice@gmail.com" + the panel switches to the connected-account view (status, last-poll, disconnect, edit).
- On failure: an error banner appears under the form with the LLM-translated message + a contextual action button (regenerate app password / fix port / etc.).

**State B — autodetect failure or manual override:**

The "Advanced" disclosure expands to show:
- IMAP host, port, security (SSL/STARTTLS/None)
- SMTP host, port, security
- Username (defaults to email; some providers like Fastmail use a separate username)

Power users land here intentionally; everyone else only sees it if autodetect couldn't resolve and the LLM-translated banner says "We couldn't find server settings for example.com — enter them manually."

## Detection chain

When the user blurs the email field (or hits Test & Save), the detector runs four steps in order. Each returns either a complete config or `None`:

1. **Hardcoded provider table** — exact-match on email domain. Hits ~90% of users globally. Stale tables don't break anyone (the LLM translator catches it), but updates are a 5-line code change.

2. **Thunderbird autoconfig** — three HTTP lookups in parallel, first-success wins:
   - `https://autoconfig.<domain>/mail/config-v1.1.xml?emailaddress=<email>`
   - `https://<domain>/.well-known/autoconfig/mail/config-v1.1.xml?emailaddress=<email>`
   - `https://autoconfig.thunderbird.net/v1.1/<domain>`
   
   Parse the returned XML (`<incomingServer>` / `<outgoingServer>` with `hostname`, `port`, `socketType`, `authentication`).

3. **MX-based ISPDB recheck** — if the apex domain misses, look up the MX record and try the ISPDB on the MX domain. Catches `@mycompany.com → MX mx.fastmail.com`.

4. **Hostname probing** — last resort. Try `imap.<domain>:993` (SSL), `mail.<domain>:993`, `<domain>:993`, each with a 5-second TCP connect timeout. Same probe for SMTP on 587 (STARTTLS) then 465 (SSL). Don't probe port 25 — ISPs commonly block outbound 25.

Steps 1–3 don't talk to the user's mail server; they're cheap. Step 4 is gated behind a "Probe" button so we don't leak credentials to random hosts on a typo.

## Provider table (2026-05)

Source of truth lives in code; this is documentation. Confirmed live values:

| Provider | IMAP | SMTP | Auth | App-password deep-link |
|---|---|---|---|---|
| Gmail | `imap.gmail.com:993` SSL | `smtp.gmail.com:587` STARTTLS | App password (2FA required); basic auth killed 2025-03 | `https://myaccount.google.com/apppasswords` |
| Outlook.com / Hotmail / Live | `outlook.office365.com:993` SSL | `smtp.office365.com:587` STARTTLS | OAuth2 preferred; app password being phased out 2026 | `https://account.live.com/proofs/AppPassword` |
| Office 365 personal | same as Outlook.com | same | OAuth2 only | (OAuth path) |
| iCloud Mail | `imap.mail.me.com:993` SSL | `smtp.mail.me.com:587` STARTTLS | App-specific password (2FA required) | `https://appleid.apple.com` (Sign-In and Security) |
| Fastmail | `imap.fastmail.com:993` SSL | `smtp.fastmail.com:465` SSL | App password | `https://www.fastmail.com/settings/security/apppasswords` |
| Yahoo | `imap.mail.yahoo.com:993` SSL | `smtp.mail.yahoo.com:465` SSL | App password (2FA required) | `https://login.yahoo.com/account/security/app-passwords` |
| Proton Mail | `127.0.0.1:1143` (Bridge) | `127.0.0.1:1025` (Bridge) | Transparent via Bridge; paid plan required | flag + link to https://proton.me/mail/bridge |
| Yandex | `imap.yandex.com:993` SSL | `smtp.yandex.com:465` SSL | App password | `https://id.yandex.com/security/app-passwords` |
| GMX | `imap.gmx.com:993` SSL | `mail.gmx.com:587` STARTTLS | Password (must enable IMAP/POP in settings first) | `https://www.gmx.com/mail/settings/` |
| Zoho | `imap.zoho.com:993` SSL | `smtp.zoho.com:465` SSL | App password (free tier requires) | `https://accounts.zoho.com/home#security/apppasswords` |
| AOL | `imap.aol.com:993` SSL | `smtp.aol.com:465` SSL | App password (2FA required) | `https://login.aol.com/account/security/app-passwords` |

**Proton Bridge special case:** detection probes `127.0.0.1:1143`; if no listener, show a banner "ProtonMail needs Bridge running. [Open Proton Bridge docs →]" and disable the form until Bridge is up. No other provider gets a hard gate.

## Test-connection mechanics

Goal: prove credentials + folder access in under 3 seconds, with a clear pass/fail signal.

Sequence (reuses today's sync `imap` crate; no async-imap migration needed yet):

1. TCP + TLS to IMAP host:port (timeout 5s).
2. `LOGIN <user> <password>` (timeout 5s).
3. `LIST "" ""` to confirm folder enumeration works. (Catches some providers that accept LOGIN but reject everything else.)
4. `LOGOUT`.

5. TCP + STARTTLS/SSL to SMTP host:port (timeout 5s).
6. `EHLO` + `AUTH LOGIN` with the same credentials (timeout 5s).
7. `RSET` + `QUIT`. Do NOT actually send mail; we don't want a verification email landing in the user's inbox.

If both halves pass → save to vault + flip the panel to connected state.
If either fails → surface the error through the translator.

## Error translation

Two-tier: static catalog for the common cases, LLM fallback for everything else.

**Static catalog** (`crates/athen-app/src/email_errors.rs`):

```
imap_authenticationfailed_gmail → "Gmail needs an app-specific password. Make one with the button above, then paste it here."
imap_authenticationfailed_outlook → "Outlook stopped accepting passwords for this method. Use the new app-password page, or sign in with browser (coming soon)."
imap_authenticationfailed_icloud_or_yahoo → "This provider needs an app-specific password instead of your account password."
imap_alert_web_login_required → "Google flagged this sign-in. Open the link above to verify, then try again."
smtp_535_5_7_8 → "Same as IMAP — the password isn't accepted. Did you paste an app password?"
smtp_535_5_7_139 → "Outlook requires modern authentication. Use the app-password link above."
smtp_5_7_0_auth_required → "The SMTP server is refusing to send without authentication. We need your password here."
tcp_connection_refused → "We can't reach <host>:<port>. The server name or port might be wrong, or a firewall is blocking us."
tls_handshake_failed → "Encryption negotiation failed. If this is a self-hosted server with a custom certificate, you may need manual settings."
tcp_timeout → "The server didn't answer in 5 seconds. Check your internet, the host name, and whether port <port> is open."
imap_starttls_unsupported → "<host> doesn't support STARTTLS on port <port>. Try implicit SSL/TLS on port 993 (or 465 for SMTP)."
imap_too_many_connections → "Too many email clients on this account. Close some and try again."
imap_over_quota → "Your mailbox is full. Free up space at the provider, then retry."
```

Each entry carries `{ title, body, action_label, action_url }`. The translator runs against the raw error string + the matched provider; if no static rule matches:

**LLM fallback** — one structured-output call:

```
System: You are an email setup assistant. Given a raw IMAP/SMTP error and the email
provider domain, return JSON: {message: string, suggestion: string, action_url: string|null}.

Rules:
- One sentence per field. No jargon (no SMTP codes, no RFC numbers, no "STARTTLS").
- Only suggest URLs you are confident exist (google.com support, apple.com support, microsoft.com support).
- If unsure of the cause, suggest "Check the server settings under Advanced."
- Never recommend reinstalling, disabling antivirus, or restarting.

User: error="<error>", domain="<domain>"
```

Cache results by `hash(error + domain)` so the same typo doesn't burn a token on every retry. Cache lives in-memory for the session; no need to persist.

## Storage shape

Today's `EmailConfig` (in `athen-core::config`) holds plaintext `imap_password`. That moves to the vault.

**New shape:**
- `EmailConfig` keeps host/port/security/folders/poll interval.
- Password reference becomes a vault pointer: scope `email:<account_id>`, key `password`. `account_id` is a UUID generated at account-add.
- On startup, `EmailMonitor` resolves the vault pointer to the live password; same for SMTP send.
- Migration: existing users with a plaintext password get a one-shot migration on first launch — write to vault, blank the config field, log it.

This is the same pattern `athen-vault` already serves for the `http_request` registered endpoints and MCP env bindings; no new vault scope conventions needed.

## Phasing — Shipped Status

**Phase 1 — Data plane [SHIPPED 2026-05-12]:**
- `crates/athen-core/src/email_provider.rs`: `ProviderHint { incoming, outgoing, auth_kind, app_password_url, notes }`.
- `crates/athen-app/src/email_autodetect.rs`: hardcoded table + Thunderbird autoconfig fetcher.
- `crates/athen-app/src/email_errors.rs`: static error catalog + matcher (50+ common IMAP/SMTP errors).
- Tauri commands: `email_detect(email) -> ProviderHint | None`, `email_test_connection(config, password) -> TestResult`, `email_translate_error(error, domain) -> TranslatedError`.
- Vault integration: passwords stored under scope `email:<account_id>`.

**Phase 2 — Settings UI [SHIPPED 2026-05-12]:**
- Settings → Email panel live: email field → live provider detection → app-password button → password field → Test & Save.
- Advanced disclosure with manual host/port/security form.
- Connected state: banner with provider + last-poll time + Disconnect + Edit.
- Error banner with LLM-translated message + contextual action button.

**Phase 3 — LLM error translator [SHIPPED 2026-05-12]:**
- `email_translate_error` Tauri command wired into the LLM router.
- Session-scoped response cache (deduped on `hash(error + domain)`).
- Graceful fallback on LLM unavailable (raw error with disclaimer).

**Phase 4 — Optional polish [DEFERRED]:**
- Hostname probing as a "Probe" button when autodetect fails.
- Per-provider hint copy ("Gmail: 2FA must be on first"; "iCloud: passwords appear at appleid.apple.com under Sign-In and Security").
- Connected-state freshness indicator improvements.

## Out of scope (and why)

| Item | Why deferred |
|---|---|
| OAuth2 (XOAUTH2) for Gmail/Outlook | Move #3 in `docs/INTEGRATIONS_PUSH.md` — needs device-code flow + token refresh plumbing that's shared with GitHub/MS Graph/Notion/Slack. |
| Switch sync `imap` → `async-imap` | The Chatmail fork is healthier, but the migration is a 1500-line rewrite for no functional gain at this stage. Revisit when we add IDLE. |
| IMAP IDLE for sub-minute push | Polling at 60s is fine for now. IDLE adds connection-lifecycle complexity (24min re-IDLE + 8min NOOP + exponential backoff + polling fallback per the IDLE research). Its own task. |
| Multiple accounts | Today's `EmailConfig` is singular. Adding a list adds UI scope (which account does `send_email` use?). Defer until users ask. |
| JMAP / MS Graph webhooks / Gmail Pub-Sub | Real push protocols, but each is per-provider and each rides on OAuth. Layer in with Move #3. |

## Related docs

- `docs/INTEGRATIONS_PUSH.md` — strategic picking menu; this is Move #2.
- `docs/TOOLS_AND_SENSES.md` — how `EmailMonitor` plugs into the sense router.
- `docs/CONFIGURATION.md` — where `EmailConfig` lives and how it's loaded.
- Code: `crates/athen-sentidos/src/email.rs` (polling monitor), `crates/athen-sentidos/src/email_send.rs` (SMTP), `crates/athen-vault` (credential storage).
