# Tools, Senses & Notifications

## 1. Agent Tools

### Built-in Tool Registry (`ShellToolRegistry`)

Defined in `crates/athen-agent/src/tools.rs`. Sixteen built-in tools:

| Tool | Risk | Backend |
|------|------|---------|
| `shell_execute` | `WritePersist` | nushell when available; otherwise `sh -c` (Unix) or `cmd /C` (Windows); sandboxed via bwrap (Linux) / Seatbelt (macOS) / Job Object + AppContainer (Windows) when available; PYTHONPATH/PATH/cwd injected through OS process API, not a shell-syntax wrapper |
| `shell_spawn` | `WritePersist` | detached spawn, log file capture |
| `shell_kill` | `WritePersist` | SIGTERM/SIGKILL on tracked PIDs only |
| `shell_logs` | `Read` | tail of spawn log file |
| `read` | `Read` | `tokio::fs`, `cat -n` style numbering, offset/limit |
| `edit` | `WritePersist` | exact-string replace, requires prior `read`, atomic write |
| `write` | `WritePersist` | full overwrite, atomic, prior `read` required for existing files |
| `grep` | `Read` | ripgrep wrapper |
| `list_directory` | `Read` | `tokio::fs` direct |
| `memory_store` | `Read` | in-session `HashMap<String,String>` |
| `memory_recall` | `Read` | same `HashMap` — key lookup or list-all |
| `web_search` | `Read` | `WebSearchProvider` (production default: `MultiSearchProvider` chaining Brave → Tavily → DDG with quota-aware cooldowns) |
| `web_fetch` | `Read` | `PageReader` (default: HybridReader, see below) |
| `install_package` | `WritePersist` | pip/npm install into `~/.athen/toolbox/`; gated by `ToolboxApprovalGate` |
| `uninstall_package` | `WritePersist` | reversible removal from toolbox (no approval needed) |
| `list_installed_packages` | `Read` | reads `~/.athen/toolbox/manifest.json` |

`shell_execute` passes the command through `RuleEngine` before dispatch; `Danger` or `Critical` risk score returns an error without executing.

### Persistent Toolbox

Defined in `crates/athen-agent/src/toolbox.rs`. The agent has a writable, persistent install location at `~/.athen/toolbox/` for pip and npm packages it decides it needs. These are NOT first-class Athen tools — they are CLI dependencies the agent installs and consumes through `shell_execute`. The manifest is the agent's *memory* of what it installed; the registry contract is unchanged.

**Layout** (paths via `athen_core::paths`; on Windows the data root is `%APPDATA%\Athen` instead of `~/.athen`):
- `<data>/toolbox/python/` — `pip install --target=…` destination, joined into `PYTHONPATH`.
- `<data>/toolbox/node/` — `npm install --prefix=…` destination; on Unix `node/bin/` is prepended to `PATH`, on Windows the prefix root itself is prepended (npm puts shims directly under the prefix on Windows, not under `bin/`).
- `<data>/toolbox/runtimes/{python,node}/` — portable runtimes installed by the onboarding wizard (see "Portable Runtimes" below). Their bin dirs are prepended to PATH at process startup.
- `<data>/toolbox/manifest.json` — atomic-written record of `{runtime, package, version_spec, installed_version, reason, installed_at, runtime_version}` per install.

**Shell env injection** (`tools.rs::build_shell_env`): every `shell_execute` command is dispatched through `Shell::execute_with(cmd, ShellOptions { env, cwd })`. `env` carries `PYTHONPATH=<toolbox/python>` joined with the existing value via `std::env::join_paths` (platform-correct separator: `:` on Unix, `;` on Windows) and the same join treatment for `PATH=<toolbox/node bin>`; `cwd` is the workspace dir. The shell adapters apply both via `Command::env()` / `current_dir()` instead of a bash-syntax `&& export …` prefix, so the same options work under nushell, cmd, sh, bash, zsh, and pwsh — the previous bash wrapper silently failed everywhere else.

**Approval flow:** `install_package` requires a `ToolboxApprovalGate`; `RouterToolboxApprovalGate` (`athen-app/src/file_gate.rs`) routes a structured `ApprovalQuestion` (`Install <pkg> (<runtime>)?` + LLM-supplied reason) through `ApprovalRouter`. The frontend listens for `approval-question` Tauri events and renders an inline dialog; Telegram is the escalation sink. Uninstall is unrestricted because it is reversible.

