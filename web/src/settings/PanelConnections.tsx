// Connections panel: owner contact, email (IMAP/SMTP) with autodetect,
// Telegram, GitHub identities, CalDAV calendar sources, web search keys
// and registered Cloud API endpoints.
//
// Request bodies: http_api.rs (EmailSaveBody, SmtpSaveBody,
// TelegramSaveBody, GithubSaveBody, CalDavAddBody, WebSearchSaveBody)
// and commands.rs (EndpointInput). Responses: settings.rs
// (SettingsResponse), settings_calendar.rs (camelCase views),
// contacts.rs (ContactView).

import { useEffect, useState } from 'react';
import type { AthenClient } from '../api/client';
import {
  ConfirmButton,
  ErrorText,
  Field,
  Loading,
  Section,
  TestBadge,
  useAction,
  useLoad,
  type TestResult,
} from './shared';

// ---- wire shapes ----

interface EmailInfo {
  enabled: boolean;
  imap_server: string;
  imap_port: number;
  username: string;
  has_password: boolean;
  use_tls: boolean;
  folders: string;
  poll_interval_secs: number;
  lookback_hours: number;
  smtp_server: string;
  smtp_port: number;
  smtp_username: string;
  has_smtp_password: boolean;
  smtp_use_tls: boolean;
  from_address: string;
}

interface TelegramInfo {
  enabled: boolean;
  has_bot_token: boolean;
  bot_token: string;
  allowed_chat_ids: number[];
  poll_interval_secs: number;
}

interface WebSearchInfo {
  brave_configured: boolean;
  brave_hint: string;
  tavily_configured: boolean;
  tavily_hint: string;
}

interface SettingsResponse {
  email: EmailInfo;
  telegram: TelegramInfo;
  web_search: WebSearchInfo;
}

interface ServerHint {
  host: string;
  port: number;
  security: 'ssl' | 'start_tls' | 'none';
}
interface ProviderHint {
  display_name: string;
  incoming: ServerHint;
  outgoing: ServerHint;
  app_password_url?: string | null;
  notes?: string | null;
}

interface ContactView {
  id: string;
  name: string;
  identifiers: { kind: string; value: string }[];
}

interface GithubIdentitySettings {
  has_token: boolean;
  user_name: string;
  user_email: string;
}
interface GithubSnapshot {
  bot: GithubIdentitySettings;
  user: GithubIdentitySettings;
}

interface CalendarSourceView {
  id: string;
  kind: string;
  displayName: string;
  baseUrl: string;
  username: string;
  enabled: boolean;
  selectedCalendars: string[];
  lastSyncAt: string | null;
  lastSyncError: string | null;
}
interface RemoteCalendarView {
  id: string;
  name: string;
  readOnly: boolean;
}
interface SyncResult {
  success: boolean;
  message: string;
}

type AuthMethod =
  | 'None'
  | 'BearerToken'
  | { Header: { name: string } }
  | { HeaderPrefixed: { name: string; prefix: string } }
  | { QueryParam: { name: string } }
  | { BasicAuth: { user: string } };

interface EndpointWire {
  id: string;
  name: string;
  provider: string;
  base_url: string;
  enabled: boolean;
  auth_method: AuthMethod;
  rate_limit_per_minute: number;
  notes: string | null;
  has_credential: boolean;
}

interface EndpointPreset {
  slug: string;
  label: string;
  provider: string;
  base_url: string;
  auth_method: AuthMethod;
  suggested_risk: string | null;
  default_rate_limit_per_minute: number;
  free_tier_blurb: string;
  signup_url: string;
  test_path: string;
}

export function PanelConnections({ client }: { client: AthenClient }) {
  const settings = useLoad(() => client.get<SettingsResponse>('/settings'), [client]);
  return (
    <>
      <OwnerSection client={client} />
      <EmailSection client={client} info={settings.data?.email ?? null} reload={settings.reload} />
      <TelegramSection client={client} info={settings.data?.telegram ?? null} reload={settings.reload} />
      <GithubSection client={client} />
      <CalendarSection client={client} />
      <WebSearchSection client={client} info={settings.data?.web_search ?? null} reload={settings.reload} />
      <EndpointsSection client={client} />
      <ErrorText error={settings.error} />
    </>
  );
}

// ---------------------------------------------------------------------------
// Owner contact
// ---------------------------------------------------------------------------

const IDENT_KINDS = ['email', 'phone', 'telegram_user', 'whatsapp', 'other'];

