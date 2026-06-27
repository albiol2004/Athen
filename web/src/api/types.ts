// Wire types for the instance HTTP API (athen-app/src/http_api.rs).
// Field names are verbatim serde snake_case — see ArcEntryResponse,
// ArcMeta, NotificationInfo on the Rust side. Keep this file free of
// DOM types: it is shared with a future React Native client.

/** Sidebar arc row — `GET /api/arcs` (persistence `ArcMeta`, snake_case). */
export interface ArcMeta {
  id: string;
  name: string;
  source: string;
  status: string;
  parent_arc_id: string | null;
  created_at: string;
  updated_at: string;
  entry_count: number;
  active_profile_id?: string | null;
  pinned_provider_id?: string | null;
  pinned_slug?: string | null;
  /** Project membership, or null when the arc belongs to no project. */
  project_id?: string | null;
  /** Workspace-relative path to this arc's Deep Research paper, if any. */
  research_paper_path?: string | null;
  /** The question driving this arc's Deep Research run, if any. */
  research_question?: string | null;
  /** Code Mode (docs/CODE_MODE.md): is this arc a coding session in a real repo? */
  code_mode?: boolean;
  /** Absolute path to the repo root the Code-Mode arc operates in. */
  code_mode_root?: string | null;
}

/** One persisted timeline row — `GET /api/arcs/{id}/entries`. */
export interface ArcEntry {
  id: number;
  /** "message" | "tool_call" | other internal types (skip unknown). */
  entry_type: string;
  /** "user" | "assistant" | "system" | sense names. */
  source: string;
  content: string;
  metadata: Record<string, unknown> | null;
  created_at: string;
  turn_id: string | null;
}

/** A Project — `GET /api/projects` (persistence `Project`, snake_case). */
export interface Project {
  id: string;
  name: string;
  folder_slug: string;
  instructions: string | null;
  summary: string | null;
  summary_updated_at: string | null;
  created_at: string;
  updated_at: string;
}

/** Global project-summary maintenance mode. */
export type SummaryMode = 'auto' | 'manual' | 'off';

/** One top-level entry in a project's workspace folder — `GET /api/projects/{id}/files`. */
export interface ProjectFileInfo {
  name: string;
  is_dir: boolean;
  size_bytes: number;
  /** RFC3339 UTC string, or null when the timestamp couldn't be read. */
  modified: string | null;
}

/** One project-scoped memory — `GET /api/projects/{id}/memories`. */
export interface MemoryInfo {
  id: string;
  content: string;
  source: string;
  timestamp: string;
  memory_type: string;
}

export interface NotificationInfo {
  id: string;
  urgency: string;
  title: string;
  body: string;
  origin: unknown;
  arc_id: string | null;
  created_at: string;
  is_read: boolean;
}

export interface ApprovalChoice {
  key: string;
  label?: string;
  /** "approve" renders as the primary button. */
  kind?: string;
}

/** `approval-question` SSE payload / ApprovalRouter question. */
export interface ApprovalQuestion {
  id: string;
  prompt?: string;
  description?: string;
  choices?: ApprovalChoice[];
  arc_id?: string | null;
}

/** Risk-gate card riding the long-poll `POST /api/messages` response. */
export interface PendingApproval {
  task_id: string;
  description?: string;
  summary?: string;
  risk_level?: string;
  risk_score?: number;
}

/** FileGate `grant-requested` SSE payload / `GET /api/grants/pending` row. */
export interface GrantRequest {
  id: string;
  access?: string;
  tool?: string;
  paths?: string[];
  detected_root?: {
    path: string;
    pathDisplay?: string;
    marker?: string;
  } | null;
}

/** Externally-tagged serde shape — same wire form the desktop sends. */
export type GrantDecision =
  | 'Allow'
  | 'AllowAlways'
  | 'Deny'
  | { AllowProjectRoot: string };

/** `agent-stream` SSE payload. */
export interface StreamEvent {
  delta?: string;
  is_final?: boolean;
  is_thinking?: boolean;
  arc_id?: string | null;
}

/** `agent-progress` SSE payload (auditor enriches terminal events). */
export interface ProgressEvent {
  step: number;
  tool_name: string;
  status: string;
  detail?: string;
  arc_id?: string | null;
  args?: unknown;
  result?: unknown;
  error?: string | null;
}

