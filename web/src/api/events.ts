// SSE subscription over `GET /api/events`. Event names and payloads are
// byte-identical to the Tauri events the desktop WebView consumes —
// handler shapes mirror frontend/app.js. Browser EventSource reconnects
// automatically; a React Native client swaps in an SSE polyfill behind
// the same handler interface.

import type { AthenClient } from './client';
import type {
  ApprovalQuestion,
  ApprovalResolved,
  DeepResearchDoneEvent,
  DeepResearchProgressEvent,
  GrantRequest,
  NotificationInfo,
  ProgressEvent,
  StreamEvent,
} from './types';

export type ConnectionStatus = 'connecting' | 'live' | 'reconnecting';

export interface EventHandlers {
  onStatus?(s: ConnectionStatus): void;
  onStream?(e: StreamEvent): void;
  onProgress?(e: ProgressEvent): void;
  onQuestion?(q: ApprovalQuestion): void;
  onApprovalResolved?(p: ApprovalResolved): void;
  onGrant?(g: GrantRequest): void;
  /** Payload is the bare grant id (answered via Telegram or another client). */
  onGrantResolvedElsewhere?(id: string): void;
  onArcUpdated?(): void;
  onNotification?(n: NotificationInfo): void;
  /** Deep Research phase tick for some arc (filter by `arc_id` in the handler). */
  onDeepResearchProgress?(e: DeepResearchProgressEvent): void;
  /** Deep Research finished for some arc (filter by `arc_id` in the handler). */
  onDeepResearchDone?(e: DeepResearchDoneEvent): void;
  /** Bus overflow — some events were dropped; refetch state via REST. */
  onLagged?(dropped: number): void;
}

/** Subscribe to the instance event stream. Returns a disposer. */
export function connectEvents(client: AthenClient, h: EventHandlers): () => void {
  const es = new EventSource(client.eventsUrl());
  h.onStatus?.('connecting');
  es.onopen = () => h.onStatus?.('live');
  es.onerror = () => h.onStatus?.('reconnecting');

  const on = <T>(name: string, fn: ((data: T) => void) | undefined) => {
    if (!fn) return;
    es.addEventListener(name, (ev) => {
      try {
        fn(JSON.parse((ev as MessageEvent).data) as T);
      } catch {
        /* malformed payload — skip */
      }
    });
  };

  on('agent-stream', h.onStream);
  on('agent-progress', h.onProgress);
  on('approval-question', h.onQuestion);
  on('approval-resolved', h.onApprovalResolved);
  on('grant-requested', h.onGrant);
  on('grant-resolved-elsewhere', h.onGrantResolvedElsewhere);
  on('notification', h.onNotification);
  on('deep-research-progress', h.onDeepResearchProgress);
  on('deep-research-done', h.onDeepResearchDone);
  on('lagged', h.onLagged);
  // No payload to parse on these — fire directly.
  if (h.onArcUpdated) es.addEventListener('arc-updated', () => h.onArcUpdated?.());

  return () => es.close();
}
