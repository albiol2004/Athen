# Implementation Status

**Total: ~803 tests**, 0 clippy warnings.

---

### athen-core (20 source files, 12 tests)
**Status**: Complete -- all types, trait contracts, and config loading.
- `error.rs`: `AthenError` enum (Io, Serialization, TaskNotFound, ToolNotFound, LlmProvider, RiskThresholdExceeded, Timeout, Sandbox, Ipc, Config, Other) + `Result<T>` alias
- `event.rs`: `SenseEvent`, `EventSource`, `EventKind`, `SenderInfo`, `NormalizedContent`, `Attachment`
- `task.rs`: `Task`, `TaskStep`, `TaskPriority` (Background..Critical), `TaskStatus` (7 states), `StepStatus`, `DomainType`
- `risk.rs`: `RiskScore` with `decision()` method, `RiskLevel`, `RiskDecision`, `RiskContext`, `BaseImpact`, `DataSensitivity`
- `contact.rs`: `Contact`, `TrustLevel` (T0..T4) with `risk_multiplier()`, `ContactIdentifier`, `IdentifierKind`
- `llm.rs`: `LlmRequest`, `LlmResponse`, `ChatMessage`, `Role`, `MessageContent`, `ToolCall`, `TokenUsage`, `FinishReason`, `LlmChunk` (includes `is_thinking: bool`, `tool_calls: Vec<ToolCall>`), `BudgetStatus`, `LlmStream`, `ModelProfile`. `LlmResponse` includes `reasoning_content: Option<String>`.
- `tool.rs`: `ToolDefinition`, `ToolBackend` (NativeMcp/Shell/Script/HttpApi), `ToolResult`, `AuthConfig`, `ScriptRuntime`, `HttpMethod`
- `sandbox.rs`: `SandboxLevel` (None/OsNative/Container), `SandboxProfile`, `SandboxCapabilities`, `Mount`
- `ipc.rs`: `IpcMessage`, `IpcPayload` (14 variants), `ProcessId`, `ProcessType`, `ProcessTarget`, `TaskProgressReport`, `TaskControlCommand`, `ControlAction`, `ProcessRegistration`, `ProcessHealthStatus`, `ApprovalRequest`, `ApprovalResponse`
- `notification.rs`: `Notification`, `NotificationUrgency` (Low/Medium/High/Critical), `NotificationOrigin` (RiskSystem/SenseRouter/Agent/System), `DeliveryResult`, `DeliveryStatus`
- `config.rs`: `AthenConfig`, `OperationMode`, `OperationConfig`, `ModelsConfig`, `ProviderConfig`, `AuthType`, `ProfileConfig`, `DomainConfig`, `SecurityConfig`, `SecurityMode`, `PersistenceConfig`, `EmailConfig` (enabled, imap_server, imap_port, username, password, use_tls, folders, poll_interval_secs, lookback_hours -- defaults: disabled, port 993, TLS on, INBOX, 60s poll, 24h lookback), `TelegramConfig` (enabled, bot_token, owner_user_id: Option<i64>, allowed_chat_ids: Vec<i64>, poll_interval_secs -- defaults: disabled, 5s poll), `NotificationConfig` (preferred_channels, escalation_timeout_secs, quiet_hours), `QuietHours` (start_hour/minute, end_hour/minute, allow_critical), `NotificationChannelKind` (InApp, Telegram), `EmbeddingConfig` (mode, provider, model, base_url, api_key), `EmbeddingMode` (Automatic, Cloud, LocalOnly, Specific, Off). Both `NotificationConfig` and `EmbeddingConfig` added to `AthenConfig` with `#[serde(default)]`.
- `config_loader.rs`: `load_config(path)`, `load_config_dir(dir)`, `save_default_config(path)`. Loads TOML files with serde defaults for missing fields. Supports split config: `config.toml` (main) + optional `models.toml` override.
- `traits/`: 11 trait files defining all inter-module contracts
  - `traits/notification.rs`: `NotificationChannel` trait with `channel_kind()` and `send()`
  - `traits/embedding.rs`: `EmbeddingProvider` trait with `provider_id()`, `dimensions()`, `embed()`, `embed_batch()`, `is_available()`
  - `traits/memory.rs`: `VectorIndex` trait: added `list_all()` default method. `KnowledgeGraph` trait: added `list_entities()`, `list_relations()`, `update_entity()`, `delete_entity()`, `delete_relation()` default methods. `EntityExtractor` trait with `extract()` method + `ExtractionResult` struct (entities: Vec<Entity>, relations: Vec<(String, String, String)>)
  - `traits/profile.rs`: `ProfileStore` trait — `list_profiles`, `load_profile(id)`, `save_profile`, `delete_profile`, `list_templates`, `resolve_templates(ids)`. Implemented in `athen-persistence`.
- `agent_profile.rs`: AgentProfile system. `AgentProfile` (id, display_name, description, persona_template_ids, custom_persona_addendum, tool_selection, expertise, model_profile_hint, builtin, timestamps; `DEFAULT_ID = "default"`). `PersonaTemplate` (id, display_name, body, builtin) categorized by `PersonaCategory` (Voice/Mission/Constraints/OutputStyle). `ToolSelection` (All/Groups/Explicit/Deny — defaults to All). `ExpertiseDeclaration` (domains, task_kinds, languages, strengths, avoid). `DomainTag` closed enum (Email, Calendar, Messaging, Coding, Research, Outreach, Marketing, Finance, Scheduling, DataAnalysis, Writing, Translation, Health, Legal, Infrastructure, Architecture, Support, SocialMedia, Other) — additive-only. `TaskKindTag` closed enum (Drafting, Editing, Summarizing, Researching, Scheduling, CodeReview, Coding, Debugging, DataAnalysis, Outreach, Triage, Other). `ResolvedAgentProfile { profile, persona_templates }` is what the executor receives — `has_custom_persona()` returns false for the seeded default so today's hardcoded persona is preserved.
- `profile_routing.rs`: Coordinator scoring helpers. `ClassifiedTask { domain, task_kind, language, raw_text }`. `classify_task(source, lower_text, language)` infers domain from `EventSource` first, falls back to `infer_domain_from_keywords(text)` when source is None or `Other` (keyword groups for Infrastructure/SocialMedia/Legal/Health/Architecture/Support/Outreach/Marketing/Coding/Research). `pick_profile(classified, profiles)` keyword-only scoring (domain match, task_kind match, anti-match penalty, builtin tiebreaker). `pick_profile_blended(classified, profiles, query_embedding, profile_embeddings)` adds semantic blend on top: `kw + sem * SEMANTIC_WEIGHT (4.0)`, filter `*kw > 0 || *sem > 0.55` so a weak embedding-only match cannot win over a clean keyword miss. `profile_embedding_text(p)` concatenates display_name + description + strengths + addendum for embedding. `cosine_similarity(a, b)` returns 0.0 on length mismatch / empty / zero norm. Backwards compatible: `None` query embedding falls through to keyword-only.

### athen-ipc (13 unit + 5 integration = 18 tests)
**Status**: Complete -- full IPC transport layer.
- `transport.rs`: `IpcTransport` trait + `UnixTransport` implementation using split `UnixStream` halves with independent `Mutex`es for concurrent send/recv. Length-prefixed framing (4-byte big-endian). 16 MiB message size limit.
- `codec.rs`: `encode()` serializes `IpcMessage` to length-prefixed JSON bytes. `decode()` deserializes. `read_length_prefix()` extracts u32 from 4 bytes.
- `server.rs`: `IpcServer` binds `UnixListener`, accepts connections, spawns per-connection reader tasks, identifies processes by first message's `source` field. Methods: `send_to()`, `broadcast()`, `broadcast_to_type()`, `route()`, `connected_count()`, `shutdown()`. `IpcClient` connects to coordinator, auto-sends Registration message.

### athen-risk (62 unit + 6 integration = 68 tests)
**Status**: Complete -- full risk evaluation engine with natural language intent detection and AuthUser bypass.
- `scorer.rs`: `RiskScorer` implementing the formula `(Ibase * Morigen * Mdatos) + Pincertidumbre`. Confidence penalty: `(1.0 - confidence)^2 * 100`. Maps total to RiskLevel and RiskDecision. Implements `RiskEvaluator` trait.
- `rules.rs`: `RuleEngine` with compiled regex patterns using `LazyLock`. Returns `RuleMatch` with `base_impact`, `data_sensitivity`, `matched_patterns`, and `intent_based` flag. Detects:
  - **Dangerous shell commands**: rm -rf, sudo, dd, mkfs, chmod 777, redirect to /dev/, pipe to sh/bash/zsh
  - **Natural language destructive intent** (`DESTRUCTIVE_INTENT` patterns): delete/remove/erase/wipe/destroy/nuke + file/folder/dir/everything/all (both orderings), format/reset/clear/empty/purge + disk/drive/partition/database/system, kill/terminate + all/every + process/service, modify/change/edit/overwrite + system/config/password/credentials, send/post/upload + data/file/secret/key/token. Intent-based matches set `intent_based = true` and receive `confidence = 0.6` (adds 16-point uncertainty penalty), pushing scores into HumanConfirm range.
  - **Secrets**: OpenAI keys (sk-...), AWS keys (AKIA...), private key headers, passwords in URLs
  - **PII**: email addresses, phone numbers
  - **Financial keywords**: payment, transfer, purchase, buy, invoice, billing, credit card
  - **External URLs**: http/https URLs
  Returns `Option<RiskScore>` -- `Some` if confident, `None` for LLM fallback.
- `llm_fallback.rs`: `LlmRiskEvaluator` takes `Box<dyn LlmRouter>`. 30-second timeout on LLM calls. Constructs structured prompt that considers what the agent would ACTUALLY DO (not just literal text) -- asks LLM to classify impact as system for any delete/remove/wipe/destroy actions. Conservative fallback on failure/timeout: `WritePersist + PersonalInfo + 0.3 confidence` (lands in HumanConfirm range, score ~89).
- `path_eval.rs`: `PathRiskEvaluator<G: GrantLookup>` -- deterministic per-path risk classification without LLM calls. Evaluates path-touching operations (Read/Write) with a fixed hierarchy: allowed system paths (read-only) < user home (read-allowed, write-risk) < granted directories (allow/deny based on arc grant) < sensitive system dirs (always deny write). Falls back to `RiskScorer` for final numeric score landing in the correct decision band. Wired by `FileGate` in athen-app for agent file access control.
- `lib.rs`: `CombinedRiskEvaluator` implementing `RiskEvaluator` -- tries rules first, falls back to LLM if rules return `None`. **AuthUser bypass**: when rules return `None` (no dangerous patterns) and trust is `AuthUser`, returns a safe score (0.5) directly -- skips LLM risk fallback entirely for benign user input, reducing latency.

