# Architecture, Core Types & Security

## Architecture Overview

Multi-process architecture. Each major component runs as its own process, communicating over IPC (Unix sockets on Linux/macOS, Named pipes on Windows).

```
┌─────────────────────────────────────────────────────────────┐
│                   SENTIDOS (Monitors)                        │
│  Polling external APIs: Email, Calendar, Messaging, User    │
│  Each runs as a separate process                            │
└─────────────────────┬───────────────────────────────────────┘
                      │ IPC (normalized SenseEvents)
                      ▼
┌─────────────────────────────────────────────────────────────┐
│                    SENSE ROUTER (Tauri app)                   │
│  LLM triage (relevance) → Arc creation → ArcEntry storage  │
│  Generic: works for email, calendar, messaging, any sense   │
│  Sends notifications via NotificationOrchestrator            │
└─────────────────────┬───────────────────────────────────────┘
                      │ SenseEvents (medium+ relevance)
                      ▼
┌─────────────────────────────────────────────────────────────┐
│                    COORDINADOR (Coordinator)                  │
│  Singleton process. Receives events, evaluates risk,         │
│  prioritizes, dispatches to agent workers.                   │
│  Contains: Router, RiskEvaluator, TaskQueue, Dispatcher     │
└─────────────────────┬───────────────────────────────────────┘
                      │ IPC (TaskAssignments)
        ┌─────────────┼─────────────┐
        ▼             ▼             ▼
┌───────────┐  ┌───────────┐  ┌───────────┐
│  Agent 1  │  │  Agent 2  │  │  Agent N  │
│  LLM +    │  │  LLM +    │  │  LLM +    │
│  Tools    │  │  Tools    │  │  Tools    │
└─────┬─────┘  └─────┬─────┘  └─────┬─────┘
      │              │              │
      └──────────────┼──────────────┘
                     ▼
┌─────────────────────────────────────────────────────────────┐
│                    EXECUTION LAYER                            │
│  Tools: MCPs (Rust binaries) + Shell (Nushell/native) +     │
│         Scripts (Python) + HTTP APIs                         │
│  Sandboxed by risk level (OS-native or container)           │
└─────────────────────────────────────────────────────────────┘
```

---

## Core Types (athen-core)

### SenseEvent (`event.rs`)
The normalized event that flows from monitors to the coordinator:
```rust
SenseEvent {
    id: Uuid,
    timestamp: DateTime<Utc>,
    source: EventSource,        // Email, Calendar, Messaging, UserInput, System
    kind: EventKind,            // NewMessage, UpdatedMessage, Reminder, Command, Alert
    sender: Option<SenderInfo>, // Identifier + optional ContactId
    content: NormalizedContent, // Summary + body (JSON) + attachments
    source_risk: RiskLevel,     // Initial risk from the sense source
    raw_id: Option<String>,     // Deduplication ID from source system
}
```

### Task (`task.rs`)
A unit of work assigned to an agent:
```rust
Task {
    id: TaskId (Uuid),
    domain: DomainType,         // Base, Communication, Code, Agenda, Files, Research
    priority: TaskPriority,     // Background, Low, Normal, High, Critical
    status: TaskStatus,         // Pending, AwaitingApproval, InProgress, Paused, Completed, Failed, Cancelled
    risk_score: Option<RiskScore>,
    risk_budget: Option<u32>,   // Max cumulative risk allowed
    risk_used: u32,             // Risk consumed so far
    steps: Vec<TaskStep>,       // Plan with per-step status, output, checkpoints
    assigned_agent: Option<AgentId>,
    source_event: Option<Uuid>, // Which SenseEvent triggered this
}
```

### RiskScore (`risk.rs`)
The dynamic risk evaluation result:
```rust
RiskScore = (Ibase * Morigen * Mdatos) + Pincertidumbre

Where:
- Ibase (BaseImpact):     Read=1, WriteTemp=10, WritePersist=40, System=90
- Morigen (TrustLevel):   AuthUser=0.5x, Trusted=1.0x, Known=1.5x, Neutral=2.0x, Unknown=5.0x
- Mdatos (DataSensitivity): Plain=1.0x, PersonalInfo=2.0x, Secrets=5.0x
- Pincertidumbre:         Based on LLM confidence (1.0->0, 0.5->25, 0.1->81)

Decision thresholds:
- 0-19:  SilentApprove  (execute, debug log)
- 20-49: NotifyAndProceed (execute, push notification)
- 50-89: HumanConfirm (pause, wait for approval)
- 90+:   HardBlock (reject automatically)
```

### RiskLevel
```
L1 (Safe):     Read-only, analysis -- silent execution
L2 (Caution):  Local reversible writes -- notify user
L3 (Danger):   External/irreversible -- require confirmation click
L4 (Critical): Financial/system config -- confirmation with challenge
```

### Contact & Trust (`contact.rs`)
```
T0 (Unknown):  5.0x multiplier -- new/external senders
T1 (Neutral):  2.0x -- in contacts, no history
T2 (Known):    1.5x -- positive interaction history
T3 (Trusted):  1.0x -- explicitly marked trusted
T4 (AuthUser): 0.5x -- the authenticated user themselves
```

