//! Arc persistence for branch-like conversation workflows.
//!
//! Arcs replace "chat sessions" with a richer model that supports branching
//! (parent arcs) and merging, similar to git branches.

use std::sync::Arc;

use chrono::Utc;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use athen_core::error::{AthenError, Result};
use athen_core::risk::TriagePlan;

/// The source that originated an Arc.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ArcSource {
    UserInput,
    Email,
    Calendar,
    Messaging,
    System,
}

impl ArcSource {
    pub fn as_str(&self) -> &str {
        match self {
            Self::UserInput => "user_input",
            Self::Email => "email",
            Self::Calendar => "calendar",
            Self::Messaging => "messaging",
            Self::System => "system",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Self {
        match s {
            "email" => Self::Email,
            "calendar" => Self::Calendar,
            "messaging" => Self::Messaging,
            "system" => Self::System,
            _ => Self::UserInput,
        }
    }

    /// Icon character for the UI sidebar.
    pub fn icon(&self) -> &str {
        match self {
            Self::UserInput => "\u{1f4ac}",
            Self::Email => "\u{1f4e7}",
            Self::Calendar => "\u{1f4c5}",
            Self::Messaging => "\u{1f4ac}",
            Self::System => "\u{2699}\u{fe0f}",
        }
    }
}

/// Status of an Arc.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ArcStatus {
    Active,
    Archived,
    Merged,
}

impl ArcStatus {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Active => "active",
            Self::Archived => "archived",
            Self::Merged => "merged",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Self {
        match s {
            "archived" => Self::Archived,
            "merged" => Self::Merged,
            _ => Self::Active,
        }
    }
}

/// A structured plan the agent builds and tracks during arc execution.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ArcPlan {
    pub goal: String,
    pub acceptance_criteria: String,
    pub steps: Vec<PlanStep>,
    pub status: PlanStatus,
}

/// A single step within an [`ArcPlan`].
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PlanStep {
    pub index: u32,
    pub description: String,
    pub status: StepStatus,
    pub output: Option<String>,
}

/// Overall status of an [`ArcPlan`].
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub enum PlanStatus {
    Drafting,
    Executing,
    Completed,
}

/// Status of a single [`PlanStep`].
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub enum StepStatus {
    Pending,
    InProgress,
    Completed,
    Skipped,
}

/// Metadata for an Arc displayed in the sidebar.
#[derive(Debug, Clone, Serialize)]
pub struct ArcMeta {
    pub id: String,
    pub name: String,
    pub source: ArcSource,
    pub status: ArcStatus,
    pub parent_arc_id: Option<String>,
    pub merged_into_arc_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub entry_count: u32,
    /// Last channel the user actually replied through, used by the
    /// approval router to pick where to ask follow-up questions. `None`
    /// means "use the default for this arc's source" (e.g. Telegram for
    /// a Messaging arc, in-app otherwise).
    pub primary_reply_channel: Option<String>,
    /// `AgentProfile::id` to run this arc's tasks under. `None` means
    /// "use the seeded default profile" — equivalent to today's behavior.
    pub active_profile_id: Option<String>,
    /// Largest `arc_entries.id` covered by the latest compaction summary
    /// for this arc. `None` means the arc has never been compacted; the
    /// executor's context view falls through to raw entries.
    pub summarized_through_entry_id: Option<i64>,
    /// Provider this arc is currently locked to for in-flight protection.
    /// Set on the first LLM call of a task (snapshotting the active
    /// provider at task start), cleared when the arc transitions back to
    /// idle. `None` means "follow the global active provider." See
    /// `docs/PROVIDER_PINNING.md`.
    pub pinned_provider_id: Option<String>,
    /// Concrete model slug that resolved on the first LLM call of the
    /// task (e.g. `"deepseek-v4-pro"`, `"claude-sonnet-4-6"`). Stored
    /// alongside `pinned_provider_id` for a future "warn on slug drift"
    /// path: if the user edits `tier_models` mid-task and the same
    /// `(provider, tier)` pair now resolves to a different slug, we'll
    /// log + surface the divergence. Today the column is captured but
    /// routing only consults `pinned_provider_id`.
    pub pinned_slug: Option<String>,
    /// User-set per-arc reasoning-effort override. Stored as the wire
    /// string (`"default"` / `"off"` / `"minimal"` / `"low"` / `"medium"`
    /// / `"high"` / `"max"`) so adding levels later doesn't require a
    /// type migration. `None` falls through to `ReasoningEffort::Default`
    /// — see `docs/REASONING_EFFORT.md` for the resolution chain.
    /// Last-write-wins: distinct from the first-call-wins `pinned_*`
    /// columns because this is a durable user preference, not an
    /// in-flight snapshot.
    pub reasoning_effort_override: Option<String>,
    /// User-set per-arc tier override. Stored as the serialized
    /// `ModelProfile` wire form (`"Judges"` / `"Fast"` / `"Code"` /
    /// `"Powerful"`; legacy `"Cheap"`) so adding tiers later doesn't require a type
    /// migration. `None` means "fall through to the task's complexity
    /// tag (if any) and otherwise to the static call-site tier". Same
    /// last-write-wins semantics as `reasoning_effort_override` —
    /// distinct from the first-call-wins `pinned_*` columns because this
    /// is a durable user preference, not an in-flight snapshot.
    pub tier_override: Option<String>,
    /// User-set per-arc security-mode override. Stored as the lowercase
    /// wire string (`"bunker"` / `"assistant"` / `"yolo"`) so adding modes
    /// later doesn't require a type migration. `None` falls through to the
    /// live global `SecurityConfig.mode`. Same last-write-wins semantics as
    /// the other `*_override` columns — a durable user preference, not an
    /// in-flight snapshot.
    pub security_mode_override: Option<String>,
    /// Optional Project this arc belongs to (`projects.id`). `None` means the
    /// arc is not part of any project. Sub-arcs inherit the parent arc's
    /// `project_id` at creation. See `docs/PROJECTS.md`.
    pub project_id: Option<String>,
    /// Filesystem path to the research paper produced for this arc, if any.
    /// `None` means no Deep Research paper has been generated. See
    /// `docs/DEEP_RESEARCH.md`.
    pub research_paper_path: Option<String>,
    /// The research question driving this arc's Deep Research run, if any.
    /// `None` means the arc is not a Deep Research arc. See
    /// `docs/DEEP_RESEARCH.md`.
    pub research_question: Option<String>,
    /// Mini-plan drafted by the triage LLM call (same call that evaluates
    /// risk + complexity — see `RiskScore.plan`). Captured once at task
    /// creation, then survives compaction as arc-level metadata. The
    /// executor renders it into a `<MISSION>` block in the static prefix,
    /// the completion judge reads `acceptance_criteria`, and the
    /// compactor reads both fields as a relevance + drift-fence signal.
    /// Both fields must be present and non-empty for the plan to be
    /// considered usable — half-plans collapse to `None` (same invariant
    /// as the parser in `athen-risk::llm_fallback`).
    pub triage_plan: Option<TriagePlan>,
    /// Free-text user goal for this arc, set by the agent's `set_user_goal`
    /// tool. Describes what the user ultimately wants to achieve.
    pub user_goal: Option<String>,
    /// Measurable criteria that determine when the goal is complete.
    pub user_goal_criteria: Option<String>,
    /// Current status of the goal: `"active"` or `"blocked"`.
    pub goal_status: Option<String>,
    /// Human-readable reason the goal is blocked (only set when
    /// `goal_status == "blocked"`).
    pub goal_blocked_reason: Option<String>,
    /// Structured plan the agent builds and updates during arc execution.
    /// Stored as a single JSON blob (`plan_json` column). `None` means no
    /// plan has been drafted yet.
    pub plan: Option<ArcPlan>,
}

/// Assemble a `TriagePlan` from two raw columns. Returns `None` unless
/// both fields are present and non-empty after trimming — the same
/// "half-plan is no plan" rule the LLM parser enforces, applied here so
/// a database that somehow ended up with one column populated and the
/// other null doesn't surface a malformed plan to downstream consumers.
fn assemble_plan(acceptance: Option<String>, scope: Option<String>) -> Option<TriagePlan> {
    match (acceptance, scope) {
        (Some(a), Some(s)) if !a.trim().is_empty() && !s.trim().is_empty() => Some(TriagePlan {
            acceptance_criteria: a,
            scope: s,
        }),
        _ => None,
    }
}

/// The type of an entry within an Arc.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum EntryType {
    Message,
    ToolCall,
    EmailEvent,
    CalendarEvent,
    SystemEvent,
    /// Compactor-generated summary collapsing earlier entries. The covered
    /// prefix is identified by `ArcMeta.summarized_through_entry_id`.
    Summary,
}

impl EntryType {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Message => "message",
            Self::ToolCall => "tool_call",
            Self::EmailEvent => "email_event",
            Self::CalendarEvent => "calendar_event",
            Self::SystemEvent => "system_event",
            Self::Summary => "summary",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Self {
        match s {
            "tool_call" => Self::ToolCall,
            "email_event" => Self::EmailEvent,
            "calendar_event" => Self::CalendarEvent,
            "system_event" => Self::SystemEvent,
            "summary" => Self::Summary,
            _ => Self::Message,
        }
    }
}

/// A single entry in an Arc.
#[derive(Debug, Clone, Serialize)]
pub struct ArcEntry {
    pub id: i64,
    pub arc_id: String,
    pub entry_type: EntryType,
    pub source: String,
    pub content: String,
    pub metadata: Option<serde_json::Value>,
    pub created_at: String,
    /// Groups entries that belong to the same conversation turn so the UI can
    /// render tool_call children under their assistant message. `None` for
    /// legacy rows written before the column existed.
    pub turn_id: Option<String>,
}

/// SQLite-backed Arc storage. Cheap to clone — wraps a shared connection.
#[derive(Clone)]
pub struct ArcStore {
    conn: Arc<Mutex<Connection>>,
}

impl ArcStore {
    /// Create a new `ArcStore` wrapping the given connection.
    pub fn new(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
    }

    /// Create the arcs and arc_entries tables if they do not exist, and run
    /// any column-level migrations against existing databases.
    pub async fn init_schema(&self) -> Result<()> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute_batch(ARC_SCHEMA_SQL)
                .map_err(|e| AthenError::Other(format!("Failed to init arc schema: {e}")))?;

            // Column-level migration: `turn_id` was added so the UI can group
            // tool_call entries under their assistant message. Older databases
            // created before this change need the column added in place.
            let has_turn_id: bool = conn
                .prepare("PRAGMA table_info(arc_entries)")
                .and_then(|mut stmt| {
                    let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
                    let mut found = false;
                    for r in rows {
                        if r? == "turn_id" {
                            found = true;
                            break;
                        }
                    }
                    Ok(found)
                })
                .map_err(|e| AthenError::Other(format!("Inspect arc_entries cols: {e}")))?;
            if !has_turn_id {
                conn.execute("ALTER TABLE arc_entries ADD COLUMN turn_id TEXT", [])
                    .map_err(|e| AthenError::Other(format!("Add turn_id column: {e}")))?;
            }

            // Index supports the rehydration query that groups by turn_id.
            conn.execute(
                "CREATE INDEX IF NOT EXISTS idx_arc_entries_turn ON arc_entries(turn_id)",
                [],
            )
            .map_err(|e| AthenError::Other(format!("Create turn_id index: {e}")))?;

            // Column-level migration: `primary_reply_channel` was added so the
            // approval router can remember which channel the user actually
            // engages on for each arc. Older databases need the column added
            // in place; new ones get it from ARC_SCHEMA_SQL.
            let has_reply_channel: bool = conn
                .prepare("PRAGMA table_info(arcs)")
                .and_then(|mut stmt| {
                    let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
                    let mut found = false;
                    for r in rows {
                        if r? == "primary_reply_channel" {
                            found = true;
                            break;
                        }
                    }
                    Ok(found)
                })
                .map_err(|e| AthenError::Other(format!("Inspect arcs cols: {e}")))?;
            if !has_reply_channel {
                conn.execute("ALTER TABLE arcs ADD COLUMN primary_reply_channel TEXT", [])
                    .map_err(|e| {
                        AthenError::Other(format!("Add primary_reply_channel column: {e}"))
                    })?;
            }

            // Column-level migration: `active_profile_id` was added so each
            // arc can run under a specific `AgentProfile`. Older databases
            // get the column added in place; new ones get it from
            // ARC_SCHEMA_SQL. NULL means "use the seeded default profile".
            let has_profile_id: bool = conn
                .prepare("PRAGMA table_info(arcs)")
                .and_then(|mut stmt| {
                    let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
                    let mut found = false;
                    for r in rows {
                        if r? == "active_profile_id" {
                            found = true;
                            break;
                        }
                    }
                    Ok(found)
                })
                .map_err(|e| AthenError::Other(format!("Inspect arcs cols (profile): {e}")))?;
            if !has_profile_id {
                conn.execute("ALTER TABLE arcs ADD COLUMN active_profile_id TEXT", [])
                    .map_err(|e| AthenError::Other(format!("Add active_profile_id: {e}")))?;
            }

            // Column-level migration: `summarized_through_entry_id` was added
            // for the arc compaction system. It points at the largest
            // `arc_entries.id` covered by the latest compaction summary.
            // Older databases need the column added in place; new ones get
            // it from ARC_SCHEMA_SQL. NULL means "never compacted".
            let has_summarized_through: bool = conn
                .prepare("PRAGMA table_info(arcs)")
                .and_then(|mut stmt| {
                    let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
                    let mut found = false;
                    for r in rows {
                        if r? == "summarized_through_entry_id" {
                            found = true;
                            break;
                        }
                    }
                    Ok(found)
                })
                .map_err(|e| AthenError::Other(format!("Inspect arcs cols (summarized): {e}")))?;
            if !has_summarized_through {
                conn.execute(
                    "ALTER TABLE arcs ADD COLUMN summarized_through_entry_id INTEGER",
                    [],
                )
                .map_err(|e| AthenError::Other(format!("Add summarized_through_entry_id: {e}")))?;
            }

            // Column-level migration: `pinned_provider_id` + `pinned_slug`
            // were added so an arc can lock onto the provider that started
            // its in-flight task — preventing mid-task provider switches
            // from rehydrating provider-A's messages onto provider B. NULL
            // means "follow the global active provider." An earlier
            // landing of this feature used `pinned_tier`; that column is
            // dropped here since the semantic was wrong (different call
            // sites legitimately use different tiers within one task).
            let cols: std::collections::HashSet<String> = conn
                .prepare("PRAGMA table_info(arcs)")
                .and_then(|mut stmt| {
                    let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
                    let mut set = std::collections::HashSet::new();
                    for r in rows {
                        set.insert(r?);
                    }
                    Ok(set)
                })
                .map_err(|e| AthenError::Other(format!("Inspect arcs cols (pinning): {e}")))?;
            if !cols.contains("pinned_provider_id") {
                conn.execute("ALTER TABLE arcs ADD COLUMN pinned_provider_id TEXT", [])
                    .map_err(|e| AthenError::Other(format!("Add pinned_provider_id: {e}")))?;
            }
            if !cols.contains("pinned_slug") {
                conn.execute("ALTER TABLE arcs ADD COLUMN pinned_slug TEXT", [])
                    .map_err(|e| AthenError::Other(format!("Add pinned_slug: {e}")))?;
            }
            if cols.contains("pinned_tier") {
                // SQLite 3.35+ supports DROP COLUMN. If the runtime is
                // older, the column lingers harmlessly — readers don't
                // reference it.
                let _ = conn.execute("ALTER TABLE arcs DROP COLUMN pinned_tier", []);
            }

