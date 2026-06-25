// Typed client for the instance HTTP API. Deliberately DOM-free
// (fetch only — available in React Native too) so this folder can be
// lifted into a shared package when the RN app lands. The SSE side
// lives in events.ts because EventSource is platform-specific.

import type {
  ArcEntry,
  ArcMeta,
  DeepResearchDepth,
  DeepResearchMode,
  DeepResearchResult,
  GrantDecision,
  GrantRequest,
  MemoryInfo,
  NotificationInfo,
  Project,
  ProjectFileInfo,
  SendResult,
  SummaryMode,
} from './types';

export interface ClientConfig {
  /** Origin of the instance, no trailing slash. Empty = same origin. */
  baseUrl: string;
  /** Contents of `<data_dir>/http_token` / `ATHEN_HTTP_TOKEN`. */
  token: string;
}

export class ApiError extends Error {
  constructor(
    message: string,
    public readonly status: number,
  ) {
    super(message);
  }
}

export class AthenClient {
  constructor(private readonly cfg: ClientConfig) {}

  get baseUrl(): string {
    return this.cfg.baseUrl;
  }

  /** SSE endpoint with the token as a query param (EventSource can't set headers). */
  eventsUrl(): string {
    return `${this.cfg.baseUrl}/api/events?token=${encodeURIComponent(this.cfg.token)}`;
  }

  private async req<T>(path: string, body?: unknown, method?: string): Promise<T> {
    let resp: Response;
    try {
      resp = await fetch(`${this.cfg.baseUrl}/api${path}`, {
        method: method ?? (body === undefined ? 'GET' : 'POST'),
        headers: {
          Authorization: `Bearer ${this.cfg.token}`,
          ...(body !== undefined ? { 'Content-Type': 'application/json' } : {}),
        },
        body: body !== undefined ? JSON.stringify(body) : undefined,
      });
    } catch {
      throw new ApiError('server unreachable', 0);
    }
    let data: unknown = null;
    try {
      data = await resp.json();
    } catch {
      /* empty or non-JSON body */
    }
    if (!resp.ok) {
      const msg =
        data && typeof data === 'object' && 'error' in data
          ? String((data as { error: unknown }).error)
          : `HTTP ${resp.status}`;
      throw new ApiError(msg, resp.status);
    }
    return data as T;
  }

  // ---- generic verbs (full command surface; see http_api.rs
  // full_surface_router for the route map) ----
  get<T = unknown>(path: string): Promise<T> {
    return this.req(path);
  }
  post<T = unknown>(path: string, body?: unknown): Promise<T> {
    return this.req(path, body, 'POST');
  }
  del<T = unknown>(path: string): Promise<T> {
    return this.req(path, undefined, 'DELETE');
  }

  // ---- arcs ----
  listArcs(): Promise<ArcMeta[]> {
    return this.req('/arcs');
  }
  newArc(): Promise<{ arc_id: string }> {
    return this.req('/arcs', undefined, 'POST');
  }
  currentArc(): Promise<{ arc_id: string | null }> {
    return this.req('/arcs/current');
  }
  arcEntries(arcId: string): Promise<ArcEntry[]> {
    return this.req(`/arcs/${encodeURIComponent(arcId)}/entries`);
  }
  /** Switch the server-side active arc; returns its entries. */
  selectArc(arcId: string): Promise<ArcEntry[]> {
    return this.req(`/arcs/${encodeURIComponent(arcId)}/select`, undefined, 'POST');
  }

  // ---- chat ----
  /** Long-poll: resolves when the agent turn finishes (or parks on pending_approval). */
  sendMessage(
    message: string,
    arcId?: string,
    images?: { mime_type: string; data: { kind: 'base64'; data: string } }[],
    attachments?: { name: string; mime_type: string; base64: string }[],
  ): Promise<SendResult> {
    return this.req('/messages', { message, arc_id: arcId, images, attachments });
  }
  /** Queue input for the running turn on `arcId` (composer while busy). */
  queueMessage(arcId: string, text: string): Promise<{ queued: boolean }> {
    return this.req('/messages/queue', { arc_id: arcId, text });
  }
  cancelAll(): Promise<{ cancelled: boolean }> {
    return this.req('/cancel', undefined, 'POST');
  }