### athen-sandbox (32 tests)
**Status**: Complete -- tiered sandboxing with auto-detection.
- `detect.rs`: `SandboxDetector::detect()` checks for bwrap, landlock, macOS sandbox, Windows sandbox, Podman, Docker. Platform-specific checks short-circuit to false on wrong OS.
- `container.rs`: `ContainerExecutor` with `ContainerRuntime` enum (Podman/Docker). Auto-detects runtime. `build_run_args()` constructs container run command with --rm, --network=none, -v mounts, --memory, --cpus, --timeout.
- `bwrap.rs` (Linux): `BwrapSandbox` builds bwrap commands per `SandboxProfile` -- ReadOnly (--ro-bind / /), RestrictedWrite (--bind for allowed paths), NoNetwork (--unshare-net), Full (--unshare-all). Always includes --die-with-parent, --new-session.
- `landlock.rs` (Linux): Stub returning "not yet implemented".
- `macos.rs`: Generates Seatbelt profiles for sandbox-exec. Platform-gated.
- `windows.rs`: Stub returning "not yet implemented". Platform-gated.
- `lib.rs`: `UnifiedSandbox` facade -- auto-detects capabilities, selects best sandbox per level (bwrap > landlock > macos > windows for OsNative; podman > docker for Container).

### athen-coordinador (37 unit + 4 integration = 41 tests)
**Status**: Complete -- full coordinator orchestration with persistence, trust, and approval management.
- `router.rs`: `DefaultRouter` implementing `EventRouter`. Maps EventSource->DomainType (Email/Messaging->Communication, Calendar->Agenda, UserInput/System->Base). Priority: UserInput/Calendar=High, Messaging/Email=Normal, System=Low.
- `queue.rs`: `PriorityTaskQueue` implementing `TaskQueue`. Uses `BinaryHeap<PrioritizedTask>` -- higher priority first, FIFO within same priority (oldest first).
- `dispatcher.rs`: `Dispatcher` manages agent availability. `register_agent()`, `unregister_agent()`, `assign_task()`, `release_agent()`, `assigned_agent()`, `force_release_all()` -- force-releases all assigned agents back to the available pool (used in single-user desktop apps where stale assignments can block new tasks).
- `risk.rs`: `CoordinatorRiskEvaluator` wrapping `Box<dyn RiskEvaluator>`. `evaluate_and_decide()` returns `RiskDecision`.
- `lib.rs`: `Coordinator` wiring all components with optional persistence, trust management, and human approval flow:
  - `.with_persistence(Box<dyn PersistentStore>)` -- attaches SQLite store for task durability. `process_event()` saves tasks after creation, `complete_task()` updates status in DB. Persistence errors are logged but never crash the system.
  - `.with_trust_manager(TrustManager)` -- enables contact-aware risk evaluation. `process_event()` resolves sender trust via `TrustManager` and factors it into risk scoring (AuthUser for UserInput, resolved trust for external senders, Neutral fallback). `complete_task()` records approval for implicit trust evolution.
  - `recover_tasks()` -- loads non-terminal tasks from persistent store and re-enqueues them on startup. Terminal statuses (Completed, Failed, Cancelled) are skipped.
  - `process_event()` routes->resolves sender trust->evaluates risk->sets status->persists->enqueues. Returns `Vec<(TaskId, RiskDecision)>` instead of `Vec<TaskId>`, so the app layer can differentiate `NotifyAndProceed` from `SilentApprove` and trigger appropriate notifications. Tasks with `HumanConfirm` risk go to `awaiting_approval` map instead of queue. `dispatch_next()` dequeues and assigns to agent. `complete_task()` releases agent->updates DB->records trust approval.
  - **Approval management**: `awaiting_approval: Mutex<HashMap<TaskId, Task>>` holds tasks pending human decision. `get_awaiting_approval()` returns the first pending task. `approve_task(id)` moves task to Pending and enqueues. `deny_task(id)` sets status to Cancelled and persists.
  - `infer_identifier_kind()` helper -- infers `IdentifierKind` (Email/Phone/Other) from sender identifier strings.
  - `task_contacts: Mutex<HashMap<TaskId, ContactId>>` -- maps task IDs to resolved contact IDs for trust feedback on completion.

### athen-memory (46 unit + 5 integration = 51 tests)
**Status**: Complete -- vector search + knowledge graph with SQLite persistence + real embeddings + LLM entity extraction + hybrid retrieval + UI management methods.
- `vector.rs`: `InMemoryVectorIndex` -- brute-force cosine similarity search. `tokio::sync::RwLock` for concurrent reads.
- `graph.rs`: `InMemoryGraph` -- BFS exploration from entry node. Scoring combines recency (exponential decay, 7-day half-life), frequency, and importance weighted by `ExploreParams`.
- `sqlite.rs`: `SqliteVectorIndex` and `SqliteGraph` -- SQLite-backed persistent versions. Embeddings stored as little-endian f32 blobs. Uses `std::sync::Mutex` (not tokio) since rusqlite is synchronous and locks are never held across `.await`. `SqliteVectorIndex::list_all()` orders by `rowid DESC` (newest first). `SqliteGraph::add_entity()` deduplicates by name (case-insensitive COLLATE NOCASE). `SqliteGraph` implements all new `KnowledgeGraph` methods (`list_entities`, `list_relations`, `update_entity`, `delete_entity`, `delete_relation`).
- `extractor.rs`: `LlmEntityExtractor` implementing `EntityExtractor` -- extracts entities and relations from text via LLM calls (Cheap profile, 30s timeout). Parses structured JSON output into `ExtractionResult`. Filters garbage entities (underscores, short names, "user"/"assistant"/"system").
- `lib.rs`: `Memory` facade implementing `MemoryStore`. `min_relevance_score: f32` field with `with_min_score()` builder (default 0.3). Three-phase intelligence:
  - **Phase 1 (Embeddings)**: `embedder: Option<Box<dyn EmbeddingProvider>>` with `with_embedder()` builder. `remember()` generates real embeddings via provider, stores content in `metadata._content`. `recall()` embeds query for semantic vector search, filters by `min_relevance_score` before returning results.
  - **Phase 2 (Entity Extraction)**: `extractor: Option<Box<dyn EntityExtractor>>` with `with_extractor()` builder. `remember()` auto-extracts entities and relations from content via LLM, adds them to the knowledge graph automatically.
  - **Phase 3 (Hybrid Retrieval)**: `recall()` performs hybrid vector search + graph exploration. Graph-connected results receive a score boost, results are deduplicated and merged for richer context.
  - `forget()` removes from vector index.
  - **UI management methods**: `list_all()`, `update()` (re-embeds on save), `list_entities()`, `list_relations()`, `update_entity()`, `delete_entity()`, `delete_relation()` -- expose full CRUD for the frontend memory management UI.

### athen-sentidos (81 tests)
**Status**: Complete -- user input monitor, full email monitor, full calendar monitor, full telegram monitor, messaging stub, polling runner.
- `user_input.rs`: `UserInputMonitor` using `tokio::sync::Mutex<mpsc::Receiver<String>>` for interior mutability. Converts strings to `SenseEvent` with EventSource::UserInput, EventKind::Command, RiskLevel::Safe. Exposes `sender()` for UI to push messages.
- `email.rs`: Full `EmailMonitor` -- real IMAP polling via `imap` v2.4 (sync, wrapped in `spawn_blocking`) + `rustls-connector` for TLS (no OpenSSL). `mailparse` for MIME body parsing (text, HTML, attachments). Tracks `last_seen_uid` for incremental polling. Uses `BODY.PEEK[]` to avoid marking emails as read. `extract_email_body()` recursively walks MIME parts. Configurable via `EmailConfig` (server, port, TLS, folders, poll interval, lookback). 15 tests.
- `calendar.rs`: Full `CalendarMonitor` -- polls SQLite every 60s, queries events within 7 days using `datetime()` for timezone-safe comparison. 21 tests.
  - `generate_reminder_events()` -- fires reminders when `minutes_until <= reminder_minutes` for each event
  - "Starting now" notification when event within 1 minute, even without explicit reminders
  - Session-level deduplication via `Mutex<HashSet<(event_id, reminder_minutes)>>`
  - Opens fresh rusqlite connection per poll (safe at 60s interval)
  - Graceful: returns empty if DB or table doesn't exist
  - Added `rusqlite` and `tempfile` (dev) dependencies to athen-sentidos
- `telegram.rs`: Full `TelegramMonitor` -- raw HTTP via `reqwest` (no Telegram framework dependency). Uses `getUpdates` long-polling with offset tracking. Handles text messages, photo captions, document captions. Owner messages (matching `owner_user_id`) -> `RiskLevel::Safe` (L1), others -> `RiskLevel::Caution` (L2). Configurable `allowed_chat_ids` filtering. Exports public `send_message(bot_token, chat_id, text)` utility function used by both the Telegram monitor and the notification system. 22 tests.
- `messaging.rs`: Stub `MessagingMonitor` -- 30s poll interval.
- `lib.rs`: Generic `SenseRunner<M: SenseMonitor>` -- polling loop with `tokio::select!` for shutdown signal. Sends events through mpsc channel.

### athen-shell (20 tests)
**Status**: Complete -- cross-platform shell execution.
- `native.rs`: `NativeShell` -- uses `sh -c` on Unix, `cmd /C` on Windows via `tokio::process::Command`. 30-second timeout. Captures stdout/stderr/exit code/execution time. `which()` uses system `which`/`where`.
- `nushell.rs`: `NushellShell` -- auto-detects `nu` binary. If available: `nu -c "command"`. If not: falls back to NativeShell with info log.
- `lib.rs`: `Shell` unified facade. `execute()` prefers nushell, `execute_native()` always native. Convenience: `run()` returns stdout, `run_ok()` returns bool, `has_program()` checks existence.