function OwnerSection({ client }: { client: AthenClient }) {
  const owner = useLoad(() => client.get<ContactView | null>('/contacts/owner'), [client]);
  const act = useAction();
  const [name, setName] = useState('');
  const [idents, setIdents] = useState<{ kind: string; value: string }[]>([]);

  useEffect(() => {
    if (owner.data) {
      setName(owner.data.name);
      setIdents(owner.data.identifiers);
    } else {
      setName('');
      setIdents([]);
    }
  }, [owner.data]);

  const save = async () => {
    const ok = await act.run(() =>
      client.post('/contacts/owner', {
        name: name.trim(),
        identifiers: idents.filter((i) => i.value.trim()).map((i) => ({ kind: i.kind, value: i.value.trim() })),
      }),
    );
    if (ok) await owner.reload();
  };

  return (
    <Section
      title="Owner"
      hint="Who Athen works for. Identifiers route inbound email / Telegram to the owner with full trust."
    >
      {owner.loading && <Loading />}
      <ErrorText error={owner.error} />
      <div className="st-row">
        <Field label="Name" grow>
          <input type="text" value={name} onChange={(e) => setName(e.target.value)} />
        </Field>
      </div>
      {idents.map((ident, i) => (
        <div key={i} className="st-row">
          <Field label="Kind">
            <select
              value={ident.kind}
              onChange={(e) =>
                setIdents(idents.map((x, j) => (j === i ? { ...x, kind: e.target.value } : x)))
              }
            >
              {IDENT_KINDS.map((k) => (
                <option key={k} value={k}>
                  {k}
                </option>
              ))}
            </select>
          </Field>
          <Field label="Value" grow>
            <input
              type="text"
              value={ident.value}
              onChange={(e) =>
                setIdents(idents.map((x, j) => (j === i ? { ...x, value: e.target.value } : x)))
              }
            />
          </Field>
          <button type="button" className="st-btn small" onClick={() => setIdents(idents.filter((_, j) => j !== i))}>
            Remove
          </button>
        </div>
      ))}
      <div className="st-row">
        <button
          type="button"
          className="st-btn small"
          onClick={() => setIdents([...idents, { kind: 'email', value: '' }])}
        >
          + Identifier
        </button>
        <button type="button" className="st-btn primary" disabled={act.pending || !name.trim()} onClick={() => void save()}>
          Save owner
        </button>
        {owner.data && (
          <ConfirmButton
            label="Clear owner"
            onConfirm={() =>
              void act.run(() => client.post('/contacts/owner/clear')).then((ok) => { if (ok) void owner.reload(); })
            }
          />
        )}
      </div>
      <ErrorText error={act.error} />
    </Section>
  );
}

// ---------------------------------------------------------------------------
// Email (IMAP + SMTP)
// ---------------------------------------------------------------------------

