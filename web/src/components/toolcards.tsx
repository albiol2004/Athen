// Tool execution cards: expandable bodies (args / result / diffs),
// collapsible groups, and delegation sub-arc inline expansion. Visual
// + behavioral port of the desktop's buildToolCardBlock / tool groups.

import { useState } from 'react';
import type { AthenClient } from '../api/client';
import type { ChatItem } from '../chat/reducer';

type ToolItem = Extract<ChatItem, { kind: 'tool' }>;

function asRecord(v: unknown): Record<string, unknown> | null {
  return v && typeof v === 'object' && !Array.isArray(v) ? (v as Record<string, unknown>) : null;
}
function str(v: unknown): string | null {
  return typeof v === 'string' ? v : null;
}

/** Compact JSON for unknown payloads; long values get a scroll box. */
function JsonBlock({ label, value }: { label: string; value: unknown }) {
  if (value === undefined || value === null) return null;
  let text: string;
  if (typeof value === 'string') text = value;
  else {
    try {
      text = JSON.stringify(value, null, 2);
    } catch {
      text = String(value);
    }
  }
  if (!text.trim()) return null;
  return (
    <div className="tc-section">
      <div className="tc-section-label">{label}</div>
      <pre className="tc-pre">{text.length > 20000 ? `${text.slice(0, 20000)}\n… (truncated)` : text}</pre>
    </div>
  );
}

/** Naive before/after view for `edit` (old_string → new_string). */
function EditBody({ args }: { args: Record<string, unknown> }) {
  const prefix = (s: string, p: string) =>
    s
      .split('\n')
      .map((l) => p + l)
      .join('\n');
  const oldS = str(args.old_string) ?? '';
  const newS = str(args.new_string) ?? '';
  return (
    <>
      {str(args.path) && <div className="tc-path">{str(args.path)}</div>}
      <div className="tc-diff">
        {oldS && <pre className="tc-pre del">{prefix(oldS, '- ')}</pre>}
        {newS && <pre className="tc-pre add">{prefix(newS, '+ ')}</pre>}
      </div>
    </>
  );
}

function ToolBody({ it, client }: { it: ToolItem; client: AthenClient }) {
  const args = asRecord(it.args);
  const result = asRecord(it.result);

  // Special bodies for the common file/shell tools; everything else
  // falls through to the generic args+result renderer.
  let special: React.ReactNode = null;
  if (it.name === 'edit' && args && (args.old_string || args.new_string)) {
    special = <EditBody args={args} />;
  } else if ((it.name === 'write' || it.name === 'read') && args) {
    const content = str(args.content) ?? str(result?.content) ?? null;
    special = (
      <>
        {str(args.path) && <div className="tc-path">{str(args.path)}</div>}
        {content && <JsonBlock label={it.name === 'write' ? 'content' : 'file'} value={content} />}
      </>
    );
  } else if ((it.name === 'shell_execute' || it.name === 'shell') && args) {
    const out = str(result?.stdout) ?? str(result?.output) ?? null;
    special = (
      <>
        {str(args.command) && <pre className="tc-pre cmd">$ {str(args.command)}</pre>}
        {out && <JsonBlock label="output" value={out} />}
      </>
    );
  }

  return (
    <div className="tc-body">
      {it.error && <div className="tc-error">{it.error}</div>}
      {special ?? (
        <>
          <JsonBlock label="args" value={it.args} />
          <JsonBlock label="result" value={it.result} />
        </>
      )}
      {it.name === 'delegate_to_agent' && <DelegationSteps it={it} client={client} />}
    </div>
  );
}

/** "Show sub-agent steps" — loads the sub-arc's tool_call entries. */
function DelegationSteps({ it, client }: { it: ToolItem; client: AthenClient }) {
  const [steps, setSteps] = useState<ToolItem[] | null>(null);
  const [loading, setLoading] = useState(false);
  const result = asRecord(it.result);
  const subArcId = str(result?.sub_arc_id) ?? str(result?.arc_id);
  if (!subArcId) return null;

  const load = async () => {
    setLoading(true);
    try {
      const entries = await client.arcEntries(subArcId);
      const out: ToolItem[] = [];
      let id = 1;
      for (const e of entries) {
        if (e.entry_type !== 'tool_call') continue;
        let meta: Record<string, unknown> = {};
        const raw = e.metadata as unknown;
        if (raw && typeof raw === 'object') meta = raw as Record<string, unknown>;
        else if (typeof raw === 'string') {
          try {
            meta = JSON.parse(raw) as Record<string, unknown>;
          } catch {
            /* ignore */
          }
        }
        const status = str(meta.status) ?? 'Completed';
        out.push({
          kind: 'tool',
          id: id++,
          key: `sub-${e.id}`,
          name: str(meta.tool) ?? e.content ?? 'tool',
          status,
          detail: str(meta.summary) ?? e.content,
          args: meta.args,
          result: meta.result,
          error: str(meta.error),
          failed: status === 'Failed' || Boolean(meta.error),
        });
      }
      setSteps(out);
    } catch {
      setSteps([]);
    } finally {
      setLoading(false);
    }
  };

  if (steps) {
    return (
      <div className="tc-substeps">
        {steps.length === 0 && <div className="tc-section-label">No recorded sub-agent steps.</div>}
        {steps.map((s) => (
          <ToolCard key={s.key} it={s} client={client} />
        ))}
      </div>
    );
  }
  return (
    <button className="tc-sub-btn" disabled={loading} onClick={() => void load()}>
      {loading ? 'Loading…' : 'Show sub-agent steps'}
    </button>
  );
}

export function ToolCard({ it, client }: { it: ToolItem; client: AthenClient }) {
  const [open, setOpen] = useState(false);
  const done =
    it.status === 'Completed' || it.status === 'completed' || it.status === 'done';
  const expandable =
    it.args !== undefined || it.result !== undefined || Boolean(it.error) || it.name === 'delegate_to_agent';
  return (
    <div className={`tc${it.failed ? ' failed' : ''}`}>
      <button
        className="tc-head"
        onClick={() => expandable && setOpen((o) => !o)}
        title={it.detail || undefined}
      >
        <span className={`dot${done ? ' done' : ''}${it.failed ? ' fail' : ''}`} />
        <span className="tc-name">{it.name}</span>
        <span className="tc-detail">{!done && !it.failed ? it.status : it.detail || ''}</span>
        {expandable && <span className={`tc-chev${open ? ' open' : ''}`}>›</span>}
      </button>
      {open && <ToolBody it={it} client={client} />}
    </div>
  );
}

/** Collapsible container for consecutive tool calls (one agent segment). */
export function ToolGroup({ items, client }: { items: ToolItem[]; client: AthenClient }) {
  // Live groups (still receiving events) render open; rehydrated history
  // collapses to a one-line strip, same as the desktop.
  const live = items.some((t) => t.live);
  return (
    <details className="tool-group" open={live ? true : undefined}>
      <summary>
        <span className="tg-count">
          {items.length} tool{items.length === 1 ? '' : 's'}
        </span>
        <span className="tg-names">
          {[...new Set(items.map((t) => t.name))].slice(0, 6).join(' · ')}
        </span>
        {items.some((t) => t.failed) && <span className="tg-failed">failed</span>}
      </summary>
      <div className="tg-body">
        {items.map((t) => (
          <ToolCard key={t.key} it={t} client={client} />
        ))}
      </div>
    </details>
  );
}
