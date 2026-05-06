# Roadmap

> **Tell us what matters most.** Open a thread in [GitHub Discussions](https://github.com/albiol2004/Athen/discussions) — this roadmap is shaped by what real users are missing, not by what's easy to build.

Athen is a proactive personal assistant that runs on your machine, watches the channels you care about (email, calendar, messages), and acts on your behalf — silently when it's safe, asking when it's not.

This page is a *direction*, not a schedule. There are no dates here. Order changes when feedback changes.

---

## Guiding principle

**Athen should belong to you, not to a review queue at Meta or LinkedIn.**

We prefer integrations built on open protocols (SMTP, IMAP, CalDAV, Matrix, RSS, Telegram) over corporate APIs that gate access behind partnership programs, verification queues, or per-message billing. Open protocols mean you paste credentials once and Athen works forever — no third party can yank your access.

This is why some popular integrations are in *Not planned* below. It's not laziness; it's a deliberate choice about whose terms Athen runs on.

---

## Now

Things being built right now.

- **Athen acts on your email.** Reading inbound messages already works; soon Athen will draft, reply, and send on your behalf using your existing email account. SMTP-based, works with Gmail, Fastmail, ProtonMail Bridge, your own server — anywhere you can paste a password.
- **Smarter web search.** The current search tool falls over too often when one provider rate-limits. Rebuilding the failover chain and adding Brave Search as the default tier so research tasks just work.
- **Understanding images and documents.** Drop a screenshot or a PDF into a conversation and Athen will see it directly — invoice extraction, "what's in this picture", PDF summaries. Available on any provider that supports vision (Claude, GPT, Gemini, DeepSeek-VL).

## Next

What we're planning to do after Now lands.

- **Athen wakes itself up at the right time.** Schedule recurring or one-shot wake-ups — "remind me to follow up if no reply in 3 days", "every Monday at 9am summarize my week", "check on this in 2 hours". A proactive assistant without time-based triggers is missing half its identity.
- **Athen shows its work.** Render code, files, HTML pages, charts, and images inline rather than dumping them as text. Save artifacts to disk with one click.
- **Athen survives crashes and reboots.** Pending actions persist so a closed laptop or a power cut never drops work mid-flight. Includes:
  - *Stale-action confirmation:* if Athen was about to send an email six hours ago, it asks "still relevant?" before acting on the resumed approval.
  - *Sense-driven invalidation:* if you reply to that email yourself in the meantime, or the calendar event was cancelled externally, Athen notices and quietly drops the obsolete task.

## Exploring

Ideas we like but aren't committed to yet. Feedback here matters most.

- **Cloud-hosted Athen.** A managed option for people who don't want their PC running 24/7. You'd still own your data, still bring your own LLM keys; we'd just run the headless instance on a European server so your assistant keeps working when your laptop is closed. Self-hosting stays free and supported regardless.
- **Google Calendar (read + write).** Pending OAuth verification with Google — a multi-week paperwork process we'll start once Athen has a public homepage and privacy policy live. Read-only via iCal subscription is a possible interim step.
- **Voice input.** Whisper-based dictation, local or via API.
- **Local file watching.** Treat your Downloads or Documents folder as a sense — Athen notices when something arrives and offers to act on it.
- **Matrix and other open chat protocols.** Same model as Telegram: Athen is a participant in your chats, not a hosted service.
- **Mobile companion app.** Telegram already covers most of this, but a real mobile app would unlock richer notifications and inline approvals.

## Not planned

Things we've considered and decided against, with the reasoning. Open a discussion if you think we're wrong — these aren't permanent rejections, but the bar to flip them is high.

- **LinkedIn integration.** The platform actively prevents third-party automation. The official Marketing API requires partner status; unofficial scrapers get user accounts banned. There is no clean "click Connect" path that won't burn the user.
- **WhatsApp via personal accounts.** Unofficial libraries violate WhatsApp's terms and result in number bans. The official Business API requires Meta-approved message templates and per-message costs — incompatible with a personal-assistant pattern.
- **Cloud-only / login-required mode.** Even if cloud hosting ships, the local-first version stays the canonical one. Athen runs on your machine, not on ours, by default.
- **Bundled LLM credits.** Athen will keep using your own keys for LLM providers. Bundled credits would mean either rationing power users or charging everyone for the heaviest tail — better to let you pay providers directly at their actual rates.
- **Config files.** All configuration lives in the UI. Athen is for non-technical users; editing TOML by hand isn't an interface, it's a barrier.

---

*This page changes when priorities change. To see what shifted recently: `git log docs/ROADMAP.md`.*