### athen-persistence (110 unit + 5 integration = 115 tests)
**Status**: Complete -- SQLite persistence with atomic checkpoints, arcs, calendar, contacts, notifications, and legacy chat history.
- `lib.rs`: `Database` struct with `new(path)` and `in_memory()`. Auto-creates tables on init (including arc, calendar, contacts, notifications, and legacy chat tables). Provides `store()` -> `SqliteStore`, `chat_store()` -> `ChatStore`, `arc_store()` -> `ArcStore`, `calendar_store()` -> `CalendarStore`, `contact_store()` -> `SqliteContactStore`, and `notification_store()` -> `NotificationStore` accessors.
- `store.rs`: `SqliteStore` implementing `PersistentStore`. Full CRUD for tasks (with steps serialized as JSON), checkpoints with SHA-256 integrity verification, pending messages with atomic pop (transaction-based select+update).
- `checkpoint.rs`: `CheckpointManager` -- atomic file-based backup (write temp -> fsync -> rename). Integrity verification with SHA-256 checksums.
- `arcs.rs`: `ArcStore` -- git-branch-like workflow containers replacing sessions. Types:
  - `ArcMeta`: id, name, source (`ArcSource`: UserInput/Email/Calendar/Messaging/System), status (`ArcStatus`: Active/Archived/Merged), parent_arc_id, merged_into_arc_id, created_at, updated_at
  - `ArcEntry`: id, arc_id, entry_type (`EntryType`: Message/ToolCall/EmailEvent/CalendarEvent/SystemEvent), source, content, metadata (JSON), created_at
  Methods:
  - `create_arc(name, source)` -- creates a new arc
  - `create_arc_with_parent(name, source, parent_id)` -- creates a branched child arc
  - `list_arcs()` -- returns all non-merged arcs ordered by `updated_at` DESC
  - `rename_arc(id, name)` -- renames an arc
  - `delete_arc(id)` -- deletes arc and all its entries
  - `archive_arc(id)` -- sets status to Archived
  - `merge_arc(source_id, target_id)` -- moves all entries from source to target arc, marks source as Merged with `merged_into_arc_id`
  - `add_entry(arc_id, entry_type, source, content, metadata)` -- adds an interaction entry
  - `load_entries(arc_id)` -- returns all entries ordered by creation time
  - `migrate_from_chat_tables()` -- auto-migrates legacy chat_sessions/chat_messages to arcs on first startup
  Schema: `arcs` (id PK, name, source, status, parent_arc_id, merged_into_arc_id, created_at, updated_at) + `arc_entries` (id PK, arc_id FK, entry_type, source, content, metadata, created_at). 10 tests.
- `calendar.rs`: `CalendarStore` (Clone) -- Athen's native internal calendar system. 19 tests. Types:
  - `CalendarEvent`: 15 fields (id, title, description, start_time, end_time, all_day, location, recurrence, reminder_minutes, color, category, created_by, arc_id, created_at, updated_at)
  - `EventCreator` (User/Agent), `Recurrence` (Daily/Weekly/Monthly/Yearly), `FiredReminder`
  Methods: full CRUD -- `create_event`, `update_event`, `delete_event`, `get_event`, `list_events` (overlap range query using `datetime()` for proper timezone handling), `list_all_events`, `get_upcoming_events`, `get_events_by_category`.
  Reminder tracking: `record_fired_reminder`, `is_reminder_fired`, `clear_old_fired_reminders`.
  `normalize_to_utc()` function normalizes any ISO 8601 timestamp (with offset like `+02:00`, bare, or `Z`) to UTC before storage. Applied in both `create_event` and `update_event`. Ensures consistent storage regardless of whether the frontend (sends UTC `Z`) or agent (may send local offset) creates the event.
  Schema: `calendar_events` table + `fired_reminders` table (composite PK).
- `contacts.rs`: `SqliteContactStore` (Clone) implementing `ContactStore` trait. Two tables: `contacts` (10 columns) + `contact_identifiers` (4 columns, UNIQUE(identifier, kind), ON DELETE CASCADE). Full CRUD with UPSERT transactions. 14 tests. Provides production persistence for contacts (previously only `InMemoryContactStore` existed for tests).
- `chat.rs`: `ChatStore` -- legacy SQLite-backed chat history. Still exists for backward compatibility but no longer used by the app. Auto-migrated to arcs on first startup.
  Schema: `chat_messages` (id, session_id, role, content, content_type, created_at) + `chat_sessions` (session_id PK, name, created_at, updated_at).
- `notifications.rs`: `NotificationStore` (Clone) -- SQLite-backed notification persistence. Schema: `notifications` table (id TEXT PK, urgency, title, body, origin, arc_id, task_id, requires_response, is_read, created_at, updated_at). Full CRUD: `save`, `load`, `list_all`, `list_unread`, `mark_read`, `mark_all_read`, `mark_arc_read`, `delete`, `delete_read`, `unread_count`. 9 tests.
- `grants.rs`: `GrantStore` -- SQLite-backed directory grant persistence. Types: `Access` (Read/Write), `GrantScope` (Arc/Global), `DirectoryGrant` (id, scope, path, access, granted_at). Full CRUD: `grant_arc`, `grant_global`, `list_arc_grants`, `list_global_grants`, `revoke`, `check` (lookup grant). System paths never writable by design. Wired by `FileGate` in athen-app for agent path permission checking. Schema: `grants` table (id PK auto, scope, arc_id, path, access, granted_at).
- `mcp.rs`: `McpStore` -- SQLite-backed MCP configuration persistence. Types: `EnabledMcp` (id, mcp_id, config JSON). CRUD: `enable`, `disable`, `list_enabled`, `get_config`, `set_config`. Wired by `McpRegistry` in athen-mcp. Schema: `enabled_mcps` table (id PK auto, mcp_id UNIQUE, config TEXT).
- `profiles.rs`: `SqliteProfileStore` implementing `ProfileStore`. Tables: `agent_profiles` (id PK string, display_name, description, persona_template_ids JSON, custom_persona_addendum, tool_selection JSON, expertise JSON, model_profile_hint, builtin, created_at, updated_at), `persona_templates` (id PK, display_name, category, body, builtin, created_at), and `arc_profiles` (arc_id PK, profile_id) for per-arc profile assignment. `seed_builtins_if_empty(now)` is **per-id idempotent** — iterates `builtin_profiles(now)` (12 entries: default, assistant, coder, devops, systems_architect, technical_support, researcher, marketing, social_media, outreach, lawyer, doctor) and inserts only missing ids, so user edits to a built-in survive future launches. `save_profile` is relaxed to **preserve the existing row's `builtin` flag and `created_at`** instead of rejecting writes to built-ins — caller's flag is ignored when a row already exists. `restore_builtin(id)` looks up `canonical_builtin_profile(id)` and rewrites via the same path, reverting any user edits. Built-in personas are framed for non-experts: doctor + lawyer carry research-oriented disclaimers (LLM safety layer enforces actual limits); social_media is platform-native (LinkedIn/TikTok/Instagram/X) distinct from broader marketing. All built-ins ship with `ToolSelection::All` — persona drives behavior, not tool restriction. Tests cover seed-all-canonical, additive-when-some-exist, edit-preserves-builtin-flag, restore-reverts-edits, restore-rejects-unknown-id.
- Schema: `tasks`, `task_steps`, `checkpoints`, `pending_messages`, `arcs`, `arc_entries`, `calendar_events`, `fired_reminders`, `contacts`, `contact_identifiers`, `notifications`, `grants`, `enabled_mcps`, `agent_profiles`, `persona_templates`, `arc_profiles`, `chat_messages` (legacy), `chat_sessions` (legacy) tables.

### athen-web (15 tests)
**Status**: Complete — search and page-reader providers behind `WebSearchProvider` / `PageReader` traits. Bundled no-key defaults, optional key-gated upgrades. Wired into `ShellToolRegistry` as the `web_search` and `web_fetch` tools.
- `lib.rs`: re-exports + `default_http_client()` (User-Agent set, 30s timeout) for adapters that don't bring their own `reqwest::Client`.
- `search/mod.rs`: `WebSearchProvider` trait (`search`, `name`) + `SearchResult { title, url, snippet }`.
- `search/duckduckgo.rs`: `DuckDuckGoSearch` — POSTs `html.duckduckgo.com/html/`, parses results with `scraper` CSS selectors, unwraps the `/l/?uddg=...` redirect wrapper so agents see real URLs. HTTP 202 surfaces as a clear "rate-limited" error. Tolerant parser: missing fields fall back to empty strings rather than failing the whole query.
- `search/tavily.rs`: `TavilySearch` — POST `api.tavily.com/search` with `api_key` in body, max_results enforced. ~1k req/month free tier.
- `reader/mod.rs`: `PageReader` trait (`fetch`, `name`) + `ReadResult { url, title, content, source }`. The `source` string identifies the tier (e.g. `"local-html"`, `"jina"`, `"wayback"`, `"cloudflare"`).
- `reader/local.rs`: `LocalReader` — plain reqwest GET with `Accept: text/markdown, text/html;q=0.9, */*;q=0.5`. Cloudflare-opted-in origins return clean markdown for free; HTML responses go through `strip_noise` (script/style blocks removed via case-insensitive scan) → `html2md::parse_html`. Title extracted via tiny regex-free `<title>` scan. UTF-8 safe `truncate_chars` cap at 40k. 5MB body cap.
- `reader/jina.rs`: `JinaReader` — `https://r.jina.ai/<url>` with `Accept: text/markdown` and `X-Return-Format: markdown` headers. Strips Jina's `Title: ... URL Source: ... Markdown Content:` prelude into structured fields. Optional bearer auth.
- `reader/wayback.rs`: `WaybackReader` — composes `LocalReader` against `https://web.archive.org/web/2id_/<url>`. The `2id_` modifier strips Wayback's nav banner so we get raw archived HTML.
- `reader/cloudflare.rs`: `CloudflareReader` — `POST api.cloudflare.com/.../browser-rendering/markdown` with bearer-auth on a CF API token. Built but not wired by default (paid).
- `reader/hybrid.rs`: `HybridReader` — chains `primary → js_fallback → archive_fallback`. Default chain: `LocalReader → JinaReader → WaybackReader`. `looks_empty` heuristic: hard floor 150 chars, soft band 150–800 only triggers fallback when JS-required markers present. Genuinely small static pages (`example.com` ~190 chars) pass through; SPA stubs and JS-warning pages fall back. Each tier failure logs at debug level so the chain decision is auditable. Tests: 4 unit (split header, SPA detection edge cases).
- Tests: 15 unit (DDG redirect unwrap, jina header split, local title/strip-noise/truncate, hybrid heuristic edge cases). No live-network tests in CI; live smoke runs via `cargo run -p athen-web --example smoke`.

