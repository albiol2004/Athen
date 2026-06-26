// Chat timeline state machine. Pure reducer so the streaming semantics
// (mirroring frontend/app.js: thinking blocks, delta accumulation,
// tool-chip upserts keyed by step+tool, sealing the open bubble when a
// tool group starts) are testable and identical across re-renders.

import type {
  ApprovalQuestion,
  ArcEntry,
  GrantRequest,
  PendingApproval,
  ProgressEvent,
  StreamEvent,
} from '../api/types';

export type ChatItem =
  | { kind: 'msg'; id: number; role: 'user' | 'agent' | 'system'; content: string; streaming?: boolean }
  | { kind: 'thinking'; id: number; content: string; done: boolean }
  | {
      kind: 'tool';
      id: number;
      key: string;
      name: string;
      status: string;
      detail?: string;
      failed?: boolean;
      args?: unknown;
      result?: unknown;
      error?: string | null;
      /** Created by a live agent-progress event (group renders open). */
      live?: boolean;
    }
  | { kind: 'question'; id: number; q: ApprovalQuestion; resolved?: string }
  | { kind: 'task'; id: number; t: PendingApproval; resolved?: string }
  | { kind: 'grant'; id: number; g: GrantRequest; resolved?: string };

export interface ChatState {
  items: ChatItem[];
  nextId: number;
}

export const initialChat: ChatState = { items: [], nextId: 1 };

export type ChatAction =
  | { type: 'reset'; entries: ArcEntry[] }
  | { type: 'user'; text: string }
  | { type: 'system'; text: string }
  | { type: 'agent'; text: string }
  | { type: 'stream'; e: StreamEvent }
  | { type: 'progress'; e: ProgressEvent }
  | { type: 'question'; q: ApprovalQuestion }
  | { type: 'task'; t: PendingApproval }
  | { type: 'grant'; g: GrantRequest }
  | { type: 'resolve'; card: 'question' | 'task' | 'grant'; refId: string; label: string };

/** Replace the item at `idx` immutably. */
function patch(items: ChatItem[], idx: number, item: ChatItem): ChatItem[] {
  const out = items.slice();
  out[idx] = item;
  return out;
}

/** Mark any open streaming bubble / live thinking block as finished. */
function sealOpen(items: ChatItem[]): ChatItem[] {
  let out = items;
  for (let i = out.length - 1; i >= 0; i--) {
    const it = out[i];
    if (it.kind === 'msg' && it.streaming) out = patch(out, i, { ...it, streaming: false });
    else if (it.kind === 'thinking' && !it.done) out = patch(out, i, { ...it, done: true });
  }
  return out;
}

export function fromEntries(entries: ArcEntry[], startId: number): { items: ChatItem[]; nextId: number } {
  const items: ChatItem[] = [];
  let id = startId;
  for (const e of entries) {
    if (e.entry_type === 'message') {
      const role = e.source === 'user' ? 'user' : e.source === 'assistant' ? 'agent' : 'system';
      if (e.content) items.push({ kind: 'msg', id: id++, role, content: e.content });
    } else if (e.entry_type === 'tool_call') {
      // Metadata is the persisted card: {tool, status, summary, args,
      // result, error} — same shape the desktop rehydrates.
      let meta: Record<string, unknown> = {};
      const raw = e.metadata as unknown;
      if (raw && typeof raw === 'object') meta = raw as Record<string, unknown>;
      else if (typeof raw === 'string') {
        try {
          meta = JSON.parse(raw) as Record<string, unknown>;
        } catch {
          /* legacy plain-text metadata */
        }
      }
      const name = (typeof meta.tool === 'string' && meta.tool) || e.content || 'tool';
      const status = (typeof meta.status === 'string' && meta.status) || 'Completed';
      items.push({
        kind: 'tool',
        id: id++,
        key: `hist-${e.id}`,
        name,
        status,
        detail: (typeof meta.summary === 'string' && meta.summary) || e.content,
        args: meta.args ?? undefined,
        result: meta.result ?? undefined,
        error: typeof meta.error === 'string' ? meta.error : null,
        failed: status === 'Failed' || Boolean(meta.error),
      });
    }
    // Other entry types (summaries, sense payloads, …) are internal.
  }
  return { items, nextId: id };
}

