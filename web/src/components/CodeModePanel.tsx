// Code Mode panel (docs/CODE_MODE.md §8) — an arc-scoped drawer, mirroring
// ChangesRail. Read-only recognition of the real git repo the Code-Mode arc is
// rooted in: repo header (root / branch / ahead·behind / upstream), worktree
// lanes, working-tree dirty files, and recent commits. All git strings are
// rendered as plain text children — React escapes them; we never use
// dangerouslySetInnerHTML on any git output.

import { useCallback, useEffect, useState } from 'react';
import type { AthenClient } from '../api/client';
import type { GitRepoState } from '../api/types';

const SHORT_HASH = 8;

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
  const [state, setState] = useState<GitRepoState | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);

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

                <CmSection title="Working tree">
                  {state.dirty.length === 0 ? (
                    <div className="cm-clean">clean</div>
                  ) : (
                    state.dirty.map((f) => (
                      <div className="cm-dirty-row" key={f.path}>
                        <span className="cm-dirty-status">{f.status}</span>
                        <span className="cm-dirty-path" title={f.path}>
                          {f.path}
                        </span>
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
      </div>
    </div>
  );
}

function CmSection({ title, children }: { title: string; children: React.ReactNode }) {
  return (
    <div className="cm-section">
      <div className="cm-section-title">{title}</div>
      <div className="cm-section-body">{children}</div>
    </div>
  );
}