### athen-agent (51 unit + 3 integration = 54 tests)
**Status**: Complete -- LLM-driven task execution with real tool calling, streaming responses (including thinking content forwarding), cancellation, tool-level risk checking, sandbox integration, session memory, LLM completion judge, graceful max-steps handling, streaming tool calls, <think> tag parsing, 2-tier tool discovery, batch tool calls, and loop protection.
- `executor.rs`: `DefaultExecutor` implementing `AgentExecutor`. Fields include optional `stream_sender: Option<mpsc::UnboundedSender<String>>` for progressive streaming and `cancel_flag: Option<Arc<AtomicBool>>` for user-initiated cancellation. Accepts optional `context_messages: Vec<ChatMessage>` prepended to conversation for session-level memory. System prompt ("You are Athen, an AI agent that ACTS first and talks second") is conversation-aware: when context messages are present, it tells the LLM "You are in an ongoing conversation". **System prompt includes current date/time and timezone** (e.g. "Current date and time: Monday, April 6, 2026 at 11:46 (CEST, UTC+02:00)") via `chrono::Local::now()`. **Calendar guidance**: tells the agent to use the local timezone offset (e.g. `2026-04-06T12:15:00+02:00`) instead of UTC `Z`, with the detected offset included dynamically. **2-tier tool discovery**: system prompt includes only one-line group summaries (e.g. "calendar_list, calendar_create, calendar_update, calendar_delete (4 calendar tools)"). Full tool schemas are not in prompt; agent discovers them on-demand by name via `get_tool_details` or reads `~/.athen/tools/*.md` files per group. Includes numbered rules: (1) never say "I'll do X" -- just do it, (2) never ask what to do -- take initiative, (3) call tools immediately, (4) only text when task is complete, (5) be concise, (6) make reasonable choices, (8) ALWAYS respond in natural language, NEVER output raw JSON (with BAD/GOOD examples). **MEMORY & KNOWLEDGE guidance section**: tells the agent to check memory/contacts when people or past interactions are mentioned. BAD/GOOD examples included (English + Spanish variants). **Loop protection**: tracks max consecutive tool-only steps with no text output; if exceeded, injects nudge to produce user-visible progress. Loop: check cancel_flag -> check timeout -> check max_steps -> build LlmRequest with small tool list -> call LlmRouter (streaming or non-streaming) -> **batch tool calls** (execute all tools in parallel, not sequentially) -> check cancel_flag between batches -> record steps via StepAuditor -> repeat until LLM says done. Tool call results fed back as `Role::Tool` messages with `tool_call_id` for OpenAI-compatible APIs. **Tool tracking**: `tools_called: Vec<String>` tracks which specific tools were called during execution.
  - **Streaming**: when `stream_sender` is set, uses `try_streaming_call()` which calls `LlmRouter::route_streaming()`, collects text deltas, and forwards each chunk through the sender. If streaming returns empty content (tool call response), falls back to non-streaming `route()` to get tool call data. **Streaming tool calls**: supports `tool_calls` arriving via streaming chunks. **Thinking content**: forwards `is_thinking` chunks and `<think>` tag content to the stream sender for frontend display. **Graceful fallback**: when streaming succeeds but non-streaming fallback errors, returns empty response instead of propagating the error.
  - **`StreamResult` struct**: returned from `try_streaming_call()`, contains collected content, tool calls, and reasoning content.
  - **`extract_think_tags()`**: parses `<think>...</think>` blocks from model responses (e.g. DeepSeek R1), extracts thinking text separately from content.
  - **`clean_model_response()`**: strips thinking tags from final displayed response, extracts text from JSON wrappers like `{"response": "text"}`, replaces empty responses with default message.
  - **Cancellation**: `cancel_flag` is checked at loop start and between each tool batch. When set to `true`, returns immediately with `{ reason: "cancelled", response: "Task cancelled by user." }`.
  - **LLM Completion Judge**: replaces the regex-based anti-lazy nudge system. `judge_completion()` is a lightweight LLM call (Cheap profile, 5 tokens, 30s timeout) that evaluates whether the agent actually completed the user's request. Checks the user's request, agent's response, and list of tools called. Returns DONE or CONTINUE. Language-agnostic (works for English, Spanish, any language). Fires at most once per execution. Defaults to DONE on failure/timeout.
  - **Graceful max-steps**: when the executor hits the step limit, it makes one final LLM call with `tools: None` asking for a summary of work accomplished, instead of returning raw JSON.
- `tools.rs`: `ShellToolRegistry` implementing `ToolRegistry` with 13 built-in tools. Provides `ShellExtraWritableProvider` trait for per-arc directory grant discovery (wired by `file_gate.rs`):
  - `shell_execute` -- runs shell commands with **2-layer safety**: (1) pre-execution risk check via `RuleEngine.evaluate()` on the actual command string -- blocks Danger/Critical commands with a descriptive error returned to the LLM, (2) sandboxed execution via bwrap with `SandboxProfile::RestrictedWrite` (writable: `/tmp`, `$HOME`, cwd, + per-arc granted paths; read-only: everything else). Graceful fallback to unsandboxed if bwrap not installed. Tool description forbids using curl/wget/lynx for web content — agent should use `web_fetch`/`web_search` instead.
  - `shell_spawn` / `shell_kill` / `shell_logs` -- detached process management with PID tracking, log-file capture, and SIGTERM-then-SIGKILL teardown. Only kills PIDs in the registry's tracked map.
  - `read` -- file reads with `cat -n`-style line numbers, optional `offset`/`limit`, NUL-byte binary detection. Workspace-relative path resolution via `paths::resolve_in_workspace`.
  - `edit` -- exact-string replace, requires a prior `read` of the file to enforce read-before-modify and detect external changes. Atomic write (sibling tmp + rename, fallback to direct write on cross-FS rename).
  - `write` -- full overwrite. Existing files require a prior `read`; new files don't. Atomic.
  - `grep` -- ripgrep wrapper.
  - `list_directory` -- lists directory entries as JSON array.
  - `memory_store` / `memory_recall` -- in-session `HashMap<String, String>` (overridden to persistent semantic memory by `AppToolRegistry`).
  - `web_search` -- delegates to `Arc<dyn WebSearchProvider>` (default `DuckDuckGoSearch`, swap via `with_web_search`). Returns `{ provider, query, results: [{title, url, snippet}] }`.
  - `web_fetch` -- delegates to `Arc<dyn PageReader>` (default `HybridReader` chaining Local → Jina → Wayback, swap via `with_page_reader`). Returns `{ url, title, content, source, content_chars }`. Rejects non-http(s) URLs early without network round-trip.
  Each tool has proper JSON Schema parameter definitions for LLM tool calling. `ShellToolRegistry` holds an optional `UnifiedSandbox` (from `athen-sandbox`), a `RuleEngine` (from `athen-risk`), the spawn-tracking map, and the web search/page reader providers — all auto-initialized on construction.
- `tool_grouping.rs`: Helpers for 2-tier tool discovery. `group_for(name)` extracts the prefix (e.g. "memory", "calendar", "files", "web" from tool names). `summarize_groups()` returns one-line summary per group for the system prompt. `is_always_revealed` covers memory + shell + file primitives + the web tools (`web_search`, `web_fetch`) so their full schemas are inline every turn — without this, small models default to whatever IS revealed (typically `shell_execute`) and use `curl` instead of the dedicated web tools.
- `tools_doc.rs`: Generates per-group markdown references. Writes to configured directory (typically `~/.athen/tools/`), one `.md` file per group. Agent reads only the group it needs via `read_file`, avoiding context bloat. Replaces monolithic TOOLS.md with on-demand modular loading.
- `auditor.rs`: `InMemoryAuditor` implementing `StepAuditor` with `tokio::sync::Mutex<HashMap<TaskId, Vec<TaskStep>>>`.
- `timeout.rs`: `DefaultTimeoutGuard` -- sets deadline at Instant::now() + duration.
- `resource.rs`: `DefaultResourceMonitor` -- reads `/proc/self/statm` on Linux for resident memory. `AtomicBool` cache for within-limits state.
- `lib.rs`: `AgentBuilder` with fluent API. `.context_messages(Vec<ChatMessage>)` sets prior conversation history for session memory. `.stream_sender(UnboundedSender<String>)` enables streaming text forwarding. `.cancel_flag(Arc<AtomicBool>)` enables user-initiated cancellation. `.tool_doc_path(PathBuf)` sets the directory where tool markdown files are written. Defaults: 50 max_steps, 5-minute timeout, InMemoryAuditor.
- Integration tests: mock LLM returns tool call -> real `ShellToolRegistry` executes -> result fed back -> LLM completes. Batch tool calls tested (multiple tools in parallel).

### athen-contacts (15 tests)
**Status**: Complete -- trust management with implicit learning. Production persistence via `SqliteContactStore` in athen-persistence (see above).
- `lib.rs`: `ContactStore` trait (save/load/find_by_identifier/list_all/delete) + `InMemoryContactStore` for testing.
- `trust.rs`: `TrustManager` with:
  - `resolve_contact()` -- finds existing or creates T0 Unknown
  - `risk_multiplier()` -- delegates to TrustLevel, returns 5.0x for blocked
  - `record_approval()` -- every 5 approvals upgrades T0->T1->T2 (never past T2, never if manual override)
  - `record_rejection()` -- tracks count in notes JSON, every 3 rejections downgrades T2->T1->T0 (never if manual override)
  - `set_trust_level()` -- sets level + trust_manual_override flag
  - `block_contact()`, `is_blocked()`, `list_contacts()`, `find_by_identifier()`

