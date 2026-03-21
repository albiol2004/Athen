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
├── crates/
│   ├── athen-core/               # Shared types + trait contracts (THE CONTRACTS)
│   ├── athen-ipc/                # IPC transport layer
│   ├── athen-sentidos/           # Sense monitors (email, calendar, messaging, user)
│   ├── athen-coordinador/        # Coordinator (router, risk eval, queue, dispatch)
│   ├── athen-agent/              # Agent worker (LLM executor, auditor, timeout)
│   ├── athen-llm/                # LLM provider adapters + router + failover
│   ├── athen-memory/             # Vector index + knowledge graph + SQLite
│   ├── athen-risk/               # Risk scorer + regex rules + LLM fallback
│   ├── athen-persistence/        # SQLite persistence, checkpoints, migrations
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
    async fn budget_remaining(&self) -> Result<BudgetStatus>;
}
```
Selects provider by profile, handles failover chains, enforces budget. Implemented in `athen-llm`.

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

SQLite stores: tasks, task steps, checkpoints, pending messages, contacts, configuration.

**Checkpoint frequency**: After every completed step, every 30s during long steps, before any risky action, before LLM calls.

**Recovery on restart**:
1. Load tasks with status != completed/failed/cancelled
2. Classify: resumable (valid checkpoint), restartable (pending), corrupted (bad checkpoint)
3. Show user recovery UI: Continue / Restart / Cancel per task
4. Execute decisions

**Atomic saves**: Write to temp file → fsync → atomic rename (POSIX guarantees).

---

## Security Model

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

### Kill Switch
- Graceful: Ctrl+Shift+K — stops tasks cleanly, saves state
- Hard: Ctrl+Shift+Alt+K — kills all processes immediately

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
Anthropic, OpenAI, Google, DeepSeek, Ollama (local). Each configurable with API key or OAuth.

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

### Sandbox Tiers
- **L1 actions**: No sandbox (read-only operations)
- **L2 actions**: OS-native sandbox (bwrap/landlock on Linux, sandbox-exec on macOS, Job Objects on Windows) — zero install required
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

### athen-ipc (13 tests)
**Status**: Complete — full IPC transport layer.
- `transport.rs`: `IpcTransport` trait + `UnixTransport` implementation using split `UnixStream` halves with independent `Mutex`es for concurrent send/recv. Length-prefixed framing (4-byte big-endian). 16 MiB message size limit.
- `codec.rs`: `encode()` serializes `IpcMessage` to length-prefixed JSON bytes. `decode()` deserializes. `read_length_prefix()` extracts u32 from 4 bytes.
- `server.rs`: `IpcServer` binds `UnixListener`, accepts connections, spawns per-connection reader tasks, identifies processes by first message's `source` field. Methods: `send_to()`, `broadcast()`, `broadcast_to_type()`, `route()`, `connected_count()`, `shutdown()`. `IpcClient` connects to coordinator, auto-sends Registration message.

### athen-risk (55 tests)
**Status**: Complete — full risk evaluation engine.
- `scorer.rs`: `RiskScorer` implementing the formula `(Ibase × Morigen × Mdatos) + Pincertidumbre`. Confidence penalty: `(1.0 - confidence)^2 × 100`. Maps total to RiskLevel and RiskDecision. Implements `RiskEvaluator` trait.
- `rules.rs`: `RuleEngine` with compiled regex patterns. Detects: dangerous shell commands (rm -rf, sudo, dd, mkfs, chmod 777, pipe to sh), secrets (OpenAI keys sk-..., AWS keys AKIA..., private key headers, passwords in URLs), PII (emails, phone numbers), financial keywords (payment, transfer, purchase, buy), external URLs. Returns `Option<RiskScore>` — `Some` if confident, `None` for LLM fallback.
- `llm_fallback.rs`: `LlmRiskEvaluator` takes `Box<dyn LlmRouter>`. Constructs structured prompt asking LLM to return JSON with impact/sensitivity/confidence/reasoning. Parses response, falls back to conservative defaults on parse failure.
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

### athen-coordinador (37 unit + 4 integration tests)
**Status**: Complete — full coordinator orchestration with persistence and trust integration.
- `router.rs`: `DefaultRouter` implementing `EventRouter`. Maps EventSource→DomainType (Email/Messaging→Communication, Calendar→Agenda, UserInput/System→Base). Priority: UserInput/Calendar=High, Messaging/Email=Normal, System=Low.
- `queue.rs`: `PriorityTaskQueue` implementing `TaskQueue`. Uses `BinaryHeap<PrioritizedTask>` — higher priority first, FIFO within same priority (oldest first).
- `dispatcher.rs`: `Dispatcher` manages agent availability. `register_agent()`, `unregister_agent()`, `assign_task()`, `release_agent()`, `assigned_agent()`.
- `risk.rs`: `CoordinatorRiskEvaluator` wrapping `Box<dyn RiskEvaluator>`. `evaluate_and_decide()` returns `RiskDecision`.
- `lib.rs`: `Coordinator` wiring all components with optional persistence and trust management:
  - `.with_persistence(Box<dyn PersistentStore>)` — attaches SQLite store for task durability. `process_event()` saves tasks after creation, `complete_task()` updates status in DB. Persistence errors are logged but never crash the system.
  - `.with_trust_manager(TrustManager)` — enables contact-aware risk evaluation. `process_event()` resolves sender trust via `TrustManager` and factors it into risk scoring (AuthUser for UserInput, resolved trust for external senders, Neutral fallback). `complete_task()` records approval for implicit trust evolution.
  - `recover_tasks()` — loads non-terminal tasks from persistent store and re-enqueues them on startup. Terminal statuses (Completed, Failed, Cancelled) are skipped.
  - `process_event()` routes→resolves sender trust→evaluates risk→sets status→persists→enqueues. `dispatch_next()` dequeues and assigns to agent. `complete_task()` releases agent→updates DB→records trust approval.
  - `infer_identifier_kind()` helper — infers `IdentifierKind` (Email/Phone/Other) from sender identifier strings.
  - `task_contacts: Mutex<HashMap<TaskId, ContactId>>` — maps task IDs to resolved contact IDs for trust feedback on completion.

### athen-memory (28 tests)
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

### athen-persistence (19 tests)
**Status**: Complete — SQLite persistence with atomic checkpoints.
- `lib.rs`: `Database` struct with `new(path)` and `in_memory()`. Auto-creates tables on init.
- `store.rs`: `SqliteStore` implementing `PersistentStore`. Full CRUD for tasks (with steps serialized as JSON), checkpoints with SHA-256 integrity verification, pending messages with atomic pop (transaction-based select+update).
- `checkpoint.rs`: `CheckpointManager` — atomic file-based backup (write temp → fsync → rename). Integrity verification with SHA-256 checksums.
- Schema: `tasks`, `task_steps`, `checkpoints`, `pending_messages` tables.

### athen-agent (30 unit + 3 integration tests)
**Status**: Complete — LLM-driven task execution with real tool calling, sandbox integration, and session memory.
- `executor.rs`: `DefaultExecutor` implementing `AgentExecutor`. Accepts optional `context_messages: Vec<ChatMessage>` prepended to conversation for session-level memory. Loop: check timeout → check max_steps → build LlmRequest with conversation history + tools → call LlmRouter → execute tool calls via ToolRegistry → record steps via StepAuditor → repeat until LLM says done. Tool call results fed back as `Role::Tool` messages with `tool_call_id` for OpenAI-compatible APIs. System prompt explicitly lists available tools by name and description so the LLM knows what it can do.
- `tools.rs`: `ShellToolRegistry` implementing `ToolRegistry` with 6 built-in tools:
  - `shell_execute` — runs shell commands, routed through bwrap sandbox when available (graceful fallback to unsandboxed if bwrap not installed)
  - `read_file` — reads file contents via `tokio::fs`
  - `write_file` — writes content to a file via `tokio::fs`
  - `list_directory` — lists directory entries as JSON array
  - `memory_store` — stores key-value pairs in in-session `HashMap<String, String>` memory
  - `memory_recall` — retrieves value by key or lists all stored keys
  Each tool has proper JSON Schema parameter definitions for LLM tool calling. `ShellToolRegistry` holds an optional `UnifiedSandbox` (from `athen-sandbox`) — auto-detected on construction. Shell commands execute inside an OS-native sandbox with `SandboxProfile::ReadOnly` when available.
- `auditor.rs`: `InMemoryAuditor` implementing `StepAuditor` with `tokio::sync::Mutex<HashMap<TaskId, Vec<TaskStep>>>`.
- `timeout.rs`: `DefaultTimeoutGuard` — sets deadline at Instant::now() + duration.
- `resource.rs`: `DefaultResourceMonitor` — reads `/proc/self/statm` on Linux for resident memory. `AtomicBool` cache for within-limits state.
- `lib.rs`: `AgentBuilder` with fluent API. `.context_messages(Vec<ChatMessage>)` sets prior conversation history for session memory. Defaults: 50 max_steps, 5-minute timeout, InMemoryAuditor.
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

### athen-llm (13 tests)
**Status**: Complete — router + Anthropic/DeepSeek providers + stubs.
- `budget.rs`: `BudgetTracker` — daily USD limit, token counting, midnight UTC reset, `can_afford()`, `record_usage()`, `status()`, `is_warning()`. Zero budget always rejected.
- `router.rs`: `CircuitBreaker` (Closed/Open/HalfOpen state machine, configurable thresholds/timeout). `DefaultLlmRouter` implementing `LlmRouter` — profile-based routing, failover chains, circuit breakers per provider, budget enforcement.
- `providers/anthropic.rs`: Full `AnthropicProvider` — POST to `/v1/messages`, proper headers (x-api-key, anthropic-version), request/response mapping, SSE streaming, cost estimation for opus/sonnet/haiku tiers.
- `providers/deepseek.rs`: Full `DeepSeekProvider` — OpenAI-compatible API at `api.deepseek.com/v1/chat/completions`. Bearer auth, request/response mapping, SSE streaming, tool call support. Cost estimation: deepseek-chat ($0.14/M input, $0.28/M output), deepseek-reasoner ($0.55/$2.19). Builder pattern: `new(api_key)`, `with_model()`, `with_base_url()`.
- `providers/openai.rs`, `google.rs`: Stubs returning "not yet implemented".
- `providers/ollama.rs`: Stub, `is_available()` returns false.

### athen-cli (0 tests)
**Status**: Complete — working agentic CLI with tool execution.
- `main.rs`: REPL loop wiring all components end-to-end. Reads `DEEPSEEK_API_KEY` from env or config. Uses `config_loader` to discover config from `~/.athen/` or `./config/`. Creates `DeepSeekProvider` → `DefaultLlmRouter` (mapped to all profiles) → `CombinedRiskEvaluator` (real rule engine + LLM fallback) → `Coordinator` (real router, queue, dispatcher). Synchronous stdin with clean EOF/Ctrl+D handling. Risk-gated: low-risk auto-approved, high-risk prompts for confirmation, hard-block rejected. Commands: `/quit`, `/exit`.
- Uses full `AgentBuilder` + `DefaultExecutor` with `ShellToolRegistry` — the agent can execute shell commands, read/write files, list directories, and use in-session memory autonomously via LLM tool calls.
- `SharedRouter`: Wrapper around `Arc<DefaultLlmRouter>` implementing `LlmRouter` trait for sharing between risk evaluator and agent.
- **Verified working**: Full agentic pipeline — user input → coordinator → risk → dispatch → agent executor → LLM → tool calls → execution → result.

### athen-app (0 tests)
**Status**: Complete — Tauri 2 desktop app with full agentic tool execution and conversation history. Verified working.
- `src/lib.rs`: Tauri composition root. Builds app, registers `AppState`, wires command handlers (`send_message`, `get_status`). Registers agent in `setup()` hook using `tauri::async_runtime::block_on()`.
- `src/main.rs`: Entry point with `windows_subsystem = "windows"` for release builds.
- `src/state.rs`: `AppState` — builds `Coordinator` + `DefaultLlmRouter` (DeepSeek) + `CombinedRiskEvaluator`. `SharedRouter` wrapper for `Arc`-based sharing. Reads `DEEPSEEK_API_KEY` from env. Contains `history: Mutex<Vec<ChatMessage>>` for session-level conversation memory persisted across messages within a session.
- `src/commands.rs`: Tauri IPC commands — `send_message` snapshots conversation history, builds a full `AgentExecutor` with `ShellToolRegistry` per request (same pattern as `athen-cli`), passes history as `context_messages` to `AgentBuilder`, executes through coordinator pipeline, then appends user+assistant messages to session history. Returns `ChatResponse` with content, risk level, domain, and tool call info. `get_status` returns connection/model info. Multi-step agentic interactions (tool call → result → next tool call → final answer) are fully supported.
- `tauri.conf.json`: Window 900x700, `frontendDist` points to `../../frontend`. **`"withGlobalTauri": true`** required in `app` section to inject `window.__TAURI__` into the webview.
- `frontend/index.html`: Chat UI shell with header, message container, input form, status bar.
- `frontend/styles.css`: Dark theme (Tokyo Night-inspired) — styled messages, risk badges (safe/caution/danger), tool call display, monospace code blocks, scrollbar.
- `frontend/app.js`: Polls for `window.__TAURI__` availability, calls `invoke('send_message')`, renders responses with risk badges and tool call info, manages input state. Error display on failures.
- **Requires system libraries**: `webkit2gtk4.1-devel gtk3-devel libsoup3-devel libappindicator-gtk3-devel` (Fedora).
- **Verified working**: Full multi-step agentic pipeline confirmed. Tested: (1) "What tools do you have?" → LLM correctly lists its 6 tools, (2) "Read https://alejandrogarcia.blog/ and write to HELLO.md" → LLM uses `shell_execute` (curl) to fetch website, then `write_file` to save formatted markdown. Multi-step tool chains work end-to-end: user input → coordinator → risk → dispatch → AgentExecutor → LLM → tool call → result fed back → next tool call → final response. Conversation history is maintained across messages within a session.

### mcp-filesystem (0 tests)
**Status**: Stub — entry point only. Standalone MCP server (no athen-core dependency).

---

## Integration Tests (25 tests)

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

### Config discovery (CLI)
1. `~/.athen/config.toml` — user-level config (checked first)
2. `./config/config.toml` — project-local config (fallback)
3. Built-in defaults if no file found

### Config files
- `config/config.toml` — operation mode, security settings, persistence paths
- `config/models.toml` — LLM providers (API keys, models), profiles (powerful/fast/code/cheap), domain-to-profile assignments
- `config/domains.toml` — per-domain settings (model profile, max steps, timeout)

### Environment variable override
`DEEPSEEK_API_KEY` env var always takes precedence over config file values. Config values like `${DEEPSEEK_API_KEY}` are treated as unresolved placeholders.

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
| `reqwest` | 0.12 (rustls-tls) | athen-llm | HTTP client (pure Rust TLS) |
| `futures` | 0.3 | athen-llm | Stream utilities |
| `regex` | 1.x | athen-risk | Pattern matching for rules engine |
| `rusqlite` | 0.32 (bundled) | athen-persistence, athen-memory | Embedded SQLite |
| `sha2` | 0.10 | athen-persistence | Checkpoint integrity checksums |
| `tempfile` | 3.x | athen-ipc (dev) | Test socket paths |
| `tracing-subscriber` | 0.3 | athen-cli | Structured log output |
| `toml` | 0.8 | athen-core | TOML config parsing |
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