function EmailSection({
  client,
  info,
  reload,
}: {
  client: AthenClient;
  info: EmailInfo | null;
  reload: () => Promise<void>;
}) {
  const act = useAction();
  const [test, setTest] = useState<TestResult | null>(null);
  const [smtpTest, setSmtpTest] = useState<TestResult | null>(null);
  const [hint, setHint] = useState<ProviderHint | null>(null);
  const [f, setF] = useState({
    enabled: false,
    username: '',
    password: '',
    imap_server: '',
    imap_port: '993',
    use_tls: true,
    folders: 'INBOX',
    poll_interval_secs: '120',
    lookback_hours: '24',
    smtp_server: '',
    smtp_port: '587',
    smtp_username: '',
    smtp_password: '',
    smtp_use_tls: true,
    from_address: '',
  });

  useEffect(() => {
    if (!info) return;
    setF((p) => ({
      ...p,
      enabled: info.enabled,
      username: info.username,
      imap_server: info.imap_server,
      imap_port: String(info.imap_port || 993),
      use_tls: info.use_tls,
      folders: info.folders || 'INBOX',
      poll_interval_secs: String(info.poll_interval_secs || 120),
      lookback_hours: String(info.lookback_hours || 24),
      smtp_server: info.smtp_server,
      smtp_port: String(info.smtp_port || 587),
      smtp_username: info.smtp_username,
      smtp_use_tls: info.smtp_use_tls,
      from_address: info.from_address,
    }));
  }, [info]);

  const detect = async () => {
    setHint(null);
    await act.run(async () => {
      const h = await client.post<ProviderHint | null>('/settings/email/detect', {
        email: f.username.trim(),
      });
      if (!h) throw new Error('No provider match — fill the server fields manually.');
      setHint(h);
      setF((p) => ({
        ...p,
        imap_server: h.incoming.host,
        imap_port: String(h.incoming.port),
        use_tls: h.incoming.security !== 'none',
        smtp_server: h.outgoing.host,
        smtp_port: String(h.outgoing.port),
        smtp_use_tls: h.outgoing.security !== 'none',
        smtp_username: p.smtp_username || p.username,
        from_address: p.from_address || p.username,
      }));
    });
  };

  const saveImap = async () => {
    const ok = await act.run(() =>
      client.post('/settings/email', {
        enabled: f.enabled,
        imap_server: f.imap_server.trim(),
        imap_port: Number(f.imap_port) || 993,
        username: f.username.trim(),
        password: f.password ? f.password : undefined,
        use_tls: f.use_tls,
        folders: f.folders,
        poll_interval_secs: Number(f.poll_interval_secs) || 120,
        lookback_hours: Number(f.lookback_hours) || 24,
      }),
    );
    if (ok) await reload();
  };

  const testImap = async () => {
    setTest(null);
    await act.run(async () => {
      const r = await client.post<TestResult>('/settings/email/test', {
        imap_server: f.imap_server.trim(),
        imap_port: Number(f.imap_port) || 993,
        username: f.username.trim(),
        password: f.password,
        use_tls: f.use_tls,
      });
      setTest(r);
    });
  };

  const saveSmtp = async () => {
    const ok = await act.run(() =>
      client.post('/settings/smtp', {
        smtp_server: f.smtp_server.trim(),
        smtp_port: Number(f.smtp_port) || 587,
        smtp_username: f.smtp_username.trim(),
        smtp_password: f.smtp_password ? f.smtp_password : undefined,
        smtp_use_tls: f.smtp_use_tls,
        from_address: f.from_address.trim(),
      }),
    );
    if (ok) await reload();
  };

  const testSmtp = async () => {
    setSmtpTest(null);
    await act.run(async () => {
      const r = await client.post<TestResult>('/settings/smtp/test', {
        smtp_server: f.smtp_server.trim(),
        smtp_port: Number(f.smtp_port) || 587,
        smtp_username: f.smtp_username.trim(),
        smtp_password: f.smtp_password,
        smtp_use_tls: f.smtp_use_tls,
        from_address: f.from_address.trim(),
      });
      setSmtpTest(r);
    });
  };

  return (
    <Section title="Email" hint="IMAP monitoring + SMTP sending.">
      {!info && <Loading />}
      <div className="st-row">
        <label className="st-check">
          <input type="checkbox" checked={f.enabled} onChange={(e) => setF({ ...f, enabled: e.target.checked })} />
          Monitor inbox
        </label>
      </div>
      <div className="st-row">
        <Field label="Email address / username" grow>
          <input type="email" value={f.username} onChange={(e) => setF({ ...f, username: e.target.value })} />
        </Field>
        <button type="button" className="st-btn" disabled={act.pending || !f.username.includes('@')} onClick={() => void detect()}>
          Autodetect
        </button>
        <Field label="Password / app password" grow>
          <input
            type="password"
            value={f.password}
            placeholder={info?.has_password ? 'unchanged' : 'app password'}
            onChange={(e) => setF({ ...f, password: e.target.value })}
          />
        </Field>
      </div>
      {hint && (
        <div className="st-dim">
          Detected {hint.display_name}.{' '}
          {hint.app_password_url && (
            <a href={hint.app_password_url} target="_blank" rel="noreferrer" style={{ color: 'var(--coral)' }}>
              Get an app password
            </a>
          )}
          {hint.notes ? ` — ${hint.notes}` : ''}
        </div>
      )}
      <div className="st-row">
        <Field label="IMAP server" grow>
          <input type="text" value={f.imap_server} onChange={(e) => setF({ ...f, imap_server: e.target.value })} />
        </Field>
        <Field label="Port">
          <input type="number" value={f.imap_port} onChange={(e) => setF({ ...f, imap_port: e.target.value })} />
        </Field>
        <label className="st-check">
          <input type="checkbox" checked={f.use_tls} onChange={(e) => setF({ ...f, use_tls: e.target.checked })} />
          TLS
        </label>
        <Field label="Folders">
          <input type="text" value={f.folders} onChange={(e) => setF({ ...f, folders: e.target.value })} />
        </Field>
        <Field label="Poll (s)">
          <input type="number" value={f.poll_interval_secs} onChange={(e) => setF({ ...f, poll_interval_secs: e.target.value })} />
        </Field>
        <Field label="Lookback (h)">
          <input type="number" value={f.lookback_hours} onChange={(e) => setF({ ...f, lookback_hours: e.target.value })} />
        </Field>
      </div>
      <div className="st-row">
        <button type="button" className="st-btn primary" disabled={act.pending} onClick={() => void saveImap()}>
          Save email
        </button>
        <button type="button" className="st-btn" disabled={act.pending || !f.password} onClick={() => void testImap()}>
          Test IMAP
        </button>
        <TestBadge result={test} />
      </div>
      <hr className="st-divider" />
      <div className="st-row">
        <Field label="SMTP server" grow>
          <input type="text" value={f.smtp_server} onChange={(e) => setF({ ...f, smtp_server: e.target.value })} />
        </Field>
        <Field label="Port">
          <input type="number" value={f.smtp_port} onChange={(e) => setF({ ...f, smtp_port: e.target.value })} />
        </Field>
        <label className="st-check">
          <input type="checkbox" checked={f.smtp_use_tls} onChange={(e) => setF({ ...f, smtp_use_tls: e.target.checked })} />
          TLS
        </label>
        <Field label="SMTP username" grow>
          <input type="text" value={f.smtp_username} onChange={(e) => setF({ ...f, smtp_username: e.target.value })} />
        </Field>
        <Field label="SMTP password" grow>
          <input
            type="password"
            value={f.smtp_password}
            placeholder={info?.has_smtp_password ? 'unchanged' : 'app password'}
            onChange={(e) => setF({ ...f, smtp_password: e.target.value })}
          />
        </Field>
        <Field label="From address" grow>
          <input type="email" value={f.from_address} onChange={(e) => setF({ ...f, from_address: e.target.value })} />
        </Field>
      </div>
      <div className="st-row">
        <button type="button" className="st-btn primary" disabled={act.pending} onClick={() => void saveSmtp()}>
          Save SMTP
        </button>
        <button type="button" className="st-btn" disabled={act.pending || !f.smtp_password} onClick={() => void testSmtp()}>
          Test SMTP
        </button>
        <TestBadge result={smtpTest} />
      </div>
      <ErrorText error={act.error} />
    </Section>
  );
}