            // Column-level migration: `reasoning_effort_override` lets a
            // user dial reasoning effort per-arc. Stored as the wire
            // string so adding levels later (e.g. `xhigh`) is a no-op
            // for the schema.
            if !cols.contains("reasoning_effort_override") {
                conn.execute(
                    "ALTER TABLE arcs ADD COLUMN reasoning_effort_override TEXT",
                    [],
                )
                .map_err(|e| AthenError::Other(format!("Add reasoning_effort_override: {e}")))?;
            }

            // Column-level migration: `tier_override` lets a user force
            // a specific `ModelProfile` tier for an arc, bypassing the
            // task-complexity heuristic. Stored as the wire form (the
            // `ModelProfile` variant name) so adding tiers later is a
            // no-op for the schema.
            if !cols.contains("tier_override") {
                conn.execute("ALTER TABLE arcs ADD COLUMN tier_override TEXT", [])
                    .map_err(|e| AthenError::Other(format!("Add tier_override: {e}")))?;
            }

            // Column-level migration: `security_mode_override` lets a user
            // force a security posture (bunker / assistant / yolo) for an
            // arc, overriding the global mode. Stored as the lowercase wire
            // string so adding modes later is a no-op for the schema.
            if !cols.contains("security_mode_override") {
                conn.execute(
                    "ALTER TABLE arcs ADD COLUMN security_mode_override TEXT",
                    [],
                )
                .map_err(|e| AthenError::Other(format!("Add security_mode_override: {e}")))?;
            }

            // Column-level migration: `triage_plan_acceptance` +
            // `triage_plan_scope` hold the mini-plan drafted by the LLM
            // risk-evaluation call at task triage. Two columns rather
            // than one JSON blob so a future "search arcs by mission"
            // can index `acceptance_criteria` directly.
            if !cols.contains("triage_plan_acceptance") {
                conn.execute(
                    "ALTER TABLE arcs ADD COLUMN triage_plan_acceptance TEXT",
                    [],
                )
                .map_err(|e| AthenError::Other(format!("Add triage_plan_acceptance: {e}")))?;
            }
            if !cols.contains("triage_plan_scope") {
                conn.execute("ALTER TABLE arcs ADD COLUMN triage_plan_scope TEXT", [])
                    .map_err(|e| AthenError::Other(format!("Add triage_plan_scope: {e}")))?;
            }

            // Column-level migration: goal data model — user_goal,
            // user_goal_criteria, goal_status, goal_blocked_reason.
            if !cols.contains("user_goal") {
                conn.execute("ALTER TABLE arcs ADD COLUMN user_goal TEXT", [])
                    .map_err(|e| AthenError::Other(format!("Add user_goal: {e}")))?;
            }
            if !cols.contains("user_goal_criteria") {
                conn.execute("ALTER TABLE arcs ADD COLUMN user_goal_criteria TEXT", [])
                    .map_err(|e| AthenError::Other(format!("Add user_goal_criteria: {e}")))?;
            }
            if !cols.contains("goal_status") {
                conn.execute("ALTER TABLE arcs ADD COLUMN goal_status TEXT", [])
                    .map_err(|e| AthenError::Other(format!("Add goal_status: {e}")))?;
            }
            if !cols.contains("goal_blocked_reason") {
                conn.execute("ALTER TABLE arcs ADD COLUMN goal_blocked_reason TEXT", [])
                    .map_err(|e| AthenError::Other(format!("Add goal_blocked_reason: {e}")))?;
            }

            // Column-level migration: `plan_json` stores the agent-authored
            // structured plan as a single JSON blob.
            if !cols.contains("plan_json") {
                conn.execute("ALTER TABLE arcs ADD COLUMN plan_json TEXT", [])
                    .map_err(|e| AthenError::Other(format!("Add plan_json: {e}")))?;
            }

            // Column-level migration: `project_id` lets an arc belong to a
            // Project (the ChatGPT/Claude-style container that groups many
            // arcs around common work). NULL means the arc is not part of
            // any project. See `docs/PROJECTS.md`.
            if !cols.contains("project_id") {
                conn.execute("ALTER TABLE arcs ADD COLUMN project_id TEXT", [])
                    .map_err(|e| AthenError::Other(format!("Add project_id: {e}")))?;
            }