### athen-llm (91 unit + 0 integration = 91 tests)
**Status**: Complete -- router with streaming + Anthropic/DeepSeek/OpenAI-compatible providers + Ollama/llama.cpp wrappers + embedding providers with auto-detection.
- `budget.rs`: `BudgetTracker` -- daily USD limit, token counting, midnight UTC reset, `can_afford()`, `record_usage()`, `status()`, `is_warning()`. Zero budget always rejected.
- `router.rs`: `CircuitBreaker` (Closed/Open/HalfOpen state machine, configurable thresholds/timeout). `DefaultLlmRouter` implementing `LlmRouter` -- profile-based routing, failover chains, circuit breakers per provider, budget enforcement. Implements both `route()` and `route_streaming()` with independent failover methods (`route_with_failover`, `route_streaming_with_failover`) -- each tries providers in priority order, respects circuit breakers, and records success/failure.
- `providers/openai.rs`: `OpenAiCompatibleProvider` -- fully generic adapter for any OpenAI-compatible API endpoint. Builder pattern: `new(base_url)`, `with_api_key()`, `with_model()`, `with_provider_id()`, `with_client()`, `with_cost_estimator()`. Convenience constructor: `openai(api_key)` for OpenAI proper. Features:
  - API key is optional -- `Authorization: Bearer` header only sent when key is present (supports local servers without auth)
  - Full tool calling support with proper wire format conversion (assistant messages with tool_calls, tool result messages with tool_call_id)
  - SSE streaming via `complete_streaming()` -- parses `data:` lines, handles `[DONE]`, extracts content deltas, finish_reason, **`reasoning_content`** (thinking models), and **`tool_calls`** from streaming chunks
  - Non-streaming `complete()` extracts `message.reasoning_content` into `LlmResponse.reasoning_content`
  - `OpenAiRequestOut` has `extra: Option<Value>` field with `#[serde(flatten)]` for provider-specific parameters
  - `CostEstimator` trait for pluggable pricing: `OpenAiCostEstimator` (gpt-4o, gpt-4o-mini, o3, etc.), `ZeroCostEstimator` (for local providers)
  - `parse_sse_chunks()` public function for reuse by wrapper providers
- `providers/deepseek.rs`: Full `DeepSeekProvider` -- OpenAI-compatible API at `api.deepseek.com/v1/chat/completions`. Bearer auth, request/response mapping, SSE streaming, tool call support. Cost estimation: deepseek-chat ($0.14/M input, $0.28/M output), deepseek-reasoner ($0.55/$2.19). Builder pattern: `new(api_key)`, `with_model()`, `with_base_url()`.
- `providers/anthropic.rs`: Full `AnthropicProvider` -- POST to `/v1/messages`, proper headers (x-api-key, anthropic-version), request/response mapping, SSE streaming, cost estimation for opus/sonnet/haiku tiers.
- `providers/ollama.rs`: `OllamaProvider` -- thin wrapper around `OpenAiCompatibleProvider` for local Ollama inference. Default URL `http://localhost:11434`, zero-cost estimation, delegates all LLM logic to inner provider. Real health check via `GET /api/tags` (returns model count). Builder: `new(model)`, `with_base_url()`, `with_model()`.
- `providers/llamacpp.rs`: `LlamaCppProvider` -- thin wrapper around `OpenAiCompatibleProvider` for llama.cpp's `llama-server`. Default URL `http://localhost:8080`, zero-cost estimation. Real health check via `GET /health`. Constructor: `new(base_url, model)`, `localhost(model)`.
- `providers/google.rs`: Stub returning "not yet implemented".
- `embeddings/`: Full embedding provider system integrated with `athen-memory` for semantic search and knowledge graph grounding.
  - `embeddings/ollama.rs`: `OllamaEmbedding` -- uses Ollama's `/api/embed` endpoint, auto-detects dimensions, batch support, model presence check via `/api/tags`. Builder: `new(model)`, `with_base_url()`. 8 tests.
  - `embeddings/openai.rs`: `OpenAiEmbedding` -- uses `/v1/embeddings` endpoint, optional API key, known dimension lookup for OpenAI models (text-embedding-3-small: 1536, text-embedding-3-large: 3072, text-embedding-ada-002: 1536), works with any OpenAI-compatible endpoint. Constructors: `openai(api_key)`, `compatible(base_url)`. Builder: `with_model()`, `with_api_key()`. 14 tests.
  - `embeddings/keyword.rs`: `KeywordEmbedding` -- TF-IDF hash projection fallback using FNV-1a hashing, 384 dimensions, L2 normalized. Zero external dependencies, always available. 13 tests.
  - `embeddings/router.rs`: `EmbeddingRouter` -- auto-detection waterfall trying providers in priority order with keyword fallback always available. 5 tests.

### athen-mcp (6 tests)
**Status**: Complete -- MCP marketplace and runtime management.
- `lib.rs`: Composition root. Exports `McpRegistry` and `EnabledEntry`.
- `catalog.rs`: `builtin_catalog()` -- hardcoded list of branded MCP entries. `CatalogEntry` struct with id, display_name, description, command (how to spawn), version, tags. `lookup(id)` retrieves entry by ID. Can later support downloadable entries.
- `registry.rs`: `McpRegistry` -- runtime state of enabled MCPs with persistent wiring. Lazy-spawns child processes. `EnabledEntry` tracks per-entry config (api_key, base_url, custom_command). Methods: `enable(id)`, `disable(id)`, `list_enabled()`, `get_config()`, `set_config()`, `is_available(id)`. Implements `McpClient` trait for tool schema discovery. Spawned processes communicate via stdio JSON-RPC streams. 6 tests covering enable/disable, config persistence, client trait delegation.

### athen-cli (0 tests)
**Status**: Complete -- working agentic CLI with tool execution.
- `main.rs`: REPL loop wiring all components end-to-end. Reads `DEEPSEEK_API_KEY` from env or config. Uses `config_loader` to discover config from `~/.athen/` or `./config/`. Creates `DeepSeekProvider` -> `DefaultLlmRouter` (mapped to all profiles) -> `CombinedRiskEvaluator` (real rule engine + LLM fallback) -> `Coordinator` (real router, queue, dispatcher). Synchronous stdin with clean EOF/Ctrl+D handling. Risk-gated: low-risk auto-approved, high-risk prompts for confirmation, hard-block rejected. Commands: `/quit`, `/exit`.
- Uses full `AgentBuilder` + `DefaultExecutor` with `ShellToolRegistry` -- the agent can execute shell commands (sandboxed when bwrap available), read/write files, list directories, and use in-session memory (store/recall) autonomously via LLM tool calls.
- `SharedRouter`: Wrapper around `Arc<DefaultLlmRouter>` implementing `LlmRouter` trait for sharing between risk evaluator and agent.
- **Verified working**: Full agentic pipeline -- user input -> coordinator -> risk -> dispatch -> agent executor -> LLM -> tool calls -> execution -> result.

### athen-app (85 tests)
**Status**: Complete -- Tauri 2 desktop app with full agentic tool execution (10 tools including calendar), streaming responses (with thinking content display), arc-based workflow management, native calendar system, sense router for email/calendar/messaging/telegram triage, notification orchestrator with multi-channel delivery (InApp + Telegram), settings UI with provider/email/telegram/notification/embedding management, contacts management UI, provider hot-swap, kill switch, real-time progress events, approval UI, config loading, persistence, path-based file gates (directory grants per arc), and Telegram bot integration. Verified working.
- `src/lib.rs`: Tauri composition root. Modules include `pub(crate) mod app_tools`, `pub(crate) mod contacts`, `pub(crate) mod file_gate`, `pub(crate) mod notifier`. Registers 58 command handlers (52 + 6 agent profile commands: `list_agent_profiles`, `set_arc_profile`, `create_agent_profile`, `update_agent_profile`, `delete_agent_profile`, `restore_agent_profile`): `send_message`, `get_status`, `approve_task`, `cancel_task`, `new_arc`, `get_arc_history`, `list_arcs`, `switch_arc`, `rename_arc`, `delete_arc`, `get_current_arc`, `branch_arc`, `merge_arcs`, `get_timeline_data`, `get_settings`, `save_provider`, `delete_provider`, `test_provider`, `save_settings`, `set_active_provider`, `save_email_settings`, `test_email_connection`, `save_telegram_settings`, `test_telegram_connection`, `list_calendar_events`, `create_calendar_event`, `update_calendar_event`, `delete_calendar_event`, `list_contacts`, `get_contact`, `set_contact_trust`, `block_contact`, `unblock_contact`, `delete_contact`, `mark_notification_seen`, `mark_notification_read`, `mark_all_notifications_read`, `list_notifications`, `delete_notification`, `delete_read_notifications`, `get_notification_settings`, `save_notification_settings`, `save_embedding_settings`, `test_embedding_provider`, `list_memories`, `update_memory`, `delete_memory`, `list_entities`, `list_relations`, `update_entity`, `delete_entity`, `delete_relation`. Registers agent in `setup()` hook. Installs `rustls::crypto::aws_lc_rs::default_provider()` at startup. Calls `state.start_calendar_monitor(app.handle().clone())` in setup. **`tracing-subscriber` initialized** for structured logging in the app.
- `src/main.rs`: Entry point with `windows_subsystem = "windows"` for release builds.
- `src/state.rs`: `AppState` -- composition root that loads TOML configuration via `find_config_dir()` (same discovery order as CLI: `~/.athen/` -> `./config/` -> defaults), resolves active provider from config, builds router and coordinator. Contains:
  - `router: Arc<RwLock<Arc<DefaultLlmRouter>>>` -- double-wrapped for runtime hot-swap: inner `Arc` is the router, `RwLock` allows atomic replacement when the user switches providers
  - `active_provider_id: Mutex<String>` -- ID of the currently active LLM provider (e.g. "deepseek", "ollama")
  - `history: Mutex<Vec<ChatMessage>>` -- arc-level conversation memory
  - `arc_id: Mutex<String>` -- current arc identifier
  - `arc_store: Option<ArcStore>` -- persistent arc storage backed by SQLite
  - `calendar_store: Option<CalendarStore>` -- calendar event storage backed by SQLite
  - `pending_message: Mutex<Option<String>>` -- stashes user's message for replay after approval
  - `model_name: Mutex<String>` -- resolved from config, returned by `get_status`
  - `cancel_flag: Arc<AtomicBool>` -- shared cancellation flag for in-progress tasks
  - `memory: Option<Arc<Memory>>` -- persistent memory system built via `build_memory()` helper (SQLite vector index + SQLite graph + keyword embeddings + LLM entity extractor)
  - `profile_store: Option<Arc<dyn ProfileStore>>` -- `SqliteProfileStore` from the database, seeded with 12 built-ins on first launch
  - `profile_embedder: Arc<dyn EmbeddingProvider>` -- `EmbeddingRouter` (keyword fallback when no provider configured) used to embed query text and per-profile descriptions for blended Coordinator routing
  - `profile_embedding_cache: ProfileEmbeddingCache` -- `Arc<RwLock<HashMap<ProfileId, (DateTime<Utc>, Vec<f32>)>>>`, lazy-populated and invalidated by `profile.updated_at`
  - `SharedRouter` wrapper implementing `LlmRouter` via `Arc<RwLock<Arc<DefaultLlmRouter>>>` -- delegates `route()`, `route_streaming()`, and `budget_remaining()` through the double-Arc indirection
  - `build_router_for_provider(id, base_url, model, api_key)` -- factory function that creates the appropriate provider type based on ID: "deepseek" -> `DeepSeekProvider`, "ollama" -> `OllamaProvider`, "llamacpp" -> `LlamaCppProvider`, anything else -> `OpenAiCompatibleProvider`
  - `restore_or_create_arc()` -- on startup, tries to restore the most recent arc's entries from SQLite; creates a new arc if none exist. Auto-migrates legacy chat sessions to arcs on first startup.
  - `start_calendar_monitor(app_handle)` -- spawns background task that polls `CalendarStore` every 60s, fires SenseEvents through sense router for reminder notifications and arc creation. Always starts (no enable flag -- just checks local DB).
  - `start_telegram_monitor(app_handle)` -- spawns `TelegramMonitor` background task. Owner messages (matching `owner_user_id`) skip sense router triage and go straight to agent execution via `execute_owner_telegram_message()`. Non-owner messages go through normal `process_sense_event()` triage. Owner responses are sent back to Telegram via `sendMessage` API (with 4096-char split for long responses). Arc creation/reuse uses 5-minute time-window grouping. Conversation history loaded from arc for context continuity.
  - **SqliteContactStore replaces InMemoryContactStore**: creates `SqliteContactStore` from the database and wires `TrustManager` with it. Trust-aware risk evaluation is now live in production.
  - `build_coordinator_with_persistence()` creates the TrustManager from SqliteContactStore and attaches it via `.with_trust_manager()`.
  - `init_notifier(app_handle)` -- creates `NotificationOrchestrator` with InApp + optional Telegram channels, wires LLM router for humanization, wires `NotificationStore` for persistence, loads persisted notifications on startup
  - Window focus tracking via `on_window_event` -- sets `notifier.set_user_present(focused)`
  - **API key resolution**: config file key takes priority over env var (e.g. saved key via Settings > `DEEPSEEK_API_KEY` env var). Env var format: `{PROVIDER_ID}_API_KEY` (e.g. `DEEPSEEK_API_KEY`, `OPENAI_API_KEY`). Config values like `${DEEPSEEK_API_KEY}` are treated as unresolved placeholders.