// ---------------------------------------------------------------------------
// Telegram
// ---------------------------------------------------------------------------

function TelegramSection({
  client,
  info,
  reload,
}: {
  client: AthenClient;
  info: TelegramInfo | null;
  reload: () => Promise<void>;
}) {
  const act = useAction();
  const [test, setTest] = useState<TestResult | null>(null);
  const [f, setF] = useState({ enabled: false, bot_token: '', chat_ids: '', poll: '5' });

  useEffect(() => {
    if (!info) return;
    setF({
      enabled: info.enabled,
      bot_token: info.bot_token ?? '',
      chat_ids: (info.allowed_chat_ids ?? []).join(', '),
      poll: String(info.poll_interval_secs || 5),
    });
  }, [info]);

  const parseIds = () =>
    f.chat_ids
      .split(/[,\s]+/)
      .map((s) => s.trim())
      .filter(Boolean)
      .map(Number)
      .filter((n) => Number.isFinite(n));

  const save = async () => {
    const ok = await act.run(() =>
      client.post('/settings/telegram', {
        enabled: f.enabled,
        bot_token: f.bot_token ? f.bot_token : undefined,
        allowed_chat_ids: parseIds(),
        poll_interval_secs: Number(f.poll) || undefined,
      }),
    );
    if (ok) await reload();
  };

  const runTest = async () => {
    setTest(null);
    await act.run(async () => {
      const r = await client.post<TestResult>('/settings/telegram/test', { bot_token: f.bot_token });
      setTest(r);
    });
  };

  return (
    <Section title="Telegram" hint="Bot bridge — talk to Athen from Telegram.">
      {!info && <Loading />}
      <div className="st-row">
        <label className="st-check">
          <input type="checkbox" checked={f.enabled} onChange={(e) => setF({ ...f, enabled: e.target.checked })} />
          Enabled
        </label>
        <Field label="Bot token" grow>
          <input
            type="password"
            value={f.bot_token}
            placeholder={info?.has_bot_token ? 'unchanged' : '123456:ABC…'}
            onChange={(e) => setF({ ...f, bot_token: e.target.value })}
          />
        </Field>
        <Field label="Allowed chat ids (comma-separated)" grow>
          <input type="text" value={f.chat_ids} onChange={(e) => setF({ ...f, chat_ids: e.target.value })} />
        </Field>
        <Field label="Poll (s)">
          <input type="number" value={f.poll} onChange={(e) => setF({ ...f, poll: e.target.value })} />
        </Field>
      </div>
      <div className="st-row">
        <button type="button" className="st-btn primary" disabled={act.pending} onClick={() => void save()}>
          Save Telegram
        </button>
        <button type="button" className="st-btn" disabled={act.pending || !f.bot_token} onClick={() => void runTest()}>
          Test
        </button>
        <TestBadge result={test} />
      </div>
      <ErrorText error={act.error} />
    </Section>
  );
}

// ---------------------------------------------------------------------------
// GitHub identities
// ---------------------------------------------------------------------------

function GithubSection({ client }: { client: AthenClient }) {
  const snap = useLoad(() => client.get<GithubSnapshot>('/settings/github'), [client]);
  return (
    <Section title="GitHub" hint="Bot / User identities injected into shell git operations per profile.">
      {snap.loading && <Loading />}
      <ErrorText error={snap.error} />
      {snap.data && (
        <>
          <GithubForm client={client} which="bot" data={snap.data.bot} reload={snap.reload} />
          <hr className="st-divider" />
          <GithubForm client={client} which="user" data={snap.data.user} reload={snap.reload} />
        </>
      )}
    </Section>
  );
}

