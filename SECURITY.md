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

Thanks for helping keep Athen safe.