            // Column-level migration: Deep Research arc metadata. NULL means
            // the arc is not a Deep Research arc. See `docs/DEEP_RESEARCH.md`.
            if !cols.contains("research_paper_path") {
                conn.execute("ALTER TABLE arcs ADD COLUMN research_paper_path TEXT", [])
                    .map_err(|e| AthenError::Other(format!("Add research_paper_path: {e}")))?;
            }
            if !cols.contains("research_question") {
                conn.execute("ALTER TABLE arcs ADD COLUMN research_question TEXT", [])
                    .map_err(|e| AthenError::Other(format!("Add research_question: {e}")))?;
            }

            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Create a new Arc with Active status.
    pub async fn create_arc(&self, id: &str, name: &str, source: ArcSource) -> Result<()> {
        let conn = self.conn.clone();
        let id = id.to_string();
        let name = name.to_string();
        let source_str = source.as_str().to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let now = Utc::now().to_rfc3339();
            conn.execute(
                "INSERT INTO arcs (id, name, source, status, created_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![id, name, source_str, "active", now, now],
            )
            .map_err(|e| AthenError::Other(format!("Create arc: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Create a new Arc with Active status, optionally belonging to a Project.
    /// Pass `None` for `project_id` to create an arc with no project (identical
    /// to [`Self::create_arc`]).
    pub async fn create_arc_in_project(
        &self,
        id: &str,
        name: &str,
        source: ArcSource,
        project_id: Option<&str>,
    ) -> Result<()> {
        let conn = self.conn.clone();
        let id = id.to_string();
        let name = name.to_string();
        let source_str = source.as_str().to_string();
        let project_id = project_id.map(|s| s.to_string());
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let now = Utc::now().to_rfc3339();
            conn.execute(
                "INSERT INTO arcs (id, name, source, status, created_at, updated_at, project_id) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![id, name, source_str, "active", now, now, project_id],
            )
            .map_err(|e| AthenError::Other(format!("Create arc in project: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Create a new Arc branched from a parent Arc. The child inherits the
    /// parent arc's `project_id` so a delegation sub-arc stays within the same
    /// Project as the arc that spawned it.
    pub async fn create_arc_with_parent(
        &self,
        id: &str,
        name: &str,
        source: ArcSource,
        parent_arc_id: &str,
    ) -> Result<()> {
        let conn = self.conn.clone();
        let id = id.to_string();
        let name = name.to_string();
        let source_str = source.as_str().to_string();
        let parent_arc_id = parent_arc_id.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let now = Utc::now().to_rfc3339();
            // Inherit the parent's project_id (if any) so the child arc lands
            // in the same Project as the arc it branched from.
            let parent_project_id: Option<String> = conn
                .query_row(
                    "SELECT project_id FROM arcs WHERE id = ?1",
                    params![parent_arc_id],
                    |row| row.get::<_, Option<String>>(0),
                )
                .map_err(|e| AthenError::Other(format!("Lookup parent project_id: {e}")))?;
            conn.execute(
                "INSERT INTO arcs (id, name, source, status, parent_arc_id, created_at, updated_at, project_id) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![id, name, source_str, "active", parent_arc_id, now, now, parent_project_id],
            )
            .map_err(|e| AthenError::Other(format!("Create arc with parent: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Get a single Arc by ID.
    pub async fn get_arc(&self, id: &str) -> Result<Option<ArcMeta>> {
        let conn = self.conn.clone();
        let id = id.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let mut stmt = conn
                .prepare(
                    "SELECT a.id, a.name, a.source, a.status, a.parent_arc_id, \
                            a.merged_into_arc_id, a.created_at, a.updated_at, \
                            COALESCE(e.cnt, 0) AS entry_count, \
                            a.primary_reply_channel, a.active_profile_id, \
                            a.summarized_through_entry_id, \
                            a.pinned_provider_id, a.pinned_slug, \
                            a.reasoning_effort_override, \
                            a.tier_override, \
                            a.triage_plan_acceptance, a.triage_plan_scope, \
                            a.user_goal, a.user_goal_criteria, \
                            a.goal_status, a.goal_blocked_reason, \
                            a.plan_json, a.security_mode_override, \
                            a.project_id, \
                            a.research_paper_path, a.research_question \
                     FROM arcs a \
                     LEFT JOIN ( \
                         SELECT arc_id, COUNT(*) AS cnt \
                         FROM arc_entries GROUP BY arc_id \
                     ) e ON a.id = e.arc_id \
                     WHERE a.id = ?1",
                )
                .map_err(|e| AthenError::Other(format!("Prepare get arc: {e}")))?;

            let mut rows = stmt
                .query_map(params![id], |row| {
                    Ok(ArcMeta {
                        id: row.get(0)?,
                        name: row.get(1)?,
                        source: ArcSource::from_str(&row.get::<_, String>(2)?),
                        status: ArcStatus::from_str(&row.get::<_, String>(3)?),
                        parent_arc_id: row.get(4)?,
                        merged_into_arc_id: row.get(5)?,
                        created_at: row.get(6)?,
                        updated_at: row.get(7)?,
                        entry_count: row.get::<_, u32>(8)?,
                        primary_reply_channel: row.get(9)?,
                        active_profile_id: row.get(10)?,
                        summarized_through_entry_id: row.get(11)?,
                        pinned_provider_id: row.get(12)?,
                        pinned_slug: row.get(13)?,
                        reasoning_effort_override: row.get(14)?,
                        tier_override: row.get(15)?,
                        triage_plan: assemble_plan(row.get(16)?, row.get(17)?),
                        user_goal: row.get(18)?,
                        user_goal_criteria: row.get(19)?,
                        goal_status: row.get(20)?,
                        goal_blocked_reason: row.get(21)?,
                        plan: row
                            .get::<_, Option<String>>(22)?
                            .and_then(|s| serde_json::from_str(&s).ok()),
                        security_mode_override: row.get(23)?,
                        project_id: row.get::<_, Option<String>>(24)?,
                        research_paper_path: row.get::<_, Option<String>>(25)?,
                        research_question: row.get::<_, Option<String>>(26)?,
                    })
                })
                .map_err(|e| AthenError::Other(format!("Query get arc: {e}")))?;

            match rows.next() {
                Some(row) => {
                    let meta = row.map_err(|e| AthenError::Other(format!("Get arc row: {e}")))?;
                    Ok(Some(meta))
                }
                None => Ok(None),
            }
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// List all arcs (including sub-arcs) ordered by most recently updated
    /// first, with entry counts. Use [`Self::list_root_arcs`] for the sidebar
    /// view that should hide delegation sub-arcs.
    pub async fn list_arcs(&self) -> Result<Vec<ArcMeta>> {
        self.list_arcs_inner(false).await
    }

    /// List only root arcs (those with no `parent_arc_id`). Sub-arcs created
    /// by `delegate_to_agent` are hidden — their content is meant to be
    /// rendered inline under the parent's tool call, not as a separate
    /// sidebar entry.
    pub async fn list_root_arcs(&self) -> Result<Vec<ArcMeta>> {
        self.list_arcs_inner(true).await
    }

    async fn list_arcs_inner(&self, roots_only: bool) -> Result<Vec<ArcMeta>> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let where_clause = if roots_only {
                "WHERE (a.parent_arc_id IS NULL OR a.source = 'user_input') "
            } else {
                ""
            };
            let sql = format!(
                "SELECT a.id, a.name, a.source, a.status, a.parent_arc_id, \
                        a.merged_into_arc_id, a.created_at, a.updated_at, \
                        COALESCE(e.cnt, 0) AS entry_count, \
                        a.primary_reply_channel, a.active_profile_id, \
                        a.summarized_through_entry_id, \
                        a.pinned_provider_id, a.pinned_slug, \
                        a.reasoning_effort_override, \
                        a.tier_override, \
                        a.triage_plan_acceptance, a.triage_plan_scope, \
                        a.user_goal, a.user_goal_criteria, \
                        a.goal_status, a.goal_blocked_reason, \
                        a.plan_json, a.security_mode_override, \
                        a.project_id, \
                        a.research_paper_path, a.research_question \
                 FROM arcs a \
                 LEFT JOIN ( \
                     SELECT arc_id, COUNT(*) AS cnt \
                     FROM arc_entries GROUP BY arc_id \
                 ) e ON a.id = e.arc_id \
                 {}ORDER BY a.updated_at DESC",
                where_clause
            );
            let mut stmt = conn
                .prepare(&sql)
                .map_err(|e| AthenError::Other(format!("Prepare list arcs: {e}")))?;

            let rows = stmt
                .query_map([], |row| {
                    Ok(ArcMeta {
                        id: row.get(0)?,
                        name: row.get(1)?,
                        source: ArcSource::from_str(&row.get::<_, String>(2)?),
                        status: ArcStatus::from_str(&row.get::<_, String>(3)?),
                        parent_arc_id: row.get(4)?,
                        merged_into_arc_id: row.get(5)?,
                        created_at: row.get(6)?,
                        updated_at: row.get(7)?,
                        entry_count: row.get::<_, u32>(8)?,
                        primary_reply_channel: row.get(9)?,
                        active_profile_id: row.get(10)?,
                        summarized_through_entry_id: row.get(11)?,
                        pinned_provider_id: row.get(12)?,
                        pinned_slug: row.get(13)?,
                        reasoning_effort_override: row.get(14)?,
                        tier_override: row.get(15)?,
                        triage_plan: assemble_plan(row.get(16)?, row.get(17)?),
                        user_goal: row.get(18)?,
                        user_goal_criteria: row.get(19)?,
                        goal_status: row.get(20)?,
                        goal_blocked_reason: row.get(21)?,
                        plan: row
                            .get::<_, Option<String>>(22)?
                            .and_then(|s| serde_json::from_str(&s).ok()),
                        security_mode_override: row.get(23)?,
                        project_id: row.get::<_, Option<String>>(24)?,
                        research_paper_path: row.get::<_, Option<String>>(25)?,
                        research_question: row.get::<_, Option<String>>(26)?,
                    })
                })
                .map_err(|e| AthenError::Other(format!("Query list arcs: {e}")))?;

            let mut arcs = Vec::new();
            for row in rows {
                arcs.push(row.map_err(|e| AthenError::Other(format!("List arcs row: {e}")))?);
            }
            Ok(arcs)
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Rename an arc.
    pub async fn rename_arc(&self, id: &str, name: &str) -> Result<()> {
        let conn = self.conn.clone();
        let id = id.to_string();
        let name = name.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let now = Utc::now().to_rfc3339();
            conn.execute(
                "UPDATE arcs SET name = ?1, updated_at = ?2 WHERE id = ?3",
                params![name, now, id],
            )
            .map_err(|e| AthenError::Other(format!("Rename arc: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Delete an arc and all its entries.
    pub async fn delete_arc(&self, id: &str) -> Result<()> {
        let conn = self.conn.clone();
        let id = id.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute("DELETE FROM arc_entries WHERE arc_id = ?1", params![id])
                .map_err(|e| AthenError::Other(format!("Delete arc entries: {e}")))?;
            conn.execute("DELETE FROM arcs WHERE id = ?1", params![id])
                .map_err(|e| AthenError::Other(format!("Delete arc: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Archive an arc by setting its status to Archived.
    pub async fn archive_arc(&self, id: &str) -> Result<()> {
        let conn = self.conn.clone();
        let id = id.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let now = Utc::now().to_rfc3339();
            conn.execute(
                "UPDATE arcs SET status = ?1, updated_at = ?2 WHERE id = ?3",
                params!["archived", now, id],
            )
            .map_err(|e| AthenError::Other(format!("Archive arc: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Merge all entries from source arc into target arc.
    ///
    /// Moves entries by updating their `arc_id`, sets the source arc status to
    /// Merged with `merged_into_arc_id` pointing to the target, and touches the
    /// target's `updated_at`.
    pub async fn merge_arc(&self, source_id: &str, target_id: &str) -> Result<()> {
        let conn = self.conn.clone();
        let source_id = source_id.to_string();
        let target_id = target_id.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let now = Utc::now().to_rfc3339();

            // Move all entries from source to target.
            conn.execute(
                "UPDATE arc_entries SET arc_id = ?1 WHERE arc_id = ?2",
                params![target_id, source_id],
            )
            .map_err(|e| AthenError::Other(format!("Merge arc entries: {e}")))?;

            // Mark source as merged.
            conn.execute(
                "UPDATE arcs SET status = ?1, merged_into_arc_id = ?2, updated_at = ?3 \
                 WHERE id = ?4",
                params!["merged", target_id, now, source_id],
            )
            .map_err(|e| AthenError::Other(format!("Mark source arc merged: {e}")))?;

            // Touch target's updated_at.
            conn.execute(
                "UPDATE arcs SET updated_at = ?1 WHERE id = ?2",
                params![now, target_id],
            )
            .map_err(|e| AthenError::Other(format!("Touch target arc: {e}")))?;

            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Delete all entries strictly after `after_entry_id` for the given arc.
    /// Returns the IDs of deleted entries (useful for checkpoint cascade).
    pub async fn delete_entries_after(
        &self,
        arc_id: &str,
        after_entry_id: i64,
    ) -> Result<Vec<i64>> {
        let conn = self.conn.clone();
        let arc_id = arc_id.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let mut stmt = conn
                .prepare("SELECT id FROM arc_entries WHERE arc_id = ?1 AND id > ?2 ORDER BY id ASC")
                .map_err(|e| {
                    AthenError::Other(format!("Prepare delete_entries_after select: {e}"))
                })?;
            let ids: Vec<i64> = stmt
                .query_map(params![arc_id, after_entry_id], |row| row.get(0))
                .map_err(|e| AthenError::Other(format!("Query delete_entries_after: {e}")))?
                .filter_map(|r| r.ok())
                .collect();
            if !ids.is_empty() {
                conn.execute(
                    "DELETE FROM arc_entries WHERE arc_id = ?1 AND id > ?2",
                    params![arc_id, after_entry_id],
                )
                .map_err(|e| AthenError::Other(format!("Delete entries after: {e}")))?;
            }
            Ok(ids)
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Delete `from_entry_id` and all entries after it for the given arc.
    /// Returns the IDs of deleted entries (inclusive of `from_entry_id`).
    pub async fn delete_entries_from(&self, arc_id: &str, from_entry_id: i64) -> Result<Vec<i64>> {
        let conn = self.conn.clone();
        let arc_id = arc_id.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let mut stmt = conn
                .prepare(
                    "SELECT id FROM arc_entries WHERE arc_id = ?1 AND id >= ?2 ORDER BY id ASC",
                )
                .map_err(|e| {
                    AthenError::Other(format!("Prepare delete_entries_from select: {e}"))
                })?;
            let ids: Vec<i64> = stmt
                .query_map(params![arc_id, from_entry_id], |row| row.get(0))
                .map_err(|e| AthenError::Other(format!("Query delete_entries_from: {e}")))?
                .filter_map(|r| r.ok())
                .collect();
            if !ids.is_empty() {
                conn.execute(
                    "DELETE FROM arc_entries WHERE arc_id = ?1 AND id >= ?2",
                    params![arc_id, from_entry_id],
                )
                .map_err(|e| AthenError::Other(format!("Delete entries from: {e}")))?;
            }
            Ok(ids)
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Update the text content of a single entry.
    pub async fn update_entry_content(&self, entry_id: i64, new_content: &str) -> Result<()> {
        let conn = self.conn.clone();
        let new_content = new_content.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let changed = conn
                .execute(
                    "UPDATE arc_entries SET content = ?1 WHERE id = ?2",
                    params![new_content, entry_id],
                )
                .map_err(|e| AthenError::Other(format!("Update entry content: {e}")))?;
            if changed == 0 {
                return Err(AthenError::Other(format!("Entry {entry_id} not found")));
            }
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Copy all entries from `source_arc_id` up to and including
    /// `up_to_entry_id` into `target_arc_id`. Returns the number of
    /// entries copied. The new entries get fresh auto-increment IDs.
    pub async fn copy_entries_up_to(
        &self,
        source_arc_id: &str,
        target_arc_id: &str,
        up_to_entry_id: i64,
    ) -> Result<u32> {
        let conn = self.conn.clone();
        let source = source_arc_id.to_string();
        let target = target_arc_id.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let count = conn
                .execute(
                    "INSERT INTO arc_entries (arc_id, entry_type, source, content, metadata, created_at, turn_id) \
                     SELECT ?1, entry_type, source, content, metadata, created_at, turn_id \
                     FROM arc_entries WHERE arc_id = ?2 AND id <= ?3 ORDER BY id ASC",
                    params![target, source, up_to_entry_id],
                )
                .map_err(|e| AthenError::Other(format!("Copy entries: {e}")))?;
            Ok(count as u32)
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Reset `summarized_through_entry_id` to NULL when truncation
    /// invalidates the compaction pointer.
    pub async fn reset_summarized_through(&self, arc_id: &str) -> Result<()> {
        let conn = self.conn.clone();
        let arc_id = arc_id.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute(
                "UPDATE arcs SET summarized_through_entry_id = NULL WHERE id = ?1",
                params![arc_id],
            )
            .map_err(|e| AthenError::Other(format!("Reset summarized_through: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Load a single entry by its ID.
    pub async fn get_entry(&self, entry_id: i64) -> Result<Option<ArcEntry>> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let mut stmt = conn
                .prepare(
                    "SELECT id, arc_id, entry_type, source, content, metadata, created_at, turn_id \
                     FROM arc_entries WHERE id = ?1",
                )
                .map_err(|e| AthenError::Other(format!("Prepare get_entry: {e}")))?;
            let mut rows = stmt
                .query_map(params![entry_id], |row| {
                    let metadata_str: Option<String> = row.get(5)?;
                    let metadata = metadata_str.and_then(|s| serde_json::from_str(&s).ok());
                    Ok(ArcEntry {
                        id: row.get(0)?,
                        arc_id: row.get(1)?,
                        entry_type: EntryType::from_str(&row.get::<_, String>(2)?),
                        source: row.get(3)?,
                        content: row.get(4)?,
                        metadata,
                        created_at: row.get(6)?,
                        turn_id: row.get(7)?,
                    })
                })
                .map_err(|e| AthenError::Other(format!("Query get_entry: {e}")))?;
            match rows.next() {
                Some(row) => {
                    Ok(Some(row.map_err(|e| {
                        AthenError::Other(format!("Entry row: {e}"))
                    })?))
                }
                None => Ok(None),
            }
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Set the agent profile this arc runs under. Pass `None` to clear (which
    /// makes the arc fall back to the seeded default profile).
    pub async fn set_active_profile_id(&self, id: &str, profile_id: Option<&str>) -> Result<()> {
        let conn = self.conn.clone();
        let id = id.to_string();
        let profile_id = profile_id.map(|s| s.to_string());
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute(
                "UPDATE arcs SET active_profile_id = ?1 WHERE id = ?2",
                params![profile_id, id],
            )
            .map_err(|e| AthenError::Other(format!("Set active_profile_id: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Atomically pin this arc to `(provider_id, slug)` if it currently
    /// has no pin. Returns `true` if the pin was written, `false` if a
    /// pin was already in place. First-call-wins semantics: callers race
    /// the very first LLM call of a task; only the first one to land
    /// records the pin, every later iteration reads the same value.
    pub async fn set_pinned_provider_if_unset(
        &self,
        id: &str,
        provider_id: &str,
        slug: &str,
    ) -> Result<bool> {
        let conn = self.conn.clone();
        let id = id.to_string();
        let provider_id = provider_id.to_string();
        let slug = slug.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let affected = conn
                .execute(
                    "UPDATE arcs SET pinned_provider_id = ?1, pinned_slug = ?2 \
                     WHERE id = ?3 AND pinned_provider_id IS NULL",
                    params![provider_id, slug, id],
                )
                .map_err(|e| AthenError::Other(format!("Set pinned provider: {e}")))?;
            Ok(affected > 0)
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Clear the provider pin (both `pinned_provider_id` and
    /// `pinned_slug`). Called when the arc transitions back to idle so
    /// the next task can re-pin against whatever active provider is in
    /// effect at that point.
    pub async fn clear_pinned_provider(&self, id: &str) -> Result<()> {
        let conn = self.conn.clone();
        let id = id.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute(
                "UPDATE arcs SET pinned_provider_id = NULL, pinned_slug = NULL WHERE id = ?1",
                params![id],
            )
            .map_err(|e| AthenError::Other(format!("Clear pinned provider: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Clear EVERY arc's provider pin. Called once at app startup: a pin
    /// is task-scoped (set on a task's first LLM call, cleared at task
    /// end), so any pin still on disk at boot is stale — it leaked from a
    /// task that was killed/crashed before it could clear, and would
    /// otherwise be inherited by the next task on that arc
    /// (`set_pinned_provider_if_unset` only writes when unset), silently
    /// freezing the arc to an old provider across a Bundle switch. Nothing
    /// can be in-flight at boot, so wiping all pins is always safe here.
    /// Returns the number of arcs that had a pin cleared.
    pub async fn clear_all_provider_pins(&self) -> Result<usize> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let n = conn
                .execute(
                    "UPDATE arcs SET pinned_provider_id = NULL, pinned_slug = NULL \
                     WHERE pinned_provider_id IS NOT NULL OR pinned_slug IS NOT NULL",
                    [],
                )
                .map_err(|e| AthenError::Other(format!("Clear all provider pins: {e}")))?;
            Ok(n)
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Set (or clear) the per-arc reasoning-effort override. Last-write-wins:
    /// a user toggling effort mid-task takes effect on the next LLM call.
    /// Pass `None` to clear; pass `Some(wire_str)` where `wire_str` is the
    /// `ReasoningEffort::to_wire_str()` form (`"default"`, `"off"`,
    /// `"minimal"`, `"low"`, `"medium"`, `"high"`, `"max"`).
    pub async fn set_reasoning_effort_override(&self, id: &str, value: Option<&str>) -> Result<()> {
        let conn = self.conn.clone();
        let id = id.to_string();
        let value = value.map(|s| s.to_string());
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute(
                "UPDATE arcs SET reasoning_effort_override = ?1 WHERE id = ?2",
                params![value, id],
            )
            .map_err(|e| AthenError::Other(format!("Set reasoning_effort_override: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Set (or clear) the per-arc tier override. Last-write-wins: a user
    /// toggling tier mid-task takes effect on the next LLM call. Pass
    /// `None` to clear; pass `Some(wire_str)` where `wire_str` is the
    /// serialized `ModelProfile` variant name (`"Judges"`, `"Fast"`,
    /// `"Code"`, `"Powerful"`; legacy `"Cheap"`). Validation of the wire form happens
    /// upstream in the Tauri command — this setter trusts its caller.
    pub async fn set_tier_override(&self, id: &str, value: Option<&str>) -> Result<()> {
        let conn = self.conn.clone();
        let id = id.to_string();
        let value = value.map(|s| s.to_string());
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute(
                "UPDATE arcs SET tier_override = ?1 WHERE id = ?2",
                params![value, id],
            )
            .map_err(|e| AthenError::Other(format!("Set tier_override: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Set (or clear) the per-arc security-mode override. Last-write-wins.
    /// Pass `None` to clear (the arc falls back to the global mode); pass
    /// `Some(wire_str)` where `wire_str` is the lowercase wire form
    /// (`"bunker"`, `"assistant"`, `"yolo"`). Validation of the wire form
    /// happens upstream in the Tauri command — this setter trusts its caller.
    pub async fn set_security_mode_override(&self, id: &str, value: Option<&str>) -> Result<()> {
        let conn = self.conn.clone();
        let id = id.to_string();
        let value = value.map(|s| s.to_string());
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute(
                "UPDATE arcs SET security_mode_override = ?1 WHERE id = ?2",
                params![value, id],
            )
            .map_err(|e| AthenError::Other(format!("Set security_mode_override: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Set (or clear) the Project this arc belongs to. Last-write-wins. Pass
    /// `None` to remove the arc from its project; pass `Some(project_id)` to
    /// move it into (or between) Projects. See `docs/PROJECTS.md`.
    pub async fn set_arc_project(&self, arc_id: &str, project_id: Option<&str>) -> Result<()> {
        let conn = self.conn.clone();
        let arc_id = arc_id.to_string();
        let project_id = project_id.map(|s| s.to_string());
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute(
                "UPDATE arcs SET project_id = ?1 WHERE id = ?2",
                params![project_id, arc_id],
            )
            .map_err(|e| AthenError::Other(format!("Set project_id: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Set (or clear) the filesystem path to the Deep Research paper produced
    /// for this arc. Last-write-wins. Pass `None` to clear it. See
    /// `docs/DEEP_RESEARCH.md`.
    pub async fn set_research_paper_path(&self, arc_id: &str, path: Option<&str>) -> Result<()> {
        let conn = self.conn.clone();
        let arc_id = arc_id.to_string();
        let path = path.map(|s| s.to_string());
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute(
                "UPDATE arcs SET research_paper_path = ?1 WHERE id = ?2",
                params![path, arc_id],
            )
            .map_err(|e| AthenError::Other(format!("Set research_paper_path: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Set (or clear) the research question driving this arc's Deep Research
    /// run. Last-write-wins. Pass `None` to clear it. See
    /// `docs/DEEP_RESEARCH.md`.
    pub async fn set_research_question(&self, arc_id: &str, question: Option<&str>) -> Result<()> {
        let conn = self.conn.clone();
        let arc_id = arc_id.to_string();
        let question = question.map(|s| s.to_string());
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute(
                "UPDATE arcs SET research_question = ?1 WHERE id = ?2",
                params![question, arc_id],
            )
            .map_err(|e| AthenError::Other(format!("Set research_question: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Store the triage plan drafted at task creation. Pass `None` to
    /// clear both columns (used when an arc rolls into a wholly new
    /// task and the prior plan no longer describes what's running).
    /// Both columns are updated atomically so an interrupted write can't
    /// leave a half-plan behind that `assemble_plan` would silently drop.
    pub async fn set_triage_plan(&self, id: &str, plan: Option<&TriagePlan>) -> Result<()> {
        let conn = self.conn.clone();
        let id = id.to_string();
        let acceptance = plan.map(|p| p.acceptance_criteria.clone());
        let scope = plan.map(|p| p.scope.clone());
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute(
                "UPDATE arcs SET triage_plan_acceptance = ?1, triage_plan_scope = ?2 \
                 WHERE id = ?3",
                params![acceptance, scope, id],
            )
            .map_err(|e| AthenError::Other(format!("Set triage_plan: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Write-once variant: store the plan only if both columns are
    /// currently NULL or empty. Returns `true` if it wrote, `false` if
    /// the arc already had a plan. Mirrors `set_pinned_provider_if_unset`
    /// — the production "first task on the arc sets the mission, later
    /// turns inherit" policy. Pass `None` and it short-circuits without
    /// writing.
    pub async fn set_triage_plan_if_absent(
        &self,
        id: &str,
        plan: Option<&TriagePlan>,
    ) -> Result<bool> {
        let Some(plan) = plan else { return Ok(false) };
        let conn = self.conn.clone();
        let id = id.to_string();
        let acceptance = plan.acceptance_criteria.clone();
        let scope = plan.scope.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let affected = conn
                .execute(
                    "UPDATE arcs \
                     SET triage_plan_acceptance = ?1, triage_plan_scope = ?2 \
                     WHERE id = ?3 \
                       AND (triage_plan_acceptance IS NULL OR triage_plan_acceptance = '') \
                       AND (triage_plan_scope IS NULL OR triage_plan_scope = '')",
                    params![acceptance, scope, id],
                )
                .map_err(|e| AthenError::Other(format!("Set triage_plan_if_absent: {e}")))?;
            Ok(affected > 0)
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Set the user goal for this arc. Resets `goal_status` to `"active"` and
    /// clears any blocked reason. Pass `criteria` as measurable completion
    /// conditions; `None` means the goal has no formal acceptance criteria.
    pub async fn set_user_goal(&self, id: &str, goal: &str, criteria: Option<&str>) -> Result<()> {
        let conn = self.conn.clone();
        let id = id.to_string();
        let goal = goal.to_string();
        let criteria = criteria.map(|s| s.to_string());
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute(
                "UPDATE arcs SET user_goal = ?1, user_goal_criteria = ?2, \
                 goal_status = 'active', goal_blocked_reason = NULL, \
                 updated_at = ?3 WHERE id = ?4",
                params![goal, criteria, Utc::now().to_rfc3339(), id],
            )
            .map_err(|e| AthenError::Other(format!("set_user_goal: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Clear all goal fields for this arc (goal text, criteria, status, and
    /// blocked reason).
    pub async fn clear_user_goal(&self, id: &str) -> Result<()> {
        let conn = self.conn.clone();
        let id = id.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute(
                "UPDATE arcs SET user_goal = NULL, user_goal_criteria = NULL, \
                 goal_status = NULL, goal_blocked_reason = NULL, \
                 updated_at = ?1 WHERE id = ?2",
                params![Utc::now().to_rfc3339(), id],
            )
            .map_err(|e| AthenError::Other(format!("clear_user_goal: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Mark this arc's goal as blocked with a human-readable reason.
    pub async fn set_goal_blocked(&self, id: &str, reason: &str) -> Result<()> {
        let conn = self.conn.clone();
        let id = id.to_string();
        let reason = reason.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute(
                "UPDATE arcs SET goal_status = 'blocked', goal_blocked_reason = ?1, \
                 updated_at = ?2 WHERE id = ?3",
                params![reason, Utc::now().to_rfc3339(), id],
            )
            .map_err(|e| AthenError::Other(format!("set_goal_blocked: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Transition this arc's goal back to `"active"`, clearing the blocked
    /// reason. No-op if the goal was already active.
    pub async fn set_goal_active(&self, id: &str) -> Result<()> {
        let conn = self.conn.clone();
        let id = id.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute(
                "UPDATE arcs SET goal_status = 'active', goal_blocked_reason = NULL, \
                 updated_at = ?1 WHERE id = ?2",
                params![Utc::now().to_rfc3339(), id],
            )
            .map_err(|e| AthenError::Other(format!("set_goal_active: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Store (or overwrite) the structured plan for this arc. The plan is
    /// serialized as a single JSON blob in the `plan_json` column.
    pub async fn set_plan(&self, id: &str, plan: &ArcPlan) -> Result<()> {
        let conn = self.conn.clone();
        let id = id.to_string();
        let json = serde_json::to_string(plan)
            .map_err(|e| AthenError::Other(format!("Serialize plan: {e}")))?;
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute(
                "UPDATE arcs SET plan_json = ?1, updated_at = ?2 WHERE id = ?3",
                params![json, Utc::now().to_rfc3339(), id],
            )
            .map_err(|e| AthenError::Other(format!("set_plan: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Clear the structured plan for this arc (sets `plan_json` to NULL).
    pub async fn clear_plan(&self, id: &str) -> Result<()> {
        let conn = self.conn.clone();
        let id = id.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute(
                "UPDATE arcs SET plan_json = NULL, updated_at = ?1 WHERE id = ?2",
                params![Utc::now().to_rfc3339(), id],
            )
            .map_err(|e| AthenError::Other(format!("clear_plan: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Set the entry-id watermark covered by the latest compaction summary.
    /// Pass `None` to clear (e.g. if a summary is invalidated).
    pub async fn set_summarized_through_entry_id(
        &self,
        id: &str,
        entry_id: Option<i64>,
    ) -> Result<()> {
        let conn = self.conn.clone();
        let id = id.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute(
                "UPDATE arcs SET summarized_through_entry_id = ?1 WHERE id = ?2",
                params![entry_id, id],
            )
            .map_err(|e| AthenError::Other(format!("Set summarized_through_entry_id: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Record the channel the user most recently engaged this arc through.
    ///
    /// Used by the approval router to bias follow-up questions toward the
    /// channel the user is already actively reading. Pass any value
    /// recognised by [`athen_core::approval::ReplyChannelKind::from_str`]
    /// (e.g. `"telegram"`, `"in_app"`).
    pub async fn set_primary_reply_channel(&self, id: &str, channel: &str) -> Result<()> {
        let conn = self.conn.clone();
        let id = id.to_string();
        let channel = channel.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute(
                "UPDATE arcs SET primary_reply_channel = ?1 WHERE id = ?2",
                params![channel, id],
            )
            .map_err(|e| AthenError::Other(format!("Set primary_reply_channel: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Update the `updated_at` timestamp for an arc.
    pub async fn touch_arc(&self, id: &str) -> Result<()> {
        let conn = self.conn.clone();
        let id = id.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let now = Utc::now().to_rfc3339();
            conn.execute(
                "UPDATE arcs SET updated_at = ?1 WHERE id = ?2",
                params![now, id],
            )
            .map_err(|e| AthenError::Other(format!("Touch arc: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Add an entry to an arc. Returns the entry's auto-generated ID.
    ///
    /// `turn_id` groups entries belonging to the same conversation turn — the
    /// user message, the agent's tool_call entries, and the final assistant
    /// reply all share the same UUID so the UI can collapse them together.
    pub async fn add_entry(
        &self,
        arc_id: &str,
        entry_type: EntryType,
        source: &str,
        content: &str,
        metadata: Option<serde_json::Value>,
        turn_id: Option<&str>,
    ) -> Result<i64> {
        let conn = self.conn.clone();
        let arc_id = arc_id.to_string();
        let entry_type_str = entry_type.as_str().to_string();
        let source = source.to_string();
        let content = content.to_string();
        let metadata_str = metadata.map(|v| v.to_string());
        let turn_id = turn_id.map(|s| s.to_string());
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let now = Utc::now().to_rfc3339();
            conn.execute(
                "INSERT INTO arc_entries (arc_id, entry_type, source, content, metadata, created_at, turn_id) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![arc_id, entry_type_str, source, content, metadata_str, now, turn_id],
            )
            .map_err(|e| AthenError::Other(format!("Add arc entry: {e}")))?;
            Ok(conn.last_insert_rowid())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Load all entries for an arc, ordered by ID ascending.
    pub async fn load_entries(&self, arc_id: &str) -> Result<Vec<ArcEntry>> {
        let conn = self.conn.clone();
        let arc_id = arc_id.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let mut stmt = conn
                .prepare(
                    "SELECT id, arc_id, entry_type, source, content, metadata, created_at, turn_id \
                     FROM arc_entries WHERE arc_id = ?1 ORDER BY id ASC",
                )
                .map_err(|e| AthenError::Other(format!("Prepare load entries: {e}")))?;

            let rows = stmt
                .query_map(params![arc_id], |row| {
                    let metadata_str: Option<String> = row.get(5)?;
                    let metadata = metadata_str.and_then(|s| serde_json::from_str(&s).ok());
                    Ok(ArcEntry {
                        id: row.get(0)?,
                        arc_id: row.get(1)?,
                        entry_type: EntryType::from_str(&row.get::<_, String>(2)?),
                        source: row.get(3)?,
                        content: row.get(4)?,
                        metadata,
                        created_at: row.get(6)?,
                        turn_id: row.get(7)?,
                    })
                })
                .map_err(|e| AthenError::Other(format!("Query load entries: {e}")))?;

            let mut entries = Vec::new();
            for row in rows {
                entries.push(row.map_err(|e| AthenError::Other(format!("Entry row: {e}")))?);
            }
            Ok(entries)
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Atomically write a compaction summary entry and advance the arc's
    /// `summarized_through_entry_id` pointer in a single transaction.
    ///
    /// This is the only path that should write `EntryType::Summary` rows.
    /// The single-transaction guarantee is what makes the design
    /// restart-resilient: if the process dies mid-call, either both the
    /// row insert and the pointer update happen, or neither does — there
    /// is no half-state where a summary exists without a pointer or vice
    /// versa.
    ///
    /// `summarized_through` is the largest `arc_entries.id` the summary
    /// covers; it becomes the new `ArcMeta.summarized_through_entry_id`.
    /// Returns the new summary entry's auto-generated id.
    pub async fn compact_arc(
        &self,
        arc_id: &str,
        summary_content: &str,
        metadata: Option<serde_json::Value>,
        summarized_through: i64,
    ) -> Result<i64> {
        let conn = self.conn.clone();
        let arc_id = arc_id.to_string();
        let summary_content = summary_content.to_string();
        let metadata_str = metadata.map(|v| v.to_string());
        tokio::task::spawn_blocking(move || {
            let mut conn = conn.blocking_lock();
            let tx = conn
                .transaction()
                .map_err(|e| AthenError::Other(format!("Begin compact tx: {e}")))?;
            let now = Utc::now().to_rfc3339();
            tx.execute(
                "INSERT INTO arc_entries (arc_id, entry_type, source, content, metadata, created_at, turn_id) \
                 VALUES (?1, 'summary', 'compactor', ?2, ?3, ?4, NULL)",
                params![arc_id, summary_content, metadata_str, now],
            )
            .map_err(|e| AthenError::Other(format!("Insert summary entry: {e}")))?;
            let new_id = tx.last_insert_rowid();
            tx.execute(
                "UPDATE arcs SET summarized_through_entry_id = ?1, updated_at = ?2 WHERE id = ?3",
                params![summarized_through, now, arc_id],
            )
            .map_err(|e| AthenError::Other(format!("Update summarized_through: {e}")))?;
            tx.commit()
                .map_err(|e| AthenError::Other(format!("Commit compact tx: {e}")))?;
            Ok(new_id)
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Load only the entries with `id > after_entry_id`, ordered ascending.
    /// Used by the compactor to fetch the verbatim tail past the latest
    /// summary's coverage.
    pub async fn load_entries_after(
        &self,
        arc_id: &str,
        after_entry_id: i64,
    ) -> Result<Vec<ArcEntry>> {
        let conn = self.conn.clone();
        let arc_id = arc_id.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let mut stmt = conn
                .prepare(
                    "SELECT id, arc_id, entry_type, source, content, metadata, created_at, turn_id \
                     FROM arc_entries WHERE arc_id = ?1 AND id > ?2 ORDER BY id ASC",
                )
                .map_err(|e| AthenError::Other(format!("Prepare load_entries_after: {e}")))?;
            let rows = stmt
                .query_map(params![arc_id, after_entry_id], |row| {
                    let metadata_str: Option<String> = row.get(5)?;
                    let metadata = metadata_str.and_then(|s| serde_json::from_str(&s).ok());
                    Ok(ArcEntry {
                        id: row.get(0)?,
                        arc_id: row.get(1)?,
                        entry_type: EntryType::from_str(&row.get::<_, String>(2)?),
                        source: row.get(3)?,
                        content: row.get(4)?,
                        metadata,
                        created_at: row.get(6)?,
                        turn_id: row.get(7)?,
                    })
                })
                .map_err(|e| AthenError::Other(format!("Query load_entries_after: {e}")))?;
            let mut entries = Vec::new();
            for row in rows {
                entries.push(row.map_err(|e| AthenError::Other(format!("Entry row: {e}")))?);
            }
            Ok(entries)
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Load the most recent compaction summary for `arc_id`, if any.
    /// Returns the highest-id row with `entry_type = 'summary'`.
    pub async fn load_latest_summary(&self, arc_id: &str) -> Result<Option<ArcEntry>> {
        let conn = self.conn.clone();
        let arc_id = arc_id.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let mut stmt = conn
                .prepare(
                    "SELECT id, arc_id, entry_type, source, content, metadata, created_at, turn_id \
                     FROM arc_entries WHERE arc_id = ?1 AND entry_type = 'summary' \
                     ORDER BY id DESC LIMIT 1",
                )
                .map_err(|e| AthenError::Other(format!("Prepare load_latest_summary: {e}")))?;
            let mut rows = stmt
                .query_map(params![arc_id], |row| {
                    let metadata_str: Option<String> = row.get(5)?;
                    let metadata = metadata_str.and_then(|s| serde_json::from_str(&s).ok());
                    Ok(ArcEntry {
                        id: row.get(0)?,
                        arc_id: row.get(1)?,
                        entry_type: EntryType::from_str(&row.get::<_, String>(2)?),
                        source: row.get(3)?,
                        content: row.get(4)?,
                        metadata,
                        created_at: row.get(6)?,
                        turn_id: row.get(7)?,
                    })
                })
                .map_err(|e| AthenError::Other(format!("Query load_latest_summary: {e}")))?;
            match rows.next() {
                Some(row) => {
                    Ok(Some(row.map_err(|e| {
                        AthenError::Other(format!("Summary row: {e}"))
                    })?))
                }
                None => Ok(None),
            }
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Migrate data from legacy `chat_sessions` and `chat_messages` tables into arcs.
    ///
    /// Returns the number of arcs migrated. Idempotent: skips migration if arcs
    /// already contain data.
    pub async fn migrate_from_chat_tables(&self) -> Result<u32> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();

            // Check if arcs already have data — skip if so.
            let arc_count: u32 = conn
                .query_row("SELECT COUNT(*) FROM arcs", [], |row| row.get(0))
                .map_err(|e| AthenError::Other(format!("Count arcs: {e}")))?;
            if arc_count > 0 {
                return Ok(0);
            }

            // Check if old tables exist.
            let has_chat_sessions: bool = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master \
                     WHERE type='table' AND name='chat_sessions'",
                    [],
                    |row| row.get::<_, u32>(0).map(|c| c > 0),
                )
                .map_err(|e| AthenError::Other(format!("Check chat_sessions: {e}")))?;

            let has_chat_messages: bool = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master \
                     WHERE type='table' AND name='chat_messages'",
                    [],
                    |row| row.get::<_, u32>(0).map(|c| c > 0),
                )
                .map_err(|e| AthenError::Other(format!("Check chat_messages: {e}")))?;

            if !has_chat_messages {
                return Ok(0);
            }

            let now = Utc::now().to_rfc3339();
            let mut migrated: u32 = 0;

            // Collect session IDs from chat_messages.
            let mut sess_stmt = if has_chat_sessions {
                conn.prepare(
                    "SELECT s.session_id, COALESCE(s.name, s.session_id), \
                            COALESCE(s.created_at, ?1), COALESCE(s.updated_at, ?1) \
                     FROM chat_sessions s",
                )
            } else {
                conn.prepare(
                    "SELECT session_id, session_id, MIN(created_at), MAX(created_at) \
                     FROM chat_messages GROUP BY session_id",
                )
            }
            .map_err(|e| AthenError::Other(format!("Prepare migrate sessions: {e}")))?;

            // Gather sessions into a vec to avoid borrow issues.
            let sessions: Vec<(String, String, String, String)> = if has_chat_sessions {
                let rows = sess_stmt
                    .query_map(params![now], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                        ))
                    })
                    .map_err(|e| AthenError::Other(format!("Query migrate sessions: {e}")))?;
                let mut v = Vec::new();
                for row in rows {
                    v.push(
                        row.map_err(|e| AthenError::Other(format!("Migrate session row: {e}")))?,
                    );
                }
                v
            } else {
                let rows = sess_stmt
                    .query_map([], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                        ))
                    })
                    .map_err(|e| AthenError::Other(format!("Query migrate sessions: {e}")))?;
                let mut v = Vec::new();
                for row in rows {
                    v.push(
                        row.map_err(|e| AthenError::Other(format!("Migrate session row: {e}")))?,
                    );
                }
                v
            };

            for (session_id, name, created_at, updated_at) in &sessions {
                // Create arc for this session.
                conn.execute(
                    "INSERT INTO arcs (id, name, source, status, created_at, updated_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        session_id,
                        name,
                        "user_input",
                        "active",
                        created_at,
                        updated_at
                    ],
                )
                .map_err(|e| AthenError::Other(format!("Insert migrated arc: {e}")))?;

                // Migrate messages as entries.
                let mut msg_stmt = conn
                    .prepare(
                        "SELECT role, content, created_at \
                         FROM chat_messages WHERE session_id = ?1 ORDER BY id ASC",
                    )
                    .map_err(|e| AthenError::Other(format!("Prepare migrate messages: {e}")))?;

                let messages: Vec<(String, String, String)> = {
                    let rows = msg_stmt
                        .query_map(params![session_id], |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, String>(1)?,
                                row.get::<_, String>(2)?,
                            ))
                        })
                        .map_err(|e| AthenError::Other(format!("Query migrate messages: {e}")))?;
                    let mut v = Vec::new();
                    for row in rows {
                        v.push(
                            row.map_err(|e| {
                                AthenError::Other(format!("Migrate message row: {e}"))
                            })?,
                        );
                    }
                    v
                };

                for (role, content, msg_created_at) in &messages {
                    conn.execute(
                        "INSERT INTO arc_entries \
                         (arc_id, entry_type, source, content, created_at) \
                         VALUES (?1, ?2, ?3, ?4, ?5)",
                        params![session_id, "message", role, content, msg_created_at],
                    )
                    .map_err(|e| AthenError::Other(format!("Insert migrated entry: {e}")))?;
                }

                migrated += 1;
            }

            Ok(migrated)
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }
}

const ARC_SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS arcs (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    source TEXT NOT NULL DEFAULT 'user_input',
    status TEXT NOT NULL DEFAULT 'active',
    parent_arc_id TEXT,
    merged_into_arc_id TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    primary_reply_channel TEXT,
    active_profile_id TEXT,
    summarized_through_entry_id INTEGER,
    pinned_provider_id TEXT,
    pinned_slug TEXT,
    reasoning_effort_override TEXT,
    tier_override TEXT,
    triage_plan_acceptance TEXT,
    triage_plan_scope TEXT,
    user_goal TEXT,
    user_goal_criteria TEXT,
    goal_status TEXT,
    goal_blocked_reason TEXT,
    plan_json TEXT,
    security_mode_override TEXT,
    project_id TEXT,
    research_paper_path TEXT,
    research_question TEXT
);

CREATE TABLE IF NOT EXISTS arc_entries (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    arc_id TEXT NOT NULL,
    entry_type TEXT NOT NULL DEFAULT 'message',
    source TEXT NOT NULL,
    content TEXT NOT NULL,
    metadata TEXT,
    created_at TEXT NOT NULL,
    FOREIGN KEY (arc_id) REFERENCES arcs(id)
);

CREATE INDEX IF NOT EXISTS idx_arc_entries_arc ON arc_entries(arc_id);
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use serde_json::json;

    async fn setup_arc_store() -> ArcStore {
        let conn = Connection::open_in_memory().expect("open in-memory db");
        let store = ArcStore::new(Arc::new(Mutex::new(conn)));
        store.init_schema().await.expect("init arc schema");
        store
    }

    /// Helper that creates an ArcStore with the chat schema also initialized,
    /// and returns both the store and the raw connection for inserting legacy data.
    async fn setup_arc_store_with_chat() -> (ArcStore, Arc<Mutex<Connection>>) {
        let conn = Connection::open_in_memory().expect("open in-memory db");
        let conn = Arc::new(Mutex::new(conn));

        // Init chat schema via spawn_blocking to avoid blocking the runtime.
        let conn_clone = conn.clone();
        tokio::task::spawn_blocking(move || {
            let c = conn_clone.blocking_lock();
            c.execute_batch(
                "CREATE TABLE IF NOT EXISTS chat_sessions (
                    session_id TEXT PRIMARY KEY,
                    name TEXT NOT NULL,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                );
                CREATE TABLE IF NOT EXISTS chat_messages (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    session_id TEXT NOT NULL,
                    role TEXT NOT NULL,
                    content TEXT NOT NULL,
                    content_type TEXT NOT NULL DEFAULT 'text',
                    created_at TEXT NOT NULL
                );",
            )
            .expect("init chat schema");
        })
        .await
        .expect("spawn blocking");

        let store = ArcStore::new(conn.clone());
        store.init_schema().await.expect("init arc schema");
        (store, conn)
    }

    #[tokio::test]
    async fn test_create_and_list_arcs() {
        let store = setup_arc_store().await;

        store
            .create_arc("arc_1", "First Arc", ArcSource::UserInput)
            .await
            .unwrap();
        store
            .create_arc("arc_2", "Email Arc", ArcSource::Email)
            .await
            .unwrap();

        let arcs = store.list_arcs().await.unwrap();
        assert_eq!(arcs.len(), 2);

        let a1 = arcs.iter().find(|a| a.id == "arc_1").unwrap();
        assert_eq!(a1.name, "First Arc");
        assert_eq!(a1.source, ArcSource::UserInput);
        assert_eq!(a1.status, ArcStatus::Active);
        assert!(a1.parent_arc_id.is_none());
        assert_eq!(a1.entry_count, 0);

        let a2 = arcs.iter().find(|a| a.id == "arc_2").unwrap();
        assert_eq!(a2.name, "Email Arc");
        assert_eq!(a2.source, ArcSource::Email);
    }

    #[tokio::test]
    async fn test_add_and_load_entries() {
        let store = setup_arc_store().await;

        store
            .create_arc("arc_1", "Test Arc", ArcSource::UserInput)
            .await
            .unwrap();

        store
            .add_entry("arc_1", EntryType::Message, "user", "Hello!", None, None)
            .await
            .unwrap();
        store
            .add_entry(
                "arc_1",
                EntryType::ToolCall,
                "assistant",
                "Running ls",
                None,
                None,
            )
            .await
            .unwrap();
        store
            .add_entry(
                "arc_1",
                EntryType::Message,
                "assistant",
                "Done!",
                None,
                None,
            )
            .await
            .unwrap();

        let entries = store.load_entries("arc_1").await.unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].entry_type, EntryType::Message);
        assert_eq!(entries[0].source, "user");
        assert_eq!(entries[0].content, "Hello!");
        assert_eq!(entries[1].entry_type, EntryType::ToolCall);
        assert_eq!(entries[1].content, "Running ls");
        assert_eq!(entries[2].content, "Done!");
    }

    #[tokio::test]
    async fn test_rename_arc() {
        let store = setup_arc_store().await;

        store
            .create_arc("arc_1", "Old Name", ArcSource::UserInput)
            .await
            .unwrap();
        store.rename_arc("arc_1", "New Name").await.unwrap();

        let arc = store.get_arc("arc_1").await.unwrap().unwrap();
        assert_eq!(arc.name, "New Name");
    }

    #[tokio::test]
    async fn test_delete_arc() {
        let store = setup_arc_store().await;

        store
            .create_arc("arc_1", "Doomed", ArcSource::UserInput)
            .await
            .unwrap();
        store
            .add_entry("arc_1", EntryType::Message, "user", "Hello", None, None)
            .await
            .unwrap();

        store.delete_arc("arc_1").await.unwrap();

        let arc = store.get_arc("arc_1").await.unwrap();
        assert!(arc.is_none());

        let entries = store.load_entries("arc_1").await.unwrap();
        assert!(entries.is_empty());
    }

    #[tokio::test]
    async fn test_archive_arc() {
        let store = setup_arc_store().await;

        store
            .create_arc("arc_1", "Active Arc", ArcSource::UserInput)
            .await
            .unwrap();
        store.archive_arc("arc_1").await.unwrap();

        let arc = store.get_arc("arc_1").await.unwrap().unwrap();
        assert_eq!(arc.status, ArcStatus::Archived);
    }

    /// `list_root_arcs` powers the sidebar; sub-arcs created by
    /// `delegate_to_agent` (carrying a `parent_arc_id`) must be hidden so
    /// they don't clutter the sidebar with empty-looking entries.
    #[tokio::test]
    async fn test_list_root_arcs_hides_sub_arcs() {
        let store = setup_arc_store().await;
        store
            .create_arc("parent", "Parent Arc", ArcSource::UserInput)
            .await
            .unwrap();
        store
            .create_arc_with_parent("sub", "Sub Arc", ArcSource::System, "parent")
            .await
            .unwrap();
        store
            .create_arc("standalone", "Standalone", ArcSource::UserInput)
            .await
            .unwrap();

        let all = store.list_arcs().await.unwrap();
        assert_eq!(all.len(), 3, "list_arcs returns every arc including sub");

        let roots = store.list_root_arcs().await.unwrap();
        assert_eq!(roots.len(), 2, "list_root_arcs hides sub-arcs");
        let ids: Vec<&str> = roots.iter().map(|a| a.id.as_str()).collect();
        assert!(ids.contains(&"parent"));
        assert!(ids.contains(&"standalone"));
        assert!(!ids.contains(&"sub"));
    }

    #[tokio::test]
    async fn test_branch_arc() {
        let store = setup_arc_store().await;

        store
            .create_arc("parent", "Parent Arc", ArcSource::UserInput)
            .await
            .unwrap();
        store
            .create_arc_with_parent("child", "Child Arc", ArcSource::UserInput, "parent")
            .await
            .unwrap();

        let child = store.get_arc("child").await.unwrap().unwrap();
        assert_eq!(child.parent_arc_id, Some("parent".to_string()));
        assert_eq!(child.status, ArcStatus::Active);

        let parent = store.get_arc("parent").await.unwrap().unwrap();
        assert!(parent.parent_arc_id.is_none());
    }

    #[tokio::test]
    async fn test_merge_arcs() {
        let store = setup_arc_store().await;

        store
            .create_arc("source", "Source Arc", ArcSource::UserInput)
            .await
            .unwrap();
        store
            .create_arc("target", "Target Arc", ArcSource::UserInput)
            .await
            .unwrap();

        store
            .add_entry(
                "source",
                EntryType::Message,
                "user",
                "From source 1",
                None,
                None,
            )
            .await
            .unwrap();
        store
            .add_entry(
                "source",
                EntryType::Message,
                "user",
                "From source 2",
                None,
                None,
            )
            .await
            .unwrap();
        store
            .add_entry(
                "target",
                EntryType::Message,
                "user",
                "In target",
                None,
                None,
            )
            .await
            .unwrap();

        store.merge_arc("source", "target").await.unwrap();

        // Source should be marked Merged.
        let source = store.get_arc("source").await.unwrap().unwrap();
        assert_eq!(source.status, ArcStatus::Merged);
        assert_eq!(source.merged_into_arc_id, Some("target".to_string()));

        // Source entries should be empty.
        let source_entries = store.load_entries("source").await.unwrap();
        assert!(source_entries.is_empty());

        // Target should have all 3 entries.
        let target_entries = store.load_entries("target").await.unwrap();
        assert_eq!(target_entries.len(), 3);

        // Target entry_count should reflect the merge.
        let target = store.get_arc("target").await.unwrap().unwrap();
        assert_eq!(target.entry_count, 3);
    }

    #[tokio::test]
    async fn test_touch_arc() {
        let store = setup_arc_store().await;

        store
            .create_arc("arc_1", "Test", ArcSource::UserInput)
            .await
            .unwrap();
        let before = store.get_arc("arc_1").await.unwrap().unwrap();
        let ts1 = before.updated_at.clone();

        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        store.touch_arc("arc_1").await.unwrap();
        let after = store.get_arc("arc_1").await.unwrap().unwrap();
        let ts2 = after.updated_at.clone();

        assert_ne!(ts1, ts2);
    }

    #[tokio::test]
    async fn test_entry_metadata() {
        let store = setup_arc_store().await;

        store
            .create_arc("arc_1", "Meta Arc", ArcSource::UserInput)
            .await
            .unwrap();

        let meta = json!({
            "tool": "shell_execute",
            "args": {"command": "ls -la"},
            "exit_code": 0
        });
        store
            .add_entry(
                "arc_1",
                EntryType::ToolCall,
                "assistant",
                "Executed ls",
                Some(meta.clone()),
                None,
            )
            .await
            .unwrap();

        let entries = store.load_entries("arc_1").await.unwrap();
        assert_eq!(entries.len(), 1);
        let entry = &entries[0];
        assert_eq!(entry.entry_type, EntryType::ToolCall);

        let loaded_meta = entry.metadata.as_ref().unwrap();
        assert_eq!(loaded_meta["tool"], "shell_execute");
        assert_eq!(loaded_meta["args"]["command"], "ls -la");
        assert_eq!(loaded_meta["exit_code"], 0);
    }

    #[tokio::test]
    async fn test_migrate_from_chat() {
        let (store, conn) = setup_arc_store_with_chat().await;

        // Insert legacy chat data via raw SQL in spawn_blocking.
        let conn_clone = conn.clone();
        tokio::task::spawn_blocking(move || {
            let c = conn_clone.blocking_lock();
            c.execute(
                "INSERT INTO chat_sessions (session_id, name, created_at, updated_at) \
                 VALUES ('s1', 'Chat One', '2025-01-01T00:00:00Z', '2025-01-01T01:00:00Z')",
                [],
            )
            .unwrap();
            c.execute(
                "INSERT INTO chat_sessions (session_id, name, created_at, updated_at) \
                 VALUES ('s2', 'Chat Two', '2025-01-02T00:00:00Z', '2025-01-02T01:00:00Z')",
                [],
            )
            .unwrap();
            c.execute(
                "INSERT INTO chat_messages (session_id, role, content, content_type, created_at) \
                 VALUES ('s1', 'user', 'Hello', 'text', '2025-01-01T00:00:00Z')",
                [],
            )
            .unwrap();
            c.execute(
                "INSERT INTO chat_messages (session_id, role, content, content_type, created_at) \
                 VALUES ('s1', 'assistant', 'Hi!', 'text', '2025-01-01T00:01:00Z')",
                [],
            )
            .unwrap();
            c.execute(
                "INSERT INTO chat_messages (session_id, role, content, content_type, created_at) \
                 VALUES ('s2', 'user', 'Hey', 'text', '2025-01-02T00:00:00Z')",
                [],
            )
            .unwrap();
        })
        .await
        .unwrap();

        let count = store.migrate_from_chat_tables().await.unwrap();
        assert_eq!(count, 2);

        // Verify arcs were created.
        let arcs = store.list_arcs().await.unwrap();
        assert_eq!(arcs.len(), 2);

        let a1 = arcs.iter().find(|a| a.id == "s1").unwrap();
        assert_eq!(a1.name, "Chat One");
        assert_eq!(a1.source, ArcSource::UserInput);
        assert_eq!(a1.entry_count, 2);

        let a2 = arcs.iter().find(|a| a.id == "s2").unwrap();
        assert_eq!(a2.name, "Chat Two");
        assert_eq!(a2.entry_count, 1);

        // Verify entries.
        let entries = store.load_entries("s1").await.unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].source, "user");
        assert_eq!(entries[0].content, "Hello");
        assert_eq!(entries[0].entry_type, EntryType::Message);
        assert_eq!(entries[1].source, "assistant");
        assert_eq!(entries[1].content, "Hi!");

        // Verify idempotency — second call should migrate 0.
        let count2 = store.migrate_from_chat_tables().await.unwrap();
        assert_eq!(count2, 0);
    }

    #[tokio::test]
    async fn test_merge_moves_all_entries() {
        let store = setup_arc_store().await;
        store
            .create_arc("src", "Source", ArcSource::Email)
            .await
            .unwrap();
        store
            .create_arc("tgt", "Target", ArcSource::UserInput)
            .await
            .unwrap();

        // Add entries to source
        store
            .add_entry("src", EntryType::EmailEvent, "email", "Email 1", None, None)
            .await
            .unwrap();
        store
            .add_entry("src", EntryType::EmailEvent, "email", "Email 2", None, None)
            .await
            .unwrap();
        store
            .add_entry("tgt", EntryType::Message, "user", "Hello", None, None)
            .await
            .unwrap();

        // Merge source into target
        store.merge_arc("src", "tgt").await.unwrap();

        // Target should have all 3 entries
        let target_entries = store.load_entries("tgt").await.unwrap();
        assert_eq!(target_entries.len(), 3);

        // Source should have 0 entries
        let source_entries = store.load_entries("src").await.unwrap();
        assert_eq!(source_entries.len(), 0);

        // Source arc should be Merged
        let src_arc = store.get_arc("src").await.unwrap().unwrap();
        assert_eq!(src_arc.status, ArcStatus::Merged);
        assert_eq!(src_arc.merged_into_arc_id.as_deref(), Some("tgt"));
    }

    #[tokio::test]
    async fn test_merged_arcs_not_in_active_list() {
        let store = setup_arc_store().await;
        store
            .create_arc("a1", "Active", ArcSource::UserInput)
            .await
            .unwrap();
        store
            .create_arc("a2", "Will Merge", ArcSource::Email)
            .await
            .unwrap();
        store.merge_arc("a2", "a1").await.unwrap();

        let arcs = store.list_arcs().await.unwrap();
        // Both are still listed (filtering by status is done at the app layer)
        let merged = arcs.iter().find(|a| a.id == "a2").unwrap();
        assert_eq!(merged.status, ArcStatus::Merged);
    }

    #[tokio::test]
    async fn test_entry_types_preserved() {
        let store = setup_arc_store().await;
        store
            .create_arc("a1", "Multi-type", ArcSource::UserInput)
            .await
            .unwrap();

        store
            .add_entry("a1", EntryType::Message, "user", "Hello", None, None)
            .await
            .unwrap();
        store
            .add_entry(
                "a1",
                EntryType::ToolCall,
                "assistant",
                "shell_execute echo hi",
                None,
                None,
            )
            .await
            .unwrap();
        store
            .add_entry(
                "a1",
                EntryType::EmailEvent,
                "email",
                "From: bob",
                None,
                None,
            )
            .await
            .unwrap();
        store
            .add_entry(
                "a1",
                EntryType::SystemEvent,
                "system",
                "Monitor started",
                None,
                None,
            )
            .await
            .unwrap();
        store
            .add_entry(
                "a1",
                EntryType::CalendarEvent,
                "calendar",
                "Meeting at 3pm",
                None,
                None,
            )
            .await
            .unwrap();

        let entries = store.load_entries("a1").await.unwrap();
        assert_eq!(entries.len(), 5);
        assert_eq!(entries[0].entry_type, EntryType::Message);
        assert_eq!(entries[1].entry_type, EntryType::ToolCall);
        assert_eq!(entries[2].entry_type, EntryType::EmailEvent);
        assert_eq!(entries[3].entry_type, EntryType::SystemEvent);
        assert_eq!(entries[4].entry_type, EntryType::CalendarEvent);
    }

    #[tokio::test]
    async fn test_entry_metadata_json() {
        let store = setup_arc_store().await;
        store
            .create_arc("a1", "Meta test", ArcSource::Email)
            .await
            .unwrap();

        let meta = serde_json::json!({
            "event_id": "uuid-123",
            "sender": "alice@test.com",
            "relevance": "high",
            "nested": {"key": "value"}
        });

        store
            .add_entry(
                "a1",
                EntryType::EmailEvent,
                "email",
                "Content",
                Some(meta.clone()),
                None,
            )
            .await
            .unwrap();

        let entries = store.load_entries("a1").await.unwrap();
        assert_eq!(entries.len(), 1);
        let loaded_meta = entries[0].metadata.as_ref().unwrap();
        assert_eq!(loaded_meta["sender"], "alice@test.com");
        assert_eq!(loaded_meta["relevance"], "high");
        assert_eq!(loaded_meta["nested"]["key"], "value");
    }

    #[tokio::test]
    async fn test_branch_preserves_parent_reference() {
        let store = setup_arc_store().await;
        store
            .create_arc("parent", "Parent Arc", ArcSource::UserInput)
            .await
            .unwrap();
        store
            .add_entry(
                "parent",
                EntryType::Message,
                "user",
                "Original message",
                None,
                None,
            )
            .await
            .unwrap();

        store
            .create_arc_with_parent("child", "Branch", ArcSource::UserInput, "parent")
            .await
            .unwrap();
        store
            .add_entry(
                "child",
                EntryType::Message,
                "user",
                "Branch message",
                None,
                None,
            )
            .await
            .unwrap();

        let child = store.get_arc("child").await.unwrap().unwrap();
        assert_eq!(child.parent_arc_id.as_deref(), Some("parent"));
        assert_eq!(child.status, ArcStatus::Active);

        // Parent should be unaffected
        let parent = store.get_arc("parent").await.unwrap().unwrap();
        assert_eq!(parent.status, ArcStatus::Active);
        let parent_entries = store.load_entries("parent").await.unwrap();
        assert_eq!(parent_entries.len(), 1);
    }

    #[tokio::test]
    async fn test_delete_arc_cascades_entries() {
        let store = setup_arc_store().await;
        store
            .create_arc("a1", "To Delete", ArcSource::UserInput)
            .await
            .unwrap();
        store
            .add_entry("a1", EntryType::Message, "user", "Msg 1", None, None)
            .await
            .unwrap();
        store
            .add_entry("a1", EntryType::Message, "assistant", "Msg 2", None, None)
            .await
            .unwrap();
        store
            .add_entry(
                "a1",
                EntryType::ToolCall,
                "assistant",
                "Tool call",
                None,
                None,
            )
            .await
            .unwrap();

        store.delete_arc("a1").await.unwrap();

        assert!(store.get_arc("a1").await.unwrap().is_none());
        assert!(store.load_entries("a1").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_archive_arc_status_change() {
        let store = setup_arc_store().await;
        store
            .create_arc("a1", "To Archive", ArcSource::UserInput)
            .await
            .unwrap();
        store.archive_arc("a1").await.unwrap();

        let arc = store.get_arc("a1").await.unwrap().unwrap();
        assert_eq!(arc.status, ArcStatus::Archived);
    }

    #[tokio::test]
    async fn test_multiple_arcs_different_sources() {
        let store = setup_arc_store().await;
        store
            .create_arc("chat1", "Chat", ArcSource::UserInput)
            .await
            .unwrap();
        store
            .create_arc("email1", "Email thread", ArcSource::Email)
            .await
            .unwrap();
        store
            .create_arc("cal1", "Meeting", ArcSource::Calendar)
            .await
            .unwrap();
        store
            .create_arc("msg1", "WhatsApp", ArcSource::Messaging)
            .await
            .unwrap();

        let arcs = store.list_arcs().await.unwrap();
        assert_eq!(arcs.len(), 4);

        let sources: Vec<&ArcSource> = arcs.iter().map(|a| &a.source).collect();
        assert!(sources.contains(&&ArcSource::UserInput));
        assert!(sources.contains(&&ArcSource::Email));
        assert!(sources.contains(&&ArcSource::Calendar));
        assert!(sources.contains(&&ArcSource::Messaging));
    }

    #[tokio::test]
    async fn test_add_entry_returns_id() {
        let store = setup_arc_store().await;
        store
            .create_arc("a1", "Test", ArcSource::UserInput)
            .await
            .unwrap();

        let id1 = store
            .add_entry("a1", EntryType::Message, "user", "First", None, None)
            .await
            .unwrap();
        let id2 = store
            .add_entry("a1", EntryType::Message, "user", "Second", None, None)
            .await
            .unwrap();

        assert!(id2 > id1);
    }

    #[tokio::test]
    async fn test_arc_entry_count_updates() {
        let store = setup_arc_store().await;
        store
            .create_arc("a1", "Count test", ArcSource::UserInput)
            .await
            .unwrap();

        let arcs = store.list_arcs().await.unwrap();
        assert_eq!(arcs[0].entry_count, 0);

        store
            .add_entry("a1", EntryType::Message, "user", "One", None, None)
            .await
            .unwrap();
        store
            .add_entry("a1", EntryType::Message, "user", "Two", None, None)
            .await
            .unwrap();

        let arcs = store.list_arcs().await.unwrap();
        assert_eq!(arcs[0].entry_count, 2);
    }

    #[tokio::test]
    async fn test_turn_id_grouping() {
        let store = setup_arc_store().await;
        store
            .create_arc("a1", "Turn test", ArcSource::UserInput)
            .await
            .unwrap();

        let turn = "turn-abc";
        store
            .add_entry(
                "a1",
                EntryType::Message,
                "user",
                "do a thing",
                None,
                Some(turn),
            )
            .await
            .unwrap();
        store
            .add_entry(
                "a1",
                EntryType::ToolCall,
                "assistant",
                "shell_execute",
                Some(serde_json::json!({"tool": "shell_execute"})),
                Some(turn),
            )
            .await
            .unwrap();
        store
            .add_entry(
                "a1",
                EntryType::Message,
                "assistant",
                "done",
                None,
                Some(turn),
            )
            .await
            .unwrap();
        store
            .add_entry("a1", EntryType::Message, "user", "next turn", None, None)
            .await
            .unwrap();

        let entries = store.load_entries("a1").await.unwrap();
        assert_eq!(entries.len(), 4);
        assert_eq!(entries[0].turn_id.as_deref(), Some(turn));
        assert_eq!(entries[1].turn_id.as_deref(), Some(turn));
        assert_eq!(entries[2].turn_id.as_deref(), Some(turn));
        assert_eq!(entries[3].turn_id, None);
    }

    /// init_schema must be idempotent and must add the `turn_id` column to a
    /// pre-existing database that was created before the column existed.
    #[tokio::test]
    async fn test_turn_id_migration_on_legacy_db() {
        let conn = Connection::open_in_memory().expect("open in-memory db");
        let conn = Arc::new(Mutex::new(conn));

        // Simulate an old DB: arcs + arc_entries WITHOUT the turn_id column.
        let conn_clone = conn.clone();
        tokio::task::spawn_blocking(move || {
            let c = conn_clone.blocking_lock();
            c.execute_batch(
                "CREATE TABLE arcs (
                    id TEXT PRIMARY KEY,
                    name TEXT NOT NULL,
                    source TEXT NOT NULL DEFAULT 'user_input',
                    status TEXT NOT NULL DEFAULT 'active',
                    parent_arc_id TEXT,
                    merged_into_arc_id TEXT,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                );
                CREATE TABLE arc_entries (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    arc_id TEXT NOT NULL,
                    entry_type TEXT NOT NULL DEFAULT 'message',
                    source TEXT NOT NULL,
                    content TEXT NOT NULL,
                    metadata TEXT,
                    created_at TEXT NOT NULL
                );
                INSERT INTO arcs (id, name, source, status, created_at, updated_at)
                  VALUES ('legacy', 'Legacy', 'user_input', 'active',
                          '2025-01-01T00:00:00Z', '2025-01-01T00:00:00Z');
                INSERT INTO arc_entries (arc_id, entry_type, source, content, created_at)
                  VALUES ('legacy', 'message', 'user', 'old hello',
                          '2025-01-01T00:00:00Z');",
            )
            .expect("seed legacy schema");
        })
        .await
        .unwrap();

        // Now run init_schema — must add the turn_id column without losing data.
        let store = ArcStore::new(conn);
        store.init_schema().await.expect("init migrates legacy db");

        let entries = store.load_entries("legacy").await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].content, "old hello");
        assert_eq!(entries[0].turn_id, None);

        // And new writes with a turn_id work end-to-end on the migrated db.
        store
            .add_entry(
                "legacy",
                EntryType::Message,
                "user",
                "post-migration",
                None,
                Some("t1"),
            )
            .await
            .unwrap();
        let entries = store.load_entries("legacy").await.unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1].turn_id.as_deref(), Some("t1"));

        // Idempotency: running init_schema again must not error.
        store.init_schema().await.expect("idempotent re-init");
    }

    /// New arcs default to no primary_reply_channel; setting it via the helper
    /// is durable and visible through both `get_arc` and `list_arcs`.
    #[tokio::test]
    async fn test_primary_reply_channel_set_and_read() {
        let store = setup_arc_store().await;
        store
            .create_arc("arc_1", "Telegram thread", ArcSource::Messaging)
            .await
            .unwrap();

        let meta = store.get_arc("arc_1").await.unwrap().expect("arc present");
        assert_eq!(meta.primary_reply_channel, None);

        store
            .set_primary_reply_channel("arc_1", "telegram")
            .await
            .unwrap();

        let meta = store.get_arc("arc_1").await.unwrap().expect("arc present");
        assert_eq!(meta.primary_reply_channel.as_deref(), Some("telegram"));

        let arcs = store.list_arcs().await.unwrap();
        let listed = arcs.iter().find(|a| a.id == "arc_1").unwrap();
        assert_eq!(listed.primary_reply_channel.as_deref(), Some("telegram"));
    }

    /// New arcs default to no active_profile_id; setting it via the helper
    /// is durable and visible through both `get_arc` and `list_arcs`.
    #[tokio::test]
    async fn test_active_profile_id_set_and_read() {
        let store = setup_arc_store().await;
        store
            .create_arc("arc_1", "Profile arc", ArcSource::UserInput)
            .await
            .unwrap();

        let meta = store.get_arc("arc_1").await.unwrap().expect("arc present");
        assert_eq!(meta.active_profile_id, None);

        store
            .set_active_profile_id("arc_1", Some("outreach"))
            .await
            .unwrap();
        let meta = store.get_arc("arc_1").await.unwrap().expect("arc present");
        assert_eq!(meta.active_profile_id.as_deref(), Some("outreach"));

        let listed = store.list_arcs().await.unwrap();
        let row = listed.iter().find(|a| a.id == "arc_1").unwrap();
        assert_eq!(row.active_profile_id.as_deref(), Some("outreach"));

        // Clearing falls back to None (i.e. seeded default profile).
        store.set_active_profile_id("arc_1", None).await.unwrap();
        let meta = store.get_arc("arc_1").await.unwrap().expect("arc present");
        assert_eq!(meta.active_profile_id, None);
    }

    /// init_schema must be idempotent and must add `active_profile_id` to a
    /// pre-existing database that was created before the column existed.
    #[tokio::test]
    async fn test_active_profile_id_migration_on_legacy_db() {
        let conn = Connection::open_in_memory().expect("open in-memory db");
        let conn = Arc::new(Mutex::new(conn));

        let conn_clone = conn.clone();
        tokio::task::spawn_blocking(move || {
            let c = conn_clone.blocking_lock();
            c.execute_batch(
                "CREATE TABLE arcs (
                    id TEXT PRIMARY KEY,
                    name TEXT NOT NULL,
                    source TEXT NOT NULL DEFAULT 'user_input',
                    status TEXT NOT NULL DEFAULT 'active',
                    parent_arc_id TEXT,
                    merged_into_arc_id TEXT,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL,
                    primary_reply_channel TEXT
                );
                CREATE TABLE arc_entries (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    arc_id TEXT NOT NULL,
                    entry_type TEXT NOT NULL DEFAULT 'message',
                    source TEXT NOT NULL,
                    content TEXT NOT NULL,
                    metadata TEXT,
                    created_at TEXT NOT NULL,
                    turn_id TEXT
                );
                INSERT INTO arcs (id, name, source, status, created_at, updated_at)
                  VALUES ('legacy', 'Legacy', 'user_input', 'active',
                          '2025-01-01T00:00:00Z', '2025-01-01T00:00:00Z');",
            )
            .expect("seed legacy schema");
        })
        .await
        .unwrap();

        let store = ArcStore::new(conn);
        store.init_schema().await.expect("init migrates legacy db");

        let meta = store.get_arc("legacy").await.unwrap().expect("arc present");
        assert_eq!(meta.active_profile_id, None);

        store
            .set_active_profile_id("legacy", Some("default"))
            .await
            .unwrap();
        let meta = store.get_arc("legacy").await.unwrap().expect("arc present");
        assert_eq!(meta.active_profile_id.as_deref(), Some("default"));

        store.init_schema().await.expect("idempotent re-init");
    }

    /// init_schema must be idempotent and must add `primary_reply_channel` to a
    /// pre-existing database that was created before the column existed.
    #[tokio::test]
    async fn test_primary_reply_channel_migration_on_legacy_db() {
        let conn = Connection::open_in_memory().expect("open in-memory db");
        let conn = Arc::new(Mutex::new(conn));

        // Simulate an old DB: arcs WITHOUT primary_reply_channel.
        let conn_clone = conn.clone();
        tokio::task::spawn_blocking(move || {
            let c = conn_clone.blocking_lock();
            c.execute_batch(
                "CREATE TABLE arcs (
                    id TEXT PRIMARY KEY,
                    name TEXT NOT NULL,
                    source TEXT NOT NULL DEFAULT 'user_input',
                    status TEXT NOT NULL DEFAULT 'active',
                    parent_arc_id TEXT,
                    merged_into_arc_id TEXT,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                );
                CREATE TABLE arc_entries (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    arc_id TEXT NOT NULL,
                    entry_type TEXT NOT NULL DEFAULT 'message',
                    source TEXT NOT NULL,
                    content TEXT NOT NULL,
                    metadata TEXT,
                    created_at TEXT NOT NULL
                );
                INSERT INTO arcs (id, name, source, status, created_at, updated_at)
                  VALUES ('legacy', 'Legacy', 'messaging', 'active',
                          '2025-01-01T00:00:00Z', '2025-01-01T00:00:00Z');",
            )
            .expect("seed legacy schema");
        })
        .await
        .unwrap();

        let store = ArcStore::new(conn);
        store.init_schema().await.expect("init migrates legacy db");

        // Pre-existing arcs default to None.
        let meta = store.get_arc("legacy").await.unwrap().expect("arc present");
        assert_eq!(meta.primary_reply_channel, None);

        // Setting on the migrated db works.
        store
            .set_primary_reply_channel("legacy", "telegram")
            .await
            .unwrap();
        let meta = store.get_arc("legacy").await.unwrap().expect("arc present");
        assert_eq!(meta.primary_reply_channel.as_deref(), Some("telegram"));

        // Idempotent.
        store.init_schema().await.expect("idempotent re-init");
    }

    #[tokio::test]
    async fn test_summarized_through_entry_id_round_trip() {
        let store = setup_arc_store().await;
        store
            .create_arc("arc_c", "Compaction Arc", ArcSource::UserInput)
            .await
            .unwrap();

        let meta = store.get_arc("arc_c").await.unwrap().expect("arc present");
        assert_eq!(meta.summarized_through_entry_id, None);

        store
            .set_summarized_through_entry_id("arc_c", Some(42))
            .await
            .unwrap();
        let meta = store.get_arc("arc_c").await.unwrap().expect("arc present");
        assert_eq!(meta.summarized_through_entry_id, Some(42));

        store
            .set_summarized_through_entry_id("arc_c", None)
            .await
            .unwrap();
        let meta = store.get_arc("arc_c").await.unwrap().expect("arc present");
        assert_eq!(meta.summarized_through_entry_id, None);
    }

    #[test]
    fn test_entry_type_summary_round_trip() {
        assert_eq!(EntryType::Summary.as_str(), "summary");
        assert_eq!(EntryType::from_str("summary"), EntryType::Summary);
        // Round-trip every variant for safety.
        for v in [
            EntryType::Message,
            EntryType::ToolCall,
            EntryType::EmailEvent,
            EntryType::CalendarEvent,
            EntryType::SystemEvent,
            EntryType::Summary,
        ] {
            assert_eq!(EntryType::from_str(v.as_str()), v);
        }
    }

    #[tokio::test]
    async fn test_compact_arc_atomic_write_and_pointer() {
        let store = setup_arc_store().await;
        store
            .create_arc("arc_x", "Compact Arc", ArcSource::UserInput)
            .await
            .unwrap();
        for i in 0..5 {
            store
                .add_entry(
                    "arc_x",
                    EntryType::Message,
                    "user",
                    &format!("turn {i}"),
                    None,
                    None,
                )
                .await
                .unwrap();
        }
        let entries_before = store.load_entries("arc_x").await.unwrap();
        let last_id = entries_before.last().unwrap().id;

        let summary_id = store
            .compact_arc(
                "arc_x",
                "Arc covers 5 user turns about X.",
                Some(json!({"covered_turns": 5})),
                last_id,
            )
            .await
            .unwrap();

        let meta = store.get_arc("arc_x").await.unwrap().expect("arc");
        assert_eq!(meta.summarized_through_entry_id, Some(last_id));

        let summary = store
            .load_latest_summary("arc_x")
            .await
            .unwrap()
            .expect("summary present");
        assert_eq!(summary.id, summary_id);
        assert_eq!(summary.entry_type, EntryType::Summary);
        assert_eq!(summary.source, "compactor");
        assert_eq!(summary.content, "Arc covers 5 user turns about X.");
        assert!(summary.metadata.is_some());
    }

    #[tokio::test]
    async fn test_load_entries_after_returns_only_tail() {
        let store = setup_arc_store().await;
        store
            .create_arc("arc_t", "Tail Arc", ArcSource::UserInput)
            .await
            .unwrap();
        let mut ids = Vec::new();
        for i in 0..4 {
            let id = store
                .add_entry(
                    "arc_t",
                    EntryType::Message,
                    "user",
                    &format!("m{i}"),
                    None,
                    None,
                )
                .await
                .unwrap();
            ids.push(id);
        }

        let after = store.load_entries_after("arc_t", ids[1]).await.unwrap();
        assert_eq!(after.len(), 2);
        assert_eq!(after[0].content, "m2");
        assert_eq!(after[1].content, "m3");

        // Sanity: pointing past the last id returns empty.
        let none = store.load_entries_after("arc_t", ids[3]).await.unwrap();
        assert!(none.is_empty());
    }

    /// Pin lifecycle: new arcs default to None on both columns; the
    /// first set wins; subsequent sets are no-ops; clear restores None;
    /// re-pin after clear is allowed.
    #[tokio::test]
    async fn test_pinned_provider_lifecycle() {
        let store = setup_arc_store().await;
        store
            .create_arc("arc_p", "Pin arc", ArcSource::UserInput)
            .await
            .unwrap();

        let meta = store.get_arc("arc_p").await.unwrap().expect("arc present");
        assert_eq!(meta.pinned_provider_id, None);
        assert_eq!(meta.pinned_slug, None);

        let wrote = store
            .set_pinned_provider_if_unset("arc_p", "deepseek", "deepseek-v4-pro")
            .await
            .unwrap();
        assert!(wrote, "first set must write");
        let meta = store.get_arc("arc_p").await.unwrap().expect("arc present");
        assert_eq!(meta.pinned_provider_id.as_deref(), Some("deepseek"));
        assert_eq!(meta.pinned_slug.as_deref(), Some("deepseek-v4-pro"));

        let wrote = store
            .set_pinned_provider_if_unset("arc_p", "anthropic", "claude-sonnet-4-6")
            .await
            .unwrap();
        assert!(!wrote, "second set must be a no-op (first-call-wins)");
        let meta = store.get_arc("arc_p").await.unwrap().expect("arc present");
        assert_eq!(meta.pinned_provider_id.as_deref(), Some("deepseek"));
        assert_eq!(meta.pinned_slug.as_deref(), Some("deepseek-v4-pro"));

        store.clear_pinned_provider("arc_p").await.unwrap();
        let meta = store.get_arc("arc_p").await.unwrap().expect("arc present");
        assert_eq!(meta.pinned_provider_id, None);
        assert_eq!(meta.pinned_slug, None);

        let wrote = store
            .set_pinned_provider_if_unset("arc_p", "anthropic", "claude-sonnet-4-6")
            .await
            .unwrap();
        assert!(wrote, "re-pin after clear must succeed");
        let listed = store.list_arcs().await.unwrap();
        let row = listed.iter().find(|a| a.id == "arc_p").unwrap();
        assert_eq!(row.pinned_provider_id.as_deref(), Some("anthropic"));
        assert_eq!(row.pinned_slug.as_deref(), Some("claude-sonnet-4-6"));
    }

    /// Startup sweep clears every arc's pin in one shot and reports the
    /// count, leaving unpinned arcs untouched.
    #[tokio::test]
    async fn test_clear_all_provider_pins() {
        let store = setup_arc_store().await;
        for id in ["arc_a", "arc_b", "arc_c"] {
            store
                .create_arc(id, "arc", ArcSource::UserInput)
                .await
                .unwrap();
        }
        store
            .set_pinned_provider_if_unset("arc_a", "opencode_go", "deepseek-v4-pro")
            .await
            .unwrap();
        store
            .set_pinned_provider_if_unset("arc_b", "deepseek", "deepseek-v4-flash")
            .await
            .unwrap();
        // arc_c left unpinned.

        let cleared = store.clear_all_provider_pins().await.unwrap();
        assert_eq!(cleared, 2, "only the two pinned arcs count");

        for id in ["arc_a", "arc_b", "arc_c"] {
            let meta = store.get_arc(id).await.unwrap().expect("arc present");
            assert_eq!(meta.pinned_provider_id, None);
            assert_eq!(meta.pinned_slug, None);
        }

        // Idempotent: a second sweep clears nothing.
        let again = store.clear_all_provider_pins().await.unwrap();
        assert_eq!(again, 0);
    }

    /// `init_schema` must add `pinned_provider_id` + `pinned_slug` to a
    /// pre-existing database created before the columns existed.
    #[tokio::test]
    async fn test_pinned_provider_migration_on_legacy_db() {
        let conn = Connection::open_in_memory().expect("open in-memory db");
        let conn = Arc::new(Mutex::new(conn));

        let conn_clone = conn.clone();
        tokio::task::spawn_blocking(move || {
            let c = conn_clone.blocking_lock();
            c.execute_batch(
                "CREATE TABLE arcs (
                    id TEXT PRIMARY KEY,
                    name TEXT NOT NULL,
                    source TEXT NOT NULL DEFAULT 'user_input',
                    status TEXT NOT NULL DEFAULT 'active',
                    parent_arc_id TEXT,
                    merged_into_arc_id TEXT,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL,
                    primary_reply_channel TEXT,
                    active_profile_id TEXT,
                    summarized_through_entry_id INTEGER
                );
                CREATE TABLE arc_entries (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    arc_id TEXT NOT NULL,
                    entry_type TEXT NOT NULL DEFAULT 'message',
                    source TEXT NOT NULL,
                    content TEXT NOT NULL,
                    metadata TEXT,
                    created_at TEXT NOT NULL,
                    turn_id TEXT
                );
                INSERT INTO arcs (id, name, source, status, created_at, updated_at)
                  VALUES ('legacy', 'Legacy', 'user_input', 'active',
                          '2025-01-01T00:00:00Z', '2025-01-01T00:00:00Z');",
            )
            .expect("seed legacy schema");
        })
        .await
        .unwrap();

        let store = ArcStore::new(conn);
        store.init_schema().await.expect("migrates legacy db");

        let meta = store.get_arc("legacy").await.unwrap().expect("arc present");
        assert_eq!(meta.pinned_provider_id, None);
        assert_eq!(meta.pinned_slug, None);

        let wrote = store
            .set_pinned_provider_if_unset("legacy", "openai", "gpt-5.5")
            .await
            .unwrap();
        assert!(wrote);
        let meta = store.get_arc("legacy").await.unwrap().expect("arc present");
        assert_eq!(meta.pinned_provider_id.as_deref(), Some("openai"));
        assert_eq!(meta.pinned_slug.as_deref(), Some("gpt-5.5"));

        store.init_schema().await.expect("idempotent re-init");
    }

    #[tokio::test]
    async fn test_load_latest_summary_picks_most_recent() {
        let store = setup_arc_store().await;
        store
            .create_arc("arc_m", "Multi-summary Arc", ArcSource::UserInput)
            .await
            .unwrap();
        for i in 0..3 {
            store
                .add_entry(
                    "arc_m",
                    EntryType::Message,
                    "user",
                    &format!("e{i}"),
                    None,
                    None,
                )
                .await
                .unwrap();
        }
        let entries = store.load_entries("arc_m").await.unwrap();
        let mid_id = entries[1].id;
        let last_id = entries[2].id;

        let s1 = store
            .compact_arc("arc_m", "summary one", None, mid_id)
            .await
            .unwrap();
        let s2 = store
            .compact_arc("arc_m", "summary two", None, last_id)
            .await
            .unwrap();

        let latest = store
            .load_latest_summary("arc_m")
            .await
            .unwrap()
            .expect("summary");
        assert_eq!(latest.id, s2);
        assert!(s2 > s1);
        assert_eq!(latest.content, "summary two");

        // Pointer reflects the most recent compaction.
        let meta = store.get_arc("arc_m").await.unwrap().expect("arc");
        assert_eq!(meta.summarized_through_entry_id, Some(last_id));
    }

    /// Per-arc reasoning-effort override: defaults to None, last-write-wins
    /// (distinct from the first-call-wins pin), and round-trips through
    /// both `get_arc` and `list_arcs`.
    #[tokio::test]
    async fn test_reasoning_effort_override_set_get_clear() {
        let store = setup_arc_store().await;
        store
            .create_arc("arc_r", "Reasoning arc", ArcSource::UserInput)
            .await
            .unwrap();

        let meta = store.get_arc("arc_r").await.unwrap().expect("arc present");
        assert_eq!(meta.reasoning_effort_override, None);

        store
            .set_reasoning_effort_override("arc_r", Some("high"))
            .await
            .unwrap();
        let meta = store.get_arc("arc_r").await.unwrap().expect("arc present");
        assert_eq!(meta.reasoning_effort_override.as_deref(), Some("high"));

        // Last-write-wins: overwrite freely.
        store
            .set_reasoning_effort_override("arc_r", Some("off"))
            .await
            .unwrap();
        let meta = store.get_arc("arc_r").await.unwrap().expect("arc present");
        assert_eq!(meta.reasoning_effort_override.as_deref(), Some("off"));

        // list_arcs surfaces the same value.
        let listed = store.list_arcs().await.unwrap();
        let row = listed.iter().find(|a| a.id == "arc_r").unwrap();
        assert_eq!(row.reasoning_effort_override.as_deref(), Some("off"));

        // Clear restores None.
        store
            .set_reasoning_effort_override("arc_r", None)
            .await
            .unwrap();
        let meta = store.get_arc("arc_r").await.unwrap().expect("arc present");
        assert_eq!(meta.reasoning_effort_override, None);
    }

    /// Per-arc tier override: defaults to None, last-write-wins, and
    /// round-trips through both `get_arc` and `list_arcs`. Modeled on the
    /// reasoning-effort override test since the semantics are identical.
    #[tokio::test]
    async fn test_tier_override_set_get_clear() {
        let store = setup_arc_store().await;
        store
            .create_arc("arc_t", "Tier arc", ArcSource::UserInput)
            .await
            .unwrap();

        let meta = store.get_arc("arc_t").await.unwrap().expect("arc present");
        assert_eq!(meta.tier_override, None);

        store
            .set_tier_override("arc_t", Some("Powerful"))
            .await
            .unwrap();
        let meta = store.get_arc("arc_t").await.unwrap().expect("arc present");
        assert_eq!(meta.tier_override.as_deref(), Some("Powerful"));

        // Last-write-wins: overwrite freely.
        store
            .set_tier_override("arc_t", Some("Cheap"))
            .await
            .unwrap();
        let meta = store.get_arc("arc_t").await.unwrap().expect("arc present");
        assert_eq!(meta.tier_override.as_deref(), Some("Cheap"));

        // list_arcs surfaces the same value.
        let listed = store.list_arcs().await.unwrap();
        let row = listed.iter().find(|a| a.id == "arc_t").unwrap();
        assert_eq!(row.tier_override.as_deref(), Some("Cheap"));

        // Clear restores None.
        store.set_tier_override("arc_t", None).await.unwrap();
        let meta = store.get_arc("arc_t").await.unwrap().expect("arc present");
        assert_eq!(meta.tier_override, None);
    }

    /// Per-arc security-mode override: defaults to None, last-write-wins,
    /// and round-trips through both `get_arc` and `list_arcs`.
    #[tokio::test]
    async fn test_security_mode_override_set_get_clear() {
        let store = setup_arc_store().await;
        store
            .create_arc("arc_s", "Security arc", ArcSource::UserInput)
            .await
            .unwrap();

        let meta = store.get_arc("arc_s").await.unwrap().expect("arc present");
        assert_eq!(meta.security_mode_override, None);

        store
            .set_security_mode_override("arc_s", Some("bunker"))
            .await
            .unwrap();
        let meta = store.get_arc("arc_s").await.unwrap().expect("arc present");
        assert_eq!(meta.security_mode_override.as_deref(), Some("bunker"));

        // Last-write-wins.
        store
            .set_security_mode_override("arc_s", Some("yolo"))
            .await
            .unwrap();
        let meta = store.get_arc("arc_s").await.unwrap().expect("arc present");
        assert_eq!(meta.security_mode_override.as_deref(), Some("yolo"));

        // list_arcs surfaces the same value.
        let listed = store.list_arcs().await.unwrap();
        let row = listed.iter().find(|a| a.id == "arc_s").unwrap();
        assert_eq!(row.security_mode_override.as_deref(), Some("yolo"));

        // Clear restores None.
        store
            .set_security_mode_override("arc_s", None)
            .await
            .unwrap();
        let meta = store.get_arc("arc_s").await.unwrap().expect("arc present");
        assert_eq!(meta.security_mode_override, None);
    }

    /// Triage plan round-trip: defaults to None, both columns are
    /// updated atomically, half-plans on the database side collapse to
    /// None when read back, and the value surfaces through both
    /// `get_arc` and `list_arcs`.
    #[tokio::test]
    async fn test_triage_plan_set_get_clear() {
        let store = setup_arc_store().await;
        store
            .create_arc("arc_p", "Plan arc", ArcSource::UserInput)
            .await
            .unwrap();

        let meta = store.get_arc("arc_p").await.unwrap().expect("arc present");
        assert!(meta.triage_plan.is_none());

        let plan = TriagePlan {
            acceptance_criteria: "Reply once with Q3 terms confirmed.".to_string(),
            scope: "NOT a multi-message thread.".to_string(),
        };
        store.set_triage_plan("arc_p", Some(&plan)).await.unwrap();
        let meta = store.get_arc("arc_p").await.unwrap().expect("arc present");
        let stored = meta.triage_plan.expect("plan stored");
        assert_eq!(stored.acceptance_criteria, plan.acceptance_criteria);
        assert_eq!(stored.scope, plan.scope);

        // list_arcs sees the same plan.
        let listed = store.list_arcs().await.unwrap();
        let row = listed.iter().find(|a| a.id == "arc_p").unwrap();
        assert_eq!(
            row.triage_plan
                .as_ref()
                .map(|p| p.acceptance_criteria.as_str()),
            Some(plan.acceptance_criteria.as_str())
        );

        // Clear restores None.
        store.set_triage_plan("arc_p", None).await.unwrap();
        let meta = store.get_arc("arc_p").await.unwrap().expect("arc present");
        assert!(meta.triage_plan.is_none());
    }

    /// Write-once helper: first call wins, subsequent calls observe and
    /// return false. Modeled on `set_pinned_provider_if_unset`.
    #[tokio::test]
    async fn test_triage_plan_if_absent_write_once() {
        let store = setup_arc_store().await;
        store
            .create_arc("arc_once", "Once", ArcSource::UserInput)
            .await
            .unwrap();

        let plan_a = TriagePlan {
            acceptance_criteria: "first plan".to_string(),
            scope: "first scope".to_string(),
        };
        let wrote = store
            .set_triage_plan_if_absent("arc_once", Some(&plan_a))
            .await
            .unwrap();
        assert!(wrote, "first write should succeed");

        let plan_b = TriagePlan {
            acceptance_criteria: "second plan".to_string(),
            scope: "second scope".to_string(),
        };
        let wrote = store
            .set_triage_plan_if_absent("arc_once", Some(&plan_b))
            .await
            .unwrap();
        assert!(!wrote, "second write must be a no-op");

        let meta = store
            .get_arc("arc_once")
            .await
            .unwrap()
            .expect("arc present");
        let stored = meta.triage_plan.expect("plan stored");
        assert_eq!(stored.acceptance_criteria, "first plan");
        assert_eq!(stored.scope, "first scope");

        // Passing None short-circuits cleanly.
        let wrote = store
            .set_triage_plan_if_absent("arc_once", None)
            .await
            .unwrap();
        assert!(!wrote);
    }

    /// Half-plans on the database side (one column populated, the other
    /// NULL or empty) must collapse to None — same invariant the LLM
    /// parser enforces, applied at read time as a defense layer in case
    /// some future code path writes only one column.
    #[tokio::test]
    async fn test_triage_plan_half_plan_collapses_to_none() {
        let store = setup_arc_store().await;
        store
            .create_arc("arc_half", "Half plan", ArcSource::UserInput)
            .await
            .unwrap();

        // Write directly via raw SQL to simulate a malformed row.
        let conn = store.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute(
                "UPDATE arcs SET triage_plan_acceptance = ?1, triage_plan_scope = ?2 \
                 WHERE id = ?3",
                params!["acceptance only", Option::<String>::None, "arc_half"],
            )
            .unwrap();
        })
        .await
        .unwrap();

        let meta = store
            .get_arc("arc_half")
            .await
            .unwrap()
            .expect("arc present");
        assert!(
            meta.triage_plan.is_none(),
            "half-plan must collapse to None on read"
        );
    }

    /// `project_id` round-trip: an arc created in a project carries it,
    /// a plain arc has `None`, `set_arc_project` updates it (and clears it),
    /// and a child arc branched from a parent inherits the parent's project.
    #[tokio::test]
    async fn test_project_id_round_trip() {
        let store = setup_arc_store().await;

        // Created in a project -> get_arc returns it (and list_arcs too).
        store
            .create_arc_in_project(
                "arc_proj",
                "In project",
                ArcSource::UserInput,
                Some("proj_1"),
            )
            .await
            .unwrap();
        let meta = store
            .get_arc("arc_proj")
            .await
            .unwrap()
            .expect("arc present");
        assert_eq!(meta.project_id.as_deref(), Some("proj_1"));
        let listed = store.list_arcs().await.unwrap();
        let row = listed.iter().find(|a| a.id == "arc_proj").unwrap();
        assert_eq!(row.project_id.as_deref(), Some("proj_1"));

        // Plain create yields None.
        store
            .create_arc("arc_plain", "No project", ArcSource::UserInput)
            .await
            .unwrap();
        let meta = store
            .get_arc("arc_plain")
            .await
            .unwrap()
            .expect("arc present");
        assert_eq!(meta.project_id, None);

        // set_arc_project moves the plain arc into a project, then clears it.
        store
            .set_arc_project("arc_plain", Some("proj_2"))
            .await
            .unwrap();
        let meta = store
            .get_arc("arc_plain")
            .await
            .unwrap()
            .expect("arc present");
        assert_eq!(meta.project_id.as_deref(), Some("proj_2"));
        store.set_arc_project("arc_plain", None).await.unwrap();
        let meta = store
            .get_arc("arc_plain")
            .await
            .unwrap()
            .expect("arc present");
        assert_eq!(meta.project_id, None);

        // A child branched from a parent inherits the parent's project_id.
        store
            .create_arc_with_parent("arc_child", "Child", ArcSource::UserInput, "arc_proj")
            .await
            .unwrap();
        let meta = store
            .get_arc("arc_child")
            .await
            .unwrap()
            .expect("arc present");
        assert_eq!(meta.project_id.as_deref(), Some("proj_1"));

        // A child of a project-less parent inherits None.
        store
            .create_arc_with_parent("arc_child2", "Child 2", ArcSource::UserInput, "arc_plain")
            .await
            .unwrap();
        let meta = store
            .get_arc("arc_child2")
            .await
            .unwrap()
            .expect("arc present");
        assert_eq!(meta.project_id, None);
    }

    /// Deep Research metadata round-trip: a fresh arc has both fields `None`,
    /// the setters persist values, and `get_arc` reloads them.
    #[tokio::test]
    async fn test_research_fields_round_trip() {
        let store = setup_arc_store().await;

        // A fresh arc has both Deep Research fields as None.
        store
            .create_arc("arc_research", "Research", ArcSource::UserInput)
            .await
            .unwrap();
        let meta = store
            .get_arc("arc_research")
            .await
            .unwrap()
            .expect("arc present");
        assert_eq!(meta.research_paper_path, None);
        assert_eq!(meta.research_question, None);

        // Setters persist both fields; get_arc reloads them.
        store
            .set_research_paper_path("arc_research", Some("/tmp/paper.md"))
            .await
            .unwrap();
        store
            .set_research_question("arc_research", Some("What is the answer?"))
            .await
            .unwrap();
        let meta = store
            .get_arc("arc_research")
            .await
            .unwrap()
            .expect("arc present");
        assert_eq!(meta.research_paper_path.as_deref(), Some("/tmp/paper.md"));
        assert_eq!(
            meta.research_question.as_deref(),
            Some("What is the answer?")
        );

        // list_arcs reflects the same values.
        let listed = store.list_arcs().await.unwrap();
        let row = listed.iter().find(|a| a.id == "arc_research").unwrap();
        assert_eq!(row.research_paper_path.as_deref(), Some("/tmp/paper.md"));
        assert_eq!(row.research_question.as_deref(), Some("What is the answer?"));

        // Clearing with None round-trips back to None.
        store
            .set_research_paper_path("arc_research", None)
            .await
            .unwrap();
        store
            .set_research_question("arc_research", None)
            .await
            .unwrap();
        let meta = store
            .get_arc("arc_research")
            .await
            .unwrap()
            .expect("arc present");
        assert_eq!(meta.research_paper_path, None);
        assert_eq!(meta.research_question, None);
    }
}
