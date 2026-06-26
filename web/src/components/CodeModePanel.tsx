// Code Mode panel (docs/CODE_MODE.md §8) — an arc-scoped drawer, mirroring
// ChangesRail. Read-only recognition of the real git repo the Code-Mode arc is
// rooted in: repo header (root / branch / ahead·behind / upstream), worktree
// lanes, working-tree dirty files, and recent commits. All git strings are
// rendered as plain text children — React escapes them; we never use
// dangerouslySetInnerHTML on any git output.

import { useCallback, useEffect, useRef, useState } from 'react';
import type { AthenClient } from '../api/client';
import type { AgentNode, ArcEntry, GitRepoState } from '../api/types';
import { ConfirmDialog } from './ConfirmDialog';
import { errMessage, useToast } from './Toast';
import { Transcript } from './Transcript';

const SHORT_HASH = 8;

// What the discard ConfirmDialog is asking about: a single repo-relative file
// (`path`) or ALL working-tree changes (`null`). Stored as panel state so the
// glass ConfirmDialog can render outside the row, matching the rest of the app.
type DiscardTarget = { path: string | null };

function shortHash(hash: string): string {
  return hash.slice(0, SHORT_HASH);
}

export function CodeModePanel({
  client,
  arcId,
  onClose,
}: {
  client: AthenClient;
  arcId: string;
  onClose: () => void;
}) {
  const { toast } = useToast();
  const [state, setState] = useState<GitRepoState | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  // Pending discard confirmation (per-file or all); null = no dialog open.
  const [discarding, setDiscarding] = useState<DiscardTarget | null>(null);
  // True while the discard request is in flight — disables the buttons.
  const [discardBusy, setDiscardBusy] = useState(false);

  const refresh = useCallback(async () => {
    setLoading(true);
    try {
      setState(await client.codeModeGitState(arcId));
      setError(null);
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setLoading(false);
    }
  }, [client, arcId]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  // GitLens-style discard (docs/CODE_MODE.md §6). `path` null = discard ALL.
  // Confirmed in the UI because it destroys uncommitted work. Returns a fresh
  // GitRepoState so we update the panel in one round-trip; errors → Toast.
  const runDiscard = useCallback(
    async (path: string | null) => {
      setDiscardBusy(true);
      try {
        setState(await client.codeModeDiscard(arcId, path));
        setError(null);
      } catch (e) {
        toast(errMessage(e), 'error');
      } finally {
        setDiscardBusy(false);
        setDiscarding(null);
      }
    },
    [client, arcId, toast],
  );

  return (
    <div className="drawer">
      <div className="drawer-head">
        <h3>Code Mode</h3>
        <div className="drawer-head-actions">
          <button className="btn-small" disabled={loading} onClick={() => void refresh()}>
            {loading ? '…' : 'Refresh'}
          </button>
          <button className="icon-btn" onClick={onClose}>
            ×
          </button>
        </div>
      </div>
      <div className="drawer-body">
        {error && <div className="drawer-error">{error}</div>}

        {state && (
          <>
            <div className="cm-header">
              <div className="cm-root" title={state.root}>
                {state.root}
              </div>
              {state.is_repo ? (
                <>
                  <div className="cm-branch-row">
                    <span className="cm-branch">
                      {state.detached
                        ? 'detached'
                        : (state.head_branch ?? 'detached')}
                    </span>
                    {(state.ahead > 0 || state.behind > 0) && (
                      <span className="cm-aheadbehind">
                        {state.ahead > 0 && <span title="commits ahead of upstream">↑{state.ahead}</span>}
                        {state.behind > 0 && <span title="commits behind upstream">↓{state.behind}</span>}
                      </span>
                    )}
                  </div>
                  {state.upstream && (
                    <div className="cm-upstream" title="upstream">
                      → {state.upstream}
                    </div>
                  )}
                </>
              ) : (
                <div className="cm-not-repo">Not a git repository.</div>
              )}
            </div>

            {state.is_repo && (
              <>
                <CmSection title="Worktrees">
                  {state.worktrees.length === 0 ? (
                    <div className="drawer-empty">No worktrees.</div>
                  ) : (
                    state.worktrees.map((w) => (
                      <div className="cm-worktree" key={w.path}>
                        <div className="cm-worktree-main">
                          <span className="cm-worktree-path" title={w.path}>
                            {w.path}
                          </span>
                          <span className="cm-worktree-meta">
                            {w.branch ?? 'detached'} · {shortHash(w.head)}
                          </span>
                        </div>
                        <div className="cm-badges">
                          {w.is_main && <span className="cm-badge main">main</span>}
                          {w.locked && <span className="cm-badge locked">locked</span>}
                        </div>
                      </div>
                    ))
                  )}
                </CmSection>

                <CmSection
                  title="Working tree"
                  headerActions={
                    state.dirty.length > 0 ? (
                      <button
                        type="button"
                        className="cm-discard-all-btn"
                        title="Discard ALL uncommitted changes"
                        disabled={discardBusy}
                        onClick={() => setDiscarding({ path: null })}
                      >
                        Discard all
                      </button>
                    ) : null
                  }
                >
                  {state.dirty.length === 0 ? (
                    <div className="cm-clean">clean</div>
                  ) : (
                    state.dirty.map((f) => (
                      <div className="cm-dirty-row" key={f.path}>
                        <span className="cm-dirty-status">{f.status}</span>
                        <span className="cm-dirty-path" title={f.path}>
                          {f.path}
                        </span>
                        <button
                          type="button"
                          className="cm-discard-btn"
                          title="Discard changes"
                          aria-label={`Discard changes to ${f.path}`}
                          disabled={discardBusy}
                          onClick={() => setDiscarding({ path: f.path })}
                        >
                          ↩
                        </button>
                      </div>
                    ))
                  )}
                </CmSection>

                <CmSection title="Recent commits">
                  {state.recent_commits.length === 0 ? (
                    <div className="drawer-empty">No commits yet.</div>
                  ) : (
                    state.recent_commits.map((c) => (
                      <div className="cm-commit" key={c.hash}>
                        <span className="cm-commit-hash">{shortHash(c.hash)}</span>
                        <span className="cm-commit-subject" title={`${c.author} · ${c.timestamp}`}>
                          {c.subject}
                        </span>
                      </div>
                    ))
                  )}
                </CmSection>
              </>
            )}
          </>
        )}

        <CmAgents client={client} arcId={arcId} />
      </div>

      {discarding &&
        (discarding.path === null ? (
          <ConfirmDialog
            title="Discard all changes?"
            body="Discard ALL uncommitted changes in this repository? This permanently removes every modification and new file. This cannot be undone."
            confirmLabel="Discard all"
            danger
            onConfirm={() => void runDiscard(null)}
            onCancel={() => setDiscarding(null)}
          />
        ) : (
          <ConfirmDialog
            title="Discard changes?"
            // Path rendered as a plain text child by ConfirmDialog (<p>{body}</p>)
            // — React escapes it; no dangerouslySetInnerHTML anywhere.
            body={`Discard changes to ${discarding.path}? This cannot be undone.`}
            confirmLabel="Discard"
            danger
            onConfirm={() => void runDiscard(discarding.path)}
            onCancel={() => setDiscarding(null)}
          />
        ))}
    </div>
  );
}

function CmSection({
  title,
  headerActions,
  children,
}: {
  title: string;
  headerActions?: React.ReactNode;
  children: React.ReactNode;
}) {
  return (
    <div className="cm-section">
      <div className="cm-section-title">
        <span>{title}</span>
        {headerActions}
      </div>
      <div className="cm-section-body">{children}</div>
    </div>
  );
}

const AGENTS_POLL_MS = 2500;

// Order nodes so each main node is immediately followed by its children
// (matched on `parent_arc_id`). The backend already returns this order, but we
// re-derive it client-side so the indentation is robust to ordering changes.
function groupAgents(nodes: AgentNode[]): { node: AgentNode; isChild: boolean }[] {
  const byParent = new Map<string, AgentNode[]>();
  for (const n of nodes) {
    if (n.parent_arc_id) {
      const arr = byParent.get(n.parent_arc_id) ?? [];
      arr.push(n);
      byParent.set(n.parent_arc_id, arr);
    }
  }
  const out: { node: AgentNode; isChild: boolean }[] = [];
  for (const n of nodes) {
    if (n.parent_arc_id) continue; // children are emitted under their parent
    out.push({ node: n, isChild: false });
    for (const child of byParent.get(n.arc_id) ?? []) {
      out.push({ node: child, isChild: true });
    }
  }
  // Any child whose parent isn't in the list (defensive) — append at root.
  for (const n of nodes) {
    if (n.parent_arc_id && !out.some((o) => o.node.arc_id === n.arc_id)) {
      out.push({ node: n, isChild: false });
    }
  }
  return out;
}

/**
 * Agents section (docs/CODE_MODE.md §14, Part 2). Lists the Code-Mode arc's
 * agent tree (main nodes + delegation sub-arcs, indented). Each row expands to
 * the agent's full transcript, rendered by the SAME renderer as the main chat
 * (`<Transcript>` → `<MessageList>`). While any node is running, polls the
 * tree + the expanded-running agent's entries every ~2.5s; the interval is
 * cleared on unmount / arc change (`arcId` dep).
 */
function CmAgents({ client, arcId }: { client: AthenClient; arcId: string }) {
  const [nodes, setNodes] = useState<AgentNode[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [expanded, setExpanded] = useState<string | null>(null);
  const [entries, setEntries] = useState<ArcEntry[] | null>(null);
  // Latest expanded id, so the polling closure reads it without re-subscribing.
  const expandedRef = useRef<string | null>(null);
  expandedRef.current = expanded;

  const loadAgents = useCallback(async () => {
    try {
      const list = await client.codeModeAgents(arcId);
      setNodes(list);
      setError(null);
      return list;
    } catch (e) {
      setError((e as Error).message);
      return null;
    }
  }, [client, arcId]);

  const loadEntries = useCallback(
    async (id: string) => {
      try {
        setEntries(await client.arcEntries(id));
      } catch {
        setEntries([]);
      }
    },
    [client],
  );

  // Initial load + reset when the panel's arc changes.
  useEffect(() => {
    setNodes(null);
    setExpanded(null);
    setEntries(null);
    void loadAgents();
  }, [loadAgents]);

  // Poll while anything is running. Re-fetches the tree and, if the expanded
  // agent is itself running, its growing transcript. Cleared on unmount /
  // arc change (loadAgents / loadEntries are keyed to arcId).
  const anyRunning = (nodes ?? []).some((n) => n.running);
  useEffect(() => {
    if (!anyRunning) return;
    const t = setInterval(() => {
      void (async () => {
        const list = await loadAgents();
        const id = expandedRef.current;
        if (id && list?.some((n) => n.arc_id === id && n.running)) {
          await loadEntries(id);
        }
      })();
    }, AGENTS_POLL_MS);
    return () => clearInterval(t);
  }, [anyRunning, loadAgents, loadEntries]);

  const toggle = (id: string) => {
    if (expanded === id) {
      setExpanded(null);
      setEntries(null);
    } else {
      setExpanded(id);
      setEntries(null);
      void loadEntries(id);
    }
  };

  const rows = nodes ? groupAgents(nodes) : [];

  return (
    <CmSection title="Agents">
      {error && <div className="drawer-error">{error}</div>}
      {nodes === null && !error && <div className="drawer-empty">Loading…</div>}
      {nodes !== null && rows.length === 0 && <div className="drawer-empty">No agents.</div>}
      {rows.map(({ node, isChild }) => (
        <div className={`cm-agent${isChild ? ' child' : ''}`} key={node.arc_id}>
          <button className="cm-agent-head" onClick={() => toggle(node.arc_id)}>
            <span className={`cm-chev${expanded === node.arc_id ? ' open' : ''}`}>›</span>
            <span className="cm-agent-title" title={node.title}>
              {node.title}
            </span>
            {node.running ? (
              <span className="cm-agent-badge running">
                running
                {node.current_tool ? ` · ${node.current_tool}` : ''}
                {node.step_count > 0 ? ` · ${node.step_count} step${node.step_count === 1 ? '' : 's'}` : ''}
              </span>
            ) : (
              <span className="cm-agent-badge done">finished</span>
            )}
          </button>
          {expanded === node.arc_id && (
            <div className="cm-agent-transcript">
              {entries === null ? (
                <div className="transcript-empty">Loading…</div>
              ) : (
                <Transcript client={client} entries={entries} />
              )}
            </div>
          )}
        </div>
      ))}
    </CmSection>
  );
}