  // ---- deep research (docs/DEEP_RESEARCH.md) ----
  /**
   * Kick off a Deep Research run on `arcId`. Resolves when the (long-running)
   * pipeline finishes; live phase updates arrive over SSE as
   * `deep-research-progress` / `deep-research-done`. `mode` is only meaningful
   * when the arc already has a paper (extend vs new); omit it for a first run.
   */
  deepResearch(
    arcId: string,
    question: string,
    depth?: DeepResearchDepth,
    mode?: DeepResearchMode,
  ): Promise<DeepResearchResult> {
    return this.req(`/arcs/${encodeURIComponent(arcId)}/deep-research`, {
      question,
      depth,
      mode,
    });
  }
  /**
   * Fetch the rendered Markdown of the arc's research paper. The response body
   * is a JSON string (the Markdown), so the generic GET helper's `resp.json()`
   * yields a `string` directly. Errors come back as the app's standard
   * `{ error }` shape and surface as an `ApiError`.
   */
  getResearchPaper(arcId: string): Promise<string> {
    return this.req(`/arcs/${encodeURIComponent(arcId)}/research-paper`);
  }

  // ---- approvals & grants ----
  approveTask(taskId: string, approved: boolean): Promise<unknown> {
    return this.req('/approvals/task', { task_id: taskId, approved });
  }
  answerQuestion(questionId: string, choiceKey: string): Promise<{ resolved: boolean }> {
    return this.req('/approvals/question', { question_id: questionId, choice_key: choiceKey });
  }
  pendingGrants(): Promise<GrantRequest[]> {
    return this.req('/grants/pending');
  }
  resolveGrant(id: string, decision: GrantDecision): Promise<{ resolved: boolean }> {
    return this.req(`/grants/${encodeURIComponent(id)}`, { decision });
  }

  // ---- projects ----
  // Wire shapes match athen-app/src/http_api.rs project handlers. The
  // instructions/active-project/assign routes use the flat bodies the
  // Settings → Projects panel already speaks.
  listProjects(): Promise<Project[]> {
    return this.req('/projects');
  }
  createProject(name: string, instructions?: string): Promise<Project> {
    return this.req('/projects', { name, instructions: instructions || undefined });
  }
  /** `instructions: ''` clears; `undefined` leaves untouched (mirrors PanelProjects). */
  updateProject(
    id: string,
    body: { name?: string; instructions?: string },
  ): Promise<Project> {
    const wire: { name?: string; instructions?: string; clear_instructions?: boolean } = {};
    if (body.name !== undefined) wire.name = body.name;
    if (body.instructions !== undefined) {
      if (body.instructions.trim()) wire.instructions = body.instructions;
      else wire.clear_instructions = true;
    }
    return this.req(`/projects/${encodeURIComponent(id)}`, wire);
  }
  deleteProject(id: string): Promise<unknown> {
    return this.req(`/projects/${encodeURIComponent(id)}/delete`, undefined, 'POST');
  }
  updateProjectSummary(id: string): Promise<unknown> {
    return this.req(`/projects/${encodeURIComponent(id)}/summary`, undefined, 'POST');
  }
  projectFiles(id: string): Promise<ProjectFileInfo[]> {
    return this.req(`/projects/${encodeURIComponent(id)}/files`);
  }
  projectMemories(id: string): Promise<MemoryInfo[]> {
    return this.req(`/projects/${encodeURIComponent(id)}/memories`);
  }
  summaryMode(): Promise<SummaryMode> {
    return this.req('/projects/summary-mode');
  }
  setSummaryMode(mode: SummaryMode): Promise<unknown> {
    return this.req('/projects/summary-mode', { mode });
  }
  /** Assign an arc to a project, or detach (`value: null`). */
  assignArcToProject(arcId: string, value: string | null): Promise<unknown> {
    return this.req(`/arcs/${encodeURIComponent(arcId)}/project`, { value });
  }
  /** Set the active project so new arcs inherit it (`value: null` clears). */
  setActiveProject(value: string | null): Promise<unknown> {
    return this.req('/active-project', { value });
  }

  // ---- notifications ----
  listNotifications(): Promise<NotificationInfo[]> {
    return this.req('/notifications');
  }
  markAllNotificationsRead(): Promise<{ ok: boolean }> {
    return this.req('/notifications/read-all', undefined, 'POST');
  }
  markNotificationRead(id: string): Promise<{ ok: boolean }> {
    return this.req(`/notifications/${encodeURIComponent(id)}/read`, undefined, 'POST');
  }
}
