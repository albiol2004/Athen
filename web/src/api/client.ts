// Typed client for the instance HTTP API. Deliberately DOM-free
// (fetch only — available in React Native too) so this folder can be
// lifted into a shared package when the RN app lands. The SSE side
// lives in events.ts because EventSource is platform-specific.

import type {
  ArcEntry,
  ArcMeta,
  GrantDecision,
  GrantRequest,
  NotificationInfo,
  SendResult,
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
  sendMessage(message: string, arcId?: string): Promise<SendResult> {
    return this.req('/messages', { message, arc_id: arcId });
  }
  /** Queue input for the running turn on `arcId` (composer while busy). */
  queueMessage(arcId: string, text: string): Promise<{ queued: boolean }> {
    return this.req('/messages/queue', { arc_id: arcId, text });
  }
  cancelAll(): Promise<{ cancelled: boolean }> {
    return this.req('/cancel', undefined, 'POST');
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
