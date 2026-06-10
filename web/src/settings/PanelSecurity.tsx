// Security panel: global security mode, attachment policy, notification
// preferences, and global directory grants.
//
// Bodies: http_api.rs (SecurityBody, AttachPolicyBody, NotifSettingsBody,
// GrantAddBody). Responses: settings.rs (SettingsResponse,
// AttachmentPolicySettings, NotificationSettingsInfo) and commands.rs
// (DirectoryGrantSummary).

import { useEffect, useState } from 'react';
import type { AthenClient } from '../api/client';
import {
  ConfirmButton,
  ErrorText,
  Field,
  Loading,
  Section,
  useAction,
  useLoad,
} from './shared';

interface SettingsResponse {
  security_mode: string;
}

interface AttachmentPolicy {
  mime_bundles: string[];
  max_attachment_mb: number;
  max_event_mb: number;
  min_inline_trust: string;
  min_download_trust: string;
  byte_ttl_days: number;
}

interface NotificationSettings {
  preferred_channels: string[];
  escalation_timeout_secs: number;
  quiet_hours_enabled: boolean;
  quiet_start_hour: number;
  quiet_start_minute: number;
  quiet_end_hour: number;
  quiet_end_minute: number;
  quiet_allow_critical: boolean;
}

interface DirectoryGrant {
  id: number;
  scope: string;
  arc_id: string | null;
  path: string;
  access: string;
}

const TRUST_LEVELS = ['Unknown', 'Neutral', 'Known', 'Trusted', 'AuthUser'];

export function PanelSecurity({ client }: { client: AthenClient }) {
  return (
    <>
      <SecurityModeSection client={client} />
      <AttachmentsSection client={client} />
      <NotificationsSection client={client} />
      <GrantsSection client={client} />
    </>
  );
}

// ---------------------------------------------------------------------------
// Security mode
// ---------------------------------------------------------------------------

const MODES = [
  {
    id: 'bunker',
    label: 'Bunker',
    desc: 'Maximum caution — every consequential action asks first.',
  },
  {
    id: 'assistant',
    label: 'Assistant',
    desc: 'Balanced — risk-scored gates, asks when impact is high.',
  },
  {
    id: 'yolo',
    label: 'Yolo',
    desc: 'No gates. Every action runs without asking. Use with care.',
  },
];

function SecurityModeSection({ client }: { client: AthenClient }) {
  const settings = useLoad(() => client.get<SettingsResponse>('/settings'), [client]);
  const act = useAction();
  const [confirmYolo, setConfirmYolo] = useState(false);

  const current = settings.data?.security_mode ?? '';

  const apply = async (mode: string) => {
    setConfirmYolo(false);
    const ok = await act.run(() => client.post('/settings/security', { security_mode: mode }));
    if (ok) await settings.reload();
  };

  const pick = (mode: string) => {
    if (mode === 'yolo') {
      setConfirmYolo(true);
      return;
    }
    void apply(mode);
  };

  return (
    <Section title="Security mode" hint="Global gate posture for agent actions. Per-arc overrides win.">
      {settings.loading && <Loading />}
      <ErrorText error={settings.error} />
      <div className="st-list">
        {MODES.map((m) => (
          <div key={m.id} className={`st-item${current === m.id ? ' selected' : ''}`}>
            <div className="st-item-main">
              <div className="st-item-title">
                {m.label}
                {current === m.id && <span className="st-badge coral">active</span>}
              </div>
              <div className="st-item-sub">{m.desc}</div>
            </div>
            <div className="st-item-actions">
              {current !== m.id && (
                <button type="button" className="st-btn small" disabled={act.pending} onClick={() => pick(m.id)}>
                  Use
                </button>
              )}
            </div>
          </div>
        ))}
      </div>
      {confirmYolo && (
        <div className="st-row" style={{ alignItems: 'center' }}>
          <span className="st-error">
            Yolo disables every approval gate — the agent acts without asking. Continue?
          </span>
          <button type="button" className="st-btn st-danger" disabled={act.pending} onClick={() => void apply('yolo')}>
            Yes, enable Yolo
          </button>
          <button type="button" className="st-btn" onClick={() => setConfirmYolo(false)}>
            Cancel
          </button>
        </div>
      )}
      <ErrorText error={act.error} />
    </Section>
  );
}

// ---------------------------------------------------------------------------
// Attachment policy
// ---------------------------------------------------------------------------

const KNOWN_BUNDLES = [
  { id: 'images', label: 'Images' },
  { id: 'pdfs', label: 'PDFs' },
  { id: 'text', label: 'Text' },
  { id: 'office', label: 'Office documents' },
];