- `src/commands.rs`: Tauri IPC commands:
  - `send_message` -- emits `agent-progress` event for risk evaluation phase, processes through coordinator pipeline, checks for awaiting approval (returns `PendingApproval` with task_id/description/risk_score/risk_level), snapshots conversation history, builds full `AgentExecutor` with `AppToolRegistry` (10 tools: 6 shell + 4 calendar) per request, passes history as `context_messages`, wires streaming sender and cancel flag, executes with 25 max_steps and 90s timeout. On failure: friendly error messages via `format_user_error()`. On cancellation: returns "Task cancelled by user." On max-steps: returns "I ran out of steps (N used) before finishing." Appends user+assistant messages to arc history. Persists entries to SQLite via `ArcStore`.
  - `approve_task` -- approves or denies a task flagged by risk system. On approve: retrieves stashed message, builds executor with streaming + cancel flag, dispatches and executes. On deny: cancels task, clears stashed message.
  - `cancel_task` -- sets `cancel_flag` to `true`; executor checks at loop start and between tool calls.
  - `get_status` -- returns actual model name and connection status.
  - `get_arc_history` -- returns current arc's entries for frontend rendering on startup.
  - `new_arc` -- clears in-memory history, creates new arc with ArcSource::UserInput.
  - `get_current_arc` -- returns current arc ID.
  - `switch_arc(arc_id)` -- loads target arc's entries from SQLite into memory, returns display messages.
  - `rename_arc(arc_id, name)` -- renames an arc.
  - `delete_arc(arc_id)` -- deletes arc and entries. If deleting the active arc, switches to next most recent or creates new one.
  - `list_arcs` -- returns `Vec<ArcMeta>` for sidebar rendering.
  - `branch_arc(parent_id, name)` -- creates a child arc branched from parent.
  - `merge_arcs(source_id, target_id)` -- merges source arc entries into target, marks source as Merged.
  - `list_calendar_events(start, end)` -- range query for calendar events.
  - `create_calendar_event(event)` -- insert calendar event and return it.
  - `update_calendar_event(event)` -- update calendar event by id.
  - `delete_calendar_event(id)` -- delete calendar event by id.
  - `mark_notification_seen(id)` -- mark single notification as seen (toast clicks).
  - `mark_notification_read(id)` -- mark single notification as read.
  - `mark_all_notifications_read` -- mark all notifications as read.
  - `list_notifications` -- return all notifications with read status.
  - `delete_notification(id)` -- delete single notification.
  - `delete_read_notifications` -- bulk delete all read notifications.
  - `format_user_error(err)` -- converts technical error strings to friendly messages (Timeout, Connection, Auth/401, rate_limit/429, max_steps, Budget, RiskThresholdExceeded). `simplify_error()` strips Rust enum formatting for the fallback case.
  - `AgentProgress` struct with `detail: Option<String>` field -- carries tool arguments/result summaries (truncated to 200 chars).
  - `TauriAuditor` -- wraps `InMemoryAuditor`, emits `agent-progress` Tauri events on each step. Extracts meaningful `detail` from step output: shell_execute -> stdout, read_file/write_file -> path, list_directory -> path, errors -> error text, completion -> response preview. `truncate_detail()` compacts newlines and truncates (UTF-8 safe via `char_indices`).
  - `extract_key_terms()` -- extracts key terms from user messages for broader memory recall (stop word filtering, Spanish + English).
  - `judge_worth_remembering()` -- LLM judge for smart auto-remember (60s timeout, Cheap profile). Only stores distilled summaries of meaningful interactions.
  - **Auto-inject memory**: uses full message + individual key terms for broader memory coverage before agent execution.
  - **Auto-remember**: after agent completes, LLM judge evaluates whether the interaction is worth remembering, stores distilled summary if so.
  - 8 new memory management commands: `list_memories`, `update_memory`, `delete_memory`, `list_entities`, `list_relations`, `update_entity`, `delete_entity`, `delete_relation`.
  - 6 agent profile commands:
    - `list_agent_profiles` -- returns all profiles + per-arc assignment for the current arc.
    - `set_arc_profile(arc_id, profile_id)` -- assigns a profile to an arc; subsequent `send_message` calls resolve the profile + persona templates and pass `ResolvedAgentProfile` into the executor.
    - `create_agent_profile(input)` / `update_agent_profile(input)` -- accepts `AgentProfileInput` (snake_case serde rename), saves via `ProfileStore`. Update is allowed for built-ins; the store preserves the `builtin` flag and `created_at`. Re-reads after save so the response carries server-side timestamps.
    - `delete_agent_profile(id)` -- user-authored only; built-ins are guarded.
    - `restore_agent_profile(id)` -- reverts a built-in to its canonical seed values via `SqliteProfileStore::restore_builtin`.
  - `send_message` also calls `force_release_all()` on the dispatcher when `dispatch_next()` returns None, then retries. Prevents "No agent available" errors from stale assignments.
  - `spawn_stream_forwarder(app_handle, arc_id)` -- spawns a background task that reads from `mpsc::UnboundedReceiver<String>` and emits `agent-stream` Tauri events with `{ delta, is_final, arc_id, is_thinking }` payload. Takes `arc_id: Option<String>` and includes it in every event so the frontend only renders streaming bubbles for the active arc. Emits `is_final: true` when the channel closes. **`is_thinking` field** forwarded from agent streaming for thinking block UI.
- `src/app_tools.rs`: `AppToolRegistry` -- wraps `ShellToolRegistry` + adds 4 calendar tools backed by `CalendarStore` + optional `memory: Option<Arc<Memory>>` for persistent `memory_store`/`memory_recall` tool overrides + optional `mcp: Option<Arc<dyn McpClient>>` for MCP-exposed tools + optional `file_gate: Option<Arc<FileGate>>` for path-gated file access. Implements `ToolRegistry` trait. Agent has 10 tools (6 shell + 4 calendar) + dynamically added MCP tools prefixed with `<mcp_id>__`. MCP tools and file-touching tools routed through `file_gate.rs` for directory-access control. Respects hexagonal architecture: no new deps on athen-agent, tools injected via composition root. 15 integration tests covering full CRUD, partial updates, range filtering, error cases.
- `src/file_gate.rs`: Path-based permission gate for file-touching tools. `FileGate` struct sits between agent tool calls and underlying executors (built-in `tokio::fs` ops, Files MCP). Every call carrying a path is routed through `PathRiskEvaluator`, which classifies the access into four bands (Allow/GrantPrompt/QueryFirst/Deny). Wires `GrantStore` (directory grants per arc) + `PathRiskEvaluator` (from `athen-risk`). Returns `ToolResult` with guidance or auto-execution based on risk assessment. Integrates system paths (never writable) + arc grants + contextual risk.
- `src/contacts.rs`: Contacts UI and management commands. 6 commands: `list_contacts`, `get_contact`, `set_contact_trust`, `block_contact`, `unblock_contact`, `delete_contact`. `ContactInfo` and `IdentifierInfo` serialization types for Tauri IPC.
- `src/notifier.rs`: Notification orchestrator and channel implementations.
  - `InAppChannel` -- emits `notification` Tauri events to the frontend
  - `TelegramChannel` -- sends notifications via `athen_sentidos::telegram::send_message` to the owner's chat
  - `NotificationOrchestrator` -- manages delivery across channels with:
    - User presence detection (AtomicBool set by Tauri window focus events)
    - Quiet hours suppression (overnight ranges, critical bypass)
    - Channel selection based on presence (InApp when present, external when away)
    - Escalation with `CancellationToken` from `tokio-util` (try next channel after timeout)
    - LLM humanization (Cheap profile, 30s timeout) -- rephrases raw notification text into natural language before delivery
    - SQLite persistence via `NotificationStore` -- survives app restarts
    - `load_persisted()` restores notifications on startup
    - Delete support: `delete_notification()` single, `delete_read_notifications()` bulk
    - `mark_arc_read()` -- auto-marks notifications as read when user switches to the related arc
  - `NotificationInfo` serializable type for frontend
  - 23 tests (channel selection, fallback, escalation, quiet hours, persistence, delete, load_persisted)
