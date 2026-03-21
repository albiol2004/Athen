# Athen — Universal AI Agent

## What is Athen?

Athen is a **universal, proactive AI agent** built as a native desktop application. It monitors your emails, calendar, messages, and direct input ("senses"), evaluates what needs doing, and executes tasks autonomously — with a dynamic risk system that decides when to act silently vs. ask for permission.

Inspired by OpenClaw but designed for **non-technical users**: single binary, native GUI (Tauri), zero runtime dependencies, cross-platform from one Rust codebase.

## Tech Stack

| Component | Technology | Why |
|-----------|-----------|-----|
| Core | Rust | Speed, memory safety, native cross-platform compilation |
| UI | Tauri 2 | Native app with web frontend, tiny binaries |
| MCPs | Rust binaries | Standalone tools, no runtime dependencies |
| Database | SQLite | Embedded, serverless, portable |
| Shell | Nushell (embedded) | Cross-platform consistent shell + native fallback |
| Sandbox | OS-native + Podman/Docker | Tiered isolation, zero user setup for OS-native |

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

## Workspace Structure

```
athen/
├── Cargo.toml                    # Workspace root
├── frontend/                     # Shared web frontend (HTML/CSS/JS)
│   ├── index.html                # App layout: sidebar + chat + settings
│   ├── styles.css                # Dark theme, streaming, tool cards, settings
│   └── app.js                    # Chat logic, streaming, sessions, settings
├── crates/
│   ├── athen-core/               # Shared types + trait contracts (THE CONTRACTS)
│   ├── athen-ipc/                # IPC transport layer
│   ├── athen-sentidos/           # Sense monitors (email, calendar, messaging, user)
│   ├── athen-coordinador/        # Coordinator (router, risk eval, queue, dispatch)
│   ├── athen-agent/              # Agent worker (LLM executor, auditor, timeout)
│   ├── athen-llm/                # LLM provider adapters + router + failover
│   ├── athen-memory/             # Vector index + knowledge graph + SQLite
│   ├── athen-risk/               # Risk scorer + regex rules + LLM fallback
│   ├── athen-persistence/        # SQLite persistence, checkpoints, chat history
│   ├── athen-contacts/           # Contact trust model + risk multipliers
│   ├── athen-sandbox/            # OS-native + container sandboxing
│   ├── athen-shell/              # Nushell embedding + native shell fallback
│   ├── athen-cli/                # CLI runner (REPL, wires all components)
│   ├── athen-app/                # Tauri desktop app (composition root)
│   └── mcps/
│       └── mcp-filesystem/       # Standalone MCP filesystem tool
```

## Design Principles

### 1. Hexagonal Architecture (Ports & Adapters)
`athen-core` defines ALL traits (ports). Every other crate provides adapters implementing those traits. No crate depends on a sibling — only on `athen-core`. The Tauri app (`athen-app`) is the composition root that wires concrete implementations together.

### 2. Dependency Rules
- `athen-core` depends on NOTHING internal (only serde, chrono, uuid, thiserror, async-trait, url, tokio-stream)
- All other crates depend on `athen-core` for trait definitions
- MCPs (`crates/mcps/*`) do NOT depend on `athen-core` — they are standalone JSON-RPC servers
- Crates never depend on sibling crates (except through `athen-core` traits)
- `athen-app` is the ONLY crate that depends on multiple siblings

### 3. Independent Testability
Every crate can be tested in complete isolation by mocking the traits it depends on. No crate needs any other part of the system running to test.

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
RiskScore = (Ibase × Morigen × Mdatos) + Pincertidumbre

Where:
- Ibase (BaseImpact):     Read=1, WriteTemp=10, WritePersist=40, System=90
- Morigen (TrustLevel):   AuthUser=0.5x, Trusted=1.0x, Known=1.5x, Neutral=2.0x, Unknown=5.0x
- Mdatos (DataSensitivity): Plain=1.0x, PersonalInfo=2.0x, Secrets=5.0x
- Pincertidumbre:         Based on LLM confidence (1.0→0, 0.5→25, 0.1→81)

Decision thresholds:
- 0-19:  SilentApprove  (execute, debug log)
- 20-49: NotifyAndProceed (execute, push notification)
- 50-89: HumanConfirm (pause, wait for approval)
- 90+:   HardBlock (reject automatically)
```

### RiskLevel
```
L1 (Safe):     Read-only, analysis — silent execution
L2 (Caution):  Local reversible writes — notify user
L3 (Danger):   External/irreversible — require confirmation click
L4 (Critical): Financial/system config — confirmation with challenge
```

### Contact & Trust (`contact.rs`)
```
T0 (Unknown):  5.0x multiplier — new/external senders
T1 (Neutral):  2.0x — in contacts, no history
T2 (Known):    1.5x — positive interaction history
T3 (Trusted):  1.0x — explicitly marked trusted
T4 (AuthUser): 0.5x — the authenticated user themselves
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
- `SenseEvent` (Monitor → Coordinator)
- `TaskAssignment` (Coordinator → Agent)
- `TaskProgress` (Agent → Coordinator)
- `TaskControl` (Coordinator → Agent: Continue/Pause/Cancel)
- `Registration` (Any → Coordinator on startup)
- `HealthPing`/`HealthPong` (Coordinator ↔ All)
- `ApprovalRequest`/`ApprovalResponse` (Coordinator ↔ UI)
- `StateUpdate` (Coordinator → UI)
- `UserCommand` (UI → Coordinator)

### Configuration (`config.rs`)
TOML-based configuration:
- `OperationMode`: AlwaysOn, WakeTimer, CloudRelay
- `SecurityMode`: Bunker (everything L2+ needs approval), Assistant (standard), Yolo (only L4)
- `ModelsConfig`: Providers, profiles (Powerful/Fast/Code/Cheap/Local), domain assignments
- `DomainConfig`: Per-domain model profile, max steps, timeout, custom options

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
    async fn route_streaming(&self, request: &LlmRequest) -> Result<LlmStream>;  // default: wraps route() as single chunk
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
- **Health checks**: Coordinator pings all processes periodically. Timeout → mark unhealthy, reassign tasks.

---

## Error Handling Strategy

Three layers of protection:

1. **Retry with exponential backoff** (default: 4 attempts, 1s→2s→4s→8s, with jitter)
2. **Fallback to alternative** (next model in priority list, alternative tool approach, cache)
3. **Circuit breaker** (if service fails >N times in M minutes, stop trying, half-open after timeout)
4. **Escalate to user** if all else fails

Per-error-type behavior:
- Rate limit → retry with longer backoff
- Network timeout → retry, then fallback to cache
- Auth expired → notify user to reauth (not retryable)
- Model overloaded → immediate fallback to next model
- Task logic error → pause, ask for clarification

