// Shared chat-timeline renderer. The exact unit/Item rendering used by the
// main <Chat> AND the read-only <Transcript> (Code-Mode agents panel), so a
// sub-agent's transcript looks identical to a main arc — no parallel renderer.
// React escapes everything; message bodies go through <Markdown> (no
// dangerouslySetInnerHTML anywhere).

import { memo, useMemo } from 'react';
import type { AthenClient } from '../api/client';
import type {
  ApprovalChoice,
  ApprovalQuestion,
  GrantDecision,
  GrantRequest,
  PendingApproval,
} from '../api/types';
import type { ChatItem } from '../chat/reducer';
import { GrantCard, QuestionCard, TaskCard, ThinkingBlock } from './cards';
import { Markdown } from './Markdown';
import { ToolCard, ToolGroup } from './toolcards';

/** Approval/grant callbacks the Item cards need. Optional: read-only
 * transcripts (sub-arcs) never surface live approval cards, so they pass
 * nothing and the cards render in their resolved/inert state. */
export interface MessageListCallbacks {
  onAnswerQuestion?: (q: ApprovalQuestion, choice: ApprovalChoice) => Promise<void>;
  onDecideTask?: (t: PendingApproval, approved: boolean) => Promise<void>;
  onDecideGrant?: (g: GrantRequest, decision: GrantDecision, label: string) => Promise<void>;
  onOpenSettings?: (tab?: string) => void;
}

type ToolItem = Extract<ChatItem, { kind: 'tool' }>;
type RenderUnit = { kind: 'group'; key: string; tools: ToolItem[] } | { kind: 'item'; item: ChatItem };

const noopAsync = async () => {};

/** Consecutive tool items collapse into one group (desktop rule). */
export function toRenderUnits(items: ChatItem[]): RenderUnit[] {
  const units: RenderUnit[] = [];
  let buf: ToolItem[] = [];
  const flush = () => {
    if (buf.length === 1) units.push({ kind: 'item', item: buf[0] });
    else if (buf.length > 1) units.push({ kind: 'group', key: buf[0].key, tools: buf });
    buf = [];
  };
  for (const it of items) {
    if (it.kind === 'tool') buf.push(it);
    else {
      flush();
      units.push({ kind: 'item', item: it });
    }
  }
  flush();
  return units;
}

// memo'd: finished items keep their object identity across renders (the
// reducer's immutable index-replace only swaps the one live item), and
// `cb` + `client` are stabilized by callers, so only the streaming item
// re-renders per delta. Sealed bubbles / completed tools skip entirely.
export const Item = memo(function Item({
  it,
  cb,
  client,
}: {
  it: ChatItem;
  cb: MessageListCallbacks;
  client: AthenClient;
}) {
  switch (it.kind) {
    case 'msg': {
      // No-provider recovery: the backend formats the "all providers
      // exhausted" failure into a friendly message containing this phrase.
      // Detect it (in agent or system bubbles) and offer a CTA to setup so
      // the user isn't stranded on a cryptic error with no recourse.
      const noProvider = /no ai provider is set up/i.test(it.content);
      if (noProvider && cb.onOpenSettings) {
        return (
          <div className={`msg ${it.role === 'system' ? 'system' : 'agent'} no-provider`}>
            <div>{it.content}</div>
            <button className="no-provider-cta" onClick={() => cb.onOpenSettings?.('models')}>
              Open Settings → Connections
            </button>
          </div>
        );
      }
      if (it.role === 'system') return <div className="msg system">{it.content}</div>;
      if (it.role === 'user') return <div className="msg user">{it.content}</div>;
      return (
        <div className={`msg agent${it.streaming ? ' streaming' : ''}`}>
          {it.streaming ? it.content : <Markdown text={it.content} />}
        </div>
      );
    }
    case 'thinking':
      return <ThinkingBlock content={it.content} done={it.done} />;
    case 'tool':
      return <ToolCard it={it} client={client} />;
    case 'question':
      return (
        <QuestionCard
          q={it.q}
          resolved={it.resolved}
          onAnswer={(c) => cb.onAnswerQuestion?.(it.q, c) ?? noopAsync()}
        />
      );
    case 'task':
      return (
        <TaskCard
          t={it.t}
          resolved={it.resolved}
          onDecide={(a) => cb.onDecideTask?.(it.t, a) ?? noopAsync()}
        />
      );
    case 'grant':
      return (
        <GrantCard
          g={it.g}
          resolved={it.resolved}
          onDecide={(d, l) => cb.onDecideGrant?.(it.g, d, l) ?? noopAsync()}
        />
      );
  }
});

/** Render a `ChatItem[]` timeline: tool runs grouped, everything else inline.
 * Shared by <Chat> (live) and <Transcript> (read-only sub-arc playback). */
export function MessageList({
  items,
  cb,
  client,
}: {
  items: ChatItem[];
  cb: MessageListCallbacks;
  client: AthenClient;
}) {
  // Recompute render units only when the timeline actually changes, so the
  // unit arrays (and grouped tool slices) keep reference identity across
  // streaming renders and the memo'd children below stay put.
  const units = useMemo(() => toRenderUnits(items), [items]);
  return (
    <>
      {units.map((u) =>
        u.kind === 'group' ? (
          <ToolGroup key={`g-${u.key}`} items={u.tools} client={client} />
        ) : (
          <Item key={u.item.id} it={u.item} cb={cb} client={client} />
        ),
      )}
    </>
  );
}