function GithubForm({
  client,
  which,
  data,
  reload,
}: {
  client: AthenClient;
  which: 'bot' | 'user';
  data: GithubIdentitySettings;
  reload: () => Promise<void>;
}) {
  const act = useAction();
  const [test, setTest] = useState<TestResult | null>(null);
  const [f, setF] = useState({ token: '', user_name: data.user_name, user_email: data.user_email });

  useEffect(() => {
    setF((p) => ({ ...p, user_name: data.user_name, user_email: data.user_email }));
  }, [data]);

  return (
    <>
      <div className="st-row">
        <span className="st-badge coral" style={{ alignSelf: 'center' }}>
          {which}
        </span>
        <Field label="Token" grow>
          <input
            type="password"
            value={f.token}
            placeholder={data.has_token ? 'unchanged' : 'ghp_…'}
            onChange={(e) => setF({ ...f, token: e.target.value })}
          />
        </Field>
        <Field label="Git author name" grow>
          <input type="text" value={f.user_name} onChange={(e) => setF({ ...f, user_name: e.target.value })} />
        </Field>
        <Field label="Git author email" grow>
          <input type="email" value={f.user_email} onChange={(e) => setF({ ...f, user_email: e.target.value })} />
        </Field>
        <button
          type="button"
          className="st-btn"
          disabled={act.pending}
          onClick={() =>
            void act
              .run(() =>
                client.post('/settings/github', {
                  identity: which,
                  token: f.token ? f.token : undefined,
                  user_name: f.user_name,
                  user_email: f.user_email,
                }),
              )
              .then((ok) => { if (ok) void reload(); })
          }
        >
          Save
        </button>
        <button
          type="button"
          className="st-btn"
          disabled={act.pending || !f.token}
          onClick={() => {
            setTest(null);
            void act.run(async () => {
              const r = await client.post<TestResult>('/settings/github/test', { token: f.token });
              setTest(r);
            });
          }}
        >
          Test
        </button>
        <TestBadge result={test} />
      </div>
      <ErrorText error={act.error} />
    </>
  );
}

// ---------------------------------------------------------------------------
// Calendar sources (CalDAV)
// ---------------------------------------------------------------------------

function CalendarSection({ client }: { client: AthenClient }) {
  const sources = useLoad(() => client.get<CalendarSourceView[]>('/settings/calendar-sources'), [client]);
  const act = useAction();
  const [adding, setAdding] = useState(false);
  const [f, setF] = useState({ display_name: '', base_url: '', username: '', password: '' });
  const [results, setResults] = useState<Record<string, SyncResult>>({});
  const [picker, setPicker] = useState<{ id: string; remote: RemoteCalendarView[]; selected: Set<string> } | null>(null);

  const add = async () => {
    const ok = await act.run(() =>
      client.post('/settings/calendar-sources', {
        display_name: f.display_name.trim(),
        base_url: f.base_url.trim(),
        username: f.username.trim(),
        password: f.password,
      }),
    );
    if (ok) {
      setAdding(false);
      setF({ display_name: '', base_url: '', username: '', password: '' });
      await sources.reload();
    }
  };

  const rowAction = async (id: string, path: string) => {
    await act.run(async () => {
      const r = await client.post<SyncResult>(`/settings/calendar-sources/${encodeURIComponent(id)}/${path}`);
      setResults((p) => ({ ...p, [id]: r }));
    });
    await sources.reload();
  };

  const openPicker = async (s: CalendarSourceView) => {
    await act.run(async () => {
      const remote = await client.get<RemoteCalendarView[]>(
        `/settings/calendar-sources/${encodeURIComponent(s.id)}/remote`,
      );
      setPicker({ id: s.id, remote, selected: new Set(s.selectedCalendars) });
    });
  };

  const savePicker = async () => {
    if (!picker) return;
    const ok = await act.run(() =>
      client.post(`/settings/calendar-sources/${encodeURIComponent(picker.id)}/calendars`, {
        calendar_ids: Array.from(picker.selected),
      }),
    );
    if (ok) {
      setPicker(null);
      await sources.reload();
    }
  };

  return (
    <Section
      title="Calendar sources"
      hint="CalDAV — iCloud, Google, Fastmail, Nextcloud… synced into the local calendar."
      actions={
        <>
          <button
            type="button"
            className="st-btn"
            disabled={act.pending || (sources.data ?? []).length === 0}
            onClick={() => void act.run(() => client.post('/settings/calendar-sources/sync-all')).then(() => sources.reload())}
          >
            Sync all
          </button>
          <button type="button" className="st-btn" onClick={() => setAdding(!adding)}>
            + Add source
          </button>
        </>
      }
    >
      {sources.loading && <Loading />}
      <ErrorText error={sources.error} />
      <div className="st-list">
        {(sources.data ?? []).map((s) => (
          <div key={s.id} className="st-item">
            <div className="st-item-main">
              <div className="st-item-title">
                {s.displayName}
                <span className="st-badge">{s.kind}</span>
                {!s.enabled && <span className="st-badge amber">disabled</span>}
              </div>
              <div className="st-item-sub">
                {s.username} · {s.selectedCalendars.length || 'all'} calendars
                {s.lastSyncAt ? ` · synced ${new Date(s.lastSyncAt).toLocaleString()}` : ''}
              </div>
              {s.lastSyncError && <div className="st-error">{s.lastSyncError}</div>}
              {results[s.id] && <TestBadge result={results[s.id]} />}
            </div>
            <div className="st-item-actions">
              <button
                type="button"
                className="st-btn small"
                disabled={act.pending}
                onClick={() =>
                  void act
                    .run(() =>
                      client.post(`/settings/calendar-sources/${encodeURIComponent(s.id)}/enabled`, {
                        enabled: !s.enabled,
                      }),
                    )
                    .then((ok) => { if (ok) void sources.reload(); })
                }
              >
                {s.enabled ? 'Disable' : 'Enable'}
              </button>
              <button type="button" className="st-btn small" disabled={act.pending} onClick={() => void rowAction(s.id, 'test')}>
                Test
              </button>
              <button type="button" className="st-btn small" disabled={act.pending} onClick={() => void rowAction(s.id, 'sync')}>
                Sync
              </button>
              <button type="button" className="st-btn small" disabled={act.pending} onClick={() => void openPicker(s)}>
                Calendars…
              </button>
              <ConfirmButton
                label="Delete"
                className="small"
                onConfirm={() =>
                  void act
                    .run(() => client.post(`/settings/calendar-sources/${encodeURIComponent(s.id)}/delete`))
                    .then((ok) => { if (ok) void sources.reload(); })
                }
              />
            </div>
            {picker?.id === s.id && (
              <div style={{ width: '100%' }}>
                {picker.remote.map((c) => (
                  <label key={c.id} className="st-check" style={{ display: 'flex', marginTop: 4 }}>
                    <input
                      type="checkbox"
                      checked={picker.selected.has(c.id)}
                      onChange={(e) => {
                        const sel = new Set(picker.selected);
                        if (e.target.checked) sel.add(c.id);
                        else sel.delete(c.id);
                        setPicker({ ...picker, selected: sel });
                      }}
                    />
                    {c.name}
                    {c.readOnly && <span className="st-badge">read-only</span>}
                  </label>
                ))}
                <div className="st-row" style={{ marginTop: 8 }}>
                  <button type="button" className="st-btn small primary" disabled={act.pending} onClick={() => void savePicker()}>
                    Save selection
                  </button>
                  <button type="button" className="st-btn small" onClick={() => setPicker(null)}>
                    Cancel
                  </button>
                </div>
              </div>
            )}
          </div>
        ))}
      </div>
      {adding && (
        <>
          <div className="st-row">
            <Field label="Display name" grow>
              <input type="text" value={f.display_name} placeholder="iCloud" onChange={(e) => setF({ ...f, display_name: e.target.value })} />
            </Field>
            <Field label="CalDAV base URL" grow>
              <input type="url" value={f.base_url} placeholder="https://caldav.icloud.com" onChange={(e) => setF({ ...f, base_url: e.target.value })} />
            </Field>
            <Field label="Username" grow>
              <input type="text" value={f.username} onChange={(e) => setF({ ...f, username: e.target.value })} />
            </Field>
            <Field label="App password" grow>
              <input type="password" value={f.password} onChange={(e) => setF({ ...f, password: e.target.value })} />
            </Field>
          </div>
          <div className="st-row">
            <button
              type="button"
              className="st-btn primary"
              disabled={act.pending || !f.display_name.trim() || !f.base_url.trim()}
              onClick={() => void add()}
            >
              Add &amp; test
            </button>
            <button type="button" className="st-btn" onClick={() => setAdding(false)}>
              Cancel
            </button>
          </div>
        </>
      )}
      <ErrorText error={act.error} />
    </Section>
  );
}

