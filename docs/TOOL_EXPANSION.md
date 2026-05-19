# Tool Expansion Menu

Survey of best-in-class CLIs / APIs across 10 categories, synthesized 2026-05-10 from parallel Haiku research. Updated 2026-05-19.

**Note:** `http_request` tool + 15 Cloud API presets (including Jina, Firecrawl, Brave, SerpAPI, etc., shipped 2026-05-10) covers stateless REST calls generically. This menu focuses on category-specific wraps (browser automation, OCR, database tools, etc.) that require tight binding, streaming, or state management.

This is a **picking menu**, not a build plan. Pick categories to wrap first, then write per-category implementation docs.

---

## Existing tool surface (baseline, do not duplicate)

`shell_execute`, `read`, `write`, `edit`, `grep`, `list_directory`, `web_search` (DDG/Tavily), `web_fetch` (Local/Jina/Wayback/Cloudflare), `email_send` (SMTP), `contacts_*`, `calendar_*`, `memory_store`/`recall`, `identity_add`, `wakeup_*`, `install_package`, `delegate_to_agent`, `fetch_attachment`.

---

## 1. Browser automation

**Top pick:** `playwright-rs` 0.8.1 (Rust, async, Apache 2.0). Backup: `chromiumoxide` (CDP-only, lighter).

**Footprint:** zero runtime deps; Chromium first-launch download ~150–200 MB (one-time, cached). Cross-platform.

**Wrap shape (5 primitives):**
- `browser_login(url, username_selector, username, password_selector, password, mfa_required?)` → `session_id`
- `browser_navigate(session_id, url, wait_selector?)`
- `browser_fill_form(session_id, fields, submit_selector?)`
- `browser_extract(session_id, query, format)` (markdown/json)
- `browser_screenshot(session_id, full_page?, clip?)`

Sessions live in memory (HashMap with 15-min idle TTL). `storage_state.json` for cookie persistence between runs.

**Risk:** HighRisk default. Form-fill on `password|credit card|ssn`-shaped fields → escalate to per-action approval. Login can be auto-approved per trusted contact after first manual session.

**Fit:** Critical for the consulting-practice flows — LinkedIn (no API), Glassdoor scraping, anything paywalled. Also enables the multi-channel outreach loop.

---

## 2. Document conversion

**Top picks:**
- **Pandoc** (Haskell, ~15 MB static binary, no deps) — universal converter, excellent for DOCX↔Markdown.
- **Pandoc + Typst** (~40 MB binary, Rust) — best Markdown → PDF chain.
- **Marker** (Python, AGPL-3.0, ~2–4 GB models) — best PDF → Markdown for AI consumption (handles tables/equations). Optional behind a UI toggle.
- **MarkItDown** (Microsoft, Python, MIT, ~100 MB) — multi-format middle ground.

**Wrap shape (2 primitives):**
- `convert_document(input_path, output_format, preserve_layout?, enable_ocr?)` — auto-routes by extension
- `generate_document(markdown, output_format, template?)` — Markdown → PDF/DOCX/PPTX

**Risk:** Read tier. All output is local files.

**Fit:** Daily utility. Reading client PDFs, generating proposals as PDF, reading invoices. Pandoc + Typst is the no-brainer ship.

---

## 3. OCR + image

**Top picks:**
- **PaddleOCR 3.x** (Python, Apache 2.0, ~10–20 MB ultra-light model) — best size/accuracy tradeoff, 106+ languages incl. ES/EN/DE.
- **Surya** (Python, ~300–500 MB models) — production quality on poor scans.
- **Tesseract 5.5** (~20 MB binary + 4–8 MB per language) — offline fallback, classic.
- **libvips** for image manipulation (3–5 MB binary, Apache 2.0, no CVEs unlike ImageMagick).

**Wrap shape (4 primitives):**
- `ocr_image(path, lang?)` — defaults Paddle, falls back to Tesseract
- `image_manipulate(path, operation, output, ...)` — crop/resize/convert/strip-EXIF via libvips
- `image_compare(a, b, method)` — SSIM / perceptual hash
- (no separate scanned-PDF tool; route through `convert_document` with `enable_ocr=true`)