- `src/sense_router.rs`: Generic sense-to-arc router that processes any `SenseEvent` through:
  - **LLM triage**: classifies event relevance as ignore/low/medium/high. Only medium+ reach the user. Spam/low-priority events are silently logged.
  - **Arc creation**: LLM generates descriptive arc names (e.g. "Meeting with John", "Server alert") based on event content.
  - **ArcEntry persistence**: stores event as `ArcEntry` with source-specific `EntryType` and metadata JSON.
  - **Context messages**: `build_context_message()` adds a system message to the Arc so the agent has full context when user opens it. Calendar events get "The user may ask you about this event, want to reschedule it, or need help preparing for it."
  - **Calendar formatting**: `format_calendar_body()` builds readable text from calendar event JSON fields (title, times, location, status like "Starting in 3h 6m"). Calendar events default sender to "Calendar". Body extraction handles calendar's structured JSON.
  - **Frontend emission**: emits `sense-event` Tauri event for real-time UI updates.
  - **Fallback**: if LLM fails or times out, defaults to "medium" relevance (better to show than miss).
  - **Time-window grouping**: `find_recent_arc_from_source()` -- when LLM triage wants a new arc, checks for a recent active arc from the same source updated within 5 minutes. If found, merges into it instead of creating a new arc. Prevents rapid-fire messages (e.g. Telegram) from spawning separate arcs. 1 test.
  - Works for email, calendar, messaging, telegram, or any future sense source. Replaces sense-specific triage code.
  - **Profile routing**: `route_new_arc_to_profile()` runs after triage when a new arc is created. Calls `classify_task` to derive a `ClassifiedTask`, embeds the query text via `profile_embedder`, lazy-caches per-profile embeddings keyed by `(id, updated_at)`, and calls `pick_profile_blended` to score profiles. Result is persisted via `ProfileStore::set_arc_profile`. Falls back to keyword-only routing on any embedding error. `process_sense_event` and `route_new_arc_to_profile` carry `#[allow(clippy::too_many_arguments)]` because they thread the embedder + cache through.
- `src/settings.rs`: Settings management commands:
  - `get_settings` -- loads `~/.athen/models.toml`, returns `SettingsResponse` with provider list (sorted: active first), active provider ID, security mode. Shows env var keys with `(env)` hint.
  - `save_provider(id, base_url, model, api_key)` -- saves/updates provider to `~/.athen/models.toml`. API key handling: `None` preserves existing, `Some("")` removes, `Some("sk-...")` updates. **Hot-reloads** when saving the active provider: builds new router and swaps via `RwLock`.
  - `delete_provider(id)` -- removes provider. If deleting the active provider, automatically switches to first remaining or "deepseek" fallback, hot-reloads router.
  - `test_provider(id, base_url, model, api_key)` -- tests connectivity. Provider-specific: Ollama -> `GET /api/tags`, llama.cpp -> `GET /health`, Anthropic -> `POST /v1/messages`, others -> `POST /v1/chat/completions`. 15-second timeout.
  - `set_active_provider(id)` -- switches active provider at runtime. Builds new router, swaps via `RwLock`, persists choice to `~/.athen/models.toml` under `assignments.active_provider`. Cloud providers require API key (checks config then env var).
  - `save_settings(security_mode)` -- saves security mode (bunker/assistant/yolo) to `~/.athen/config.toml`.
  - `save_email_settings(server, port, username, password, use_tls, folders, poll_interval, lookback_hours)` -- saves email/IMAP config to `~/.athen/config.toml`. Restarts email monitor if enabled.
  - `test_email_connection(server, port, username, password, use_tls)` -- tests IMAP connectivity with provided credentials.
  - `save_telegram_settings(enabled, bot_token, owner_user_id, allowed_chat_ids, poll_interval_secs)` -- saves Telegram bot config to `~/.athen/config.toml`. Restarts telegram monitor if enabled.
  - `test_telegram_connection(bot_token)` -- tests Telegram bot API connectivity via `getMe` endpoint.
  - `save_embedding_settings(mode, provider, model, base_url, api_key)` -- saves embedding config to `~/.athen/config.toml`.
  - `test_embedding_provider(provider, model, base_url, api_key)` -- tests embedding provider connectivity.
  - `get_notification_settings` -- returns notification config (preferred channels, escalation timeout, quiet hours).
  - `save_notification_settings(preferred_channels, escalation_timeout_secs, quiet_hours)` -- saves notification config to `~/.athen/config.toml`.
  - `EmbeddingSettingsInfo` response type for embedding settings UI.
  - Helper types: `ProviderInfo` (id, name, type, base_url, model, has_api_key, api_key_hint, is_active), `SettingsResponse` (includes `TelegramSettingsInfo` with the actual `bot_token` so fields populate on reload), `TestResult`. `mask_api_key()` shows first 3 + last 4 chars.
- `src/process.rs`: Child process lifecycle management (stub).
- **Email monitor wiring**: On app launch, if `email.enabled = true` in config, starts `EmailMonitor` in background. Polls IMAP at configured interval, feeds `SenseEvent`s through the sense router for LLM triage and arc creation.
- **Calendar monitor wiring**: Always starts on app launch (no enable flag -- it just checks the local DB). `start_calendar_monitor()` background task polls every 60s, fires `SenseEvent`s through the sense router for reminder notifications and arc creation.
- **Telegram monitor wiring**: On app launch, if `telegram.enabled = true` in config, starts `TelegramMonitor` in background. Owner messages skip triage and go directly to agent execution with responses sent back via Telegram `sendMessage` API.
- `tauri.conf.json`: Window 900x700, `frontendDist` points to `../../frontend`. **`"withGlobalTauri": true`** required in `app` section to inject `window.__TAURI__` into the webview.
- `frontend/index.html`: Full app layout with sidebar, chat area, settings page, and calendar view:
  - **Sidebar**: arc list with `+ New Arc` button, Settings, Calendar, and Contacts buttons at bottom, hamburger toggle for mobile. Arc list populated dynamically from `list_arcs`. Arcs show source icons (message/email/calendar/system) and branch indicators for child arcs.
  - **Chat area**: header with logo + "New Arc" button + per-arc agent profile picker (`#arc-profile-picker`, calls `set_arc_profile` on change), message container, input form with send/stop buttons, status bar.
  - **Profile manager**: Settings → Agent Profiles section listing all profiles with edit/clone/delete (user-authored) or edit/clone/restore-default (built-ins). Modal editor for create/edit/clone with: display name, id, description, persona addendum textarea, domain/task-kind/avoid chip grids, free-form strengths input, model profile hint. Saves via `create_agent_profile` / `update_agent_profile`. Restore button calls `restore_agent_profile`.
  - **Stop button**: red square, initially hidden, shown during processing. Calls `cancel_task`.
  - **Settings page**: provider cards area, "Add Provider" button with template dropdown (DeepSeek, OpenAI, Anthropic, Ollama, llama.cpp, Custom), security mode selector (Assistant/Bunker/YOLO), email settings section (IMAP config, test connection, app password hints), telegram settings section (bot token, owner user ID, allowed chat IDs, poll interval, test connection, enable/disable toggle), notification settings section (channel priority, escalation timeout, quiet hours), embedding settings section (simple/advanced toggle), back button.
  - **Notifications**: Notifications button in sidebar with unread count badge, notifications-view container for full notification list.
  - **Calendar view**: full `#calendar-view` container with header (prev/next/today/view-select), calendar grid, event modal with all fields (title, date, time, all-day, location, description, category dropdown, color picker, reminder dropdown, recurrence dropdown).
  - **Memory view**: Memory tab with sidebar button, two sub-tabs (Memories + Knowledge Graph). Memories: search, inline editing (re-embeds on save), delete. Knowledge Graph: expandable entity cards with relations and metadata.
