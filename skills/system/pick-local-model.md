# Running a Local AI Model with Athen

Athen can use AI models that run entirely on your own computer — no internet required, no monthly subscription, and nothing leaves your machine. This guide explains when that makes sense, what hardware you need, and how to get it working.

## Why run a local model?

- **Privacy**: Your messages, files, and tasks never leave your device. Useful if you're handling sensitive business data, personal documents, or anything you'd rather not send to a cloud service.
- **Offline use**: Works without an internet connection once the model is downloaded.
- **No cost**: No API bills, no usage caps after the model is downloaded.
- **Trade-off**: Local models are generally slower and less capable than the best cloud models. For complex multi-step tasks, a cloud model will usually do better.

## Hardware requirements

The most important factor is RAM (system memory) or VRAM (graphics card memory). The model must fit entirely in memory to run at a usable speed.

| Available RAM / VRAM | Models that fit | Examples |
|---|---|---|
| 8 GB RAM | 1B – 3B parameter models | Llama 3.2-1B, Qwen 2.5-3B |
| 16 GB RAM | 7B – 8B parameter models | Llama 3.1-8B, Qwen 3.5-7B, Gemma 3-9B |
| 24 GB VRAM (GPU) | Up to 14B parameter models | Qwen 2.5-14B |
| 32 GB+ VRAM (GPU) | 32B+ parameter models | Qwen 2.5-32B, DeepSeek-V3-0324 |

A GPU is not required — these models can run on CPU-only machines. However, CPU-only inference is significantly slower (expect 5–20 seconds per response instead of under 5 seconds).

If you are unsure how much RAM your computer has: on Windows, open Task Manager → Performance → Memory. On macOS, open Activity Monitor → Memory tab. On Linux, run `free -h` in a terminal.

## Option 1: Install Ollama (recommended for most users)

Ollama is the simplest way to run local models. It handles downloading models, managing memory, and exposes a local server that Athen connects to automatically.

**Install Ollama:**

- **macOS or Linux**: Open a terminal and run:
  ```
  curl -fsSL https://ollama.com/install.sh | sh
  ```
- **Windows**: Download the installer from [https://ollama.com/download](https://ollama.com/download) and run it.

After installing, Ollama runs in the background automatically. You can verify it is running by opening [http://localhost:11434](http://localhost:11434) in your browser — you should see a short text response.

**Download a model:**

Open a terminal and run one of these commands. Pick the size that fits your RAM (see the table above):

```
ollama pull qwen3.5:7b        # Recommended — best tool use, 16 GB RAM
ollama pull qwen3.5:4b        # Smaller option, 8 GB RAM
ollama pull llama3.1:8b       # Alternative, 16 GB RAM
ollama pull gemma3:9b         # Google's model, 16 GB RAM
```

The download may take several minutes depending on your internet connection. The 7B model is about 4–5 GB.

**Connect Athen to Ollama:**

1. Open Athen → Settings → Models.
2. Click "+ Add Provider" and select "Ollama".
3. The URL defaults to `http://localhost:11434` — leave it as-is.
4. In the "Model" field, type the model name you pulled (e.g. `qwen3.5:7b`).
5. Set the **Model Family** dropdown to match your model (see the table below).
6. Click "Test Connection" to confirm Athen can reach Ollama.
7. Click "Set Active" to make this provider active.

## Option 2: Install llama.cpp (advanced users)

llama.cpp gives you more control — useful if you want to run quantized GGUF files, tune inference parameters, or use a GPU more efficiently.

1. Download a pre-built release from [https://github.com/ggml-org/llama.cpp/releases](https://github.com/ggml-org/llama.cpp/releases) and unzip it.
2. Download a GGUF model file from Hugging Face (search for "GGUF" versions of the model you want).
3. Start the server:
   ```
   ./llama-server --model /path/to/model.gguf --port 8080 --ctx-size 8192
   ```
4. In Athen → Settings → Models, add a "llama.cpp" provider. The default URL is `http://localhost:8080`. Leave the model name as `default`.
5. Set the **Model Family** to match your downloaded model.

## Choosing a model

**Recommended starting point: Qwen 3.5-7B via Ollama**

Qwen 3.5 models have excellent support for tool use (the ability to search the web, read files, manage your calendar, and so on). Most other local models have weaker tool use and may not be able to complete complex tasks reliably.

If you have only 8 GB of RAM, try Qwen 3.5-4B or Qwen 2.5-3B. Tool use will be less reliable on the smaller sizes, but still functional for simple tasks.

## Setting the Model Family — important

This is the most commonly missed step. The **Model Family** tells Athen how the model formats its responses. Getting it wrong causes garbled output or tool use that silently fails.

In Settings → Models → expand your provider → find the **Model Family** dropdown:

| Model you downloaded | Select this family |
|---|---|
| qwen3.5:* | Qwen 3.5 (local) |
| qwen3.6:* | Qwen 3.6 (local) |
| gemma4:* / gemma-4:* | Gemma 4 (local) |
| llama3.2:* | Llama 3.2 (Vision / 70B class) |
| llama3.3:* | Llama 3.3 70B instruct |
| llama3.1:* | Llama 3.3 70B instruct |
| mistral:* | Mistral Large 3 |
| Any other / unsure | Default (safe fallback) |

## Common problems

**The model is too slow or the app freezes**

The model is probably too large for your available memory and is reading from your hard drive instead of RAM. Try a smaller model (e.g. switch from 7B to 4B or 3B). Check RAM usage in your system monitor while Athen is running.

**"Athen can't connect to Ollama" or Test Connection fails**

Make sure Ollama is running. On macOS and Windows it should start automatically. On Linux, run `ollama serve` in a terminal. Check that [http://localhost:11434](http://localhost:11434) loads in your browser.

**"The model isn't available"**

You need to pull the model first. Open a terminal and run `ollama pull <model-name>` with the exact name you typed in Athen's Model field (e.g. `ollama pull qwen3.5:7b`).

**Tool use doesn't work — Athen can't search or read files**

Make sure the Model Family is set correctly (see the table above). If you are using a Qwen model, select "Qwen 3.5 (local)" — the Default family does not know how to parse Qwen's tool-call format.

**I set the wrong family and got garbled responses**

Go back to Settings → Models, expand the provider, and fix the Model Family dropdown. Changing it takes effect immediately on the next message.
