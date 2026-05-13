# Subscription-Relay Providers — Picking Menu

Synthesized 2026-05-13 from three parallel Haiku research streams (Windsurf/Cursor wrappers, GitHub Copilot proxies, Poe/Perplexity/xAI/HF/DDG and friends).

**Question driving this doc:** Several community projects expose paid AI-product subscriptions (Cursor, Windsurf, Copilot, Poe, …) as OpenAI-compatible local HTTP endpoints so a single $20/mo plan can be used programmatically. Should Athen ship first-class providers for any of them?

**Scope exclusions:** ChatGPT/Claude.ai/Gemini retail subscriptions are off the table — their ToS unambiguously bans programmatic reuse and Anthropic/OpenAI have enforced. This doc covers only adjacent products.

---

## TL;DR verdict

| Project | Subscription wrapped | ToS verdict | Athen integration |
|---|---|---|---|
| dwgx/WindsurfAPI | Windsurf Pro $15/mo | 🔴 Explicit ban (reverse-eng + resale + programmatic access clauses) | Skip |
| 7836246/cursor2api | Cursor Pro $20/mo | 🔴 Explicit ban + active enforcement (Anthropic Jan-2026 ban wave hit Cursor users) | Skip |
| ericc-ch/copilot-api & caozhiyuan/copilot-api | GitHub Copilot $10/mo | 🟡 Unpublished-API + abuse-detection ban risk | **`http_request` preset only**, user self-hosts |
| Poe (official OpenAI-compatible API) | Poe $20/mo | 🟡 First-party use OK, "relay/resell" forbidden | **`http_request` preset**, with disclaimer |
| Perplexity / xAI / Mistral / You.com | n/a — official paid APIs | 🟢 | Already covered by `openai_compat` adapter or existing presets |
| chat.deepseek.com wrappers | n/a — official API is cheapest on market | 🔴 + pointless | Skip |
| DuckDuckGo Chat, HuggingChat | n/a — free, reverse-engineered | 🔴 + fragile | Skip |
| Tabnine, JetBrains AI, Zed, Cody | — | No mature wrappers exist | Nothing to do |
| **magai.co** ($20/$40/$200/mo) | 50+ models behind one chat UI | 🟡 Legit company, direct provider relationships — but **intentionally no API** (founder published "Why Magai Will Never Have an API"). ToS forbids automated use. | Skip — there's nothing to call |
| **aizolo.com** ($9.9/mo) | "All-in-one" Claude+GPT+Gemini subscription | 🔴 **Almost certainly account-pooling.** $9.9/mo for "Claude + GPT + Gemini" is mechanically impossible without violating Anthropic/OpenAI account-sharing ToS. BYOK mode is legit; "built-in" mode isn't. Scam Detector 36/100, near-zero Reddit traction. | Skip (user's gut was right) |
| **Atlas Cloud** (atlascloud.ai, pay-per-token) | Multimodal aggregator (LLM + image + video) | 🟡 Legit funded company, OpenAI-compatible at `api.atlascloud.ai/v1`, **but ToS §7 explicitly bans "engage in automated use" / "access through automated or non-human means"**. Also: $0.26/M for DeepSeek vs Together's $0.14 — not price-competitive for LLM-only. | Skip — Together AI / Fireworks fill the same shape with agent-permissive ToS |
| **OpenCode Go** ($5 first mo, then $10/mo) | Curated open-source coding models bundle — DeepSeek V4 Pro/Flash, Qwen 3.5/3.6, Kimi K2.5/K2.6, GLM-5.1, MiniMax M2.5/M2.7 — direct provider contracts, not account-pooled | 🟢 **"Use Go with any agent"** is the explicit on-site copy. OpenAI-compatible at `https://opencode.ai/zen/go/v1`. Generic ToS boilerplate about automation is CYA; the marketing surface and the supported-tools docs are the load-bearing signal (same pattern that flipped MiniMax Token Plan). Tiered per-model 5h rolling windows: 31.6K req/5h DeepSeek V4 Flash, 10K req/5h Qwen 3.5 Plus, 3.45K req/5h V4 Pro, 1.15K req/5h Kimi K2.6. | **Wire as first-class provider** via `OpenAiCompatibleProvider`. Same shape as DeepSeek wire path. Cheapest legitimate coding bundle Athen can ship today; complements MiniMax (which adds Anthropic-compat + prompt-cache at $20+). |
| **ZenMux Pro** ($20/$100/$400/mo) | Enterprise aggregator across ~200 models (OpenAI/Anthropic/Google/DeepSeek/…) via official provider contracts | 🔴 Legitimate, but no differentiation over OpenRouter (already a Cloud APIs preset). "Insurance" upsell (latency/hallucination coverage) doesn't fit agent loops. ToS quiet on agent automation. | Skip — OpenRouter fills this niche with a smaller markup and explicit agent-loop tolerance. |