### ToolBackend (`tool.rs`)
Tools can execute through multiple backends:
```rust
ToolBackend::NativeMcp { binary_path }   // Compiled Rust MCP binary (stdio JSON-RPC)
ToolBackend::Shell { command, native }    // Nushell (cross-platform) or native shell
ToolBackend::Script { runtime, source }   // Python execution
ToolBackend::HttpApi { endpoint, method, auth }  // Direct HTTP API calls
```

### SandboxLevel (`sandbox.rs`)
Tiered isolation based on risk:
```rust
SandboxLevel::None                        // L1 read-only actions
SandboxLevel::OsNative { profile }        // L2: bwrap/landlock (Linux), sandbox-exec/Seatbelt (macOS), Job Object + AppContainer (Windows)
SandboxLevel::Container { image, mounts } // L3+: Podman/Docker
```

### IPC Messages (`ipc.rs`)
All inter-process communication uses `IpcMessage` envelopes with `IpcPayload` variants:
- `SenseEvent` (Monitor -> Coordinator)
- `TaskAssignment` (Coordinator -> Agent)
- `TaskProgress` (Agent -> Coordinator)
- `TaskControl` (Coordinator -> Agent: Continue/Pause/Cancel)
- `Registration` (Any -> Coordinator on startup)
- `HealthPing`/`HealthPong` (Coordinator <-> All)
- `ApprovalRequest`/`ApprovalResponse` (Coordinator <-> UI)
- `StateUpdate` (Coordinator -> UI)
- `UserCommand` (UI -> Coordinator)

### Configuration (`config.rs`)
TOML-based configuration:
- `OperationMode`: AlwaysOn, WakeTimer, CloudRelay
- `SecurityMode`: Bunker (everything L2+ needs approval), Assistant (standard), Yolo (only L4)
- `ModelsConfig`: Providers (keyed `HashMap<String, ProviderConfig>`), Bundles (named per-tier loadouts, one active), domain assignments
- `DomainConfig`: Per-domain model profile, max steps, timeout, custom options
- `EmailConfig`: enabled, imap_server, imap_port, username, password, use_tls, folders, poll_interval_secs, lookback_hours
- `TelegramConfig`: enabled, bot_token, owner_user_id (Option<i64>), allowed_chat_ids (Vec<i64>), poll_interval_secs (default 5). Added as `telegram: TelegramConfig` field on `AthenConfig` with `#[serde(default)]`.

### Bundles (`config.rs`) — SHIPPED
Named per-tier `(connection, slug)` loadouts replace the legacy `active_provider + tier_models` flat structure.

```rust
Bundle {
    id: Uuid,
    name: String,
    tiers: HashMap<ModelProfile, BundleTier>,  // sparse; missing tiers fall back along a ladder
}

BundleTier {
    connection_id: String,   // references a key in models.providers
    slug: String,            // wire-format model slug
}
```

The active bundle id is stored in `models.assignments["active_bundle"]`. On first load after upgrade, `AthenConfig::synthesize_default_bundle_if_empty()` migrates existing `active_provider + tier_models` config into a single "Default" Bundle so no user action is required. Cross-vendor mixing is first-class — each tier in a bundle can reference a different connection. See `docs/BUNDLES.md`.

---

## Trait Contracts (athen-core::traits)

These are the interfaces that define how components interact. Implementations go in the respective crates.

### SenseMonitor (`traits/sense.rs`)
```rust
trait SenseMonitor: Send + Sync + 'static {
    fn sense_id(&self) -> &str;
    async fn init(&mut self, config: &AthenConfig) -> Result<()>;
    async fn poll(&self) -> Result<Vec<SenseEvent>>;
    fn poll_interval(&self) -> Duration;
    async fn shutdown(&self) -> Result<()>;
}
```
Implemented in `athen-sentidos` for each sense type.

### EventRouter (`traits/coordinator.rs`)
```rust
trait EventRouter: Send + Sync {
    async fn route(&self, event: SenseEvent) -> Result<Vec<Task>>;
}
```
Classifies events and creates tasks. Implemented in `athen-coordinador`.

### RiskEvaluator (`traits/coordinator.rs`)
```rust
trait RiskEvaluator: Send + Sync {
    async fn evaluate(&self, task: &Task, context: &RiskContext) -> Result<RiskScore>;
    fn requires_approval(&self, score: &RiskScore) -> bool;
}
```
Two-step: fast regex rules first, LLM fallback for ambiguous cases. Implemented in `athen-risk`.

### TaskQueue (`traits/coordinator.rs`)
```rust
trait TaskQueue: Send + Sync {
    async fn enqueue(&self, task: Task) -> Result<TaskId>;
    async fn dequeue(&self) -> Result<Option<Task>>;
    async fn update_status(&self, id: TaskId, status: TaskStatus) -> Result<()>;
    async fn pending_count(&self) -> Result<usize>;
}
```
Priority queue for tasks. Implemented in `athen-coordinador`.

### AgentExecutor (`traits/agent.rs`)
```rust
trait AgentExecutor: Send + Sync {
    async fn execute(&self, task: Task) -> Result<TaskResult>;
}
```
LLM-driven task execution loop. Implemented in `athen-agent`.