function AttachmentsSection({ client }: { client: AthenClient }) {
  const policy = useLoad(() => client.get<AttachmentPolicy>('/settings/attachments'), [client]);
  const act = useAction();
  const [f, setF] = useState<AttachmentPolicy | null>(null);

  useEffect(() => {
    if (policy.data) setF(policy.data);
  }, [policy.data]);

  const save = async () => {
    if (!f) return;
    const ok = await act.run(() =>
      client.post('/settings/attachments', {
        mime_bundles: f.mime_bundles,
        max_attachment_mb: Number(f.max_attachment_mb) || 0,
        max_event_mb: Number(f.max_event_mb) || 0,
        min_inline_trust: f.min_inline_trust,
        min_download_trust: f.min_download_trust,
        byte_ttl_days: Number(f.byte_ttl_days) || 0,
      }),
    );
    if (ok) await policy.reload();
  };

  return (
    <Section title="Attachments" hint="What inbound attachments Athen will accept and store.">
      {policy.loading && <Loading />}
      <ErrorText error={policy.error} />
      {f && (
        <>
          <div className="st-row">
            {KNOWN_BUNDLES.map((b) => (
              <label key={b.id} className="st-check">
                <input
                  type="checkbox"
                  checked={f.mime_bundles.includes(b.id)}
                  onChange={(e) =>
                    setF({
                      ...f,
                      mime_bundles: e.target.checked
                        ? [...f.mime_bundles, b.id]
                        : f.mime_bundles.filter((x) => x !== b.id),
                    })
                  }
                />
                {b.label}
              </label>
            ))}
          </div>
          <div className="st-row">
            <Field label="Max attachment (MB)">
              <input
                type="number"
                value={f.max_attachment_mb}
                onChange={(e) => setF({ ...f, max_attachment_mb: Number(e.target.value) })}
              />
            </Field>
            <Field label="Max per event (MB)">
              <input
                type="number"
                value={f.max_event_mb}
                onChange={(e) => setF({ ...f, max_event_mb: Number(e.target.value) })}
              />
            </Field>
            <Field label="Min trust to inline">
              <select
                value={f.min_inline_trust}
                onChange={(e) => setF({ ...f, min_inline_trust: e.target.value })}
              >
                {TRUST_LEVELS.map((t) => (
                  <option key={t} value={t}>
                    {t}
                  </option>
                ))}
              </select>
            </Field>
            <Field label="Min trust to download">
              <select
                value={f.min_download_trust}
                onChange={(e) => setF({ ...f, min_download_trust: e.target.value })}
              >
                {TRUST_LEVELS.map((t) => (
                  <option key={t} value={t}>
                    {t}
                  </option>
                ))}
              </select>
            </Field>
            <Field label="Keep bytes (days)">
              <input
                type="number"
                value={f.byte_ttl_days}
                onChange={(e) => setF({ ...f, byte_ttl_days: Number(e.target.value) })}
              />
            </Field>
          </div>
          <div className="st-row">
            <button type="button" className="st-btn primary" disabled={act.pending} onClick={() => void save()}>
              Save policy
            </button>
          </div>
        </>
      )}
      <ErrorText error={act.error} />
    </Section>
  );
}

// ---------------------------------------------------------------------------
// Notification preferences
// ---------------------------------------------------------------------------

