# Tools, Senses & Notifications

## 1. Agent Tools

### Built-in Tool Registry (`ShellToolRegistry`)

Defined in `crates/athen-agent/src/tools.rs`. Thirteen built-in tools:

| Tool | Risk | Backend |
|------|------|---------|
| `shell_execute` | `WritePersist` | `sh -c` (sandboxed when bwrap available) |
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
| `web_search` | `Read` | `WebSearchProvider` (default: DuckDuckGo HTML scrape) |
| `web_fetch` | `Read` | `PageReader` (default: HybridReader, see below) |

`shell_execute` passes the command through `RuleEngine` before dispatch; `Danger` or `Critical` risk score returns an error without executing.

### Web access (`web_search`, `web_fetch`)

Backed by the `athen-web` crate. Two ports, each with bundled no-key defaults plus optional key-gated upgrades.

**`WebSearchProvider`** — `crates/athen-web/src/search/`:
- `DuckDuckGoSearch` (bundled default) — POSTs to `html.duckduckgo.com/html/`, parses the SERP with `scraper`, unwraps DDG's `/l/?uddg=` redirect links so the agent gets real URLs. Handles HTTP 202 rate-limits with a clear error.
- `TavilySearch` (key-gated) — `api.tavily.com/search`. Free tier: ~1k req/month, no card. Better answer-ready snippets than raw SERPs.

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
- N MCP tools when `mcp.is_some()` — each tool returned by `McpClient::list_tools` is namespaced `<mcp_id>__<tool_name>` (e.g. `files__read_file`) using `MCP_TOOL_SEPARATOR`.

**Description override:** when `memory.is_some()`, the inner `memory_store` / `memory_recall` descriptions are rewritten to point at persistent semantic memory rather than the in-session `HashMap` (`app_tools.rs:820-829`).

**`call_tool` dispatch order** (`app_tools.rs:930-1024`):
1. **`FileGate` interception** — if a `file_gate` is set and `FileGate::is_file_tool(name)` matches, the call is routed through `gate.handle()` which evaluates the path against `PathRiskEvaluator` + grants. The gate either runs the op directly (paths outside the sandbox), or hands back to a `dispatch_inside_sandbox` closure (paths the MCP can serve).
2. **MCP routing** — names containing `MCP_TOOL_SEPARATOR` are split and forwarded to `McpClient::call_tool(mcp_id, tool, args)`.
3. **Persistent memory override** — `memory_store` / `memory_recall` are intercepted to call `Memory::remember` / `Memory::recall` instead of the inner `HashMap`.
4. **Built-in match** — calendar and contacts tools dispatch to `do_calendar_*` / `do_contacts_*` async methods.
5. **Fallback** — anything else delegates to `inner.call_tool(name, args)` (the original `ShellToolRegistry`).

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

### Filesystem MCP (`mcp-filesystem`)

`crates/mcps/mcp-filesystem/` — standalone rmcp server binary speaking JSON-RPC over stdio. Takes a single `SANDBOX_ROOT` argument. All paths are validated: absolute paths rejected, `..` traversal blocked (`lib.rs:72-100`). Tools exposed: `read_file`, `write_file`, `list_dir`, `move_file`, `create_dir`, `delete_file`, `delete_dir`.

Default sandbox root: `~/.athen/files` (auto-created on first enable, `registry.rs:186-196`).

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
| `SandboxLevel::OsNative` | `shell_execute` (default) | bwrap (Linux, preferred) → landlock (Linux fallback) → sandbox-exec (macOS) → Job Objects (Windows) |
| `SandboxLevel::Container` | High-risk / L3+ operations | Podman (preferred) → Docker fallback |

`shell_execute` uses `SandboxProfile::RestrictedWrite { allowed_paths }` with default writable set of `/tmp` + `~/.athen/` + `cwd` + any arc grant paths provided by `ShellExtraWritableProvider` (`tools.rs:205-228`). System paths are always excluded from the extra writable set (`paths::is_system_path`).

If bwrap fails at runtime (namespace creation errors on restricted CI), the executor falls back to unsandboxed shell rather than breaking the command (`tools.rs:235-247`).

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

Primary: embedded Nushell (`athen-shell`) — same commands on all platforms.
Fallback: native shell (bash/zsh/pwsh) for platform-specific tools when Nushell is unavailable.
`shell_execute` always goes through `Shell::execute` which handles the backend selection transparently.
