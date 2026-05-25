# Connect Cloud APIs to Athen

Cloud APIs are external web services that Athen can call to do useful things — check the weather, convert currencies, translate text, search the web, find email addresses, and more. Once connected, the agent uses them automatically whenever a task calls for it. This guide walks through adding one or more of the 15 built-in presets.

## Prerequisites

- An account with the API provider you want to add (most have a free sign-up)
- An API key from that provider (some presets work without a key at all)

## Steps

### 1. Open the Cloud APIs panel

Go to **Settings** → **Connections** → **Cloud APIs** → click **Add Endpoint**.

### 2. Pick a preset

A dropdown lists 15 built-in presets. Select the one you want. The form fills in the server address, authentication method, and suggested settings automatically. Here is what each preset does, along with its free tier:

**Jina Reader** — converts any web page to clean text that the agent can read. Free: 10 million tokens one-time with a key, or 20 requests per minute without one. Sign up at [jina.ai/api-dashboard](https://jina.ai/api-dashboard/).

**Firecrawl** — scrapes and crawls websites, including JavaScript-heavy pages. Free: 1,000 credits per month. Sign up at [firecrawl.dev](https://www.firecrawl.dev/).

**Brave Search** — web search with news, images, and AI summaries. Free: approximately 1,000 queries per month on new accounts. Sign up at [api.search.brave.com/app/keys](https://api.search.brave.com/app/keys).

**SerpAPI** — search engine results from Google, Bing, YouTube, and 100+ other engines. Free: 100 searches per month (non-commercial use). Sign up at [serpapi.com](https://serpapi.com/users/sign_up).

**Hunter.io** — finds professional email addresses for a company or person. Free: 50 credits per month. Sign up at [hunter.io](https://hunter.io/api-keys).

**Apollo.io** — B2B contact and company search and enrichment. Free: 100 email credits per month. Sign up at [apollo.io](https://app.apollo.io/#/settings/integrations).

**People Data Labs** — person and company data enrichment. Free: 100 lookups per month. Sign up at [peopledatalabs.com](https://www.peopledatalabs.com/).

**DeepL** — professional-quality translation for 30+ languages. Free: 500,000 characters per month. Sign up at [deepl.com/pro-api](https://www.deepl.com/pro-api). Note: free keys must use the `api-free.deepl.com` endpoint (filled in automatically).

**NewsAPI** — current news headlines and articles from thousands of sources. Free: 100 requests per day (development use only). Sign up at [newsapi.org](https://newsapi.org/register).

**Open-Meteo** — weather forecasts (temperature, precipitation, wind, etc.) for any location. **No key needed** — free up to 10,000 requests per day. No sign-up required; just add the preset and it works immediately.

**Frankfurter (FX)** — daily currency exchange rates for about 30 currencies, sourced from the European Central Bank. **No key needed** — unlimited free use. Add the preset and it works immediately.

**OpenCage Geocoding** — converts addresses to coordinates and coordinates to addresses. Free: 2,500 requests per day. Sign up at [opencagedata.com](https://opencagedata.com/users/sign_up).

**ElevenLabs TTS** — converts text to speech in many languages and voices. Free: 10,000 characters per month (attribution required). Sign up at [elevenlabs.io](https://elevenlabs.io/app/settings/api-keys).

**OpenRouter (LLM fallback)** — access to many AI models through one endpoint, including several free models (DeepSeek R1, Llama 3.3 70B, Qwen3, and others). Sign up at [openrouter.ai/keys](https://openrouter.ai/keys).

**Groq (LLM + Whisper)** — very fast AI text generation and Whisper audio transcription. Free: 30 requests per minute, with cascading limits on tokens and audio seconds. Sign up at [console.groq.com/keys](https://console.groq.com/keys).

### 3. Get your API key

Click the **Sign up** link shown under the preset name (or use the URLs above) to create a free account. After signing up, find the API key in the provider's settings or dashboard. It is usually labelled "API key", "API token", or "Secret key".

For **Open-Meteo** and **Frankfurter**, skip this step — they need no key.

### 4. Enter your API key

Paste the key into the **API Key** field in Athen. The form shows the correct field name for each provider (some use a Bearer token, others a header like `X-Api-Key`, and a few use a URL query parameter — Athen handles the technical differences automatically).

### 5. Test the connection

Click **Test**. Athen makes a small test request to the provider and checks that the key is accepted. A green checkmark means everything is working.

### 6. Save

Click **Save**. The endpoint appears in your Cloud APIs list and is immediately available to the agent.

### How the agent uses these endpoints

When you give the agent a task that matches a connected API — for example, "translate this email to Spanish" or "what is the weather in Berlin this weekend?" — the agent calls the relevant endpoint automatically using the `http_request` tool. You do not need to tell it which service to use; it figures that out from the task.

The agent will ask for confirmation before calling APIs that are rated as medium or higher risk (which covers most contact and outreach endpoints such as Hunter.io and Apollo.io). Weather, currency, translation, and search endpoints are rated low risk and are used without confirmation.

## Common Issues

**"API key rejected" or 401 / 403 error**
You may have copied the key incorrectly (extra space, missing characters). Go back to the provider's dashboard, copy the key again, and update the entry. For DeepL, make sure you use the free-tier endpoint (`api-free.deepl.com`) with a free key — free keys end in `:fx` and do not work on the paid endpoint.

**"Too many requests" or 429 error**
You have hit the provider's rate limit. Free tiers are limited — SerpAPI allows 100 searches per month, NewsAPI 100 per day, and so on. Wait until the limit resets, or upgrade the plan if you need more capacity.

**Open-Meteo or Frankfurter not returning data**
These need no key, but you still need an internet connection. Check that Athen can reach the internet. If the problem persists, the service may be temporarily down — try again later.

**NewsAPI returns no results for my topic**
The developer (free) plan only searches news from the past month and does not allow production use. It also blocks some query types. For broader coverage, consider Brave Search or SerpAPI instead.

**The agent is not calling the API I added**
The agent decides when to use each endpoint based on what the task requires. If it is not using an endpoint you expected, try asking more explicitly — for example, "use DeepL to translate this" — or describe the task in a way that matches the service (for example, "find the email address for the CEO of Acme Corp" will prompt use of Hunter.io or Apollo.io).