**Net move:** add two presets to `athen-app/src/http_presets.rs` (Poe + self-hosted Copilot relay). No new bespoke provider in `athen-llm`. Total work: ~30 minutes.

---

## Why this is `http_request`-shaped, not provider-shaped

Every interesting wrapper here already speaks OpenAI-compatible Chat Completions on a local port. The user already runs the relay binary on their machine; Athen just needs a base URL + bearer token. That matches the existing Cloud APIs preset substrate ([CLOUD_APIS.md](CLOUD_APIS.md)) exactly — no streaming-protocol bespokeness, no OAuth refresh, no state-carrying weirdness. Per the "generic `http_request` > bespoke wraps when shell+curl works" rule, don't write `athen-llm/src/providers/poe.rs`.

The only argument for a bespoke `Provider` impl would be if we want chat-style streaming through the agent executor's normal LLM path, not the agent-tool path. **That is a real argument** — the `http_request` tool isn't a substitute for an LLM provider when you want Athen to *think* using Poe's GPT-5 backend. If that lands as a need, add it as a thin variant of the existing `openai_compat` provider with a configurable base URL + auth header, not a new crate.

---

## Per-project notes

### 🔴 Windsurf (dwgx/WindsurfAPI, guanxiaol/WindsurfPoolAPI)

- 2.3k stars, active May 2026, 100+ models including Claude 4.7 / GPT-5.5 / Gemini 3.1
- Windsurf ToS (terms-of-service-individual): bans reverse engineering, programmatic access via non-Windsurf software, API resale, account transfer. Four separate clauses, all unambiguous.
- Skip. Atheneum-grade legal exposure for a feature the user can get for $5/mo direct via DeepSeek.

### 🔴 Cursor (7836246/cursor2api, eisbaw/cursor_api_demo)

- Same ToS profile as Windsurf.
- **Active enforcement signal:** Anthropic's January 2026 ban wave specifically targeted unauthorized Claude-via-Cursor paths. Accounts auto-banned via TLS fingerprinting and behavioural biometrics. Public dev.to write-ups.
- Skip.

### 🟡 GitHub Copilot (ericc-ch/copilot-api 3.9k★, caozhiyuan/copilot-api updated May 2026)

- Exposes the full Copilot Chat backend: GPT-5/5.4/5.5/mini and Claude family via Copilot's own multiplexer.
- GitHub Extension Developer Policy: no reverse engineering, no unpublished APIs. Both wrappers explicitly warn "excessive automated use may trigger abuse detection and suspend Copilot access".
- Athen's agent loop is by definition "excessive automated use". Wiring this as a first-class provider would burn user subscriptions on contact.
- **Workable pattern:** the user runs `copilot-api` locally on `http://localhost:4141`, Athen treats it as a generic OpenAI-compatible endpoint via an `http_request` preset. User shoulders the ToS risk explicitly. Athen does not advertise this beyond the preset entry — no marketing copy, no onboarding suggestion.

### 🟡 Poe by Quora — official OpenAI-compatible API

