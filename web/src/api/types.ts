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

/** Long-poll `POST /api/messages` response (`AgentResponse` shape). */
export interface SendResult {
  /** Final assistant text — the no-stream fallback. */
  content?: string;
  risk_level?: string;
  domain?: string;
  tool_calls?: unknown[];
  pending_approval?: PendingApproval | null;
}