/** `approval-resolved` SSE payload. */
export interface ApprovalResolved {
  task_id?: string;
  approved?: boolean;
}

// ---- Deep Research (docs/DEEP_RESEARCH.md) ----

/** Depth knob for a Deep Research run; `standard` is the default. */
export type DeepResearchDepth = 'quick' | 'standard' | 'deep';

/** Re-trigger mode when an arc already has a paper. Omit for a first run. */
export type DeepResearchMode = 'extend' | 'new';

/**
 * `POST /api/arcs/{id}/deep-research` response (`DeepResearchResult`,
 * snake_case verbatim — see commands::deep_research_core).
 */
export interface DeepResearchResult {
  arc_id: string;
  paper_path: string;
  question: string;
  depth: string;
  sub_questions: string[];
  workers_total: number;
  workers_ok: number;
  extended: boolean;
}

/** `deep-research-progress` SSE payload (emitted from state.rs). */
export interface DeepResearchProgressEvent {
  arc_id: string;
  // `refining` is a gap-review step that runs on `deep` depth between reading
  // rounds; quick/standard runs never emit it.
  phase: 'planning' | 'reading' | 'refining' | 'synthesizing';
  detail: string;
  workers_total: number;
  workers_done: number;
  workers_ok: number;
}

/** `deep-research-done` SSE payload (emitted from commands::deep_research_core). */
export interface DeepResearchDoneEvent {
  arc_id: string;
  paper_path: string;
  question: string;
  workers_ok: number;
  workers_total: number;
  sub_questions: string[];
  extended: boolean;
}

// ---- Code Mode (docs/CODE_MODE.md) ----

/** One working-tree dirty file from `git status --porcelain=v2`. */
export interface DirtyFile {
  path: string;
  status: string;
}

/** One entry from `git worktree list --porcelain`. */
export interface WorktreeInfo {
  path: string;
  branch: string | null;
  head: string;
  is_main: boolean;
  locked: boolean;
}

/** One commit from `git log`. */
export interface CommitInfo {
  hash: string;
  subject: string;
  author: string;
  timestamp: string;
}

/**
 * One node in a Code-Mode arc's agent tree — `GET /api/arcs/{id}/code-mode/agents`
 * (`AgentNode`, snake_case verbatim — see athen-app/src/commands.rs). The main
 * arc(s) plus their delegation sub-arcs; `running` + `current_tool` + `step_count`
 * are merged live from the active-agent registry.
 */
export interface AgentNode {
  arc_id: string;
  parent_arc_id: string | null;
  title: string;
  source: string;
  is_main: boolean;
  running: boolean;
  started_at: string | null;
  step_count: number;
  current_tool: string | null;
}

/**
 * `GET /api/arcs/{id}/code-mode/git` response (`GitRepoState`, snake_case
 * verbatim — see athen-app/src/code_mode.rs). Read-only recognition of the
 * real repo the Code-Mode arc is rooted in.
 */
export interface GitRepoState {
  root: string;
  is_repo: boolean;
  head_branch: string | null;
  detached: boolean;
  upstream: string | null;
  ahead: number;
  behind: number;
  dirty: DirtyFile[];
  worktrees: WorktreeInfo[];
  recent_commits: CommitInfo[];
}

// ---- Remote Access (docs/REMOTE_ACCESS.md §7) ----

/** `GET /api/remote-access` — config (password never returned). */
export interface RemoteAccessInfo {
  enabled: boolean;
  port: number;
  username: string;
  tunnel_enabled: boolean;
  has_password: boolean;
}

/** `GET /api/remote-access/status` — live listener + tunnel state. */
export interface RemoteAccessStatus {
  listening: boolean;
  local_url: string | null;
  tunnel_url: string | null;
  cloudflared_installed: boolean;
  last_error: string | null;
}

/** Long-poll `POST /api/messages` response (`AgentResponse` shape). */
export interface SendResult {
  /** Final assistant text — the no-stream fallback. */
  content?: string;
  risk_level?: string;
  domain?: string;
  tool_calls?: unknown[];
  pending_approval?: PendingApproval | null;
}