- This is the most interesting one. **Official** API at `https://api.poe.com/v1` (or `creator.poe.com`), no reverse engineering involved. $20/mo subscription includes credits across Claude Opus, GPT-5, Gemini Pro, Llama 3, Grok-3, plus 100+ community bots.
- Poe API ToS: explicitly bans "reselling, syndicating, relaying access to third parties without written consent". First-party use by the subscriber's own software is the gray-but-defensible case (the user is consuming their own subscription, not relaying to anyone else).
- **Ship pattern:** add a Cloud APIs preset, surface a one-line disclaimer in Settings ("for personal use only; do not expose this endpoint to other people"). The disclaimer is load-bearing — it's what makes this defensibly first-party rather than relay.
- Risk: Quora can revoke without notice (stated). Don't make it a default.

### 🟢 Perplexity, xAI Grok, Mistral, You.com

- Each ships an official paid API. No subscription wrapping needed — the wrappers exist because the vendor declined to ship an API, and these vendors didn't decline.
- Perplexity's Sonar models are already accessible via the existing OpenAI-compatible adapter. Same for xAI, Mistral, You.com. Already covered.

### 🔴 chat.deepseek.com session wrappers

- DeepSeek's *official* API is $0.14/1M tokens — the cheapest on the market. The session-relay wrappers exist for users without a credit card, not for economic reasons.
- Athen already integrates the official API. Skip the wrappers.

### 🔴 DuckDuckGo AI Chat, HuggingChat reverse-engineered

- Free but fragile (model lineup rotates, `x-vqd-4` header rotates, accounts get rate-limited by IP). Reverse-engineered, ToS-violating, no subscription to relay.
- Not worth production support.

### Phind Pro

- Shut down January 2026. Dead end.

### Tabnine Pro, JetBrains AI Pro $8/mo, Zed, Cody

- No community wrappers exist. Either the products don't multiplex frontier models (Tabnine, Cody on their own backbone) or the subscription cost is low enough nobody bothered (JetBrains).
- Tabnine and JetBrains both now support BYOAI (provide your own provider key). That's the inverse direction — they consume Athen-style providers, not the other way around.

### 🟡 magai.co — legitimate aggregator with no API on purpose

- Real SaaS company (founder Dustin Stout, ~$600k ARR 2024, Crunchbase + G2 + Trustpilot present). Direct API relationships with OpenAI/Anthropic/Google/Mistral/etc. **Not reverse-engineered.**
- $20/$40/$200/mo Starter/Pro/Ultra. 50+ models in one chat UI plus image/video gen.
- **Founder published a post titled "Why Magai Will Never Have an API (And Why That's the Point)"** — the no-API stance is deliberate product philosophy, not a roadmap gap.
- ToS (verbatim): *"Use, launch, develop, or distribute any automated system, including without limitation, any spider, robot, cheat utility, scraper, or offline reader… data mining, robots, or similar data gathering and extraction tools…"* Commercial resale explicitly banned.
- **Skip.** Magai solves "which subscription should the user pay for", not "what backend should Athen call". Different problem space entirely.

### 🔴 aizolo.com — almost certainly account-pooling, user's gut was right

- BKABHI INNOVATIONS LAB (India + US), $9.9/mo "all-in-one AI subscription" claiming Claude + GPT + Gemini + Grok + Perplexity access.
- Two paths: **BYOK** (paste your own keys, just a chat UI wrapper — legit) and **subscription "built-in"** (aizolo provides the model access for $9.9/mo).
- The subscription path is mechanically impossible at $9.9/mo without ToS violation. Anthropic/OpenAI don't license retail subscriptions for resale, and at the claimed margin you'd need account-pooling (rotating scraped/shared ChatGPT Plus + Claude.ai Pro accounts). This is exactly the pattern Anthropic banned in January 2026 (TLS fingerprinting + behavioural biometrics).
- Red flag corroboration: Scam Detector 36/100 "Questionable", only 3 Trustpilot reviews, blog posts with titles like "How to Bypass Claude 4.5 Sonnet Message Limit" and "Free AI Wrapper for OpenAI Key", no Reddit/HN discussion despite a price point that would go viral if legit, ToS conspicuously silent on how they source closed-model access without user keys.
- **Skip.** Wiring this into Athen would put users at risk of having their aizolo account banned (and possibly any associated upstream account if they used BYOK with the same email).

