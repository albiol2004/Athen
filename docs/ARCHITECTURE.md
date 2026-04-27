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
SandboxLevel::OsNative { profile }        // L2: bwrap/landlock (Linux), sandbox-exec (macOS), Job Objects (Windows)
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
- `ModelsConfig`: Providers, profiles (Powerful/Fast/Code/Cheap/Local), domain assignments
- `DomainConfig`: Per-domain model profile, max steps, timeout, custom options
- `EmailConfig`: enabled, imap_server, imap_port, username, password, use_tls, folders, poll_interval_secs, lookback_hours
- `TelegramConfig`: enabled, bot_token, owner_user_id (Option<i64>), allowed_chat_ids (Vec<i64>), poll_interval_secs (default 5). Added as `telegram: TelegramConfig` field on `AthenConfig` with `#[serde(default)]`.

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
```

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
When bwrap is available, shell commands execute inside an OS-native sandbox with `SandboxProfile::RestrictedWrite`. Writable paths: `/tmp`, `$HOME`, current working directory. Everything else is read-only. Graceful fallback to unsandboxed execution if bwrap is not installed.

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