- `frontend/styles.css`: Dark theme (Tokyo Night-inspired). Top of `:root` declares `color-scheme: dark` so WebKitGTK renders native `<select>` popups on dark surfaces (fix for white-on-white dropdowns in agent picker / onboarding / memory mode). Global `select, option` overrides set `var(--surface-2)` background + `var(--text)` color. Profile manager styles: `.profile-list`, `.profile-card`, `.profile-card-actions`, `.profile-modal`, `.profile-chip-grid`, `.profile-chip`. Header profile picker: `.header-right`, `.profile-picker-wrap`, `.profile-picker-label`, `.profile-picker`. `.mcp-card-icon` is now a flex container that holds an inline SVG (folder/globe/terminal/database/mail/calendar) instead of literal text. Sidebar with arc items (source icons, branch indicators, rename/delete on hover, notification dots with pulse animation), tool execution cards with status icons (check/cross/spinner) and fade-in animation, streaming message bubbles, chat bubbles with avatars, risk badges, code blocks with language labels, approval dialog, sense event cards with relevance badges, settings page with provider cards (expand/collapse), email settings section, telegram settings section, notification settings section, and embedding settings section, contacts view with trust badges (color-coded: Unknown=red, Neutral=gray, Known=yellow, Trusted=green, AuthUser=blue), calendar view (month grid, week grid, event pills, day cells, modal, color picker dots -- ~350 lines), toast container and notification-toast styles (urgency variants: low=blue, medium=amber, high=orange, critical=red), notification-item styles (unread blue border, read dimmed), notif-badge (red pill), channel-order-item, time-input, advanced-toggle, api-key-field, auto-growing textarea, stop button (red), **thinking block styles** (collapsible details), mobile responsive.
- `frontend/app.js`: Full chat frontend with:
  - **Streaming**: listens for `agent-stream` Tauri events (now with `arc_id` and `is_thinking` fields). Creates streaming bubble on first chunk only if `arc_id` matches `activeArcId`; background arcs get a pulsing blue notification dot on the sidebar instead. Appends text progressively via `textContent` (safe, fast). On `is_final`, re-renders full text with markdown for proper formatting, resets `streamingBubble` and `streamingText` to prevent multiple rapid streams from merging. Tracks `streamingBubble`, `streamingText`, `didReceiveStreamChunks` state. **Thinking content**: `is_thinking` chunks rendered in a collapsible `<details>` block.
  - **Tool execution cards**: listens for `agent-progress` events. Creates `tool-steps-container` div, appends `tool-execution-card` elements with status class (completed/failed/in-progress), status icon (checkmark/cross/dot), tool name, and truncated detail text. Cards have fade-in CSS animation.
  - **Arc sidebar**: `loadArcs()` fetches arc list, `renderArcList()` builds sidebar items with name, source icon (message/email/calendar/system), relative date, entry count badge, and branch indicator for child arcs. Double-click or pencil icon to rename (inline contenteditable). Delete button with confirmation. Branch button creates child arc. Merged arcs hidden from sidebar. Auto-names new arcs from first user message (~30 chars). Active arc highlighted. Timeline toggle button in sidebar header.
  - **Arc timeline view**: Full-screen time-aligned multi-lane graph, toggled via button in sidebar. Each arc is a vertical column (most recently active = rightmost). Each entry is a colored node (blue=message, amber=tool, purple=email, green=calendar, gray=system) positioned by timestamp. Vertical rail lines connect nodes within each arc. Sticky column headers show source icon, arc name, entry count -- click to open. Time axis on left margin with relative labels ("Now", "5m ago", "Yesterday"). Hover nodes for content preview tooltips. Entries within 2 minutes are grouped into the same row. Merged arcs dimmed (40% opacity), archived at 60%. "Back" and "+ New Arc" buttons in header. Auto-refreshes every 30 seconds. Backend: `get_timeline_data` command returns all arcs with their entries in a single call. Full-screen overlay (`position: fixed; z-index: 100`). Five-view toggle: chat <-> settings <-> timeline <-> calendar <-> contacts.
  - **Sense event cards**: listens for `sense-event` Tauri events. Renders source-specific icons, LLM relevance badges (Important/Urgent), agent's reasoning text, and context-aware action buttons (Summarize/Draft Reply/Add to Calendar/Open Arc). Calendar events get "What should I prepare?" button instead of "Summarize"; action buttons switch to event's Arc first before sending prompt; prompts are short (agent has context from system message).
  - **Calendar view**: `showCalendar()` / `hideCalendar()` view switching. Month view (7-column grid) and week view (time grid with positioned events). Event modal for create/edit/delete. Category-to-color mapping.
  - **Kill switch**: stop button (red square) replaces send button during processing. Calls `cancel_task` command. Escape key also cancels. `isProcessing` flag controls button visibility.
  - **Error handling**: `format_user_error()` produces friendly messages. Retry button for transient errors (stores `lastMessage`, `retryLastMessage()` re-submits). "Open Settings" link for auth errors.
  - **Markdown renderer** (inline, no dependencies): fenced code blocks with language labels, inline code, headers (h1-h3), ordered/unordered lists, bold, italic, links. Code blocks protected from inline transformations.
  - **XSS protection**: user messages use `textContent` (never innerHTML), assistant messages go through markdown renderer with `escapeHtml()` on code blocks.
  - **Real-time progress**: status bar shows "Step N: tool_name (status)" and "Evaluating risk..." during risk phase.
  - **Approval dialog**: shows risk badge, score, description, approve/deny buttons.
  - **Contacts view**: sidebar button, contact cards with color-coded trust badges (Unknown=red, Neutral=gray, Known=yellow, Trusted=green, AuthUser=blue), expandable details, trust level dropdown, block/unblock/delete actions.
  - **Arc notification dots**: `arcsWithNotifications` Set, `markArcWithNotification()`, CSS pulse animation on sidebar items. Dot cleared when switching to that arc.
  - **`arc-updated` event listener**: refreshes sidebar when background Telegram execution completes.
  - **Toast notification system**: listens for `notification` Tauri events. Slide-in toasts with urgency-based styling (low=blue, medium=amber, high=orange, critical=red). Auto-dismiss for Low/Medium urgency. Click-to-open-arc navigation. Mark-seen on click via `mark_notification_seen` command.
  - **Notifications view**: full notification list with read/unread state. Delete per item, "Clear read" bulk delete, "Mark all read" button. Notification badge on sidebar (unread count, auto-updates via polling). Seven-view toggle: chat <-> settings <-> timeline <-> calendar <-> contacts <-> notifications <-> memory.
  - **Settings page**: loads providers via `get_settings`, renders provider cards with expand/collapse. Edit fields for base URL, model, API key (masked display, show/hide toggle). "Test Connection" and "Save" buttons per provider. "Set Active" button to switch provider. "Delete" with confirmation. Add provider via template selection. Security mode dropdown with contextual hints. Email settings section with IMAP server/port/username/password fields, TLS toggle, test connection button, and app password hints for Gmail/Outlook. Telegram settings section with bot token field (populated on reload), owner user ID, allowed chat IDs, poll interval, test connection, enable/disable toggle. Notification settings section with channel priority list, escalation timeout, quiet hours (start/end time inputs, allow-critical toggle). Embedding settings section with simple mode dropdown (Automatic/Cloud/Local Only/Specific/Off) + advanced toggle revealing provider/model/URL/API key fields with test connection button.
  - **Agent profile picker** (header) and **profile manager** (Settings):
    - `loadAgentProfiles()` fetches all profiles + per-arc assignment; `renderProfilePicker()` populates the header dropdown; `onProfileChange()` calls `set_arc_profile`.
    - `loadProfileManager()` + `renderProfileList()` + `buildProfileCard()` render the manager. Built-ins get a "Restore default" button; Edit is enabled for all (built-in edits are persisted but the `builtin` flag is preserved).
    - `openProfileEditor(mode, profile?)` opens the modal in create/edit/clone mode. Chip grids built from `PROFILE_DOMAINS` / `PROFILE_TASK_KINDS` constants. `saveProfileFromEditor()` builds the snake_case `AgentProfileInput` and calls `create_agent_profile` / `update_agent_profile`.
    - `mcpIconSvg(name)` maps icon name → inline SVG (folder/globe/terminal/database/mail/calendar) so MCP cards render an icon instead of literal text like "Folder".
  - **Auto-growing textarea**: expands with content up to 150px. Enter sends, Shift+Enter for newline.
  - **Smooth scroll**: `requestAnimationFrame` + `scrollTo` on new messages and tool cards.
- **Requires system libraries**: `webkit2gtk4.1-devel gtk3-devel libsoup3-devel libappindicator-gtk3-devel` (Fedora).
- **Verified working**: Full multi-step agentic pipeline with streaming confirmed. Tested: (1) "What tools do you have?" -> LLM correctly lists its 10 tools (6 shell + 4 calendar), (2) "Read https://alejandrogarcia.blog/ and write to HELLO.md" -> LLM uses `shell_execute` (curl) to fetch website, then `write_file` to save formatted markdown. Streaming renders progressively with markdown finalization. Tool execution cards show in real time with status icons. Arc persistence across app restarts. Provider hot-swap works without restart. Settings UI tested for add/edit/delete/test/activate providers. Email monitor tested with real IMAP server. Calendar monitor fires reminders for upcoming events. Telegram monitor: owner messages trigger direct agent execution with responses sent back to Telegram; non-owner messages triaged through sense router. Contacts UI: trust levels, block/unblock, delete working. Memory system: auto-inject context from past interactions, auto-remember meaningful conversations via LLM judge, full memory management UI (search, edit, delete memories and knowledge graph entities).

### mcp-filesystem (0 tests)
**Status**: Stub -- entry point only. Standalone MCP server (no athen-core dependency).

---

## Integration Tests (28 integration tests + 15 app_tools tests + 23 notifier tests + memory management tests)

Integration tests wire real implementations together (no mocks except LLM).

### athen-coordinador/tests/integration_pipeline.rs (4 tests)
- User input flows through coordinator to create task with correct domain/priority/status
- Dangerous commands ("sudo rm -rf") get blocked by real rule engine
- Dispatch assigns tasks to registered agents, release cycle works
- Priority ordering: High (UserInput) dispatched before Normal (Email)

### athen-ipc/tests/integration_ipc.rs (5 tests)
- Monitor sends SenseEvent to coordinator via IPC server/client
- Coordinator broadcasts HealthPing to multiple agent clients
- Coordinator routes TaskAssignment to specific agent only
- Client reconnection after disconnect
- 5 concurrent clients sending simultaneously -- no messages lost

### athen-persistence/tests/integration_persistence.rs (5 tests)
- Full task lifecycle (create->update steps->checkpoint->complete->filter)
- Checkpoint survives simulated crash (file-based DB, drop, reopen)
- Pending message queue ordering (FIFO, pop atomicity, no re-pop)
- 10 concurrent task operations -- no corruption
- CheckpointManager file atomicity (temp->fsync->rename)

### athen-risk/tests/integration_risk.rs (6 tests)
- Same action, different trust levels -> proportional score changes
- Trust evolution (5 approvals upgrades T0->T1->T2) reduces risk over time
- Rule engine: AuthUser vs Unknown sender for dangerous commands
- Data sensitivity escalation (Plain->PersonalInfo->Secrets)
- Uncertainty penalty impact on otherwise safe actions
- CombinedEvaluator chooses rules vs LLM based on pattern match

### athen-memory/tests/integration_memory.rs (5 tests)
- Knowledge graph: build contact network, explore at depth 1 vs 2
- Vector search: cosine similarity ranking with known embeddings
- Memory facade: remember/recall/forget lifecycle
- SQLite persistence across connection drop/reopen
- Graph exploration respects max_nodes, max_depth, relevance_threshold

### athen-agent/tests/integration_agent.rs (3 tests)
- Mock LLM returns shell_execute tool call -> real ShellToolRegistry runs `echo hello` -> result fed back -> LLM completes
- Mock LLM requests read_file -> real tool reads temp file -> correct content returned
- Multi-step: tool call -> result -> another tool call -> result -> final answer

---

## Running the CLI

```bash
# Set API key and run
DEEPSEEK_API_KEY=sk-... cargo run -p athen-cli --release

# Or build first, then run the binary directly
cargo build -p athen-cli --release
DEEPSEEK_API_KEY=sk-... ./target/release/athen-cli
```

The CLI reads from stdin, processes through the full pipeline (coordinator -> risk -> dispatch -> LLM), and prints responses. Exit with Ctrl+D, `/quit`, or `/exit`.