function NotificationsSection({ client }: { client: AthenClient }) {
  const prefs = useLoad(() => client.get<NotificationSettings>('/settings/notifications'), [client]);
  const act = useAction();
  const [f, setF] = useState<NotificationSettings | null>(null);

  useEffect(() => {
    if (prefs.data) setF(prefs.data);
  }, [prefs.data]);

  const toggleChannel = (ch: string, on: boolean) => {
    if (!f) return;
    setF({
      ...f,
      preferred_channels: on
        ? [...f.preferred_channels.filter((c) => c !== ch), ch]
        : f.preferred_channels.filter((c) => c !== ch),
    });
  };

  const save = async () => {
    if (!f) return;
    const ok = await act.run(() =>
      client.post('/settings/notifications', {
        preferred_channels: f.preferred_channels,
        escalation_timeout_secs: Number(f.escalation_timeout_secs) || 0,
        quiet_hours_enabled: f.quiet_hours_enabled,
        quiet_start_hour: f.quiet_start_hour,
        quiet_start_minute: f.quiet_start_minute,
        quiet_end_hour: f.quiet_end_hour,
        quiet_end_minute: f.quiet_end_minute,
        quiet_allow_critical: f.quiet_allow_critical,
      }),
    );
    if (ok) await prefs.reload();
  };

  return (
    <Section title="Notifications" hint="Where approvals and alerts get delivered, in preference order.">
      {prefs.loading && <Loading />}
      <ErrorText error={prefs.error} />
      {f && (
        <>
          <div className="st-row">
            <label className="st-check">
              <input
                type="checkbox"
                checked={f.preferred_channels.includes('inapp')}
                onChange={(e) => toggleChannel('inapp', e.target.checked)}
              />
              In-app
            </label>
            <label className="st-check">
              <input
                type="checkbox"
                checked={f.preferred_channels.includes('telegram')}
                onChange={(e) => toggleChannel('telegram', e.target.checked)}
              />
              Telegram
            </label>
            <Field label="Escalation timeout (s)">
              <input
                type="number"
                value={f.escalation_timeout_secs}
                onChange={(e) => setF({ ...f, escalation_timeout_secs: Number(e.target.value) })}
              />
            </Field>
          </div>
          <div className="st-row">
            <label className="st-check">
              <input
                type="checkbox"
                checked={f.quiet_hours_enabled}
                onChange={(e) => setF({ ...f, quiet_hours_enabled: e.target.checked })}
              />
              Quiet hours
            </label>
            {f.quiet_hours_enabled && (
              <>
                <Field label="From (hh:mm)">
                  <input
                    type="text"
                    style={{ width: 70 }}
                    value={`${String(f.quiet_start_hour).padStart(2, '0')}:${String(f.quiet_start_minute).padStart(2, '0')}`}
                    onChange={(e) => {
                      const [h, m] = e.target.value.split(':').map(Number);
                      setF({ ...f, quiet_start_hour: h || 0, quiet_start_minute: m || 0 });
                    }}
                  />
                </Field>
                <Field label="To (hh:mm)">
                  <input
                    type="text"
                    style={{ width: 70 }}
                    value={`${String(f.quiet_end_hour).padStart(2, '0')}:${String(f.quiet_end_minute).padStart(2, '0')}`}
                    onChange={(e) => {
                      const [h, m] = e.target.value.split(':').map(Number);
                      setF({ ...f, quiet_end_hour: h || 0, quiet_end_minute: m || 0 });
                    }}
                  />
                </Field>
                <label className="st-check">
                  <input
                    type="checkbox"
                    checked={f.quiet_allow_critical}
                    onChange={(e) => setF({ ...f, quiet_allow_critical: e.target.checked })}
                  />
                  Critical alerts break through
                </label>
              </>
            )}
          </div>
          <div className="st-row">
            <button type="button" className="st-btn primary" disabled={act.pending} onClick={() => void save()}>
              Save notifications
            </button>
          </div>
        </>
      )}
      <ErrorText error={act.error} />
    </Section>
  );
}

// ---------------------------------------------------------------------------
// Global directory grants
// ---------------------------------------------------------------------------

function GrantsSection({ client }: { client: AthenClient }) {
  const grants = useLoad(() => client.get<DirectoryGrant[]>('/grants/global'), [client]);
  const act = useAction();
  const [path, setPath] = useState('');
  const [access, setAccess] = useState<'read' | 'write'>('read');

  const add = async () => {
    const p = path.trim();
    if (!p) return;
    const ok = await act.run(() => client.post('/grants/global', { path: p, access }));
    if (ok) {
      setPath('');
      await grants.reload();
    }
  };

  return (
    <Section
      title="Directory permissions"
      hint="Global grants — directories the agent may always touch without asking."
    >
      {grants.loading && <Loading />}
      <ErrorText error={grants.error} />
      {!grants.loading && (grants.data ?? []).length === 0 && (
        <div className="st-dim">No global grants.</div>
      )}
      <div className="st-list">
        {(grants.data ?? []).map((g) => (
          <div key={g.id} className="st-item">
            <div className="st-item-main">
              <div className="st-item-title st-mono" style={{ fontWeight: 500 }}>
                {g.path}
              </div>
            </div>
            <span className={`st-badge ${g.access === 'write' ? 'amber' : ''}`}>{g.access}</span>
            <ConfirmButton
              label="Revoke"
              className="small"
              onConfirm={() =>
                void act
                  .run(() => client.post(`/grants/global/${g.id}/revoke`))
                  .then((ok) => { if (ok) void grants.reload(); })
              }
            />
          </div>
        ))}
      </div>
      <div className="st-row">
        <Field label="Path" grow>
          <input
            type="text"
            className="st-mono"
            value={path}
            placeholder="/home/me/projects"
            onChange={(e) => setPath(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === 'Enter') void add();
            }}
          />
        </Field>
        <Field label="Access">
          <select value={access} onChange={(e) => setAccess(e.target.value as 'read' | 'write')}>
            <option value="read">read</option>
            <option value="write">write</option>
          </select>
        </Field>
        <button type="button" className="st-btn primary" disabled={act.pending || !path.trim()} onClick={() => void add()}>
          Grant
        </button>
      </div>
      <ErrorText error={act.error} />
    </Section>
  );
}