// ---------------------------------------------------------------------------
// Web search
// ---------------------------------------------------------------------------

function WebSearchSection({
  client,
  info,
  reload,
}: {
  client: AthenClient;
  info: WebSearchInfo | null;
  reload: () => Promise<void>;
}) {
  const act = useAction();
  const [brave, setBrave] = useState('');
  const [tavily, setTavily] = useState('');
  const [test, setTest] = useState<TestResult | null>(null);

  const save = async () => {
    const ok = await act.run(() =>
      client.post('/settings/websearch', {
        brave_api_key: brave ? brave : undefined,
        tavily_api_key: tavily ? tavily : undefined,
      }),
    );
    if (ok) {
      setBrave('');
      setTavily('');
      await reload();
    }
  };

  const runTest = async (provider: string, key: string) => {
    setTest(null);
    await act.run(async () => {
      const r = await client.post<TestResult>('/settings/websearch/test', { provider, api_key: key });
      setTest(r);
    });
  };

  return (
    <Section title="Web search" hint="Optional API keys — DuckDuckGo works without any.">
      {!info && <Loading />}
      <div className="st-row">
        <Field label="Brave API key" grow>
          <input
            type="password"
            value={brave}
            placeholder={info?.brave_configured ? `unchanged (${info.brave_hint})` : 'BSA…'}
            onChange={(e) => setBrave(e.target.value)}
          />
        </Field>
        <button type="button" className="st-btn" disabled={act.pending || !brave} onClick={() => void runTest('brave', brave)}>
          Test
        </button>
        <Field label="Tavily API key" grow>
          <input
            type="password"
            value={tavily}
            placeholder={info?.tavily_configured ? `unchanged (${info.tavily_hint})` : 'tvly-…'}
            onChange={(e) => setTavily(e.target.value)}
          />
        </Field>
        <button type="button" className="st-btn" disabled={act.pending || !tavily} onClick={() => void runTest('tavily', tavily)}>
          Test
        </button>
        <button type="button" className="st-btn primary" disabled={act.pending || (!brave && !tavily)} onClick={() => void save()}>
          Save keys
        </button>
      </div>
      <TestBadge result={test} />
      <ErrorText error={act.error} />
    </Section>
  );
}