### StepAuditor (`traits/agent.rs`)
```rust
trait StepAuditor: Send + Sync {
    async fn record_step(&self, task_id: TaskId, step: &TaskStep) -> Result<()>;
    async fn get_steps(&self, task_id: TaskId) -> Result<Vec<TaskStep>>;
}
```

### TimeoutGuard (`traits/agent.rs`)
```rust
trait TimeoutGuard: Send + Sync {
    fn remaining(&self) -> Duration;
    fn is_expired(&self) -> bool;
}
```

### ResourceMonitor (`traits/agent.rs`)
```rust
trait ResourceMonitor: Send + Sync {
    async fn current_usage(&self) -> Result<ResourceUsage>;
    fn is_within_limits(&self) -> bool;
}
```

### LlmProvider (`traits/llm.rs`)
```rust
trait LlmProvider: Send + Sync {
    fn provider_id(&self) -> &str;
    async fn complete(&self, request: &LlmRequest) -> Result<LlmResponse>;
    async fn complete_streaming(&self, request: &LlmRequest) -> Result<LlmStream>;
    async fn is_available(&self) -> bool;
}
```
One implementation per provider in `athen-llm/src/providers/`.

### LlmRouter (`traits/llm.rs`)
```rust
trait LlmRouter: Send + Sync {
    async fn route(&self, request: &LlmRequest) -> Result<LlmResponse>;
    async fn route_streaming(&self, request: &LlmRequest) -> Result<LlmStream>;
    async fn budget_remaining(&self) -> Result<BudgetStatus>;
}
```
Selects provider by profile, handles failover chains, enforces budget. `route_streaming` has a default implementation that falls back to `route()` and returns the full response as a single `LlmChunk`, so existing implementations work without changes. `DefaultLlmRouter` overrides with real streaming (failover + circuit breakers). Implemented in `athen-llm`.

### VectorIndex (`traits/memory.rs`)
```rust
trait VectorIndex: Send + Sync {
    async fn upsert(&self, id: &str, embedding: Vec<f32>, metadata: Value) -> Result<()>;
    async fn search(&self, query_embedding: Vec<f32>, top_k: usize) -> Result<Vec<SearchResult>>;
    async fn delete(&self, id: &str) -> Result<()>;
}
```

### KnowledgeGraph (`traits/memory.rs`)
```rust
trait KnowledgeGraph: Send + Sync {
    async fn add_entity(&self, entity: Entity) -> Result<EntityId>;
    async fn add_relation(&self, from: EntityId, relation: &str, to: EntityId) -> Result<()>;
    async fn explore(&self, entry: EntityId, params: ExploreParams) -> Result<Vec<GraphNode>>;
}
```

### MemoryStore (`traits/memory.rs`)
```rust
trait MemoryStore: Send + Sync {
    async fn remember(&self, item: MemoryItem) -> Result<()>;
    async fn recall(&self, query: &str, limit: usize) -> Result<Vec<MemoryItem>>;
    async fn forget(&self, id: &str) -> Result<()>;
}
```

### ToolRegistry (`traits/tool.rs`)
```rust
trait ToolRegistry: Send + Sync {
    async fn list_tools(&self) -> Result<Vec<ToolDefinition>>;
    async fn call_tool(&self, name: &str, args: Value) -> Result<ToolResult>;
}
```
Unified facade over MCP/Shell/Script/HTTP backends.

### ToolProcessManager (`traits/tool.rs`)
```rust
trait ToolProcessManager: Send + Sync {
    async fn start(&self, tool_name: &str) -> Result<()>;
    async fn stop(&self, tool_name: &str) -> Result<()>;
    async fn is_running(&self, tool_name: &str) -> bool;
}
```

### SandboxExecutor (`traits/sandbox.rs`)
```rust
trait SandboxExecutor: Send + Sync {
    async fn detect_capabilities(&self) -> Result<SandboxCapabilities>;
    async fn execute(&self, command: &str, args: &[&str], sandbox: &SandboxLevel) -> Result<SandboxOutput>;
}
```

### ShellExecutor (`traits/shell.rs`)
```rust
trait ShellExecutor: Send + Sync {
    async fn execute(&self, command: &str) -> Result<SandboxOutput>;
    async fn execute_native(&self, command: &str) -> Result<SandboxOutput>;
    async fn which(&self, program: &str) -> Result<Option<PathBuf>>;
}

// Extension trait for shell-agnostic env/cwd plumbing:
trait ShellExecutorExt: ShellExecutor {
    async fn execute_with(&self, command: &str, opts: ShellOptions) -> Result<SandboxOutput>;
}
struct ShellOptions { env: Vec<(String, String)>, cwd: Option<PathBuf> }
```
`execute_with` carries env vars and working directory as **structured options**, not as `cd … && export … && (cmd)` text wrapped around the user command. Each adapter (nushell, native) applies them via `tokio::process::Command::env()` / `current_dir()`, so the same options work under nushell, cmd, sh, bash, zsh, and pwsh. The previous bash-syntax wrapper silently failed under nushell on Windows because the export-and-chain syntax isn't valid nushell.

### Memory System (VectorIndex, KnowledgeGraph, MemoryStore)
Three complementary traits working together:

**VectorIndex** (`traits/memory.rs:VectorIndex`) — Semantic search over stored knowledge:
```rust
async fn upsert(&self, id: &str, embedding: Vec<f32>, metadata: Value) -> Result<()>;
async fn search(&self, query_embedding: Vec<f32>, top_k: usize) -> Result<Vec<SearchResult>>;
async fn delete(&self, id: &str) -> Result<()>;
```
Implemented in `athen-memory/src/vector.rs` with in-memory brute-force cosine similarity.

**KnowledgeGraph** (`traits/memory.rs:KnowledgeGraph`) — Structured entity relationships:
```rust
async fn add_entity(&self, entity: Entity) -> Result<EntityId>;
async fn add_relation(&self, from: EntityId, relation: &str, to: EntityId) -> Result<()>;
async fn explore(&self, entry: EntityId, params: ExploreParams) -> Result<Vec<GraphNode>>;
```
Entities (Person, Organization, Project, Event, Document, Concept) connected by typed relations. BFS exploration respects `ExploreParams`:
- `max_depth: u8` — Stops graph traversal at this depth (default: 3)
- `max_nodes: u16` — Caps result set (default: 50)
- `recency_weight, frequency_weight, importance_weight` — Edge scoring combines recency (7-day half-life), frequency (via strength), and explicit importance (0.0–1.0)

**Decay & Strength** — Edges track `strength` (0.0–1.0, starts at 0.5, reinforced on use) and `last_used` timestamp. Effective strength decays with half-life 30 days: `strength * exp(-t * ln(2) / 30d)`, never dropping below 0.01. Reinforcement (used in agent context-switching) adds up to 1.0 and updates `last_used` to now.

**MemoryStore** (`traits/memory.rs:MemoryStore`) — Unified facade:
```rust
async fn remember(&self, item: MemoryItem) -> Result<()>;  // Embed + extract entities + graph population
async fn recall(&self, query: &str, limit: usize) -> Result<Vec<MemoryItem>>;  // Hybrid retrieval
async fn forget(&self, id: &str) -> Result<()>;
```
Implemented in `athen-memory/src/lib.rs:Memory` as a composition of vector + graph + embedder + extractor.

**Remember Flow** (3 phases):
1. Embed content and store in vector index
2. Extract entities via `EntityExtractor` (LLM-based: `LlmEntityExtractor`, or fallback to manual metadata parsing)
3. Populate knowledge graph: add entities, create relations with importance weights, store extracted entity names in vector metadata

**Entity Extraction** (`athen-memory/src/extractor.rs:LlmEntityExtractor`) — LLM gate deciding "is this worth remembering?":
- Uses `ModelProfile::Cheap` with 30-second timeout (conservative: empty result on timeout)
- Extracts entities + relations with importance scores (0.9=critical, 0.5=notable, 0.2=minor)
- Graceful fallback: if extraction fails/times out, proceeds with manual metadata parsing

**Recall Flow** (hybrid retrieval):
1. Vector search: fetch `limit * 3` results (cosine similarity)
2. Extract entity names from results → search for related entities
3. Graph-connected results get boosted score: `score * 0.5 + 0.5`
4. Merge, deduplicate, filter by `min_relevance_score` (default 0.3), rank by composite score, return top `limit`

**Edit & Deduplication** — `Memory.update(id, new_content)` re-embeds and upserts. Forget removes from vector index but leaves graph entities intact (no reference counting). Deduplication happens at recall time via ID deduplication in merged result set.

Implementations: `InMemoryVectorIndex` (simple brute-force) and `InMemoryGraph` (BFS with decay). SQLite backends available in `sqlite.rs` for persistence.

### PersistentStore (`traits/persistence.rs`)
```rust
trait PersistentStore: Send + Sync {
    async fn save_task(&self, task: &Task) -> Result<()>;
    async fn load_task(&self, id: TaskId) -> Result<Option<Task>>;
    async fn list_tasks(&self, filter: TaskFilter) -> Result<Vec<Task>>;
    async fn save_checkpoint(&self, task_id: TaskId, data: Value) -> Result<()>;
    async fn load_checkpoint(&self, task_id: TaskId) -> Result<Option<Value>>;
    async fn save_pending_message(&self, msg: &IpcMessage) -> Result<()>;
    async fn pop_pending_messages(&self, limit: usize) -> Result<Vec<IpcMessage>>;
}
```

### Vault (`traits/vault.rs`)
```rust
trait Vault: Send + Sync {
    async fn set(&self, scope: &str, key: &str, value: &str) -> Result<()>;
    async fn get(&self, scope: &str, key: &str) -> Result<Option<String>>;
    async fn delete(&self, scope: &str, key: &str) -> Result<()>;
    async fn list(&self, scope: &str) -> Result<Vec<String>>;
}
```
Encrypted at-rest storage for secrets (API keys, passwords, OAuth tokens). Secrets are addressed by `(scope, key)` where scope is a logical namespace such as `endpoint:jina`, `imap:gmail`, or `oauth:google`. SHIPPED 2026-05-10: Two implementations in `athen-vault`: `KeyringVault` (OS keychain via the `keyring` crate, with a SQLite index for `list`) and `EncryptedFileVault` (chacha20poly1305 with AAD bound to `(scope, key)` to prevent row-swap attacks). Use `athen_vault::open_vault(data_dir, "athen")` — tries keychain first with a sentinel round-trip, falls back to encrypted file on failure.

