# Troubleshooting: Athen Is Not Responding

If Athen stops answering, gives an error message, or seems to hang after you send a message, work through this checklist. Start at step 1 — most issues are resolved within the first three steps.

## Step 1: Is a provider configured?

Athen needs at least one AI provider (either a cloud service with an API key, or a local model running on your computer) to generate responses.

1. Open Settings → Models.
2. Check that at least one provider is listed and has the word "Active" or a checkmark indicating it is selected.
3. If no provider is configured at all, you need to add one. See the onboarding guide or the "pick-local-model" guide for instructions.

## Step 2: Test the connection — do this first

The built-in connection test is the fastest way to diagnose most problems.

1. Open Settings → Models.
2. Find the active provider (the one Athen is currently using).
3. Click **"Test Connection"**.

You will see either a success message or an error. Common error messages and what they mean:

| Error message | Likely cause |
|---|---|
| "Connection refused" or "Cannot connect" | The service is not reachable — either it is offline, or (for local models) not running |
| "401 Unauthorized" or "Invalid API key" | Your API key is wrong or has been revoked |
| "404 Not Found" | The model slug (name) is wrong — the provider doesn't recognize it |
| "429 Too Many Requests" | You have hit a usage limit or rate limit on your account |
| "500 Internal Server Error" | Something is wrong on the provider's side — usually temporary |
| Timeout / no response | Network issue, or the local server (Ollama / llama.cpp) is not running |

## Step 3: Is the API key valid?

For cloud providers (DeepSeek, OpenAI, Anthropic, Google, Mistral, etc.):

1. Open Settings → Models → expand the provider.
2. Check that the API key field is filled in and does not start with `${` (which means it was not resolved).
3. If you recently reset or rotated your API key, update it here.
4. You can verify the key is valid by visiting the provider's dashboard (links below).

If the key looks correct but Test Connection returns "Unauthorized," the key may have been revoked. Generate a new one from the provider's dashboard and paste it into Athen.

## Step 4: Is there a network issue?

If Test Connection times out or reports a network error:

1. Try opening the provider's website in your browser. If that also fails, your internet connection itself may be down.
2. If you are on a corporate or university network, your firewall may be blocking connections to the provider's API. Try a different network (e.g. switch to mobile data temporarily) to confirm.
3. For local models (Ollama / llama.cpp): the service runs on your own computer, so network issues don't apply. Instead, check that the local server is running (see step 7).

## Step 5: Is the model slug correct?

The model slug is the exact name sent to the provider's API. If it is wrong, the provider returns a "404 Not Found" error.

1. Open Settings → Models → expand the provider.
2. Check the **Model** (or "Model Slug") field.
3. Compare it against the provider's current model list. Providers occasionally rename or retire models.

Common mistakes:
- Using an old model name that has been deprecated (e.g. `deepseek-chat` instead of `deepseek-v4-flash`)
- A typo in the model name
- Using a model that requires a different tier of your subscription

If you are not sure what to put, click the Model Family dropdown and select a family — Athen will pre-fill the default slug for that family.

## Step 6: Is the Model Family set correctly?

The Model Family tells Athen how to interpret the AI's responses. This does not affect connectivity, but a wrong family causes the agent to misread tool calls and produce garbled or empty answers.

Signs of a wrong Model Family:
- Athen's responses are empty or show raw markup like `<tool_call>...</tool_call>`
- Athen says it cannot perform tool use even though it should be able to
- The "Thinking..." indicator appears but no answer follows

To fix:
1. Settings → Models → expand the provider.
2. Set the **Model Family** dropdown to match the model you are using. Refer to the "pick-local-model" guide for a model-to-family table, or the seed table in the per-model quirks documentation.

## Step 7: Is your quota exhausted?

Cloud providers have usage limits. When you exceed them, you will see a "429 Too Many Requests" or "quota exceeded" error.

Check your usage on the provider's dashboard:

| Provider | Dashboard / usage page |
|---|---|
| DeepSeek | [https://platform.deepseek.com/usage](https://platform.deepseek.com/usage) |
| OpenAI | [https://platform.openai.com/usage](https://platform.openai.com/usage) |
| Anthropic | [https://console.anthropic.com/settings/limits](https://console.anthropic.com/settings/limits) |
| Google (Gemini) | [https://aistudio.google.com/](https://aistudio.google.com/) |
| Mistral | [https://console.mistral.ai/](https://console.mistral.ai/) |

If you are on a free tier, you may have hit a per-day or per-minute rate limit. Wait a few minutes and try again, or upgrade your plan.

## Step 8: Is the provider down?

Providers occasionally have outages. Check their status pages:

| Provider | Status page |
|---|---|
| DeepSeek | [https://status.deepseek.com](https://status.deepseek.com) |
| OpenAI | [https://status.openai.com](https://status.openai.com) |
| Anthropic | [https://status.anthropic.com](https://status.anthropic.com) |
| Google (Gemini) | [https://status.cloud.google.com](https://status.cloud.google.com) |
| Mistral | [https://mistral.ai](https://mistral.ai) |

If the provider is down, there is nothing to do except wait. You can switch to a different provider in Settings → Models while you wait.

## Step 9: Local model — is the server running?

If you are using Ollama or llama.cpp and the connection test fails:

**Ollama:**
- Open [http://localhost:11434](http://localhost:11434) in your browser. If it does not load, Ollama is not running.
- On Linux: open a terminal and run `ollama serve`.
- On macOS / Windows: look for the Ollama icon in the system tray and make sure it shows as running.

**llama.cpp:**
- The server must be started manually each time. Open a terminal and run your `llama-server` command (with the `--port 8080` flag).
- Check that [http://localhost:8080](http://localhost:8080) responds in your browser.

Also make sure you have actually downloaded the model you are trying to use. For Ollama, run `ollama list` in a terminal to see downloaded models. If your model is not listed, run `ollama pull <model-name>` to download it.

## Still not working?

If none of the above steps resolve the issue, try:

1. Restarting Athen.
2. Switching to a different provider temporarily to confirm the problem is provider-specific.
3. Checking the Athen logs — on Linux these appear in the terminal if you launched Athen from a terminal. Look for lines containing "error" or "failed" near the timestamp when the problem occurred.