**Risk:** Read tier.

**Fit:** Lower urgency — vision-capable LLMs already read images natively. Useful for cheap/local-model fallback and searchable-archive workflows. Ship after the consulting-flow categories.

---

## 4. GitHub / Git

**Top pick:** `gh` (GitHub CLI, Go, ~15–30 MB single binary, MIT). Auth via `gh auth login` (browser OAuth, system keyring storage).

**Wrap shape (5 primitives, all dispatch via `gh`):**
- `github_list_issues(repo, state, labels?, limit?)` — Read
- `github_search_issues(query, repo?, limit?)` — Read
- `github_check_runs(repo, ref?)` — Read (CI status)
- `github_open_pr(repo, title, body, head, base, draft?, labels?)` — WritePersist
- `github_comment_issue(repo, issue_number, body)` — WritePersist

Defer `github_merge_pr` (Critical, irreversible) until approval router supports stronger gates.

**Risk:** Per-tool as above. Always-allow Read tier; per-action approval on writes for first interactions, then per-repo allowlist.

**Fit:** High for the user (engineer). Bespoke wrappers beat raw `gh` because: parseable JSON output, structured tool cards, granular risk classes per action.

---

## 5. Audio / video / transcription

**Top picks:**
- **whisper.cpp** (C++, MIT, single binary + 75 MB–2.9 GB models) — local transcription. `whisper-rs` Rust FFI bindings already exist.
- **ffmpeg** (LGPL, ~30 MB binary) — universal media. **Wrap, don't expose** — LLMs flail on its CLI.
- **yt-dlp** (Python+ffmpeg, Unlicense, ~50–100 MB) — URL download.
- **gpu-screen-recorder** (Linux, GPL-3) / OBS CLI (cross-platform) — screen recording.

**Wrap shape (5 primitives):**
- `transcribe_audio(path, lang?)` → text
- `media_extract_audio(input, output)`
- `media_trim(input, start_sec, end_sec, output)`
- `media_convert(input, output, quality?)`
- `download_url(url, format?, output_dir)`

**Risk:** Read tier (local file output).

**Fit:** Mid. Transcription is genuinely useful for meeting notes / voice notes the user might send. Media manipulation is occasional. Download is low-urgency. Ship transcription first.

---

## 6. Database

**Top picks:**
- **DuckDB** (~50 MB single binary, MIT) — Swiss army for ad-hoc analytics; queries CSV/Parquet/JSON directly, attaches SQLite/Postgres/MySQL.
- **psql / pgcli / mycli / sqlite3** for engine-specific shells.
- **atlas** (~20 MB Go binary, Apache 2.0) — declarative schema migrations with shadow-DB dry-run.

**Wrap shape (4 primitives):**
- `db_query(database, sql, limit?, dry_run?)` — Read
- `db_schema(database, table?, include_indexes?)` — Read
- `db_execute(database, sql, allow_writes, dry_run?)` — WritePersist (if `allow_writes`)
- `db_migrate(database, migration_dir, dry_run?, direction, target_version?)` — Critical, dry_run defaults true

**Connection model:** new "Registered Databases" persistence table (like contacts) — agent passes a registered name, never a connection string. Tokens stay out of prompts.

**Risk:** SELECT → Read auto-approve. Mutations → require `allow_writes=true` + per-action approval. Migrations → dry-run first, approval on commit.

**Fit:** Medium-high for engineering work. The Registered Databases pattern is the unlock — same shape we use for contacts.

---

## 7. Social media

**Reality check:**
- **X**: pay-per-use ($0.01/post, $0.005/read) — no free tier as of 2024.
- **LinkedIn**: official posting API requires enterprise partnership ($10–50k/yr). Personal automation = scraping (browser-automation territory).
- **Bluesky**: free, app-password auth (no OAuth dance), open protocol — easiest target.
- **Mastodon**: free, personal access token, agent-friendly.
- **Threads / Instagram**: friction-heavy (Meta partnership gates).