### ProfileStore (`traits/profile.rs`)
```rust
trait ProfileStore: Send + Sync {
    async fn get_profile(&self, id: &str) -> Result<Option<AgentProfile>>;
    async fn list_profiles(&self) -> Result<Vec<AgentProfile>>;
    async fn save_profile(&self, profile: &AgentProfile) -> Result<()>;
    async fn delete_profile(&self, id: &str) -> Result<()>;
    async fn get_template(&self, id: &str) -> Result<Option<PersonaTemplate>>;
    async fn list_templates(&self) -> Result<Vec<PersonaTemplate>>;
    async fn save_template(&self, template: &PersonaTemplate) -> Result<()>;
    async fn delete_template(&self, id: &str) -> Result<()>;
    async fn resolve_templates(&self, ids: &[TemplateId]) -> Result<Vec<PersonaTemplate>>;
    async fn get_or_default(&self, id: Option<&ProfileId>) -> Result<AgentProfile>;
}
```
Storage for `AgentProfile` and `PersonaTemplate` rows. Implementations seed built-in rows on first use. `delete_profile` / `delete_template` refuse to remove built-ins — they are clonable but not deletable so a "Reset to default" action always has somewhere to land. Implemented in `athen-persistence`.

### IdentityStore (`traits/identity.rs`)
```rust
trait IdentityStore: Send + Sync {
    async fn list_categories(&self) -> Result<Vec<IdentityCategory>>;
    async fn get_category(&self, name: &str) -> Result<Option<IdentityCategory>>;
    async fn upsert_category(&self, category: &IdentityCategory) -> Result<()>;
    async fn delete_category(&self, name: &str) -> Result<()>;
    async fn list_entries(&self, category: Option<&str>) -> Result<Vec<IdentityEntry>>;
    async fn get_entry(&self, id: Uuid) -> Result<Option<IdentityEntry>>;
    async fn upsert_entry(&self, entry: &IdentityEntry) -> Result<()>;
    async fn delete_entry(&self, id: Uuid) -> Result<()>;
    async fn entries_for_profile(&self, profile_id: &str) -> Result<Vec<(IdentityCategory, Vec<IdentityEntry>)>>;
}
```
Storage for user-editable identity (personality, rules, knowledge, team, and custom categories). Implementations seed the four canonical categories on first use. Listing is deterministically ordered by `sort_order` so the static prompt-cache prefix stays valid. `entries_for_profile` is the prompt-builder entry point — filters by `applies_to` tag and groups by category, omitting empty categories. Implemented in `athen-persistence`. See `docs/IDENTITY.md`.

### CheckpointStore (`traits/checkpoint.rs`) — SHIPPED
File-snapshot port for agent action undo. Implemented in `athen-checkpoint` (gix-backed bare git repo). One bare repo shared across the app, one branch per arc, one tag per action, cross-arc blob dedup for free.

```rust
trait CheckpointStore: Send + Sync {
    /// Snapshot pre-state of paths. Returns Some(entry_id) when at least one
    /// path was snapshotted; None when all paths were filtered out.
    async fn snapshot_paths(
        &self, arc_id: &str, entry_id: &str, turn_id: Option<&str>,
        tool_name: &str, args_summary: &str, paths: &[PathBuf],
    ) -> Result<Option<String>>;

    /// Revert a single action by entry_id. Idempotent.
    async fn revert_action(&self, entry_id: &str) -> Result<RevertOutcome>;

    /// Cascade-revert to just before entry_id: restores filesystem state and
    /// drops entry_id + all newer actions. Walks newest-first.
    async fn rewind_to_before(&self, arc_id: &str, entry_id: &str) -> Result<RevertOutcome>;

    /// List action records for an arc, newest first.
    async fn list_actions(&self, arc_id: &str) -> Result<Vec<ActionRecord>>;

    /// Drop snapshot history for an archived arc.
    async fn forget_arc(&self, arc_id: &str) -> Result<()>;
}
```

The agent never sees this layer. `ShellToolRegistry` calls `maybe_snapshot()` before each destructive tool (`write`, `edit`); the UI later calls `revert_action` / `rewind_to_before` when the user clicks Revert in the Changes rail. Failures are logged but never block the tool — a missing snapshot degrades to "Revert unavailable" rather than blocking the command.

`ActionRecord` carries `entry_id`, `turn_id`, `tool_name`, `args_summary`, `created_at`, `paths`, and `reverted` (flag so the UI can grey the button without losing the history row). `RevertOutcome` reports `restored`, `recreated`, `deleted`, and `failed` paths plus `discarded` count.

### SkillStore (`traits/skill.rs`)
```rust
trait SkillStore: Send + Sync {
    async fn list(&self, profile: Option<&str>) -> Result<Vec<Skill>>;
    async fn get(&self, slug: &str) -> Result<Option<Skill>>;
    async fn load_body(&self, slug: &str) -> Result<String>;
    async fn upsert(&self, slug: &str, frontmatter: &SkillFrontmatter, body: &str) -> Result<()>;
    async fn delete(&self, slug: &str) -> Result<()>;
    async fn sync(&self) -> Result<SyncReport>;
}
```
Storage for user-authored procedural playbooks (Claude-Code-style `SKILL.md` folders). Storage is hybrid: bodies live on disk as plain `SKILL.md` files (source of truth, human-editable, git-friendly) and SQLite holds a derived index for cheap listing. Bodies are loaded lazily via `load_body` when the agent calls the `load_skill` tool. `sync` reconciles the SQLite index against the filesystem on boot. User skills shadow bundled skills with the same slug. Implemented in `athen-persistence`. See `docs/SKILLS.md`.