### 🟢 OpenCode Go ($5 first month, $10/mo ongoing) — wire as first-class provider

- Direct provider contracts with the listed open-source-model labs (DeepSeek, Alibaba/Qwen, Moonshot/Kimi, Zhipu/GLM, MiniMax). Not account-pooled — these are commercial-tier API contracts that OpenCode resells through a unified gateway. Backed by Anomaly (the Charm team's company).
- Dual-protocol like MiniMax Token Plan: most models on OpenAI Chat Completions (`https://opencode.ai/zen/go/v1/chat/completions`), MiniMax M2.5/M2.7 on Anthropic Messages (`https://opencode.ai/zen/go/v1/messages`). Standard `Authorization: Bearer sk-...` header on both.
- **Verified model lineup (2026-05-13):**
  - OpenAI protocol: `glm-5.1`, `glm-5`, `kimi-k2.6`, `kimi-k2.5`, `deepseek-v4-pro`, `deepseek-v4-flash`, `mimo-v2.5-pro`, `mimo-v2.5`, `qwen3.6-plus`, `qwen3.5-plus`
  - Anthropic protocol: `minimax-m2.7`, `minimax-m2.5`
- **Marketing copy is authoritative:** the landing page says "Yes, you can use Go with any agent. Follow the setup instructions in your preferred coding agent." That overrides the generic "no automated use" boilerplate elsewhere in ToS — same pattern as MiniMax Token Plan, same lesson from the Alibaba/Z.ai/Kimi correction sweep. When a provider explicitly markets an agent-shaped use case, the marketing copy is the contract.
- Per-model 5h rolling windows (sized per tier, current $10 numbers):
  - DeepSeek V4 Flash: **31.6K req/5h** — overkill for any single-user agent loop
  - Qwen 3.5 Plus: 10K req/5h
  - DeepSeek V4 Pro: 3.45K req/5h
  - Kimi K2.6: 1.15K req/5h (too tight for sustained loops — reserve for one-shot harder reasoning)
- For Athen's risk-scorer + executor + judge loop (≈3-5 req/turn), V4 Flash gives ~1.2K turns/hour of headroom. Use Flash as the default loop model, V4 Pro for higher-stakes single calls, Qwen 3.5 Plus as fallback.
- $10/mo flat-rate vs DeepSeek direct's ~$0.14/$0.28 per M tokens: break-even is roughly at moderate daily use. For Athen-shaped workloads (proactive agent firing on every email + calendar event + manual), flat-rate wins on predictability even if not on raw $/req.
- **At the $10 price point, this is the better pick than MiniMax Token Plan Starter** (1.5K req/5h, 1 concurrent — starves Athen's risk+exec+judge concurrency on the very first turn). MiniMax earns wiring at Plus ($20+) for its Anthropic-compat + prompt-cache angle, not at Starter for raw throughput. Different value props.
- **Wire shape:** add a new provider configuration option in Settings → LLM Providers that constructs `OpenAiCompatibleProvider::new("https://opencode.ai/zen/go/v1")` with the user's `sk-...` key, model picker exposing the bundled lineup. No bespoke `Provider` impl needed.

### 🔴 ZenMux Pro — legit but redundant with OpenRouter

- Enterprise aggregator, ~200 models via stated direct provider contracts. OpenAI-compatible (`https://zenmux.ai/v1`) and Anthropic-compatible variants.
- Subscription tiers $20/$100/$400/mo bundle insurance (latency, hallucination coverage) on top of pay-as-you-go. The insurance frame doesn't fit autonomous agent loops — Athen's risk system already gates execution; post-hoc latency/hallucination coverage is consumer-app shaped.
- ToS quiet on agent automation (no explicit ban, no explicit permission).
- **Skip.** OpenRouter is already wired as a Cloud APIs preset, runs ~500 models, has explicit agent-loop tolerance, and charges a ~5.5% markup that's negligible vs the switching cost. No net gain.

### 🟡 Atlas Cloud (atlascloud.ai) — legit but ToS-blocked

- Real funded company (founder Jerry Tang, ex-Natixis/Neon Chain; CTO ex-Google), SOC 2 Type I/II + HIPAA certified, OpenAI-compatible API at `https://api.atlascloud.ai/v1`.
- 300+ models across LLM/image/video — but on raw LLM pricing they're not competitive: $0.26/M for DeepSeek V3.2 vs Together AI's $0.14/M for the same model. The competitive angle is **multimodal breadth** (one API for video + image + LLM), not LLM margins.
- **ToS §7 verbatim:** *"engage in automated use"* and *"access… through automated or non-human means"* are both in the prohibited-activities list. §11 grants unilateral termination "at any time, without warning, in our sole discretion".
- **Skip for Athen.** The same niche (open-source LLM inference, OpenAI-compatible) is filled by Together AI and Fireworks, both of which explicitly permit agent use and are 2× cheaper on the headline DeepSeek price.
- Worth a second look only if Athen ever needs video generation, where Atlas Cloud's Seedance 2.0 at $0.022/sec is the cheapest in the market.

---

## 2026-05-13 follow-up: subscription→API bundles (Chinese labs + xAI/Mistral)

User asked the natural follow-up: "what about subscription products that subsidize programmatic use, like a flat $20/mo deal?" Four more parallel Haiku streams: xAI/Mistral free+paid limits, MiniMax bundling, the rest of the Chinese-lab market (Moonshot, Zhipu, Alibaba, Baichuan, 01.AI, StepFun, ByteDance, Tencent, Baidu, iFlytek, SenseTime), then a focused Z.ai + Kimi Coding Plan verification.

### Findings

| Subscription | Bundles API for agent use? |
|---|---|
| Mistral Le Chat Pro $14.99 | ❌ Chat-only, API separate |
| xAI SuperGrok $30 / Heavy $300 | ❌ Chat-only, ToS forbids programmatic use |
| xAI **X Premium+** $40 | 🟡 Single blog claim ("hidden benefit nobody talks about"), unverified on first-party x.ai/api |
| Perplexity Pro $20 | ❌ Used to include $5/mo Sonar credit, silently removed early 2026 |
| MiniMax Hailuo Pro / Agent Pro | ❌ Video/image credits and chat-only, no API bundling |
| **MiniMax Token Plan ($10–$50+/mo)** | ✅ **THE EXCEPTION.** Coding-plan structure (`sk-cp-` keys, dedicated `api.minimax.io/anthropic` subdomain with prompt-cache, supported-tools includes LangChain + Portkey + generic OpenAI clients). **No client whitelist, no UA enforcement, no agent-use ban in ToS** — ceiling is per-tier concurrency (1-4 concurrent agents) and request quotas (1,500-30,000 req/5h). Wireable into Athen as a regular OpenAI/Anthropic-compatible provider. |
| Moonshot Kimi / Zhipu GLM / ByteDance Doubao / Tencent Hunyuan / Baidu ERNIE / iFlytek / SenseTime / Baichuan / 01.AI / StepFun | ❌ All same shape — consumer chat tier separate from API billing |
| **Alibaba Cloud Coding Plan Pro ($50/mo)** | 🟡 Real product, real multi-vendor (Qwen+Kimi+GLM+MiniMax via OpenAI/Anthropic-compatible endpoints). Explicitly **supports agentic coding tools** (Claude Code, Cline, Cursor, Qwen Code, Codex, OpenClaw, Kilo Code…). $3 Lite tier discontinued 2026-03-20. **Catch:** client whitelist (see below) blocks unrecognised tools. |
| **Z.ai GLM Coding Plan (Lite $10 / Pro $30 / Max $80)** | 🟡 Z.ai's English-brand version of the same shape. 20+ supported clients incl. Claude Code, Cline, Cursor, Continue.dev. Models: GLM-5.1, GLM-5-Turbo, GLM-4.7. **ToS:** *"If the system detects usage through unauthorized or unsupported tools (such as SDK-based access or third-party integrations), some subscription benefits may be restricted."* — Athen *is* "SDK-based access". |
| **Kimi Coding Plan (Moderato $15 / Allegretto $31 / Allegro $79 / Vivace $159)** | 🔴 **Hard client whitelist** — only 5 approved tools: Claude Code, Roo Code, OpenCode, Kilo Code, Kimi CLI. Non-whitelisted clients rejected with "only available for Coding Agents". **User-Agent tampering explicitly prohibited and triggers account suspension.** |

### The pattern (revised — client whitelist, not agent ban)

Earlier draft of this doc claimed "every flat-rate bundle bans autonomous agent use". That read was wrong. The actual line:

**Flat-rate coding bundles explicitly *welcome* agentic tool loops** (Claude Code, Cline, Cursor are all multi-turn tool-call agents — they're the target market). **Most gate on client identity, not behavior.** Each gating provider maintains a list of recognised coding tools and enforces it via User-Agent / endpoint patterns. Unrecognised clients are either:
- **Hard-rejected** by the backend (Kimi, with explicit "no UA spoofing" clause)
- **Soft-degraded** ("benefits may be restricted", Z.ai)
- **Tolerated for now but ToS-restricted** (Alibaba "use only in coding tools like Claude Code")
- **Not gated at all** — MiniMax. Token Plan docs explicitly list LangChain and Portkey alongside named tools, and the FAQ specifies concurrency-cap-per-tier as the actual enforcement. The provider chose throughput-based ceiling over identity-based gatekeeping.

Why: the unit economics work because Claude Code / Cline / Cursor have natural session ceilings (a human is somewhere in the loop). What providers fear isn't agent loops — it's a third-party app pointing 24/7 traffic at a $30/mo bundle. Hence: name-based gatekeeping.

**For Athen, this means most of the coding plans aren't wirable as-is.** Athen identifies as "Athen", not as Claude Code. Spoofing User-Agent is suspension-triggering on Kimi and ToS-skirting elsewhere. The honest path for Kimi/Z.ai/Alibaba is either:
- Commercial outreach — ask each provider to add Athen to their supported-tools list. Z.ai has added many community tools; worth trying.
- Skip the bundles, use direct pay-as-you-go APIs.

**MiniMax Token Plan is the exception that proves the rule:** same coding-bundle structure, but the published docs (`/token-plan/other-tools`) explicitly list LangChain + Portkey + generic OpenAI clients alongside named tools (Cherry Studio, Codex CLI, Zed, Roo Code, Kilo Code, Qwen Code, OpenHands, nanobot, Open WebUI, Pi, Droid, OpenCode, Claude Desktop, Grok CLI, Xcode). No UA whitelist, no SDK-access prohibition. The enforcement mechanism is per-tier **concurrency limits (1-4 concurrent agents)** plus request quotas, which is identity-agnostic. This makes MiniMax Token Plan the only coding-plan-shaped subscription in the market that Athen can wire as a first-class provider without commercial outreach. Two protocols available — **OpenAI-compat** (`api.minimax.io/v1`) and **Anthropic-compat** (`api.minimax.io/anthropic`, with prompt-cache). Caveat: position language *"designed for individual, interactive developer use"* in the FAQ — concurrency cap is the real ceiling, not the words; treat the cap as authoritative.

This is also why Cursor Pro / Windsurf Pro / GitHub Copilot ToS are absolute (no agent loops) — they *are* the coding tool, not a backend the user brings their own tool to. Different shape from the Chinese labs' plans.

The only way to get "cheap inference for an agent loop" without being on someone's whitelist is **pay-as-you-go on the cheapest providers**, where unit economics balance per-request:

| Provider | $/M input | $/M output | Already wired? |
|---|---|---|---|
| DeepSeek V4 | $0.14 | $0.28 | ✅ |
| Mistral Nemo | $0.02 | $0.06 | ❌ (add via `openai_compat`) |
| Grok 3 Mini | $0.10 | $0.30 | ❌ (add via `openai_compat`, never default — data sharing surfaces user content) |
| Qwen3.5 (DashScope direct, `dashscope-intl.aliyuncs.com`) | ~$0.30 | ~$0.90 | partial (via OpenRouter); direct wire cuts markup |
| GLM-5 / GLM-4.7 (Z.ai direct, `api.z.ai`) | ~$0.30 | ~$0.90 | partial (via OpenRouter); direct wire cuts markup |
| Kimi K2.6 (Moonshot direct, `platform.moonshot.ai`) | $0.57 | $2.30 | partial (via OpenRouter); direct wire cuts markup |
| Groq (Llama 3.3 70B free tier 14.4K req/day) | $0.59 | $0.79 | ✅ |

OpenRouter already aggregates most Chinese providers with a small markup. Direct integration cuts the markup but adds N more provider configs. **No client whitelist on direct pay-as-you-go APIs** — that gating is exclusive to the flat-rate bundles. Athen can identify as itself.

## The rule going forward

When a user says "can Athen use my X subscription?":

1. If X has an **official paid API** (Perplexity, xAI, Mistral, OpenRouter, DeepSeek, Groq, Qwen via DashScope, GLM via Z.ai, Kimi via Moonshot, etc.) — use it. That's what `openai_compat` is for. No client gating on direct pay-as-you-go APIs.
2. If X has an **official OpenAI-compatible endpoint tied to subscription credits** (Poe today, maybe future others) — ship as a Cloud APIs preset with a "personal use only" disclaimer.
3. If X is a **flat-rate coding-tool bundle with NO client whitelist** — wire as a first-class provider. The real ceiling is per-tier concurrency or quota, not client identity. Two known examples today:
   - **MiniMax Token Plan** ($10-$50+/mo) — `sk-cp-` keys, OpenAI- and Anthropic-compatible endpoints, prompt-cache on the Anthropic side, concurrency cap 1-4 per tier.
   - **OpenCode Go** ($5 first mo, $10 ongoing) — OpenAI-compatible at `opencode.ai/zen/go/v1`, generous per-model 5h windows (31.6K req for DeepSeek V4 Flash). "Use Go with any agent" is the explicit on-site copy.
4. If X is a **flat-rate coding-tool bundle WITH a client whitelist** (Alibaba Coding Plan, Z.ai GLM Coding Plan, Kimi Coding Plan, Cursor Pro, Windsurf Pro, GitHub Copilot) — don't wire. Athen isn't on the supported-tool list; UA spoofing is suspension-triggering on Kimi and ToS-skirting elsewhere. Commercial outreach is the only honest path onto these lists, not engineering.
4. If X requires a **reverse-engineered local relay** the user must run themselves (Copilot proxies, Cursor proxies) — at most ship a preset pointing at `localhost`, never bundle the relay binary, never advertise it.
5. If X's ToS explicitly bans the path even for self-use (Cursor, Windsurf retail, ChatGPT/Claude.ai/Gemini, magai.co, Atlas Cloud) — say no.
6. **If X claims to give the user Claude + GPT + Gemini for under ~$15/mo combined, it's account-pooling.** No exceptions. The unit economics don't exist without ToS violation. Whether the user gets banned today or in three months when the provider's detection improves is the only variable. Examples: aizolo.com, sites named "AI hub", "ChatGPT free", "GPT-4 cheap", etc. Hard skip.

**Saved research (revised 2026-05-13 after Z.ai/Kimi re-check):** flat-rate coding bundles are NOT banned-by-ToS for agentic use — they explicitly welcome Claude Code / Cline / Cursor / OpenClaw / Kilo Code. What they gate on is **client identity, not behavior**. Athen is gated out by name. Re-evaluate only if (a) a provider drops the whitelist, or (b) Athen earns inclusion via commercial outreach.

The principle: **Athen ships the integration shape, the user owns the legal posture.** Athen never spoofs client identity to slip past a whitelist.
