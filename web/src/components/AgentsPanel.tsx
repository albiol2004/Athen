// Active-agents watch panel: every running executor (user-driven,
// sense-driven, wake-up, delegation) with a cancel button each.

import { useEffect, useState } from 'react';
import type { AthenClient } from '../api/client';

interface ActiveAgent {
  task_id: string;
  arc_id: string | null;
  source: unknown;
  title: string;
  started_at: string;
  current_tool?: string | null;
  current_action?: string | null;
}

function ago(iso: string): string {
  const s = Math.max(0, (Date.now() - Date.parse(iso)) / 1000);
  if (s < 60) return `${Math.floor(s)}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m`;
  return `${Math.floor(s / 3600)}h`;
}

export function AgentsPanel({
  client,
  onClose,
  onOpenArc,
}: {
  client: AthenClient;
  onClose: () => void;
  onOpenArc: (arcId: string) => void;
}) {
  const [agents, setAgents] = useState<ActiveAgent[]>([]);
  const [error, setError] = useState<string | null>(null);

  const refresh = async () => {
    try {
      setAgents(await client.get<ActiveAgent[]>('/agents'));
      setError(null);
    } catch (e) {
      setError((e as Error).message);
    }
  };
  useEffect(() => {
    void refresh();
    const t = setInterval(() => void refresh(), 3000);
    return () => clearInterval(t);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [client]);

  return (
    <div className="drawer">
      <div className="drawer-head">
        <h3>Active agents</h3>
        <button className="icon-btn" onClick={onClose}>
          ×
        </button>
      </div>
      <div className="drawer-body">
        {error && <div className="drawer-error">{error}</div>}
        {agents.length === 0 && !error && <div className="drawer-empty">Nothing running.</div>}
        {agents.map((a) => (
          <div className="agent-row" key={a.task_id}>
            <div className="agent-main">
              <button
                className="agent-title"
                onClick={() => a.arc_id && onOpenArc(a.arc_id)}
                title={a.arc_id ? 'Open arc' : undefined}
              >
                {a.title || a.task_id.slice(0, 8)}
              </button>
              <span className="agent-meta">
                {ago(a.started_at)} ago{a.current_tool ? ` · ${a.current_tool}` : ''}
              </span>
            </div>
            <button
              className="agent-cancel"
              onClick={() => void client.post(`/agents/${a.task_id}/cancel`).then(refresh)}
            >
              Stop
            </button>
          </div>
        ))}
      </div>
    </div>
  );
}