### Provider Pinning (`athen-app/src/state.rs`) — SHIPPED
First-call-wins semantics: the first LLM call on an arc snapshots the active provider id and resolved model slug onto the arc row (`pinned_provider_id`, `pinned_slug` columns in SQLite). Subsequent calls on the same arc read these back, isolating the in-flight task from mid-flight provider switches or `tier_models` edits.

```rust
struct EffectiveProviderTarget {
    provider_id: String,
    pinned_slug: Option<String>,  // None → consult live tier_models (unpinned arcs)
}
```

`resolve_effective_provider_for_arc(arc_store, arc_id, active_provider_id, tier)` resolves the target (reading or writing the pin). `arc_router_for(state, target)` builds a per-arc `LlmRouter` with `override_slug` when a pin is in force so every LLM call on the arc uses exactly the pinned slug. If the pinned provider has been removed from config, the function falls back to the active provider with `pinned_slug: None` (refuse to send a foreign slug to a different provider). See `docs/PROVIDER_PINNING.md`.

### Proactive Hints (`athen-app/src/proactive_hints.rs`) — SHIPPED
Background rules engine that surfaces one-liner nudges when the user's config is missing important integrations. Rate-limited to 1 hint per hour; permanently dismissable per `hint_id`.

```rust
struct ProactiveHint {
    hint_id: String,
    title: String,
    body: String,
    action_panel: Option<String>,   // Settings panel to navigate to
    skill_topic: Option<String>,    // athen_docs topic slug
}
```

Six rules fire in order: `no_calendar_source`, `no_email`, `no_search_key`, `no_telegram`, `embedding_off`, `local_no_family`. `evaluate_rules(ctx)` returns all triggered hints; `ProactiveHintChecker::check_and_emit()` filters out permanently dismissed ones, applies the 1h rate limit, and emits the first actionable hint via `proactive-hint` Tauri event. Also delivers through the notifier for Telegram-away delivery.

`HintDismissalStore` (SQLite, in `athen-persistence`) tracks permanent dismissals. `HintContext` is a pure-data snapshot (config, calendar source count, provider id, is_local_provider flag) with no store references — cheap to construct per check loop.

### Tier Classifier (`athen-app/src/commands.rs`) — SHIPPED
`classify_tier_for_turn(router, user_message, history_digest)` asks the Cheap-tier LLM (5s timeout, falls back to `(None, false)` on any error) to classify an in-app direct message turn:

- **complexity**: `"low"` | `"medium"` | `"high"` — drives tier selection (low→Fast, high→Powerful/Code)
- **is_code_task**: `bool` — true only for reading/writing/debugging source code on a software project

The classifier is consulted in `send_message` before the executor starts so the right model tier is selected per-turn rather than using a single fixed profile for all in-app interactions.

### ArcCompactor (`traits/compaction.rs`)
```rust
trait ArcCompactor: Send + Sync {
    async fn should_compact(&self, arc_id: &str, trigger_tokens: u32) -> Result<bool>;
    async fn compact(&self, arc_id: &str, target_tokens: u32) -> Result<CompactionOutcome>;
    async fn load_context_view(&self, arc_id: &str) -> Result<ArcContextView>;
    // Default method:
    async fn prepare_context(&self, arc_id: &str, trigger_tokens: u32, target_tokens: u32) -> Result<ArcContextView>;
}
```
The executor's gateway into arc history — direct reads of `arc_entries` from the context-build path are forbidden; they bypass the compaction view. `prepare_context` (default impl) chains `should_compact` → optional `compact` → `load_context_view` in one call; compaction failures are swallowed (best-effort) so a stale summary degrades gracefully to "all entries verbatim" rather than blocking dispatch. `compact(arc_id, 0)` forces compaction regardless of budget. Implemented in `athen-app`. See `docs/ARC_COMPACTION.md`.

### EmailSender (`traits/email_sender.rs`)
```rust
trait EmailSender: Send + Sync {
    async fn send(&self, email: &OutboundEmail) -> Result<SentEmail>;
    async fn test_connection(&self) -> Result<()>;
    fn name(&self) -> &'static str;
}
```
Outbound email port for the agent's `email_send` tool. `OutboundEmail` carries `to`/`cc`/`bcc`, subject, plain-text body, optional HTML body (sent as multipart/alternative), and optional `in_reply_to` for threading. `test_connection` is used by the Settings UI "Test SMTP" button without sending a message. Implemented in `athen-sentidos` as `LettreSmtpSender`.