**Top picks:**
- **Postiz** (open-source self-host, $0; or Cloud $29/mo) — OAuth broker for 30+ platforms. Single REST API publishes to N platforms.
- Direct: `egg-mode` (X, Rust), `atrium` (Bluesky, Rust), `elefren` (Mastodon, Rust).

**Wrap shape (single `social_*` namespace, 6 primitives):**
- `social_post(platform, text, media?, reply_to?, scheduled_for?)`
- `social_reply(platform, post_id, text, media?)`
- `social_list_mentions(platform, since?, limit?)`
- `social_list_inbox(platform, unread_only?, limit?)` — DMs
- `social_get_profile(platform, handle)`
- `social_search(platform, query, limit?)`

**Risk:** All public posts → HighRisk, per-message approval. DMs → Medium. Reads → Low.

**Fit:** High for the consulting practice (thought leadership). Recommend Postiz as the backend (sidesteps OAuth ceremony) + Bluesky/Mastodon direct for free testing. LinkedIn via Postiz; X paywall is a budget choice.

---

## 8. Outreach / messaging

**Reality check (2026):**
- Gmail Workspace cold-email is dead at scale (~15–25/day max after 2024–25 crackdowns + SPF/DKIM/DMARC mandates).
- Transactional APIs: Postmark ($15/mo for 10k), Resend ($20/mo for 50k), SendGrid ($19.95/mo for 50k), Amazon SES ($0.10/1k cheapest at scale).
- Cold-email platforms: **Smartlead** ($39–94/mo, best API + webhooks), Instantly, Lemlist, Apollo.

**Build vs buy split:**
- **Build (Athen-native):** sequences on top of existing wakeup system, for ≤50 emails/day. Owns deliverability + bounce.
- **Buy (Smartlead API integration):** for 50–500/day. Athen sends contact + template + variables; Smartlead handles warmup + tracking.

**Wrap shape (6 primitives):**
- `outreach_send(channel, contact_id, template_id, variables, delay_until?)` — channel = email/linkedin/telegram
- `outreach_sequence_create(name, steps, contacts, start_date?)` → sequence_id
- `outreach_check_replies(since?)` → list of `{contact_id, channel, body, sentiment, intent_class, thread_id}`
- `outreach_pause_sequence(sequence_id, contact_id?)`
- `outreach_set_contact_state(contact_id, state)` — paused/active/unsubscribed
- (need `email_search`/`email_inbox` tool — currently missing — IMAP or Gmail API)

**Risk:** HighRisk. Per-message approval for first 5–10 sends per template, then optional silent for trusted-template + allowlisted-segment combinations.

**Fit:** Critical for the consulting practice. The biggest single category for Alex specifically.

**Cross-cutting:** depends on a new `email_search` / `email_inbox` tool (currently missing — `email_send` is one-way). That's a foundational gap to close first.

---

## 9. Scraping

**Top picks:**
- **Jina Reader** (already wired in `web_fetch`) with `x-json-schema` header for structured extraction — free.
- **Firecrawl** (open-source + cloud, free 1k/mo, $49/mo for 50k) — `/extract` with JSON Schema.
- **Crawl4AI** (open-source Python + Playwright) — for JS-heavy targets.
- **Scrapfly / Bright Data** (cloud, $30/mo+) — only for anti-bot-heavy commercial targets.

**Quick win:** extend existing `web_fetch` with a `schema?` parameter. When supplied, route through Jina's `/extract` (free) — returns JSON matching the schema instead of markdown. Single-line agent unlock.

**Wrap shape (3 primitives, additive to `web_fetch`):**
- `web_fetch_json(url, schema?)` — structured single-page (Jina/Firecrawl)
- `web_crawl_paginated(start_url, next_selector, max_pages, schema?)` — bulk traverse
- `web_search_results_scrape(engine, query, n_results)` — SERP extraction

