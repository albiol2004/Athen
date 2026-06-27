// Tool execution cards: expandable bodies (args / result / diffs),
// collapsible groups, and delegation sub-arc inline expansion. Visual
// + behavioral port of the desktop's buildToolCardBlock / tool groups.

import { memo, useState } from 'react';
import type { AthenClient } from '../api/client';
import type { ChatItem } from '../chat/reducer';
import { Markdown } from './Markdown';
import { toolIconSvg } from './toolIcons';
import { endpointIcon } from '../api/endpointIcons';

type ToolItem = Extract<ChatItem, { kind: 'tool' }>;

// Placeholder strings the backend persists when a sub-agent produced no
// usable text — never worth rendering as a "final message".
const FINAL_PLACEHOLDERS = new Set([
  '(specialist returned no text)',
  '(specialist task failed without a response)',
]);

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

/**
 * Delegation expansion (docs/CODE_MODE.md §14, Part 1 — both normal + Code
 * mode). Shows the sub-agent's FINAL message (read straight from the tool
 * result `content` — no fetch) plus, collapsed by default, the ordered list of
 * tools the sub-agent used (lazy-loaded from the sub-arc's `tool_call` entries).
 */
function DelegationSteps({ it, client }: { it: ToolItem; client: AthenClient }) {
  const [steps, setSteps] = useState<ToolItem[] | null>(null);
  const [loading, setLoading] = useState(false);
  const [toolsOpen, setToolsOpen] = useState(false);
  const result = asRecord(it.result);
  const subArcId = str(result?.sub_arc_id) ?? str(result?.arc_id);

  // Final message comes from the tool result `content` — already present, no
  // fetch. Skip empty / placeholder text (the backend's "no text" sentinels).
  const content = (str(result?.content) ?? '').trim();
  const hasFinal = content.length > 0 && !FINAL_PLACEHOLDERS.has(content);
  const verified = result?.verified;
  const verifyNote = str(result?.verification_note);
  const showVerifyWarning = verified === false && Boolean(verifyNote);

  if (!subArcId && !hasFinal && !showVerifyWarning) return null;

  const load = async () => {
    if (!subArcId) return;
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

  const toggleTools = () => {
    const next = !toolsOpen;
    setToolsOpen(next);
    if (next && steps === null && !loading) void load();
  };

  return (
    <div className="tc-delegation">
      {hasFinal && (
        <div className="tc-final">
          <div className="tc-section-label">Final message</div>
          <div className="tc-final-body">
            <Markdown text={content} />
          </div>
        </div>
      )}
      {showVerifyWarning && (
        <div className="tc-verify-warning" title="Sub-agent output failed verification">
          {verifyNote}
        </div>
      )}
      {subArcId && (
        <div className="tc-tools-used">
          <button className="tc-sub-btn" onClick={toggleTools}>
            {loading
              ? 'Loading…'
              : steps
                ? `Tools used (${steps.length})`
                : 'Tools used'}
            <span className={`tc-chev${toolsOpen ? ' open' : ''}`}>›</span>
          </button>
          {toolsOpen && steps && (
            <div className="tc-substeps">
              {steps.length === 0 && (
                <div className="tc-section-label">No recorded sub-agent steps.</div>
              )}
              {steps.map((s) => (
                <ToolCard key={s.key} it={s} client={client} />
              ))}
            </div>
          )}
        </div>
      )}
    </div>
  );
}

// memo'd: a completed tool item keeps its object reference across renders
// (reducer.patch only replaces the one item that changed), so finished
// cards skip re-render while a later tool/message streams. `client` is a
// stable singleton passed straight down from Shell.
export const ToolCard = memo(function ToolCard({ it, client }: { it: ToolItem; client: AthenClient }) {
  const [open, setOpen] = useState(false);
  const done =
    it.status === 'Completed' || it.status === 'completed' || it.status === 'done';
  const running = !done && !it.failed;
  const expandable =
    it.args !== undefined || it.result !== undefined || Boolean(it.error) || it.name === 'delegate_to_agent';
  // For a registered Cloud API call, prefer the provider's cached logo as
  // the marker so the card is recognizable at a glance.
  const epIcon =
    it.name === 'http_request'
      ? endpointIcon((asRecord(it.args)?.endpoint as string | undefined) ?? null)
      : null;
  const markClass = `tc-mark${running ? ' running' : ''}${done ? ' done' : ''}${it.failed ? ' fail' : ''}`;
  return (
    <div className={`tc${it.failed ? ' failed' : ''}${running ? ' running' : ''}`}>
      <button
        className={`tc-head${running ? ' running' : ''}`}
        onClick={() => expandable && setOpen((o) => !o)}
        title={it.detail || undefined}
      >
        {epIcon ? (
          <span className={markClass}>
            <img className="tc-mark-img" src={epIcon} alt="" />
          </span>
        ) : (
          <span className={markClass} dangerouslySetInnerHTML={{ __html: toolIconSvg(it.name) }} />
        )}
        <span className="tc-name">{it.name}</span>
        <span className="tc-detail">{!done && !it.failed ? it.status : it.detail || ''}</span>
        {expandable && <span className={`tc-chev${open ? ' open' : ''}`}>›</span>}
      </button>
      {open && <ToolBody it={it} client={client} />}
    </div>
  );
});

/** Collapsible container for consecutive tool calls (one agent segment). */
export const ToolGroup = memo(function ToolGroup({ items, client }: { items: ToolItem[]; client: AthenClient }) {
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
});