### TelegramSender (`traits/telegram_sender.rs`)
```rust
trait TelegramSender: Send + Sync {
    async fn send(&self, msg: &OutboundTelegramMessage) -> Result<SentTelegramMessage>;
    async fn test_connection(&self) -> Result<()>;
    fn default_chat_id(&self) -> Option<i64>;
    fn name(&self) -> &'static str;
}
```
Outbound Telegram port for the agent's `send_telegram` tool. Supports text and file attachments (`TelegramAttachmentKind::Photo | Document | Auto`). When `chat_id` is omitted on the message the adapter uses its configured owner-chat default. `test_connection` authenticates via `getMe` without sending. Implemented in `athen-sentidos` as `BotApiTelegramSender`.

### ApprovalSink (`traits/approval.rs`)
```rust
trait ApprovalSink: Send + Sync {
    fn channel_kind(&self) -> ReplyChannelKind;
    async fn ask(&self, question: ApprovalQuestion) -> Result<ApprovalAnswer>;
    async fn cancel(&self, question_id: Uuid) -> Result<()>;  // default: no-op
}
```
A single channel through which an approval question can be delivered and awaited. Multiple sinks (in-app, Telegram) race in parallel; whichever answers first wins and the router cancels the rest. In-app sink parks a oneshot keyed by `question.id`; Telegram sink sends an inline keyboard and resolves on the corresponding `callback_query`. See `docs/ARCHITECTURE.md` §IPC for the `ApprovalRequest`/`ApprovalResponse` IPC messages that feed this.

### MCP-BYO (`athen-core/src/traits/mcp.rs`) — SHIPPED
The `McpCatalogEntry` now supports user-supplied stdio MCP servers alongside bundled ones:

```rust
enum McpSource {
    Bundled { binary_name: String },
    Download { url: String, binary_name: String },  // reserved
    Process {                                        // BYO: Claude Desktop / Cursor compatible
        command: String,
        args: Vec<String>,
        env: Vec<EnvBinding>,        // vault-backed secrets never appear in persisted config
        working_dir: Option<String>,
    },
}
```

`McpCatalogEntry` also carries `base_risk: BaseImpact` (per-server fallback risk, defaults to `WritePersist` for backward compat) and `tool_risks: HashMap<String, BaseImpact>` (per-tool overrides by bare tool name, no `<mcp_id>__` prefix). When a tool isn't keyed in `tool_risks`, `base_risk` applies. Tools are namespaced `<mcp_id>__<tool_name>` (e.g. `slack__post_message`) in `AppToolRegistry`.

### CalendarSource (`traits/calendar_source.rs`)
```rust
trait CalendarSource: Send + Sync {
    async fn authenticate(&mut self, config: &CalendarConfig) -> Result<()>;
    async fn list_calendars(&self) -> Result<Vec<RemoteCalendar>>;
    async fn pull_events(&self, calendar_id: &str, window: &(DateTime<Utc>, DateTime<Utc>)) -> Result<Vec<RemoteEvent>>;
}
```
Syncs remote calendar events into the local `CalendarStore`. Implemented in `athen-caldav` (RFC 4791 CalDAV, generic over iCloud/Google-via-CalDAV/Fastmail/Nextcloud/Yandex). SHIPPED 2026-05-15. Producer side in `athen-app/src/calendar_sources.rs` runs a background loop polling each enabled source. Reconciliation key: `(source_id, remote_id)`. See crate-level docs in `IMPLEMENTATION.md`.

### HttpEndpointStore (`traits/http_endpoint.rs`)
```rust
trait HttpEndpointStore: Send + Sync {
    async fn list(&self) -> Result<Vec<RegisteredEndpoint>>;
    async fn get(&self, id: Uuid) -> Result<Option<RegisteredEndpoint>>;
    async fn get_by_name(&self, name: &str) -> Result<Option<RegisteredEndpoint>>;
    async fn upsert(&self, endpoint: &RegisteredEndpoint) -> Result<()>;
    async fn delete(&self, id: Uuid) -> Result<()>;
    async fn record_call(&self, id: Uuid) -> Result<()>;
    async fn set_enabled(&self, id: Uuid, enabled: bool) -> Result<()>;
}
```
Storage for `RegisteredEndpoint` rows backing the `http_request` agent tool. Names are unique (case-insensitive); the UUID is the primary key for rename safety. `get_by_name` is the hot path used by the tool dispatcher. `record_call` bumps the call counter and `last_used` after a successful call (non-fatal on failure). SHIPPED 2026-05-10. Implemented in `athen-persistence`. See `docs/CLOUD_APIS.md`.

### SystemReminderBuilder (`traits/reminder.rs`)
```rust
trait SystemReminderBuilder: Send + Sync {
    fn build(&self, ctx: &ReminderContext<'_>) -> Option<String>;
}
```
Builds ephemeral reminder text the executor injects into the message stream every few iterations to fight tool-selection drift on long arcs. Returning `None` skips injection for that iteration. The returned string is the body only — the executor wraps it in `<system-reminder>...</system-reminder>` tags. Implementations must be cheap: `build` runs once per loop iteration, so heavy lookups (template resolution, identity excerpts) belong in the constructor. Reminders sit in the dynamic suffix and never invalidate the cached static prefix.

### IpcTransport (`athen-ipc/src/transport.rs`)
```rust
trait IpcTransport: Send + Sync {
    async fn send(&self, message: &IpcMessage) -> Result<()>;
    async fn recv(&self) -> Result<IpcMessage>;
    async fn close(&self) -> Result<()>;
}
```