// ---------------------------------------------------------------------------
// Cloud APIs (registered HTTP endpoints)
// ---------------------------------------------------------------------------

type AuthKind = 'None' | 'BearerToken' | 'Header' | 'HeaderPrefixed' | 'QueryParam' | 'BasicAuth';

interface EndpointForm {
  id: string | null;
  name: string;
  provider: string;
  base_url: string;
  authKind: AuthKind;
  authName: string;
  authPrefix: string;
  authUser: string;
  credential: string;
  rate_limit: string;
  notes: string;
  hasCredential: boolean;
  testPath: string;
}

function authToWire(f: EndpointForm): AuthMethod {
  switch (f.authKind) {
    case 'None':
      return 'None';
    case 'BearerToken':
      return 'BearerToken';
    case 'Header':
      return { Header: { name: f.authName } };
    case 'HeaderPrefixed':
      return { HeaderPrefixed: { name: f.authName, prefix: f.authPrefix } };
    case 'QueryParam':
      return { QueryParam: { name: f.authName } };
    case 'BasicAuth':
      return { BasicAuth: { user: f.authUser } };
  }
}

function authFromWire(a: AuthMethod): Pick<EndpointForm, 'authKind' | 'authName' | 'authPrefix' | 'authUser'> {
  if (a === 'None') return { authKind: 'None', authName: '', authPrefix: '', authUser: '' };
  if (a === 'BearerToken') return { authKind: 'BearerToken', authName: '', authPrefix: '', authUser: '' };
  if ('Header' in a) return { authKind: 'Header', authName: a.Header.name, authPrefix: '', authUser: '' };
  if ('HeaderPrefixed' in a)
    return { authKind: 'HeaderPrefixed', authName: a.HeaderPrefixed.name, authPrefix: a.HeaderPrefixed.prefix, authUser: '' };
  if ('QueryParam' in a) return { authKind: 'QueryParam', authName: a.QueryParam.name, authPrefix: '', authUser: '' };
  return { authKind: 'BasicAuth', authName: '', authPrefix: '', authUser: a.BasicAuth.user };
}

