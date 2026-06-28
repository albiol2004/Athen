// Right-side "Inspector" drawer — a single wide drawer that hosts both the
// Deep Research paper (previously a centered modal) and the full Read / Write /
// Edit / Delete tool detail (content + diff). Mirrors the desktop inspector.
// Content is opened contextually via InspectorContext (see useInspector), so
// callers deep in the chat tree don't need props threaded down.

import { createContext, useContext, useEffect } from 'react';
import { Markdown } from './Markdown';
import { ToolDetailBody, type ToolItem } from './toolcards';

export type InspectorContent =
  | { kind: 'paper'; title: string; markdown: string }
  | { kind: 'tool'; tool: ToolItem };

export const InspectorContext = createContext<{ open: (c: InspectorContent) => void } | null>(null);

export function useInspector() {
  return useContext(InspectorContext);
}

/** Last path segment of a (possibly windows) path; '' when unusable. */
function basename(path: unknown): string {
  if (typeof path !== 'string') return '';
  const parts = path.split(/[\\/]/).filter(Boolean);
  return parts[parts.length - 1] ?? '';
}

function toolTitle(tool: ToolItem): string {
  const args = tool.args && typeof tool.args === 'object' ? (tool.args as Record<string, unknown>) : null;
  const base = basename(args?.path);
  return base ? `${tool.name} · ${base}` : tool.name;
}

export function InspectorPanel({
  content,
  onClose,
}: {
  content: InspectorContent | null;
  onClose: () => void;
}) {
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') onClose();
    };
    document.addEventListener('keydown', onKey);
    return () => document.removeEventListener('keydown', onKey);
  }, [onClose]);

  const title = content
    ? content.kind === 'paper'
      ? content.title
      : toolTitle(content.tool)
    : 'Inspector';

  return (
    <aside className="drawer wide inspector-drawer">
      <div className="drawer-head">
        <h3 title={title}>{title}</h3>
        <div className="drawer-head-actions">
          <button className="icon-btn" aria-label="Close" onClick={onClose}>
            ×
          </button>
        </div>
      </div>
      <div className="drawer-body">
        {content?.kind === 'paper' && <Markdown text={content.markdown} />}
        {content?.kind === 'tool' && <ToolDetailBody tool={content.tool} />}
      </div>
    </aside>
  );
}