---

## IPC Protocol

- **Transport**: Unix sockets (Linux/macOS), Named pipes (Windows)
- **Codec**: JSON-RPC 2.0 (MessagePack as future optimization)
- **Pattern**: Request/Response + Events (pub/sub for notifications)
- **Discovery**: Coordinator starts first, opens socket at known path. Monitors, agents, and UI connect and register.
- **Health checks**: Coordinator pings all processes periodically. Timeout -> mark unhealthy, reassign tasks.

---

## Error Handling Strategy

Three layers of protection:

1. **Retry with exponential backoff** (default: 4 attempts, 1s->2s->4s->8s, with jitter)
2. **Fallback to alternative** (next model in priority list, alternative tool approach, cache)
3. **Circuit breaker** (if service fails >N times in M minutes, stop trying, half-open after timeout)
4. **Escalate to user** if all else fails

Per-error-type behavior:
- Rate limit -> retry with longer backoff
- Network timeout -> retry, then fallback to cache
- Auth expired -> notify user to reauth (not retryable)
- Model overloaded -> immediate fallback to next model
- Task logic error -> pause, ask for clarification

---

## Persistence & Recovery

SQLite stores: tasks, task steps, checkpoints, pending messages, arcs, arc entries, calendar events, fired reminders, contacts, contact identifiers, notifications, configuration. (Legacy chat_messages/chat_sessions tables still exist for backward compatibility but are no longer used -- auto-migrated to arcs on first startup.)

**Checkpoint frequency**: After every completed step, every 30s during long steps, before any risky action, before LLM calls.

**Recovery on restart**:
1. Load tasks with status != completed/failed/cancelled
2. Classify: resumable (valid checkpoint), restartable (pending), corrupted (bad checkpoint)
3. Show user recovery UI: Continue / Restart / Cancel per task
4. Execute decisions

**Atomic saves**: Write to temp file -> fsync -> atomic rename (POSIX guarantees).

---

## Security Model

### 3-Layer Defense Architecture

Risk is evaluated at three independent layers -- any layer can block a dangerous action:

**Layer 1: User Message Risk (Coordinator)**
Rule engine evaluates the user's natural language input before any LLM call. Catches both literal shell patterns (`rm -rf`, `sudo`) and natural language destructive intent ("delete all files", "wipe the database"). Intent-based matches add an uncertainty penalty pushing scores into HumanConfirm range. If rules are inconclusive, falls back to LLM risk evaluation (10-second timeout, conservative defaults on failure).

**Layer 2: Tool Execution Risk (Agent)**
`ShellToolRegistry.do_shell_execute()` runs `RuleEngine.evaluate()` on every actual shell command before execution. This catches dangerous commands regardless of what language the user spoke -- the LLM may translate "borra todo" into `rm -rf /` and this layer catches it. Commands classified as Danger or Critical are blocked with an error returned to the LLM.

**Layer 3: OS-Native Sandbox (Shell)**
Shell commands execute inside the platform's OS-native sandbox with `SandboxProfile::RestrictedWrite`. Writable paths: `/tmp`, `$HOME` (or `%APPDATA%\Athen` on Windows), current working directory. Everything else is read-only. Backends: bwrap (Linux), `sandbox-exec` Seatbelt (macOS), Job Object + AppContainer (Windows). All three auto-detected at startup; if the active backend errors out at runtime (e.g. bwrap namespace creation fails on restricted CI; AppContainer profile creation fails inside an existing container), the executor falls back to unsandboxed execution rather than breaking the command. Layers 1 and 2 still apply so dangerous commands remain blocked regardless of sandbox availability.

### Operation Modes
- **Bunker**: Everything L2+ requires approval. Maximum caution.
- **Assistant**: Standard risk evaluation. Normal operation.
- **YOLO**: Only L4 requires approval. Minimal friction.

### Prompt Injection Defense
- External content NEVER passed directly as LLM instructions
- Content sandboxed between delimiters with escaping
- Pattern detection for known injection techniques ("Ignore previous instructions", etc.)
- Detection increases Pincertidumbre dramatically

### Anti-Loop Protection
- Max steps per task (default: 50)
- Max time per task (default: 5 min)
- Repetition detection: 3 identical actions without progress -> pause
- Graceful max-steps handling: when the executor hits the limit, it makes one final LLM call (with tools disabled) asking for a summary of work done so far, instead of returning raw JSON

### Kill Switch
- **UI Stop button**: Red square replaces send button during task execution. Sets `cancel_flag: Arc<AtomicBool>` to true. Executor checks at loop start + between each tool call. Returns "cancelled" result immediately. Escape key also triggers cancellation.
- **Backend**: `cancel_task` Tauri command sets the shared `AtomicBool`. The executor in `athen-agent` checks `cancel_flag.load(Relaxed)` at two points: (1) top of the execution loop, (2) between individual tool calls in a multi-tool response.
- Graceful: Ctrl+Shift+K -- stops tasks cleanly, saves state (planned)
- Hard: Ctrl+Shift+Alt+K -- kills all processes immediately (planned)

### Deletion Safety
**Everything deleted goes to trash. Always reversible.**
