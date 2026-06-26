// Read-only timeline playback for a sub-arc (docs/CODE_MODE.md §14, Part 2).
// Reuses the SAME renderer as the main <Chat> — entries are converted to
// ChatItems via the reducer's `fromEntries`, then handed to <MessageList>, so
// a sub-agent transcript renders identically (tool groups, markdown bubbles,
// thinking blocks). Approval callbacks are inert: a persisted sub-arc never
// surfaces live approval cards. No dangerouslySetInnerHTML.

import { useMemo } from 'react';
import type { AthenClient } from '../api/client';
import type { ArcEntry } from '../api/types';
import { type ChatItem, fromEntries } from '../chat/reducer';
import { MessageList } from './MessageList';

export function Transcript({
  client,
  entries,
  items,
}: {
  client: AthenClient;
  /** Raw arc entries — converted via `fromEntries` (the same path `reset` uses). */
  entries?: ArcEntry[];
  /** Pre-built items, if a caller already has them. Takes precedence. */
  items?: ChatItem[];
}) {
  const built = useMemo<ChatItem[]>(() => {
    if (items) return items;
    return entries ? fromEntries(entries, 1).items : [];
  }, [entries, items]);

  if (built.length === 0) {
    return <div className="transcript-empty">No transcript yet.</div>;
  }
  return (
    <div className="transcript">
      <MessageList items={built} cb={{}} client={client} />
    </div>
  );
}