---

## Persistence & Recovery

SQLite stores: tasks, task steps, checkpoints, pending messages, chat messages, chat sessions, contacts, configuration.

**Checkpoint frequency**: After every completed step, every 30s during long steps, before any risky action, before LLM calls.

**Recovery on restart**:
1. Load tasks with status != completed/failed/cancelled
2. Classify: resumable (valid checkpoint), restartable (pending), corrupted (bad checkpoint)
3. Show user recovery UI: Continue / Restart / Cancel per task
4. Execute decisions

**Atomic saves**: Write to temp file → fsync → atomic rename (POSIX guarantees).

---

## Security Model

### 3-Layer Defense Architecture

Risk is evaluated at three independent layers — any layer can block a dangerous action:

**Layer 1: User Message Risk (Coordinator)**
Rule engine evaluates the user's natural language input before any LLM call. Catches both literal shell patterns (`rm -rf`, `sudo`) and natural language destructive intent ("delete all files", "wipe the database"). Intent-based matches add an uncertainty penalty pushing scores into HumanConfirm range. If rules are inconclusive, falls back to LLM risk evaluation (10-second timeout, conservative defaults on failure).

**Layer 2: Tool Execution Risk (Agent)**
`ShellToolRegistry.do_shell_execute()` runs `RuleEngine.evaluate()` on every actual shell command before execution. This catches dangerous commands regardless of what language the user spoke — the LLM may translate "borra todo" into `rm -rf /` and this layer catches it. Commands classified as Danger or Critical are blocked with an error returned to the LLM.

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
- Repetition detection: 3 identical actions without progress → pause
- Graceful max-steps handling: when the executor hits the limit, it makes one final LLM call (with tools disabled) asking for a summary of work done so far, instead of returning raw JSON

