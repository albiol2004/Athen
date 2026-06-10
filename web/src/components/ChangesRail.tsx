// Changes rail: file-mutating actions in the current arc, newest first,
// with point-in-time revert. Revert always cascades newest→clicked via
// the rewind endpoint (never reverts one action in isolation).

import { useEffect, useState } from 'react';
import type { AthenClient } from '../api/client';

interface ActionRecord {
  entry_id: string;
  turn_id: string | null;
  tool_name: string;
  args_summary: string;
  created_at: string;
  paths?: string[];
  [k: string]: unknown;
}

function ago(iso: string): string {
  const s = Math.max(0, (Date.now() - Date.parse(iso)) / 1000);
  if (s < 60) return 'now';
  if (s < 3600) return `${Math.floor(s / 60)}m ago`;
  if (s < 86400) return `${Math.floor(s / 3600)}h ago`;
  return `${Math.floor(s / 86400)}d ago`;
}

export function ChangesRail({
  client,
  arcId,
  refreshKey,
  onClose,
  onReverted,
}: {
  client: AthenClient;
  arcId: string;
  /** Bump to re-fetch (Shell bumps on completed edit/write events). */
  refreshKey: number;
  onClose: () => void;
  onReverted: () => void;
}) {
  const [actions, setActions] = useState<ActionRecord[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [confirming, setConfirming] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    client
      .get<ActionRecord[]>(`/arcs/${encodeURIComponent(arcId)}/snapshots`)
      .then((a) => {
        setActions([...a].reverse());
        setError(null);
      })
      .catch((e) => setError((e as Error).message));
  }, [client, arcId, refreshKey]);

  const revert = async (entryId: string) => {
    setBusy(true);
    try {
      await client.post(`/arcs/${encodeURIComponent(arcId)}/rewind`, { action_id: entryId });
      setConfirming(null);
      onReverted();
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="drawer">
      <div className="drawer-head">
        <h3>Changes</h3>
        <button className="icon-btn" onClick={onClose}>
          ×
        </button>
      </div>
      <div className="drawer-body">
        {error && <div className="drawer-error">{error}</div>}
        {actions.length === 0 && !error && <div className="drawer-empty">No file changes yet.</div>}
        {actions.map((a) => (
          <div className="change-row" key={a.entry_id}>
            <div className="change-main">
              <span className="change-tool">{a.tool_name}</span>
              <span className="change-summary" title={a.args_summary}>
                {(a.paths && a.paths[0]) || a.args_summary}
              </span>
              <span className="change-when">{ago(a.created_at)}</span>
            </div>
            {confirming === a.entry_id ? (
              <button className="change-revert sure" disabled={busy} onClick={() => void revert(a.entry_id)}>
                {busy ? '…' : 'Sure?'}
              </button>
            ) : (
              <button
                className="change-revert"
                title="Revert to before this action (everything newer is rolled back too)"
                onClick={() => {
                  setConfirming(a.entry_id);
                  setTimeout(() => setConfirming((c) => (c === a.entry_id ? null : c)), 3000);
                }}
              >
                Revert
              </button>
            )}
          </div>
        ))}
      </div>
    </div>
  );
}