**Runtime probing:** `probe_runtimes` detects `python` / `pip` / `node` / `npm` once with a 5 s per-binary timeout, cached in a `Mutex<Option<RuntimeProbe>>`. The probe walks per-platform alias lists in priority order:
- Python: `["python3","python"]` on Unix; `["python","py","python3"]` on Windows (the official installer typically only exposes `python` and `py`).
- pip: `["pip3","pip"]` on Unix; `["pip","pip3"]` on Windows.
- npm: `["npm"]` on Unix; `["npm.cmd","npm"]` on Windows (the real binary is the `.cmd` shim).

On Windows each candidate is first resolved through `where.exe` and any hit under `\Microsoft\WindowsApps\` is **skipped** — that's where Windows registers the App Execution Alias for `python.exe`/`python3.exe` that opens the Microsoft Store on activation rather than running anything. Belt-and-suspenders: any `--version` output mentioning "Microsoft Store" or "was not found" is also rejected, in case the alias reaches stdout with a successful exit code. The wizard's runtime installer calls `invalidate_runtime_probe_cache()` after a fresh portable install so the new interpreter shows up on the next probe without restarting.

The probe result + manifest summary is injected into the system prompt so the model knows what's available and what's already installed without an extra tool call. The executor also injects a `SHELL ENVIRONMENT` slot (with the actual shell kind from `detect_shell_kind()` — `"nushell"`, `"sh"`, or `"cmd"`) listing per-shell don'ts and Windows-specific tips (`python` not `python3`, bind 127.0.0.1 for HTTP servers, etc.).

**Uninstall internals:** Python uninstall walks `dist-info/RECORD` (PEP 503-normalized name match), since `pip uninstall` does not support `--target`. Node uses `npm uninstall --prefix=<node_dir>`. Manifest is updated atomically after success.

**UI:** Settings → "Shell Toolbox" panel lists installed packages grouped by runtime (`list_toolbox_packages`/`clear_toolbox` Tauri commands); separate from the Tools panel because these are not registry tools.

### Portable Runtimes (Onboarding Wizard)

`crates/athen-agent/src/runtimes.rs`. When the host doesn't already have Python or Node, the onboarding wizard's "Runtimes" step offers a one-click portable install. Design rules:

- **Never bundle in the installer** (would add ~50 MB for users who already have Python).
- **Never copy the user's existing install** — breaks on Windows because of registry `PythonCore` keys for the `py` launcher, MSVC DLL linkage, and scattered files.
- **Detect-then-install on demand** into `<data>/toolbox/runtimes/{python,node}/`; prepend the resulting bin dirs to the process PATH so every other code path keeps working unchanged.

**Pinned versions** (single hardcoded source of truth, no external manifest file per the no-config-files rule):
- Python `3.12.7` from the python-build-standalone `20241016` release, `install_only` archive (uniform `tar.gz` layout across Unix and Windows, includes pip — avoids the Python embeddable's "no pip, fetch get-pip.py" bootstrap).
- Node `22.11.0` from `nodejs.org/dist`, `tar.gz` on Unix and `zip` on Windows.

**Verification:** SHA-256 fetched from the published sidecar (`.sha256` for python-build-standalone, `SHASUMS256.txt` for Node) and compared against the downloaded archive. This is a tripwire against accidental corruption — it trusts the same TLS origin as the download, so it does not protect against a compromised origin.

**Process integration:**
- `init_portable_path()` runs at app startup (`athen-app/src/lib.rs`) and again after every successful install. It prepends the portable bin dirs to the process `PATH` (Windows: `<runtimes>/python/` + `<runtimes>/python/Scripts/` + `<runtimes>/node/`; Unix: `<runtimes>/python/bin/` + `<runtimes>/node/bin/`). Idempotent on repeated calls.
- After install, `toolbox::invalidate_runtime_probe_cache()` is called so the next `probe_runtimes()` picks up the new interpreter.
- Concurrent install attempts of the same runtime are serialised through a single `Mutex` (`INSTALL_LOCK`) so a fast double-click in the wizard doesn't race two extracts into the same dir.

**Tauri surface:**
- `get_runtime_status` → `RuntimesStatus { system_python, system_node, portable_python, portable_node, python_pinned_version, node_pinned_version, python_supported, node_supported }`.
- `install_runtime { kind: "python" | "node" }` → streams `runtime-install-progress` events with `{ kind, progress: { phase: "resolving" | "downloading" | "verifying" | "extracting" | "done", downloaded?, total? } }`.

Skipping the wizard step is fully supported; Athen falls back to whatever the next probe finds at runtime, same as before this step existed.

### Web access (`web_search`, `web_fetch`)

Backed by the `athen-web` crate. Two ports, each with bundled no-key defaults plus optional key-gated upgrades.

**`WebSearchProvider`** — `crates/athen-web/src/search/`:
- `BraveSearch` (key-gated) — `api.search.brave.com/res/v1/web/search` with `X-Subscription-Token` header. Free tier: 2k req/month, no card. First-class SERP results from a fully independent index.
- `TavilySearch` (key-gated) — `api.tavily.com/search`. Free tier: ~1k req/month, no card. Better answer-ready snippets than raw SERPs.
- `DuckDuckGoSearch` (bundled, no key) — POSTs to `html.duckduckgo.com/html/`, parses the SERP with `scraper`, unwraps DDG's `/l/?uddg=` redirect links so the agent gets real URLs. Handles HTTP 202 rate-limits with a clear error. Always available as the chain's floor.
- `MultiSearchProvider` (production wrapper) — quota-aware fan-out. `athen-app::state::build_web_search_provider` walks the user's configured keys and stacks slots in order: Brave (`keyed`) → Tavily (`keyed`) → DDG (`floor`). On a rate-limit (HTTP 429 / "too many requests") the slot cools for 15min; on a quota / subscription error (HTTP 402, "quota exceeded", "subscription") it cools for 24h; other errors (network / JSON / 5xx) leave the slot armed. The `floor` slot never cools, so something always answers. Cooldowns are in-memory: a restart retries every provider once and rediscovers state from responses. The wrapper exposes `last_used()` to surface which underlying provider answered the most recent call — the `web_search` tool returns this as the `answered_by` field so users can audit per-call routing without log diving. Per-attempt decisions log at `info!` (`trying provider=...`, `provider answered hits=N`, `provider failed (no cooldown), trying next`, `skipping (in cooldown)`).

Keys are managed entirely through the UI (onboarding wizard's `search` step + Settings → Web Search). `save_web_search_settings(brave_api_key, tavily_api_key)` follows the same `None`/`Some("")`/`Some(value)` semantics as the LLM-key commands. Newly-saved keys take effect after a restart, since `MultiSearchProvider` is built once during `AppState::new()`.

**`PageReader`** — `crates/athen-web/src/reader/`:
- `LocalReader` (no key) — plain `reqwest` GET with `Accept: text/markdown` header (Cloudflare-opted-in sites return clean markdown for free), then `html2md::parse_html` on HTML responses with `<script>`/`<style>` stripped first. UTF-8-safe truncation at 40k chars.
- `JinaReader` (no key, free 500/min) — `r.jina.ai/<url>`, server-side JS rendering, returns markdown. Optional API key for higher quotas.
- `WaybackReader` (no key) — `web.archive.org/web/2id_/<url>`, raw archived snapshot, last-resort fallback for paywalled / blocked / dead pages.
- `CloudflareReader` (key-gated) — Browser Rendering REST API `/markdown` endpoint, $0.09/browser-hour. Built but not wired by default.
- `HybridReader` (default reader) — chains `Local → Jina → Wayback`. Smart fallback heuristic: hard floor at 150 chars, soft band 150–800 chars triggers fallback only if explicit JS-required markers ("Please enable JavaScript" etc.) are present. Result's `source` field tells the agent which tier produced the content.

The system prompt's `WEB ACCESS` section steers the model away from `curl`/`wget`/`lynx` for web content; both web tools are in `is_always_revealed` so their full schemas are inline every turn.

### Composition (`AppToolRegistry`)

`crates/athen-app/src/app_tools.rs:34` composes the production tool surface from optional adapters:

```rust
struct AppToolRegistry {
    inner: ShellToolRegistry,
    calendar: Option<CalendarStore>,
    contacts: Option<SqliteContactStore>,
    memory:   Option<Arc<Memory>>,
    mcp:      Option<Arc<dyn McpClient>>,
    file_gate: Option<Arc<FileGate>>,
}
```

Constructed eagerly with the first four; `mcp` and `file_gate` are attached via builders (`with_mcp`, `with_file_gate`) so the same struct works in tests without those subsystems.

**Tools added on top of the inner six:**
- 4 calendar tools when `calendar.is_some()` — `calendar_list/create/update/delete` (`Read`, `WritePersist`, `WritePersist`, `WritePersist`).
- 5 contacts tools when `contacts.is_some()` — `contacts_list/search/create/update/delete` (`Read`, `Read`, `WritePersist`, `WritePersist`, `WritePersist`).
- N MCP tools when `mcp.is_some()` — each tool returned by `McpClient::list_tools` is namespaced `<mcp_id>__<tool_name>` (e.g. `slack__post_message`) using `MCP_TOOL_SEPARATOR`.

**Description override:** when `memory.is_some()`, the inner `memory_store` / `memory_recall` descriptions are rewritten to point at persistent semantic memory rather than the in-session `HashMap` (`app_tools.rs:820-829`).

**`call_tool` dispatch order** (`app_tools.rs:930-1024`):
1. **`FileGate` interception** — if a `file_gate` is set and `FileGate::is_file_tool(name)` matches, the call is routed through `gate.handle()` which evaluates the path against `PathRiskEvaluator` + grants. The gate either runs the op directly (paths outside the sandbox), or hands back to a `dispatch_inside_sandbox` closure (paths the MCP can serve).
2. **MCP routing** — names containing `MCP_TOOL_SEPARATOR` are split and forwarded to `McpClient::call_tool(mcp_id, tool, args)`.
3. **Persistent memory override** — `memory_store` / `memory_recall` are intercepted to call `Memory::remember` / `Memory::recall` instead of the inner `HashMap`.
4. **Built-in match** — calendar and contacts tools dispatch to `do_calendar_*` / `do_contacts_*` async methods.
5. **Fallback** — anything else delegates to `inner.call_tool(name, args)` (the original `ShellToolRegistry`).

### Vision input (Phase 1)

The chat composer accepts images via paperclip button, drag-and-drop onto the input area, or Ctrl/Cmd-V paste. Each attached image becomes a chip with a thumbnail and a remove button. On submit, images are base64-encoded and forwarded to the `send_message` Tauri command as `images: Vec<ImageInput>`. The user message reaches the executor as `MessageContent::Multimodal { text, images }` for the first turn.

End-to-end behaviour:
- Provider with `supports_vision: true` (Claude 3.5+, GPT-4o, Gemini 1.5+): images are serialised into the provider-native content blocks and the model sees them.
- Provider with `supports_vision: false` (DeepSeek standard, plain Ollama/llama.cpp): the request is rejected up-front by `providers::reject_multimodal()` with a clear "this provider/model does not support image input" error — never silently dropped.

Phase 1 limits (deliberate cuts):
- Up to 5 images per turn, 10 MB each (composer-side limits).
- Images are not persisted — reopening the arc shows the text but not the picture. Phase 2 will add proper attachment storage.
- Approval-card path (`approve_task`) does not yet restash images; the direct in-app execution path is the supported flow.
- Sense-side images (e.g. an emailed picture) are not yet auto-forwarded into the agent's prompt.

### Two-Tier Tool Discovery

The executor (`crates/athen-agent/src/executor.rs`) keeps the system prompt small by separating tools into two tiers:

- **Tier 1 — capability index** (always in prompt): one line per group listing tool names and a one-liner description. Groups are derived from tool name prefixes: `memory`, `calendar`, `shell`, `files`, `contacts`, and any MCP id (`tool_grouping.rs:16-28`).
- **Tier 2 — revealed schemas** (inline in prompt): full description for each tool that has been called at least once this session ("tolerant dispatch") plus all `memory_*` tools which are always revealed (`tool_grouping.rs:101-105`).

When `tool_doc_dir` is set (defaults to `~/.athen/tools/`), the system prompt instructs the agent to call `read_file(path="<dir>/<group>.md")` for full schemas of any group it hasn't dispatched yet. `tools_doc.rs` generates and maintains these files: one `.md` per group, stale files removed when an MCP is disabled (`tools_doc.rs:54-88`).

### Batch Tool Calls

All tool calls in a single LLM response are dispatched concurrently via `futures::future::join_all` (`executor.rs:933`). Results are threaded back into the conversation in input order.

### Loop Protection

A `HashMap<sig, count>` tracks `"tool_name|args"` call signatures across the whole run (`executor.rs:509-514`). Any signature called more than `SIGNATURE_REPEAT_LIMIT = 3` times returns a hard error instead of dispatching (`executor.rs:878-912`). Duplicate calls *within the same batch* also return an error without dispatching (`executor.rs:913-928`).

### `<think>` Tag Parsing

`extract_think_tags()` (`executor.rs:32-58`) strips `<think>…</think>` blocks from model content (used by llama.cpp / Ollama). Extracted text is forwarded to the UI via the stream sender with a `\x02` prefix so the UI can render it separately as reasoning content.

### Completion Judge

After a text-only response (no tool calls) the executor calls a second cheap LLM call (`executor.rs:360-427`) to verify the task was actually completed. If the judge returns `CONTINUE`, the executor injects a nudge message and loops once more (`has_been_judged` flag prevents infinite judging).

### SSE Tool-Call Parsing

`try_streaming_call()` (`executor.rs:435-495`) collects both text deltas and tool calls from SSE chunks. If streaming yields no content and no tool calls, the executor falls back to a non-streaming call to recover tool call data (`executor.rs:678-714`).

### Tool Output Truncation

Centralised at `crates/athen-agent/src/tool_truncation.rs`, applied at the executor's tool-result serialisation point (`executor.rs:1691–1694`) — the audit trail keeps the **full** untruncated result, only the model-visible bytes are capped.

**Three policies:**

- `TruncationPolicy::None` — pass through unchanged. Used for tools whose output is bounded at source (`memory_recall`, `email_send` ack, `web_search` clamped to 20 results upstream).
- `TruncationPolicy::Chars { max }` — keep the first `max` bytes, append a marker. UTF-8 boundary-safe slicing.
- `TruncationPolicy::HeadTail { head, tail }` — keep prologue + epilogue, drop the middle. Best for shell output where the interesting bits cluster at start (command echo) and end (exit code, last error line).

Markers are explicit so the model knows it was cut and can re-query:

```text
[TRUNCATED: N bytes elided of M total. Refine your query...]
[TRUNCATED: N bytes elided in the middle of M total. Refine your query...]
```

**Per-tool limits** (`tool_truncation.rs::policy_for`, lines 37–65):

| Tool | Policy | Limit |
|---|---|---|
| `shell_execute` | HeadTail | 8 KB head + 4 KB tail |
| `shell_logs` | HeadTail | 4 KB head + 8 KB tail |
| `shell_spawn`, `shell_kill` | None | unbounded (small output) |
| `read` | Chars | 40 KB |
| `grep` | Chars | 20 KB |
| `list_directory` | Chars | 8 KB |
| `write`, `edit` | Chars | 2 KB |
| `web_fetch` | Chars | 20 KB |
| `web_search` | None | clamped to 20 results upstream |
| `memory_store`, `memory_recall` | None | bounded at source |
| `email_send` | None | small ack |
| `install_package`, `uninstall_package`, `list_installed_packages` | Chars | 8 KB |
| Unknown / MCP tools (fallback) | Chars | 20 KB |

Limits are **fixed**, not configurable via UI — keeping them off the settings surface is deliberate (they're plumbing, not user policy).

**Independent of compaction.** Tool truncation caps the LLM context per-turn at the result-serialisation point; compaction (`docs/ARC_COMPACTION.md`) summarises old conversation history. Different layers, no interference.

**Other truncation points** (separate from `tool_truncation.rs`, but worth knowing):

- **PDF inline budget** (`crates/athen-sentidos/src/pdf_extract.rs:35`) — `DEFAULT_INLINE_CHAR_BUDGET = 6000`. Beyond this, PDFs surface a "PDF text inlined (X of Y chars); call read_attachment_full(...) for the rest" hint and the agent fetches the rest on demand.
- **Web reader body cap** (`crates/athen-web/src/reader/local.rs:17–18`) — 5 MB body cap + 40 K char output cap with `[... truncated, original was longer than N chars ...]` marker.

**Test coverage**: 8 tests in `tool_truncation::tests` (`tool_truncation.rs:126–213`) cover passthrough, both truncation modes, UTF-8 boundary safety, known-tool dispatch, and the unknown-tool fallback.

---

## 2. MCP Servers

### Catalog and Registry

`crates/athen-mcp/` is split into two parts:

- **`catalog.rs`** — hardcoded list of branded MCPs the user can enable. Currently one entry: **Files** (`id: "files"`). Future entries can be downloadable (`McpSource::Download`).
- **`registry.rs`** — runtime state: `McpRegistry` holds an `enabled` map and a `clients` map of lazy-spawned child processes. Enabling an MCP eagerly spawns the child and runs the rmcp handshake so config errors surface immediately (`registry.rs:100-116`). Disabling drops the live client which kills the process.

### Enable / Disable via UI

Tauri commands in `crates/athen-app/src/commands.rs`:
- `list_mcp_catalog` — returns all catalog entries with their enabled state.
- `enable_mcp(mcp_id, config)` — calls `McpRegistry::enable`, persists to SQLite.
- `disable_mcp(mcp_id)` — calls `McpRegistry::disable`, persists to SQLite.

Enabled state survives restarts via SQLite; the registry is rebuilt on startup from persisted state.

### Directory Grant System

Defined in `crates/athen-app/src/file_gate.rs`. The `PathRiskEvaluator` classifies every path-touching tool call. The first time the agent touches a directory outside the default safe set (`/tmp`, `~/.athen/`, cwd), a `PendingGrantRequest` is created and the user is prompted via the UI.

Tauri commands:
- `list_pending_grants` — returns pending approval requests to the UI.
- `resolve_pending_grant(id, decision)` — `Allow`, `AllowAlways`, or `Deny`. `AllowAlways` writes a permanent grant for the current arc.
- `add_global_grant(path, access)` — adds a permanent grant not tied to an arc.
- `list_arc_grants`, `list_global_grants`, `revoke_arc_grant`, `revoke_global_grant`.

`ShellToolRegistry` accepts a `ShellExtraWritableProvider` (`tools.rs:31-34`); `AppToolRegistry` wires it to the `GrantStore` so arc grants are reflected in the sandbox writable set.

---

## 3. Senses (Monitors)

All monitors implement `SenseMonitor` from `athen-core`. `SenseRunner<M>` drives any monitor in a polling loop, forwarding `SenseEvent`s through an `mpsc::Sender` (`athen-sentidos/src/lib.rs:23-98`).

### Source priority (highest → lowest)

1. **UserInput** — `RiskLevel::Safe`. UI layer pushes strings to `UserInputMonitor::sender()`, drained as `EventKind::Command` events. No network, no latency.
2. **Calendar** — `RiskLevel::Safe`. Polls `~/.athen/athen.db` every 60 s via `tokio::task::spawn_blocking` to keep SQLite off the async runtime (`calendar.rs:324-358`). `query_upcoming_events` returns events whose `start_time` is within the maximum lead time of any reminder, plus a small look-ahead. `generate_reminder_events` (`calendar.rs:271-305`) emits one `EventKind::Reminder` per `(event_id, reminder_minutes)` tuple as the start time approaches that lead time, plus an extra "starting now" reminder when `0 ≤ minutes_until ≤ 1`. A per-monitor `Mutex<HashSet<(String, i64)>>` (`fired_reminders`) deduplicates within the session; cross-restart dedup is the persistence layer's responsibility (`fired_reminders` table in athen-persistence).
3. **Telegram** — `RiskLevel::Caution`. Polls `getUpdates` via long-polling HTTP. Tracks `last_update_id` to avoid reprocessing. Owner chat messages are elevated to `L1` trust at the coordinator layer; all other senders are triaged normally. (`telegram.rs:73-76`)
4. **Messaging** — iMessage/WhatsApp **stub** (`messaging.rs`). 30 s default poll interval; `poll()` always returns an empty vec. Wired into the runner like the others so swapping in a real implementation is a one-file change.
5. **Email** — `RiskLevel::Caution`. IMAP poll every 60 s (configurable). Tracks `last_seen_uid` to fetch only new unseen messages. Attachments parsed via `mailparse`. (`email.rs:27-55`)

Each monitor normalizes its input into `SenseEvent` (uuid, timestamp, `EventSource`, `EventKind`, `SenderInfo`, `NormalizedContent`, `source_risk`, `raw_id`) before the coordinator sees it.

---

## 4. Sandbox

`crates/athen-sandbox/` — `UnifiedSandbox` auto-detects capabilities via `SandboxDetector` and selects the best available backend.

### Tier mapping

| Level | When used | Backend selection |
|-------|-----------|-------------------|
| `SandboxLevel::None` | Read-only ops, filesystem tools | Direct `tokio::process` |
| `SandboxLevel::OsNative` | `shell_execute` (default) | bwrap (Linux, preferred) → landlock (Linux fallback) → `sandbox-exec` Seatbelt (macOS) → Job Object + AppContainer (Windows) |
| `SandboxLevel::Container` | High-risk / L3+ operations | Podman (preferred) → Docker fallback |

All three OS-native backends share the same `SandboxProfile` enum (`ReadOnly` / `RestrictedWrite { allowed_paths }` / `NoNetwork` / `Full`); each backend translates the profile into its native primitives:
- **bwrap (Linux)** — `--ro-bind /` + per-path `--bind`; `--unshare-net` for `NoNetwork`; `--unshare-all` + minimal binds for `Full`. (`crates/athen-sandbox/src/bwrap.rs`)
- **Seatbelt (macOS)** — Lisp profile with `(allow default)` + `(deny file-write*)` + per-path `(allow file-write* (subpath "..."))`; `(deny network*)` for `NoNetwork`/`Full`. Profile written to a tempfile and passed to `/usr/bin/sandbox-exec -f`. (`crates/athen-sandbox/src/macos.rs`)
- **Job Object + AppContainer (Windows)** — Two-tier: a Job Object caps memory/process count and ties the child tree to the parent via `KILL_ON_JOB_CLOSE`; an AppContainer with a per-execution unique SID provides FS isolation via ACL grants on `allowed_paths` and the resolved binary. The `internetClient` capability is included only for `ReadOnly`/`RestrictedWrite`; `NoNetwork`/`Full` ship empty capability list, so the AppContainer has no socket access. AppContainer profiles ship since Win8/Server 2012. APIs reached by dynamic-load from `userenv.dll` because the `windows` 0.59 crate doesn't expose them through any feature flag. (`crates/athen-sandbox/src/windows.rs`)

`shell_execute` uses `SandboxProfile::RestrictedWrite { allowed_paths }` with default writable set of `/tmp` + `~/.athen/` (or `%APPDATA%\Athen` on Windows) + `cwd` + any arc grant paths provided by `ShellExtraWritableProvider` (`tools.rs:205-228`). System paths are always excluded from the extra writable set (`paths::is_system_path`).

If the OS-native backend fails at runtime (e.g. bwrap namespace errors on restricted CI; AppContainer profile-creation failure inside an existing container), the executor falls back to unsandboxed shell rather than breaking the command (`tools.rs:235-247`). Risk evaluation (`RuleEngine`) still runs on every command on every platform, so dangerous commands remain blocked regardless of sandbox availability.

---

## 5. Notifications

### Channel hierarchy

`NotificationOrchestrator` in `crates/athen-app/src/notifier.rs` delivers through an ordered list of `NotificationChannel` implementations:

1. **`InAppChannel`** — emits a Tauri `"notification"` event to the frontend (instant, no external dependency).
2. **`TelegramChannel`** — sends via Bot API using `athen_sentidos::telegram::send_message`.
3. *(future)* OS notification, messaging channels.

Channel selection (`notifier.rs:541-557`):
- User present (window focused) → prefer `InApp`.
- User absent → skip `InApp`, use first external channel.
- If delivery fails → try next channel in list.

### Window focus tracking

`lib.rs:120-129` registers a `WindowEvent::Focused` listener on startup that calls `orchestrator.set_user_present(focused)`. This is the signal the orchestrator uses to route in-app vs. external.

### Escalation

For `High` or `Critical` notifications that `requires_response`, the orchestrator spawns a timer task (`escalation_timeout_secs`, default 300 s). If not seen before timeout, the next channel is tried. Cancellation via `CancellationToken` when `mark_seen` is called (`notifier.rs:649-724`).

### Quiet hours

Non-critical notifications during the configured quiet window are stored as `Pending` (not delivered). `flush_pending` delivers them when the window ends. Critical notifications bypass quiet hours unconditionally.

### Humanized text

Before delivery, `humanize()` calls a `ModelProfile::Cheap` LLM with a 30 s timeout to rephrase title+body into a short, casual natural-language sentence. On failure or timeout it falls through to the original text (`notifier.rs:468-517`).

### Persistence

When a `NotificationStore` (SQLite) is attached, every notification is persisted on delivery and read/unread status is written on `mark_read`/`mark_all_read`. `load_persisted()` is called at startup to restore state across restarts (`notifier.rs:386-418`). `list_notifications` always prefers the DB over in-memory.

### UI commands (Tauri)

`mark_notification_seen`, `list_notifications`, `mark_notification_read`, `mark_all_notifications_read`, `delete_notification`, `delete_read_notifications` — all in `commands.rs:1541-1622`.

Settings: `get_notification_settings`, `save_notification_settings` in `settings.rs:1061-1153`.

---

## 6. Contacts

### Storage and trust

`ContactStore` trait (`crates/athen-contacts/src/lib.rs:17-23`) + `TrustManager` (`trust.rs`). `InMemoryContactStore` for tests; production uses the SQLite-backed implementation in `athen-persistence`.

### Trust levels and risk multipliers

| Level | M_origen | Auto-evolves |
|-------|---------|--------------|
| `Unknown` (T0) | 5.0× | yes — new contacts start here |
| `Neutral` (T1) | 2.0× | yes |
| `Known` (T2) | 1.5× | yes — ceiling for auto-upgrade |
| `Trusted` | 1.0× | manual only |
| `AuthUser` | 0.5× | always the local user |

Auto-upgrade: every 5 user approvals of actions from that contact triggers a level bump, capped at T2 (`trust.rs:70-95`). Auto-downgrade: every 3 rejections drops one level (`trust.rs:107-130`). Both are suppressed if `trust_manual_override = true`.

Blocked contacts always receive the `Unknown` (5.0×) multiplier regardless of trust level (`trust.rs:59-64`).

### LLM-based matching

When a new message arrives from an external sender, the agent prompt instructs it to call `contacts_search` and ask the user before merging identifiers into an existing contact. Auto-merge without confirmation is explicitly prohibited (`executor.rs:301-311`).

### IPC commands (Tauri)

All in `crates/athen-app/src/contacts.rs`:
`list_contacts`, `get_contact`, `set_contact_trust`, `block_contact`, `unblock_contact`, `delete_contact`, `create_contact`, `update_contact`.

---

## Cross-Platform Shell

Primary: embedded Nushell shipped as a Tauri sidecar (`externalBin`), or `nu` on PATH if present. Same surface on all platforms when available.
Fallback: native shell — `sh -c` on Unix, `cmd /C` on Windows.

**Shell-agnostic env/cwd plumbing.** The trait extension `ShellExecutor::execute_with(cmd, ShellOptions { env, cwd })` carries environment variables and working directory as **structured options**, not as `cd … && export … && (cmd)` text wrapped around the user command. Each adapter (nushell, native) applies them via `tokio::process::Command::env()` / `current_dir()` so the same plumbing works under nushell, cmd, sh, bash, zsh, and pwsh — the previous bash-syntax wrapper silently failed under nushell on the first beta tester's Windows machine because the export-and-chain syntax isn't valid nushell. PATH composition uses `std::env::join_paths` so the platform-correct separator (`:` on Unix, `;` on Windows) is used and Windows path elements containing `:` (drive letters) survive unmangled.

**Shell-kind detection.** `athen_agent::detect_shell_kind()` returns `"nushell"`, `"sh"`, or `"cmd"` (cached, computed once per process). The `AgentBuilder::shell_kind(kind)` builder threads it into the executor; the `SHELL ENVIRONMENT` slot in the system prompt then tells the agent what's actually running so it doesn't generate bash-only constructs the active shell rejects.

**Sandboxing scope.** All three desktop platforms have a real OS-native sandbox backend now: bwrap on Linux, `sandbox-exec` (Seatbelt) on macOS, Job Object + AppContainer on Windows. See the Sandbox tier-mapping table above for what each profile translates to. Backends are auto-detected at startup by `SandboxDetector`; if none is available the executor falls through to direct `tokio::process::Command` execution but still applies `RuleEngine` risk gating on the command string.

**Windows path quirks.** `athen_core::paths::canonicalize_loose` strips the `\\?\` (and `\\?\UNC\`) verbatim prefix that `std::fs::canonicalize` adds on Windows. Without this, comparing a canonicalized path against a lexically-normalized one (e.g. for `path_within(target, athen_data_dir)` membership checks) returned false for paths inside Athen's own data dir, causing spurious file-grant approval prompts on Windows.