function EndpointsSection({ client }: { client: AthenClient }) {
  const endpoints = useLoad(() => client.get<EndpointWire[]>('/endpoints'), [client]);
  const presets = useLoad(() => client.get<EndpointPreset[]>('/endpoints/presets'), [client]);
  const act = useAction();
  const [form, setForm] = useState<EndpointForm | null>(null);
  const [tests, setTests] = useState<Record<string, TestResult>>({});

  const blank = (): EndpointForm => ({
    id: null,
    name: '',
    provider: '',
    base_url: '',
    authKind: 'BearerToken',
    authName: '',
    authPrefix: '',
    authUser: '',
    credential: '',
    rate_limit: '30',
    notes: '',
    hasCredential: false,
    testPath: '',
  });

  const applyPreset = (p: EndpointPreset) => {
    setForm({
      ...blank(),
      name: p.label,
      provider: p.provider,
      base_url: p.base_url,
      ...authFromWire(p.auth_method),
      rate_limit: String(p.default_rate_limit_per_minute || 30),
      notes: p.free_tier_blurb,
      testPath: p.test_path,
    });
  };

  const openEdit = (e: EndpointWire) => {
    setForm({
      id: e.id,
      name: e.name,
      provider: e.provider,
      base_url: e.base_url,
      ...authFromWire(e.auth_method),
      credential: '',
      rate_limit: String(e.rate_limit_per_minute || 0),
      notes: e.notes ?? '',
      hasCredential: e.has_credential,
      testPath: '',
    });
  };

  const save = async () => {
    if (!form) return;
    const ok = await act.run(() =>
      client.post('/endpoints', {
        id: form.id ?? undefined,
        name: form.name.trim(),
        provider: form.provider.trim(),
        base_url: form.base_url.trim(),
        enabled: true,
        auth_method: authToWire(form),
        rate_limit_per_minute: Number(form.rate_limit) || 0,
        notes: form.notes || undefined,
        credential: form.credential ? form.credential : undefined,
      }),
    );
    if (ok) {
      setForm(null);
      await endpoints.reload();
    }
  };

  const runTest = async (e: EndpointWire) => {
    await act.run(async () => {
      const r = await client.post<{ success?: boolean; message?: string; status?: number }>(
        `/endpoints/${encodeURIComponent(e.id)}/test`,
        { path: null },
      );
      setTests((p) => ({
        ...p,
        [e.id]: {
          success: Boolean(r.success),
          message: r.message ?? JSON.stringify(r),
        },
      }));
    });
  };

  return (
    <Section
      title="Cloud APIs"
      hint="Registered HTTP endpoints the http_request tool can call, with vault-backed credentials."
      actions={
        <>
          <select
            className="st-btn"
            value=""
            onChange={(e) => {
              const p = (presets.data ?? []).find((x) => x.slug === e.target.value);
              if (p) applyPreset(p);
            }}
          >
            <option value="">+ From preset…</option>
            {(presets.data ?? []).map((p) => (
              <option key={p.slug} value={p.slug}>
                {p.label}
              </option>
            ))}
          </select>
          <button type="button" className="st-btn" onClick={() => setForm(blank())}>
            + Custom
          </button>
        </>
      }
    >
      {endpoints.loading && <Loading />}
      <ErrorText error={endpoints.error ?? presets.error} />
      {!endpoints.loading && (endpoints.data ?? []).length === 0 && (
        <div className="st-dim">No endpoints registered.</div>
      )}
      <div className="st-list">
        {(endpoints.data ?? []).map((e) => (
          <div key={e.id} className="st-item">
            <div className="st-item-main">
              <div className="st-item-title">
                {e.name}
                {!e.enabled && <span className="st-badge amber">disabled</span>}
                {e.has_credential ? (
                  <span className="st-badge green">key set</span>
                ) : (
                  e.auth_method !== 'None' && <span className="st-badge red">no key</span>
                )}
              </div>
              <div className="st-item-sub st-mono">{e.base_url}</div>
              {tests[e.id] && <TestBadge result={tests[e.id]} />}
            </div>
            <div className="st-item-actions">
              <button
                type="button"
                className="st-btn small"
                disabled={act.pending}
                onClick={() =>
                  void act
                    .run(() => client.post(`/endpoints/${encodeURIComponent(e.id)}/enabled`, { enabled: !e.enabled }))
                    .then((ok) => { if (ok) void endpoints.reload(); })
                }
              >
                {e.enabled ? 'Disable' : 'Enable'}
              </button>
              <button type="button" className="st-btn small" disabled={act.pending} onClick={() => void runTest(e)}>
                Test
              </button>
              <button type="button" className="st-btn small" onClick={() => openEdit(e)}>
                Edit
              </button>
              <ConfirmButton
                label="Delete"
                className="small"
                onConfirm={() =>
                  void act
                    .run(() => client.post(`/endpoints/${encodeURIComponent(e.id)}/delete`))
                    .then((ok) => { if (ok) void endpoints.reload(); })
                }
              />
            </div>
          </div>
        ))}
      </div>
      {form && (
        <>
          <hr className="st-divider" />
          <div className="st-row">
            <Field label="Name" grow>
              <input type="text" value={form.name} onChange={(e) => setForm({ ...form, name: e.target.value })} />
            </Field>
            <Field label="Provider" grow>
              <input type="text" value={form.provider} onChange={(e) => setForm({ ...form, provider: e.target.value })} />
            </Field>
            <Field label="Base URL" grow>
              <input type="url" value={form.base_url} onChange={(e) => setForm({ ...form, base_url: e.target.value })} />
            </Field>
          </div>
          <div className="st-row">
            <Field label="Auth">
              <select
                value={form.authKind}
                onChange={(e) => setForm({ ...form, authKind: e.target.value as AuthKind })}
              >
                <option value="None">None</option>
                <option value="BearerToken">Bearer token</option>
                <option value="Header">Header</option>
                <option value="HeaderPrefixed">Header with prefix</option>
                <option value="QueryParam">Query param</option>
                <option value="BasicAuth">Basic auth</option>
              </select>
            </Field>
            {(form.authKind === 'Header' || form.authKind === 'HeaderPrefixed' || form.authKind === 'QueryParam') && (
              <Field label={form.authKind === 'QueryParam' ? 'Param name' : 'Header name'}>
                <input type="text" value={form.authName} onChange={(e) => setForm({ ...form, authName: e.target.value })} />
              </Field>
            )}
            {form.authKind === 'HeaderPrefixed' && (
              <Field label="Prefix">
                <input type="text" value={form.authPrefix} onChange={(e) => setForm({ ...form, authPrefix: e.target.value })} />
              </Field>
            )}
            {form.authKind === 'BasicAuth' && (
              <Field label="Username">
                <input type="text" value={form.authUser} onChange={(e) => setForm({ ...form, authUser: e.target.value })} />
              </Field>
            )}
            {form.authKind !== 'None' && (
              <Field label="Credential" grow>
                <input
                  type="password"
                  value={form.credential}
                  placeholder={form.hasCredential ? 'unchanged' : 'API key / token'}
                  onChange={(e) => setForm({ ...form, credential: e.target.value })}
                />
              </Field>
            )}
            <Field label="Rate limit / min">
              <input type="number" value={form.rate_limit} onChange={(e) => setForm({ ...form, rate_limit: e.target.value })} />
            </Field>
          </div>
          <Field label="Notes" grow>
            <input type="text" value={form.notes} onChange={(e) => setForm({ ...form, notes: e.target.value })} />
          </Field>
          <div className="st-row">
            <button
              type="button"
              className="st-btn primary"
              disabled={act.pending || !form.name.trim() || !form.base_url.trim()}
              onClick={() => void save()}
            >
              {form.id ? 'Save endpoint' : 'Add endpoint'}
            </button>
            <button type="button" className="st-btn" onClick={() => setForm(null)}>
              Cancel
            </button>
          </div>
        </>
      )}
      <ErrorText error={act.error} />
    </Section>
  );
}
