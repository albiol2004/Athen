// Wake-ups panel: list / enable / delete / create scheduled triggers.
// Wire shapes per wakeup_commands.rs (WakeupView, CreateWakeupReq,
// ScheduleReq tagged enum {kind: one_shot|cron|interval}).

import { useEffect, useState } from 'react';
import type { AthenClient } from '../api/client';

interface WakeupView {
  id: string;
  instruction: string;
  schedule_kind: string;
  schedule_summary: string;
  next_fire_at: string | null;
  last_fired_at: string | null;
  enabled: boolean;
  autonomy: string;
  origin: string;
  profile: string;
}

export function Wakeups({ client, onClose }: { client: AthenClient; onClose: () => void }) {
  const [rows, setRows] = useState<WakeupView[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [showForm, setShowForm] = useState(false);
  const [confirming, setConfirming] = useState<string | null>(null);

  // form state
  const [instruction, setInstruction] = useState('');
  const [kind, setKind] = useState<'one_shot' | 'cron' | 'interval'>('one_shot');
  const [at, setAt] = useState('');
  const [expr, setExpr] = useState('0 9 * * *');
  const [everyMin, setEveryMin] = useState(60);
  const [autonomy, setAutonomy] = useState('safe_only');
  const [saving, setSaving] = useState(false);

  const refresh = () =>
    client
      .get<WakeupView[]>('/wakeups')
      .then((r) => {
        setRows(r);
        setError(null);
      })
      .catch((e) => setError((e as Error).message));
  useEffect(() => {
    void refresh();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [client]);

  const create = async () => {
    setSaving(true);
    setError(null);
    try {
      const schedule =
        kind === 'one_shot'
          ? { kind, at: new Date(at).toISOString() }
          : kind === 'cron'
            ? { kind, expr, tz: Intl.DateTimeFormat().resolvedOptions().timeZone }
            : { kind, every_seconds: Math.max(60, everyMin * 60), anchor: null };
      await client.post('/wakeups', { instruction, schedule, autonomy });
      setShowForm(false);
      setInstruction('');
      await refresh();
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setSaving(false);
    }
  };

  return (
    <div className="drawer wide">
      <div className="drawer-head">
        <h3>Scheduled wake-ups</h3>
        <div className="drawer-head-actions">
          <button className="btn-small" onClick={() => setShowForm((s) => !s)}>
            {showForm ? 'Cancel' : 'New'}
          </button>
          <button className="icon-btn" onClick={onClose}>
            ×
          </button>
        </div>
      </div>
      <div className="drawer-body">
        {error && <div className="drawer-error">{error}</div>}
        {showForm && (
          <div className="wakeup-form">
            <textarea
              rows={2}
              placeholder="What should Athen do when this fires?"
              value={instruction}
              onChange={(e) => setInstruction(e.target.value)}
            />
            <div className="wakeup-kind">
              {(['one_shot', 'cron', 'interval'] as const).map((k) => (
                <label key={k}>
                  <input type="radio" checked={kind === k} onChange={() => setKind(k)} />
                  {k === 'one_shot' ? 'Once' : k === 'cron' ? 'Cron' : 'Interval'}
                </label>
              ))}
            </div>
            {kind === 'one_shot' && (
              <input
                type="text"
                placeholder="2026-06-11 09:00 (local)"
                value={at}
                onChange={(e) => setAt(e.target.value)}
              />
            )}
            {kind === 'cron' && (
              <input type="text" value={expr} onChange={(e) => setExpr(e.target.value)} placeholder="0 9 * * MON-FRI" />
            )}
            {kind === 'interval' && (
              <label className="wakeup-every">
                Every
                <input
                  type="number"
                  min={1}
                  value={everyMin}
                  onChange={(e) => setEveryMin(Number(e.target.value) || 60)}
                />
                minutes
              </label>
            )}
            <select value={autonomy} onChange={(e) => setAutonomy(e.target.value)}>
              <option value="safe_only">Safe only (stops on risky actions)</option>
              <option value="notify_only">Notify only (never acts outward)</option>
              <option value="auto">Auto (only critical pauses)</option>
            </select>
            <button className="btn-primary" disabled={saving || !instruction.trim()} onClick={() => void create()}>
              {saving ? 'Creating…' : 'Create wake-up'}
            </button>
          </div>
        )}
        {rows.length === 0 && !showForm && <div className="drawer-empty">No scheduled tasks yet.</div>}
        {rows.map((w) => (
          <div className="wakeup-row" key={w.id}>
            <label className="wakeup-toggle">
              <input
                type="checkbox"
                checked={w.enabled}
                onChange={(e) =>
                  void client.post(`/wakeups/${w.id}/enabled`, { enabled: e.target.checked }).then(refresh)
                }
              />
            </label>
            <div className="wakeup-main">
              <span className="wakeup-instruction">{w.instruction}</span>
              <span className="wakeup-meta">
                {w.schedule_summary}
                {w.next_fire_at && ` · next ${new Date(w.next_fire_at).toLocaleString()}`}
                {w.origin === 'agent' && ' · created by agent'}
              </span>
            </div>
            {confirming === w.id ? (
              <button
                className="change-revert sure"
                onClick={() => void client.post(`/wakeups/${w.id}/delete`).then(refresh)}
              >
                Sure?
              </button>
            ) : (
              <button
                className="change-revert"
                onClick={() => {
                  setConfirming(w.id);
                  setTimeout(() => setConfirming((c) => (c === w.id ? null : c)), 3000);
                }}
              >
                Delete
              </button>
            )}
          </div>
        ))}
      </div>
    </div>
  );
}