### Kill Switch
- **UI Stop button**: Red square (&#9632;) replaces send button during task execution. Sets `cancel_flag: Arc<AtomicBool>` to true. Executor checks at loop start + between each tool call. Returns "cancelled" result immediately. Escape key also triggers cancellation.
- **Backend**: `cancel_task` Tauri command sets the shared `AtomicBool`. The executor in `athen-agent` checks `cancel_flag.load(Relaxed)` at two points: (1) top of the execution loop, (2) between individual tool calls in a multi-tool response.
- Graceful: Ctrl+Shift+K — stops tasks cleanly, saves state (planned)
- Hard: Ctrl+Shift+Alt+K — kills all processes immediately (planned)

### Deletion Safety
**Everything deleted goes to trash. Always reversible.**

---

## Domains

Tasks are classified into domains with optimized flows:

| Domain | Model Profile | Max Steps | Timeout | Notes |
|--------|--------------|-----------|---------|-------|
| base | fast | 50 | 5min | Fallback for uncategorized tasks |
| communication | fast | 20 | 3min | Group by thread, wait 30s for related messages |
| code | powerful | 100 | 15min | Require tests, sandbox execution |
| agenda | fast | 15 | 2min | Check conflicts, notify before events |
| files | fast | 30 | 5min | Document management |
| research | powerful | 50 | 10min | Web search, synthesis |

---

## LLM Configuration

### Providers
Anthropic, OpenAI, Google, DeepSeek, Ollama (local), llama.cpp (local). Each configurable with API key or OAuth. Any OpenAI-compatible endpoint supported via `OpenAiCompatibleProvider`.

### Model Profiles
- **Powerful**: Claude Opus → Gemini Ultra → o3 (fallback: DeepSeek)
- **Fast**: DeepSeek → Gemini Pro → Claude Sonnet
- **Code**: Claude Opus → DeepSeek
- **Cheap**: DeepSeek → Local models
- **Local**: Ollama models only (max privacy)

### Failover
If a model fails: try next in priority list. If rate limited: wait and retry same model. Circuit breaker if persistent failures.

### Budget
Optional daily USD limit with warning threshold. Per-provider rate limits. Token tracking.

---

## Tool Execution

The agent can use tools through 4 backends, choosing the best for each situation:

1. **NativeMcp**: Compiled Rust binaries, stdio JSON-RPC. Fastest, most portable.
2. **Shell**: Nushell (cross-platform default) or native shell (bash/zsh/pwsh). For CLI tools, curl, etc.
3. **Script**: Python execution for data processing, ML tasks, etc.
4. **HttpApi**: Direct HTTP calls to external services.

### Built-in Tools (ShellToolRegistry)
The agent has 6 built-in tools available via `ShellToolRegistry`:
1. `shell_execute` — runs shell commands (sandboxed when bwrap available, with pre-execution risk check)
2. `read_file` — reads file contents via `tokio::fs`
3. `write_file` — writes content to files via `tokio::fs`
4. `list_directory` — lists directory entries as JSON
5. `memory_store` — stores key-value pairs in in-session memory (HashMap)
6. `memory_recall` — retrieves by key or lists all stored keys

### Sandbox Tiers
- **L1 actions**: No sandbox (read-only operations)
- **L2 actions**: OS-native sandbox (bwrap/landlock on Linux, sandbox-exec on macOS, Job Objects on Windows) — zero install required. Default profile: `RestrictedWrite` (writable: `/tmp`, `$HOME`, cwd; read-only: everything else).
- **L3+ actions**: Container (Podman preferred, Docker fallback). Auto-detected. If unavailable, offer to install or fall back to manual approval.

### Cross-Platform Shell
Primary: Embedded Nushell (Rust-native, same commands everywhere).
Fallback: Native platform shell for platform-specific tools.

---

## Senses (Monitors)

Priority order:
1. **USER**: Always highest priority, never questioned
2. **Calendar**: Agent-managed deadlines take priority
3. **Messaging** (iMessage/WhatsApp): Usually more urgent
4. **Email**: Lowest priority sense

Each sense normalizes its input to `SenseEvent` format before sending to the coordinator.

### Notification Channels
When the agent needs to contact the user:
- App in foreground → in-app notification
- App in background → preferred messaging channel (iMessage/WhatsApp)
- Configurable quiet hours

---

## Operation Modes

1. **Always-On**: PC stays awake 24/7. Immediate reactivity. ~15-30W idle.
2. **Wake Timer**: System suspends, wakes every N minutes for polling. ~2-5W average. Max delay = wake interval.
3. **Cloud Relay** (paid): Monitors run on cloud server, push to local PC. PC can be off. Immediate reactivity.

---

## Coding Guidelines

- All async code uses `tokio` runtime
- All traits use `#[async_trait]` from the `async-trait` crate
- Error handling via `thiserror` with the `AthenError` enum and `Result<T>` alias from `athen-core::error`
- Serialization via `serde` with `Serialize`/`Deserialize` derives
- IDs are `uuid::Uuid` with v4 generation
- Timestamps are `chrono::DateTime<Utc>`
- Platform-specific code uses `#[cfg(target_os = "...")]`
- Logging via `tracing` crate
- Tests should mock trait dependencies, not real services
- HTTP via `reqwest` with `rustls-tls` (pure Rust TLS, no OpenSSL system dependency)
- Clippy clean: `cargo clippy --workspace` must produce zero warnings

---

## Implementation Status

### athen-core (16 source files, 9 tests)
**Status**: Complete — all types, trait contracts, and config loading.
- `error.rs`: `AthenError` enum (Io, Serialization, TaskNotFound, ToolNotFound, LlmProvider, RiskThresholdExceeded, Timeout, Sandbox, Ipc, Config, Other) + `Result<T>` alias
- `event.rs`: `SenseEvent`, `EventSource`, `EventKind`, `SenderInfo`, `NormalizedContent`, `Attachment`
- `task.rs`: `Task`, `TaskStep`, `TaskPriority` (Background..Critical), `TaskStatus` (7 states), `StepStatus`, `DomainType`
- `risk.rs`: `RiskScore` with `decision()` method, `RiskLevel`, `RiskDecision`, `RiskContext`, `BaseImpact`, `DataSensitivity`
- `contact.rs`: `Contact`, `TrustLevel` (T0..T4) with `risk_multiplier()`, `ContactIdentifier`, `IdentifierKind`
- `llm.rs`: `LlmRequest`, `LlmResponse`, `ChatMessage`, `Role`, `MessageContent`, `ToolCall`, `TokenUsage`, `FinishReason`, `LlmChunk`, `BudgetStatus`, `LlmStream`, `ModelProfile`
- `tool.rs`: `ToolDefinition`, `ToolBackend` (NativeMcp/Shell/Script/HttpApi), `ToolResult`, `AuthConfig`, `ScriptRuntime`, `HttpMethod`
- `sandbox.rs`: `SandboxLevel` (None/OsNative/Container), `SandboxProfile`, `SandboxCapabilities`, `Mount`
- `ipc.rs`: `IpcMessage`, `IpcPayload` (14 variants), `ProcessId`, `ProcessType`, `ProcessTarget`, `TaskProgressReport`, `TaskControlCommand`, `ControlAction`, `ProcessRegistration`, `ProcessHealthStatus`, `ApprovalRequest`, `ApprovalResponse`
- `config.rs`: `AthenConfig`, `OperationMode`, `OperationConfig`, `ModelsConfig`, `ProviderConfig`, `AuthType`, `ProfileConfig`, `DomainConfig`, `SecurityConfig`, `SecurityMode`, `PersistenceConfig`
- `config_loader.rs`: `load_config(path)`, `load_config_dir(dir)`, `save_default_config(path)`. Loads TOML files with serde defaults for missing fields. Supports split config: `config.toml` (main) + optional `models.toml` override.
- `traits/`: 9 trait files defining all inter-module contracts

### athen-ipc (13 unit + 5 integration = 18 tests)
**Status**: Complete — full IPC transport layer.
- `transport.rs`: `IpcTransport` trait + `UnixTransport` implementation using split `UnixStream` halves with independent `Mutex`es for concurrent send/recv. Length-prefixed framing (4-byte big-endian). 16 MiB message size limit.
- `codec.rs`: `encode()` serializes `IpcMessage` to length-prefixed JSON bytes. `decode()` deserializes. `read_length_prefix()` extracts u32 from 4 bytes.
- `server.rs`: `IpcServer` binds `UnixListener`, accepts connections, spawns per-connection reader tasks, identifies processes by first message's `source` field. Methods: `send_to()`, `broadcast()`, `broadcast_to_type()`, `route()`, `connected_count()`, `shutdown()`. `IpcClient` connects to coordinator, auto-sends Registration message.

### athen-risk (55 unit + 6 integration = 61 tests)
**Status**: Complete — full risk evaluation engine with natural language intent detection.
- `scorer.rs`: `RiskScorer` implementing the formula `(Ibase × Morigen × Mdatos) + Pincertidumbre`. Confidence penalty: `(1.0 - confidence)^2 × 100`. Maps total to RiskLevel and RiskDecision. Implements `RiskEvaluator` trait.
- `rules.rs`: `RuleEngine` with compiled regex patterns using `LazyLock`. Returns `RuleMatch` with `base_impact`, `data_sensitivity`, `matched_patterns`, and `intent_based` flag. Detects:
  - **Dangerous shell commands**: rm -rf, sudo, dd, mkfs, chmod 777, redirect to /dev/, pipe to sh/bash/zsh
  - **Natural language destructive intent** (`DESTRUCTIVE_INTENT` patterns): delete/remove/erase/wipe/destroy/nuke + file/folder/dir/everything/all (both orderings), format/reset/clear/empty/purge + disk/drive/partition/database/system, kill/terminate + all/every + process/service, modify/change/edit/overwrite + system/config/password/credentials, send/post/upload + data/file/secret/key/token. Intent-based matches set `intent_based = true` and receive `confidence = 0.6` (adds 16-point uncertainty penalty), pushing scores into HumanConfirm range.
  - **Secrets**: OpenAI keys (sk-...), AWS keys (AKIA...), private key headers, passwords in URLs
  - **PII**: email addresses, phone numbers
  - **Financial keywords**: payment, transfer, purchase, buy, invoice, billing, credit card
  - **External URLs**: http/https URLs
  Returns `Option<RiskScore>` — `Some` if confident, `None` for LLM fallback.
- `llm_fallback.rs`: `LlmRiskEvaluator` takes `Box<dyn LlmRouter>`. 10-second timeout on LLM calls. Constructs structured prompt that considers what the agent would ACTUALLY DO (not just literal text) — asks LLM to classify impact as system for any delete/remove/wipe/destroy actions. Conservative fallback on failure/timeout: `WritePersist + PersonalInfo + 0.3 confidence` (lands in HumanConfirm range, score ~89).
- `lib.rs`: `CombinedRiskEvaluator` implementing `RiskEvaluator` — tries rules first, falls back to LLM if rules return `None`.

### athen-sandbox (32 tests)
**Status**: Complete — tiered sandboxing with auto-detection.
- `detect.rs`: `SandboxDetector::detect()` checks for bwrap, landlock, macOS sandbox, Windows sandbox, Podman, Docker. Platform-specific checks short-circuit to false on wrong OS.
- `container.rs`: `ContainerExecutor` with `ContainerRuntime` enum (Podman/Docker). Auto-detects runtime. `build_run_args()` constructs container run command with --rm, --network=none, -v mounts, --memory, --cpus, --timeout.
- `bwrap.rs` (Linux): `BwrapSandbox` builds bwrap commands per `SandboxProfile` — ReadOnly (--ro-bind / /), RestrictedWrite (--bind for allowed paths), NoNetwork (--unshare-net), Full (--unshare-all). Always includes --die-with-parent, --new-session.
- `landlock.rs` (Linux): Stub returning "not yet implemented".
- `macos.rs`: Generates Seatbelt profiles for sandbox-exec. Platform-gated.
- `windows.rs`: Stub returning "not yet implemented". Platform-gated.
- `lib.rs`: `UnifiedSandbox` facade — auto-detects capabilities, selects best sandbox per level (bwrap > landlock > macos > windows for OsNative; podman > docker for Container).

### athen-coordinador (37 unit + 4 integration = 41 tests)
**Status**: Complete — full coordinator orchestration with persistence, trust, and approval management.
- `router.rs`: `DefaultRouter` implementing `EventRouter`. Maps EventSource→DomainType (Email/Messaging→Communication, Calendar→Agenda, UserInput/System→Base). Priority: UserInput/Calendar=High, Messaging/Email=Normal, System=Low.
- `queue.rs`: `PriorityTaskQueue` implementing `TaskQueue`. Uses `BinaryHeap<PrioritizedTask>` — higher priority first, FIFO within same priority (oldest first).
- `dispatcher.rs`: `Dispatcher` manages agent availability. `register_agent()`, `unregister_agent()`, `assign_task()`, `release_agent()`, `assigned_agent()`.
- `risk.rs`: `CoordinatorRiskEvaluator` wrapping `Box<dyn RiskEvaluator>`. `evaluate_and_decide()` returns `RiskDecision`.
- `lib.rs`: `Coordinator` wiring all components with optional persistence, trust management, and human approval flow:
  - `.with_persistence(Box<dyn PersistentStore>)` — attaches SQLite store for task durability. `process_event()` saves tasks after creation, `complete_task()` updates status in DB. Persistence errors are logged but never crash the system.
  - `.with_trust_manager(TrustManager)` — enables contact-aware risk evaluation. `process_event()` resolves sender trust via `TrustManager` and factors it into risk scoring (AuthUser for UserInput, resolved trust for external senders, Neutral fallback). `complete_task()` records approval for implicit trust evolution.
  - `recover_tasks()` — loads non-terminal tasks from persistent store and re-enqueues them on startup. Terminal statuses (Completed, Failed, Cancelled) are skipped.
  - `process_event()` routes→resolves sender trust→evaluates risk→sets status→persists→enqueues. Tasks with `HumanConfirm` risk go to `awaiting_approval` map instead of queue. `dispatch_next()` dequeues and assigns to agent. `complete_task()` releases agent→updates DB→records trust approval.
  - **Approval management**: `awaiting_approval: Mutex<HashMap<TaskId, Task>>` holds tasks pending human decision. `get_awaiting_approval()` returns the first pending task. `approve_task(id)` moves task to Pending and enqueues. `deny_task(id)` sets status to Cancelled and persists.
  - `infer_identifier_kind()` helper — infers `IdentifierKind` (Email/Phone/Other) from sender identifier strings.
  - `task_contacts: Mutex<HashMap<TaskId, ContactId>>` — maps task IDs to resolved contact IDs for trust feedback on completion.

### athen-memory (28 unit + 5 integration = 33 tests)
**Status**: Complete — vector search + knowledge graph with SQLite persistence.
- `vector.rs`: `InMemoryVectorIndex` — brute-force cosine similarity search. `tokio::sync::RwLock` for concurrent reads.
- `graph.rs`: `InMemoryGraph` — BFS exploration from entry node. Scoring combines recency (exponential decay, 7-day half-life), frequency, and importance weighted by `ExploreParams`.
- `sqlite.rs`: `SqliteVectorIndex` and `SqliteGraph` — SQLite-backed persistent versions. Embeddings stored as little-endian f32 blobs. Uses `std::sync::Mutex` (not tokio) since rusqlite is synchronous and locks are never held across `.await`.
- `lib.rs`: `Memory` facade implementing `MemoryStore`. `remember()` stores in vector + extracts entities to graph. `recall()` searches vector index. `forget()` removes from vector.

### athen-sentidos (23 tests)
**Status**: Complete — user input monitor + stubs + polling runner.
- `user_input.rs`: `UserInputMonitor` using `tokio::sync::Mutex<mpsc::Receiver<String>>` for interior mutability. Converts strings to `SenseEvent` with EventSource::UserInput, EventKind::Command, RiskLevel::Safe. Exposes `sender()` for UI to push messages.
- `email.rs`: Stub `EmailMonitor` — 60s poll interval, source_risk Caution.
- `calendar.rs`: Stub `CalendarMonitor` — 300s poll interval.
- `messaging.rs`: Stub `MessagingMonitor` — 30s poll interval.
- `lib.rs`: Generic `SenseRunner<M: SenseMonitor>` — polling loop with `tokio::select!` for shutdown signal. Sends events through mpsc channel.

### athen-shell (20 tests)
**Status**: Complete — cross-platform shell execution.
- `native.rs`: `NativeShell` — uses `sh -c` on Unix, `cmd /C` on Windows via `tokio::process::Command`. 30-second timeout. Captures stdout/stderr/exit code/execution time. `which()` uses system `which`/`where`.
- `nushell.rs`: `NushellShell` — auto-detects `nu` binary. If available: `nu -c "command"`. If not: falls back to NativeShell with info log.
- `lib.rs`: `Shell` unified facade. `execute()` prefers nushell, `execute_native()` always native. Convenience: `run()` returns stdout, `run_ok()` returns bool, `has_program()` checks existence.

### athen-persistence (30 unit + 5 integration = 35 tests)
**Status**: Complete — SQLite persistence with atomic checkpoints and chat history.
- `lib.rs`: `Database` struct with `new(path)` and `in_memory()`. Auto-creates tables on init (including chat tables). Provides `store()` → `SqliteStore` and `chat_store()` → `ChatStore` accessors.
- `store.rs`: `SqliteStore` implementing `PersistentStore`. Full CRUD for tasks (with steps serialized as JSON), checkpoints with SHA-256 integrity verification, pending messages with atomic pop (transaction-based select+update).
- `checkpoint.rs`: `CheckpointManager` — atomic file-based backup (write temp → fsync → rename). Integrity verification with SHA-256 checksums.
- `chat.rs`: `ChatStore` — SQLite-backed persistent chat history with multi-session support. Types: `PersistedChatMessage` (id, session_id, role, content, content_type, created_at), `SessionMeta` (session_id, name, created_at, updated_at, message_count). Methods:
  - `save_message(session_id, role, content, content_type)` — inserts a message
  - `load_messages(session_id)` — returns all messages ordered by creation time
  - `list_sessions()` — returns distinct session IDs ordered by most recent message
  - `clear_session(session_id)` — deletes all messages for a session
  - `create_session(session_id, name)` — creates session metadata entry
  - `rename_session(session_id, name)` — updates session name
  - `delete_session(session_id)` — deletes session metadata and all messages
  - `touch_session(session_id)` — updates `updated_at` timestamp
  - `list_sessions_with_meta()` — returns `Vec<SessionMeta>` with message counts, ordered by `updated_at` DESC. Auto-migrates legacy sessions (messages without metadata entries).
  Schema: `chat_messages` (id, session_id, role, content, content_type, created_at) + `chat_sessions` (session_id PK, name, created_at, updated_at). Session IDs use format `session_YYYYMMDD_HHMMSS`.
- Schema: `tasks`, `task_steps`, `checkpoints`, `pending_messages`, `chat_messages`, `chat_sessions` tables.

### athen-agent (32 unit + 3 integration = 35 tests)
**Status**: Complete — LLM-driven task execution with real tool calling, streaming responses, cancellation, tool-level risk checking, sandbox integration, session memory, anti-lazy nudge, and graceful max-steps handling.
- `executor.rs`: `DefaultExecutor` implementing `AgentExecutor`. Fields include optional `stream_sender: Option<mpsc::UnboundedSender<String>>` for progressive streaming and `cancel_flag: Option<Arc<AtomicBool>>` for user-initiated cancellation. Accepts optional `context_messages: Vec<ChatMessage>` prepended to conversation for session-level memory. System prompt ("You are Athen, an AI agent that ACTS first and talks second") is conversation-aware: when context messages are present, it tells the LLM "You are in an ongoing conversation". Includes numbered rules: (1) never say "I'll do X" — just do it, (2) never ask what to do — take initiative, (3) call tools immediately, (4) only text when task is complete, (5) be concise, (6) make reasonable choices. BAD/GOOD examples included. Loop: check cancel_flag → check timeout → check max_steps → build LlmRequest → call LlmRouter (streaming or non-streaming) → execute tool calls via ToolRegistry → check cancel_flag between tool calls → record steps via StepAuditor → repeat until LLM says done. Tool call results fed back as `Role::Tool` messages with `tool_call_id` for OpenAI-compatible APIs.
  - **Streaming**: when `stream_sender` is set, uses `try_streaming_call()` which calls `LlmRouter::route_streaming()`, collects text deltas, and forwards each chunk through the sender. If streaming returns empty content (tool call response), falls back to non-streaming `route()` to get tool call data.
  - **Cancellation**: `cancel_flag` is checked at loop start and between each tool call. When set to `true`, returns immediately with `{ reason: "cancelled", response: "Task cancelled by user." }`.
  - **Anti-lazy nudge**: on the first response (steps_completed == 0) when tools are available, detects lazy phrases ("let me", "i'll ", "i will ", "i can ", "i would ", "would you like me", "shall i", "do you want me") and re-prompts: "Don't tell me what you'll do -- just do it. Use your tools now."
  - **Graceful max-steps**: when the executor hits the step limit, it makes one final LLM call with `tools: None` asking for a summary of work accomplished, instead of returning raw JSON.
- `tools.rs`: `ShellToolRegistry` implementing `ToolRegistry` with 6 built-in tools:
  - `shell_execute` — runs shell commands with **2-layer safety**: (1) pre-execution risk check via `RuleEngine.evaluate()` on the actual command string — blocks Danger/Critical commands with a descriptive error returned to the LLM, (2) sandboxed execution via bwrap with `SandboxProfile::RestrictedWrite` (writable: `/tmp`, `$HOME`, cwd; read-only: everything else). Graceful fallback to unsandboxed if bwrap not installed.
  - `read_file` — reads file contents via `tokio::fs`
  - `write_file` — writes content to a file via `tokio::fs`
  - `list_directory` — lists directory entries as JSON array
  - `memory_store` — stores key-value pairs in in-session `HashMap<String, String>` memory
  - `memory_recall` — retrieves value by key or lists all stored keys
  Each tool has proper JSON Schema parameter definitions for LLM tool calling. `ShellToolRegistry` holds an optional `UnifiedSandbox` (from `athen-sandbox`) and a `RuleEngine` (from `athen-risk`) — both auto-initialized on construction.
- `auditor.rs`: `InMemoryAuditor` implementing `StepAuditor` with `tokio::sync::Mutex<HashMap<TaskId, Vec<TaskStep>>>`.
- `timeout.rs`: `DefaultTimeoutGuard` — sets deadline at Instant::now() + duration.
- `resource.rs`: `DefaultResourceMonitor` — reads `/proc/self/statm` on Linux for resident memory. `AtomicBool` cache for within-limits state.
- `lib.rs`: `AgentBuilder` with fluent API. `.context_messages(Vec<ChatMessage>)` sets prior conversation history for session memory. `.stream_sender(UnboundedSender<String>)` enables streaming text forwarding. `.cancel_flag(Arc<AtomicBool>)` enables user-initiated cancellation. Defaults: 50 max_steps, 5-minute timeout, InMemoryAuditor.
- Integration tests: mock LLM returns tool call → real `ShellToolRegistry` executes → result fed back → LLM completes.

### athen-contacts (15 tests)
**Status**: Complete — trust management with implicit learning.
- `lib.rs`: `ContactStore` trait (save/load/find_by_identifier/list_all/delete) + `InMemoryContactStore` for testing.
- `trust.rs`: `TrustManager` with:
  - `resolve_contact()` — finds existing or creates T0 Unknown
  - `risk_multiplier()` — delegates to TrustLevel, returns 5.0x for blocked
  - `record_approval()` — every 5 approvals upgrades T0→T1→T2 (never past T2, never if manual override)
  - `record_rejection()` — tracks count in notes JSON, every 3 rejections downgrades T2→T1→T0 (never if manual override)
  - `set_trust_level()` — sets level + trust_manual_override flag
  - `block_contact()`, `is_blocked()`, `list_contacts()`, `find_by_identifier()`

### athen-llm (40 unit + 1 doc-test = 41 tests)
**Status**: Complete — router with streaming + Anthropic/DeepSeek/OpenAI-compatible providers + Ollama/llama.cpp wrappers.
- `budget.rs`: `BudgetTracker` — daily USD limit, token counting, midnight UTC reset, `can_afford()`, `record_usage()`, `status()`, `is_warning()`. Zero budget always rejected.
- `router.rs`: `CircuitBreaker` (Closed/Open/HalfOpen state machine, configurable thresholds/timeout). `DefaultLlmRouter` implementing `LlmRouter` — profile-based routing, failover chains, circuit breakers per provider, budget enforcement. Implements both `route()` and `route_streaming()` with independent failover methods (`route_with_failover`, `route_streaming_with_failover`) — each tries providers in priority order, respects circuit breakers, and records success/failure.
- `providers/openai.rs`: `OpenAiCompatibleProvider` — fully generic adapter for any OpenAI-compatible API endpoint. Builder pattern: `new(base_url)`, `with_api_key()`, `with_model()`, `with_provider_id()`, `with_client()`, `with_cost_estimator()`. Convenience constructor: `openai(api_key)` for OpenAI proper. Features:
  - API key is optional — `Authorization: Bearer` header only sent when key is present (supports local servers without auth)
  - Full tool calling support with proper wire format conversion (assistant messages with tool_calls, tool result messages with tool_call_id)
  - SSE streaming via `complete_streaming()` — parses `data:` lines, handles `[DONE]`, extracts content deltas and finish_reason
  - `CostEstimator` trait for pluggable pricing: `OpenAiCostEstimator` (gpt-4o, gpt-4o-mini, o3, etc.), `ZeroCostEstimator` (for local providers)
  - `parse_sse_chunks()` public function for reuse by wrapper providers
- `providers/deepseek.rs`: Full `DeepSeekProvider` — OpenAI-compatible API at `api.deepseek.com/v1/chat/completions`. Bearer auth, request/response mapping, SSE streaming, tool call support. Cost estimation: deepseek-chat ($0.14/M input, $0.28/M output), deepseek-reasoner ($0.55/$2.19). Builder pattern: `new(api_key)`, `with_model()`, `with_base_url()`.
- `providers/anthropic.rs`: Full `AnthropicProvider` — POST to `/v1/messages`, proper headers (x-api-key, anthropic-version), request/response mapping, SSE streaming, cost estimation for opus/sonnet/haiku tiers.
- `providers/ollama.rs`: `OllamaProvider` — thin wrapper around `OpenAiCompatibleProvider` for local Ollama inference. Default URL `http://localhost:11434`, zero-cost estimation, delegates all LLM logic to inner provider. Real health check via `GET /api/tags` (returns model count). Builder: `new(model)`, `with_base_url()`, `with_model()`.
- `providers/llamacpp.rs`: `LlamaCppProvider` — thin wrapper around `OpenAiCompatibleProvider` for llama.cpp's `llama-server`. Default URL `http://localhost:8080`, zero-cost estimation. Real health check via `GET /health`. Constructor: `new(base_url, model)`, `localhost(model)`.
- `providers/google.rs`: Stub returning "not yet implemented".

### athen-cli (0 tests)
**Status**: Complete — working agentic CLI with tool execution.
- `main.rs`: REPL loop wiring all components end-to-end. Reads `DEEPSEEK_API_KEY` from env or config. Uses `config_loader` to discover config from `~/.athen/` or `./config/`. Creates `DeepSeekProvider` → `DefaultLlmRouter` (mapped to all profiles) → `CombinedRiskEvaluator` (real rule engine + LLM fallback) → `Coordinator` (real router, queue, dispatcher). Synchronous stdin with clean EOF/Ctrl+D handling. Risk-gated: low-risk auto-approved, high-risk prompts for confirmation, hard-block rejected. Commands: `/quit`, `/exit`.
- Uses full `AgentBuilder` + `DefaultExecutor` with `ShellToolRegistry` — the agent can execute shell commands (sandboxed when bwrap available), read/write files, list directories, and use in-session memory (store/recall) autonomously via LLM tool calls.
- `SharedRouter`: Wrapper around `Arc<DefaultLlmRouter>` implementing `LlmRouter` trait for sharing between risk evaluator and agent.
- **Verified working**: Full agentic pipeline — user input → coordinator → risk → dispatch → agent executor → LLM → tool calls → execution → result.

### athen-app (0 tests)
**Status**: Complete — Tauri 2 desktop app with full agentic tool execution, streaming responses, persistent chat history with multi-session support, settings UI with provider management, provider hot-swap, kill switch, real-time progress events, approval UI, config loading, and persistence. Verified working.
- `src/lib.rs`: Tauri composition root. Registers 17 command handlers: `send_message`, `get_status`, `approve_task`, `cancel_task`, `new_session`, `get_history`, `list_sessions`, `switch_session`, `rename_session`, `delete_session`, `get_current_session`, `get_settings`, `save_provider`, `delete_provider`, `test_provider`, `save_settings`, `set_active_provider`. Registers agent in `setup()` hook.
- `src/main.rs`: Entry point with `windows_subsystem = "windows"` for release builds.
- `src/state.rs`: `AppState` — composition root that loads TOML configuration via `find_config_dir()` (same discovery order as CLI: `~/.athen/` → `./config/` → defaults), resolves active provider from config, builds router and coordinator. Contains:
  - `router: Arc<RwLock<Arc<DefaultLlmRouter>>>` — double-wrapped for runtime hot-swap: inner `Arc` is the router, `RwLock` allows atomic replacement when the user switches providers
  - `active_provider_id: Mutex<String>` — ID of the currently active LLM provider (e.g. "deepseek", "ollama")
  - `history: Mutex<Vec<ChatMessage>>` — session-level conversation memory
  - `session_id: Mutex<String>` — current session identifier (`session_YYYYMMDD_HHMMSS` format)
  - `chat_store: Option<ChatStore>` — persistent chat storage backed by SQLite
  - `pending_message: Mutex<Option<String>>` — stashes user's message for replay after approval
  - `model_name: Mutex<String>` — resolved from config, returned by `get_status`
  - `cancel_flag: Arc<AtomicBool>` — shared cancellation flag for in-progress tasks
  - `SharedRouter` wrapper implementing `LlmRouter` via `Arc<RwLock<Arc<DefaultLlmRouter>>>` — delegates `route()`, `route_streaming()`, and `budget_remaining()` through the double-Arc indirection
  - `build_router_for_provider(id, base_url, model, api_key)` — factory function that creates the appropriate provider type based on ID: "deepseek" → `DeepSeekProvider`, "ollama" → `OllamaProvider`, "llamacpp" → `LlamaCppProvider`, anything else → `OpenAiCompatibleProvider`
  - `restore_or_create_session()` — on startup, tries to restore the most recent session's messages from SQLite; creates a new session if none exist
  - **API key resolution**: config file key takes priority over env var (e.g. saved key via Settings > `DEEPSEEK_API_KEY` env var). Env var format: `{PROVIDER_ID}_API_KEY` (e.g. `DEEPSEEK_API_KEY`, `OPENAI_API_KEY`). Config values like `${DEEPSEEK_API_KEY}` are treated as unresolved placeholders.
- `src/commands.rs`: Tauri IPC commands:
  - `send_message` — emits `agent-progress` event for risk evaluation phase, processes through coordinator pipeline, checks for awaiting approval (returns `PendingApproval` with task_id/description/risk_score/risk_level), snapshots conversation history, builds full `AgentExecutor` with `ShellToolRegistry` per request, passes history as `context_messages`, wires streaming sender and cancel flag, executes with 25 max_steps and 90s timeout. On failure: friendly error messages via `format_user_error()`. On cancellation: returns "Task cancelled by user." On max-steps: returns "I ran out of steps (N used) before finishing." Appends user+assistant messages to session history. Persists messages to SQLite via `persist_message()`.
  - `approve_task` — approves or denies a task flagged by risk system. On approve: retrieves stashed message, builds executor with streaming + cancel flag, dispatches and executes. On deny: cancels task, clears stashed message.
  - `cancel_task` — sets `cancel_flag` to `true`; executor checks at loop start and between tool calls.
  - `get_status` — returns actual model name and connection status.
  - `get_history` — returns current session's User/Assistant messages for frontend rendering on startup.
  - `new_session` — clears in-memory history, generates new session ID, creates session metadata entry.
  - `get_current_session` — returns current session ID.
  - `switch_session(session_id)` — loads target session's messages from SQLite into memory, returns display messages.
  - `rename_session(session_id, name)` — renames a session.
  - `delete_session(session_id)` — deletes session and messages. If deleting the active session, switches to next most recent or creates new one.
  - `list_sessions` — returns `Vec<SessionMeta>` for sidebar rendering.
  - `format_user_error(err)` — converts technical error strings to friendly messages (Timeout, Connection, Auth/401, rate_limit/429, max_steps, Budget, RiskThresholdExceeded). `simplify_error()` strips Rust enum formatting for the fallback case.
  - `AgentProgress` struct with `detail: Option<String>` field — carries tool arguments/result summaries (truncated to 200 chars).
  - `TauriAuditor` — wraps `InMemoryAuditor`, emits `agent-progress` Tauri events on each step. Extracts meaningful `detail` from step output: shell_execute → stdout, read_file/write_file → path, list_directory → path, errors → error text, completion → response preview. `truncate_detail()` compacts newlines and truncates.
  - `spawn_stream_forwarder(app_handle)` — spawns a background task that reads from `mpsc::UnboundedReceiver<String>` and emits `agent-stream` Tauri events with `{ delta, is_final }` payload. Emits `is_final: true` when the channel closes.
- `src/settings.rs`: Settings management commands:
  - `get_settings` — loads `~/.athen/models.toml`, returns `SettingsResponse` with provider list (sorted: active first), active provider ID, security mode. Shows env var keys with `(env)` hint.
  - `save_provider(id, base_url, model, api_key)` — saves/updates provider to `~/.athen/models.toml`. API key handling: `None` preserves existing, `Some("")` removes, `Some("sk-...")` updates. **Hot-reloads** when saving the active provider: builds new router and swaps via `RwLock`.
  - `delete_provider(id)` — removes provider. If deleting the active provider, automatically switches to first remaining or "deepseek" fallback, hot-reloads router.
  - `test_provider(id, base_url, model, api_key)` — tests connectivity. Provider-specific: Ollama → `GET /api/tags`, llama.cpp → `GET /health`, Anthropic → `POST /v1/messages`, others → `POST /v1/chat/completions`. 15-second timeout.
  - `set_active_provider(id)` — switches active provider at runtime. Builds new router, swaps via `RwLock`, persists choice to `~/.athen/models.toml` under `assignments.active_provider`. Cloud providers require API key (checks config then env var).
  - `save_settings(security_mode)` — saves security mode (bunker/assistant/yolo) to `~/.athen/config.toml`.
  - Helper types: `ProviderInfo` (id, name, type, base_url, model, has_api_key, api_key_hint, is_active), `SettingsResponse`, `TestResult`. `mask_api_key()` shows first 3 + last 4 chars.
- `src/process.rs`: Child process lifecycle management (stub).
- `tauri.conf.json`: Window 900x700, `frontendDist` points to `../../frontend`. **`"withGlobalTauri": true`** required in `app` section to inject `window.__TAURI__` into the webview.
- `frontend/index.html`: Full app layout with sidebar, chat area, and settings page:
  - **Sidebar**: session list with `+ New Chat` button, Settings button at bottom, hamburger toggle for mobile. Session list populated dynamically from `list_sessions`.
  - **Chat area**: header with logo + "New Chat" button, message container, input form with send/stop buttons, status bar.
  - **Stop button**: red square (&#9632;), initially hidden, shown during processing. Calls `cancel_task`.
  - **Settings page**: provider cards area, "Add Provider" button with template dropdown (DeepSeek, OpenAI, Anthropic, Ollama, llama.cpp, Custom), security mode selector (Assistant/Bunker/YOLO), back button.
- `frontend/styles.css`: Dark theme (Tokyo Night-inspired) — sidebar with session items (rename/delete on hover), tool execution cards with status icons (check/cross/spinner) and fade-in animation, streaming message bubbles, chat bubbles with avatars, risk badges, code blocks with language labels, approval dialog, settings page with provider cards (expand/collapse), auto-growing textarea, stop button (red), mobile responsive.
- `frontend/app.js`: Full chat frontend with:
  - **Streaming**: listens for `agent-stream` Tauri events. Creates streaming bubble on first chunk, appends text progressively via `textContent` (safe, fast). On `is_final`, re-renders full text with markdown for proper formatting. Tracks `streamingBubble`, `streamingText`, `didReceiveStreamChunks` state.
  - **Tool execution cards**: listens for `agent-progress` events. Creates `tool-steps-container` div, appends `tool-execution-card` elements with status class (completed/failed/in-progress), status icon (checkmark/cross/dot), tool name, and truncated detail text. Cards have fade-in CSS animation.
  - **Session sidebar**: `loadSessions()` fetches session list, `renderSessionList()` builds sidebar items with name, relative date, message count badge. Double-click or pencil icon to rename (inline contenteditable). Delete button with confirmation. Auto-names new sessions from first user message (~30 chars). Active session highlighted.
  - **Kill switch**: stop button (red &#9632;) replaces send button during processing. Calls `cancel_task` command. Escape key also cancels. `isProcessing` flag controls button visibility.
  - **Error handling**: `format_user_error()` produces friendly messages. Retry button for transient errors (stores `lastMessage`, `retryLastMessage()` re-submits). "Open Settings" link for auth errors.
  - **Markdown renderer** (inline, no dependencies): fenced code blocks with language labels, inline code, headers (h1-h3), ordered/unordered lists, bold, italic, links. Code blocks protected from inline transformations.
  - **XSS protection**: user messages use `textContent` (never innerHTML), assistant messages go through markdown renderer with `escapeHtml()` on code blocks.
  - **Real-time progress**: status bar shows "Step N: tool_name (status)" and "Evaluating risk..." during risk phase.
  - **Approval dialog**: shows risk badge, score, description, approve/deny buttons.
  - **Settings page**: loads providers via `get_settings`, renders provider cards with expand/collapse. Edit fields for base URL, model, API key (masked display, show/hide toggle). "Test Connection" and "Save" buttons per provider. "Set Active" button to switch provider. "Delete" with confirmation. Add provider via template selection. Security mode dropdown with contextual hints.
  - **Auto-growing textarea**: expands with content up to 150px. Enter sends, Shift+Enter for newline.
  - **Smooth scroll**: `requestAnimationFrame` + `scrollTo` on new messages and tool cards.
- **Requires system libraries**: `webkit2gtk4.1-devel gtk3-devel libsoup3-devel libappindicator-gtk3-devel` (Fedora).
- **Verified working**: Full multi-step agentic pipeline with streaming confirmed. Tested: (1) "What tools do you have?" → LLM correctly lists its 6 tools, (2) "Read https://alejandrogarcia.blog/ and write to HELLO.md" → LLM uses `shell_execute` (curl) to fetch website, then `write_file` to save formatted markdown. Streaming renders progressively with markdown finalization. Tool execution cards show in real time with status icons. Session persistence across app restarts. Provider hot-swap works without restart. Settings UI tested for add/edit/delete/test/activate providers.

### mcp-filesystem (0 tests)
**Status**: Stub — entry point only. Standalone MCP server (no athen-core dependency).

---

## Integration Tests (28 tests)

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
- 5 concurrent clients sending simultaneously — no messages lost

### athen-persistence/tests/integration_persistence.rs (5 tests)
- Full task lifecycle (create→update steps→checkpoint→complete→filter)
- Checkpoint survives simulated crash (file-based DB, drop, reopen)
- Pending message queue ordering (FIFO, pop atomicity, no re-pop)
- 10 concurrent task operations — no corruption
- CheckpointManager file atomicity (temp→fsync→rename)

### athen-risk/tests/integration_risk.rs (6 tests)
- Same action, different trust levels → proportional score changes
- Trust evolution (5 approvals upgrades T0→T1→T2) reduces risk over time
- Rule engine: AuthUser vs Unknown sender for dangerous commands
- Data sensitivity escalation (Plain→PersonalInfo→Secrets)
- Uncertainty penalty impact on otherwise safe actions
- CombinedEvaluator chooses rules vs LLM based on pattern match

### athen-memory/tests/integration_memory.rs (5 tests)
- Knowledge graph: build contact network, explore at depth 1 vs 2
- Vector search: cosine similarity ranking with known embeddings
- Memory facade: remember/recall/forget lifecycle
- SQLite persistence across connection drop/reopen
- Graph exploration respects max_nodes, max_depth, relevance_threshold

### athen-agent/tests/integration_agent.rs (3 tests)
- Mock LLM returns shell_execute tool call → real ShellToolRegistry runs `echo hello` → result fed back → LLM completes
- Mock LLM requests read_file → real tool reads temp file → correct content returned
- Multi-step: tool call → result → another tool call → result → final answer

---

## Configuration

TOML-based configuration with split files and sensible defaults.

### Config discovery (CLI and Tauri app)
1. `~/.athen/config.toml` — user-level config (checked first)
2. `./config/config.toml` — project-local config (fallback)
3. Built-in defaults if no file found

Both `athen-cli` and `athen-app` use the same discovery logic. The Tauri app also creates `~/.athen/` if it does not exist and opens SQLite at `~/.athen/athen.db`.

### Config files
- `config/config.toml` — operation mode, security settings, persistence paths
- `config/models.toml` — LLM providers (API keys, models), profiles (powerful/fast/code/cheap), domain-to-profile assignments
- `config/domains.toml` — per-domain settings (model profile, max steps, timeout)

### API key resolution order
1. Saved config key (`~/.athen/models.toml`) — takes priority (user explicitly saved via Settings UI)
2. Environment variable (e.g. `DEEPSEEK_API_KEY`, `OPENAI_API_KEY`) — fallback
3. Config values like `${DEEPSEEK_API_KEY}` are treated as unresolved placeholders and skipped

This order ensures that keys explicitly saved through the Settings UI always take precedence over environment variables.

### Config loading API (`athen-core::config_loader`)
```rust
load_config(path) -> Result<AthenConfig>       // Load single TOML file
load_config_dir(dir) -> Result<AthenConfig>     // Load config.toml + optional models.toml
save_default_config(path) -> Result<()>         // Write defaults to file
```

---

## External Dependencies

| Crate | Version | Used by | Purpose |
|-------|---------|---------|---------|
| `tokio` | 1.x (full) | All | Async runtime |
| `serde` / `serde_json` | 1.x | All | Serialization |
| `uuid` | 1.x (v4, serde) | All | Unique identifiers |
| `chrono` | 0.4 (serde) | All | Timestamps |
| `thiserror` | 2.x | athen-core | Error derive macro |
| `async-trait` | 0.1 | All traits | Async trait support |
| `tracing` | 0.1 | All | Structured logging |
| `url` | 2.x (serde) | athen-core | URL type for HttpApi backend |
| `tokio-stream` | 0.1 | athen-core, athen-llm | Stream trait for LLM streaming |
| `reqwest` | 0.12 (rustls-tls) | athen-llm, athen-app | HTTP client (pure Rust TLS) |
| `futures` | 0.3 | athen-llm | Stream utilities |
| `regex` | 1.x | athen-risk | Pattern matching for rules engine |
| `rusqlite` | 0.32 (bundled) | athen-persistence, athen-memory | Embedded SQLite |
| `sha2` | 0.10 | athen-persistence | Checkpoint integrity checksums |
| `tempfile` | 3.x | athen-ipc (dev) | Test socket paths |
| `tracing-subscriber` | 0.3 | athen-cli | Structured log output |
| `toml` | 0.8 | athen-core, athen-app | TOML config parsing (core: loading, app: settings save/load) |
| `tauri` | 2.x | athen-app | Desktop app framework |
| `tauri-build` | 2.x | athen-app (build) | Tauri build system |

All HTTP uses `rustls-tls` (pure Rust) — no OpenSSL system dependency needed.

---

## Running the CLI

```bash
# Set API key and run
DEEPSEEK_API_KEY=sk-... cargo run -p athen-cli --release

# Or build first, then run the binary directly
cargo build -p athen-cli --release
DEEPSEEK_API_KEY=sk-... ./target/release/athen-cli
```

The CLI reads from stdin, processes through the full pipeline (coordinator → risk → dispatch → LLM), and prints responses. Exit with Ctrl+D, `/quit`, or `/exit`.