export function chatReducer(state: ChatState, a: ChatAction): ChatState {
  switch (a.type) {
    case 'reset': {
      const { items, nextId } = fromEntries(a.entries, 1);
      return { items, nextId };
    }

    case 'user':
    case 'system':
    case 'agent': {
      const role = a.type === 'user' ? 'user' : a.type === 'system' ? 'system' : 'agent';
      return {
        items: [...sealOpen(state.items), { kind: 'msg', id: state.nextId, role, content: a.text }],
        nextId: state.nextId + 1,
      };
    }

    case 'stream': {
      const { delta = '', is_final, is_thinking } = a.e;
      let items = state.items;
      let nextId = state.nextId;

      if (is_thinking) {
        const last = items[items.length - 1];
        if (last && last.kind === 'thinking' && !last.done) {
          items = patch(items, items.length - 1, { ...last, content: last.content + delta });
        } else {
          items = [...items, { kind: 'thinking', id: nextId++, content: delta, done: false }];
        }
        return { items, nextId };
      }

      // First answer delta closes the live thinking block.
      const lastThinking = items[items.length - 1];
      if (lastThinking && lastThinking.kind === 'thinking' && !lastThinking.done) {
        items = patch(items, items.length - 1, { ...lastThinking, done: true });
      }

      const last = items[items.length - 1];
      if (last && last.kind === 'msg' && last.streaming) {
        items = patch(items, items.length - 1, {
          ...last,
          content: last.content + delta,
          streaming: !is_final,
        });
      } else if (delta || !is_final) {
        items = [
          ...items,
          { kind: 'msg', id: nextId++, role: 'agent', content: delta, streaming: !is_final },
        ];
      }
      return { items, nextId };
    }

    case 'progress': {
      const { step, tool_name, status, detail, error, args, result } = a.e;
      // Step 0 is risk triage / lifecycle noise, same skip as the desktop.
      if (step === 0 || tool_name === 'Task completed') return state;
      const key = `${step}-${tool_name}`;
      const idx = state.items.findIndex((it) => it.kind === 'tool' && it.key === key);
      const failed = Boolean(error) || status === 'Failed' || status === 'Error';
      if (idx >= 0) {
        const existing = state.items[idx] as Extract<ChatItem, { kind: 'tool' }>;
        return {
          ...state,
          items: patch(state.items, idx, {
            ...existing,
            status,
            detail: detail || existing.detail,
            failed: failed || existing.failed,
            // The auditor enriches terminal events with full args+result.
            args: args ?? existing.args,
            result: result ?? existing.result,
            error: error ?? existing.error,
          }),
        };
      }
      // New tool: seal the open bubble — the next delta is a new segment.
      return {
        items: [
          ...sealOpen(state.items),
          {
            kind: 'tool',
            id: state.nextId,
            key,
            name: tool_name,
            status,
            detail,
            failed,
            args,
            result,
            error,
            live: true,
          },
        ],
        nextId: state.nextId + 1,
      };
    }

    case 'question': {
      if (state.items.some((it) => it.kind === 'question' && it.q.id === a.q.id)) return state;
      return {
        items: [...state.items, { kind: 'question', id: state.nextId, q: a.q }],
        nextId: state.nextId + 1,
      };
    }

    case 'task': {
      if (state.items.some((it) => it.kind === 'task' && it.t.task_id === a.t.task_id)) return state;
      return {
        items: [...state.items, { kind: 'task', id: state.nextId, t: a.t }],
        nextId: state.nextId + 1,
      };
    }

    case 'grant': {
      if (state.items.some((it) => it.kind === 'grant' && it.g.id === a.g.id)) return state;
      return {
        items: [...state.items, { kind: 'grant', id: state.nextId, g: a.g }],
        nextId: state.nextId + 1,
      };
    }

    case 'resolve': {
      const idx = state.items.findIndex((it) => {
        if (it.kind !== a.card) return false;
        if (it.kind === 'question') return it.q.id === a.refId;
        if (it.kind === 'task') return it.t.task_id === a.refId;
        if (it.kind === 'grant') return it.g.id === a.refId;
        return false;
      });
      if (idx < 0) return state;
      const it = state.items[idx];
      return { ...state, items: patch(state.items, idx, { ...it, resolved: a.label } as ChatItem) };
    }
  }
}