**Risk:** Read tier. Escalate to Caution if `web_crawl_paginated` is unbounded (regex too loose) or anti-bot bypass enabled. Respect robots.txt by default; per-domain UI override.

**Fit:** High for outreach personalization (read prospect's company site → extract structured signals). The `web_fetch_json` extension is nearly free to ship.

---

## 10. Job search

**Top picks (Switzerland/EU coverage):**
- **Adzuna** API — 19 countries incl. CH, free tier on RapidAPI.
- **TheirStack** — 315k+ sources, $25/mo for 200 credits. Best volume.
- **Jooble** — EU-focused, generous free tier.
- **JSearch (RapidAPI)** — Google-for-Jobs aggregator, real-time.
- **HN "Who is hiring"** — official HN API + community parsers ([HNHIRING.com](https://hnhiring.com)). Startup-heavy, founder-posted, high signal.
- **RemoteOK** — free public API, 30k+ remote listings.
- **AIDevBoard** — AI-specific, REST API free 100 req/hr / $49/mo Pro.
- **No API** for: LinkedIn Jobs, Glassdoor, Toptal, Malt, Comatch, Catalant — scraping or aggregator only. Malt (CH-resident, EU consulting) is most relevant for Alex but has no API; route via browser automation if the volume is worth it.

**Wrap shape (3 primitives):**
- `jobs_search(query, location?, remote_only?, sources?, limit?)` — Read
- `jobs_watch(query, location?, remote_only?, sources, frequency)` → wakeup_id (re-runs search, fires sense event on new match)
- `jobs_extract_contacts(job_id, contact_type?)` — finds recruiter/hiring manager email/LinkedIn for follow-up

**Risk:** Read tier. Auto-apply not supported by any 2026 platform via agent API; out of scope.

**Fit:** Direct match for the consulting use case. `jobs_watch` composes naturally with wakeups (already shipped) and the outreach loop (above). Three-tool shipment unlocks an end-to-end flow: monitor → research → personalised cold email.

---

## Cross-cutting prerequisites surfaced by the survey

These aren't categories but they unblock multiple categories:

1. **`email_search` / `email_inbox` tool** — currently missing; outreach is a one-way street without it. IMAP wrapping is the simplest cross-provider path; Gmail API + MS Graph for first-class providers as enrichment.
2. **Registered Databases store** (athen-persistence) — same shape as contacts. Lets agents reference DBs by name, not connection string.
3. **OAuth-redirect-catcher** in Tauri — needed for X / LinkedIn / Gmail API / OAuth-gated scrapers. Currently auth is per-provider ad hoc.
4. **`web_fetch_json(url, schema?)`** — single-line extension to `web_fetch` using existing Jina chain. Nearly free, unlocks scraping + outreach personalization + job-search enrichment.

---

## Recommended ship order for Alex's use case

The user is an AI Engineer in Switzerland building a consulting practice and wants admin automation. The best return on a single feature ship for **his** stated goals:

1. **`web_fetch_json` schema extension** (1 day) — unlocks structured extraction across scraping + outreach + jobs. Lowest cost, highest leverage.
2. **`email_search` + `email_inbox`** (2–3 days) — closes the inbound side of email. Required for the outreach loop to actually work.
3. **Outreach primitives + sequences on wakeups** (1 week) — `outreach_send`, `outreach_sequence_create`, `outreach_check_replies`. The flagship feature for the consulting practice.
4. **Job search primitives** (3–4 days) — `jobs_search`, `jobs_watch`, `jobs_extract_contacts`. Composes with #3 to deliver "monitor → research → personalised email" end-to-end.
5. **Browser automation primitives** (1 week) — needed for LinkedIn, Glassdoor, Malt; also unblocks anti-bot scraping. Use `playwright-rs` or `chromiumoxide` (not Node Playwright).
6. **Social media via Postiz** (3–4 days) — thought-leadership posting with scheduling.

Categories 7–10 (GitHub, document conversion, audio/video, OCR, database) are general-utility wins worth shipping after the consulting flows land.
