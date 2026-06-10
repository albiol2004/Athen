// Models panel: Connections (LLM provider credentials), Bundles
// (per-tier loadouts), and the Embeddings provider.
//
// Endpoint + body shapes: athen-app/src/http_api.rs (ProviderSaveBody,
// BundleUpdateBody, EmbeddingsSaveBody…), response shapes from
// settings.rs (SettingsResponse) and bundle_settings.rs (BundleView).

import { useCallback, useEffect, useState } from 'react';
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

// ---- wire shapes (serde field names, see settings.rs) ----

interface ProviderInfo {
  id: string;
  name: string;
  provider_type: string;
  base_url: string;
  model: string;
  has_api_key: boolean;
  api_key_hint: string;
  is_active: boolean;
  family: string;
  context_window_tokens: number;
  temperature?: number | null;
  tier_models?: Record<string, string>;
}

interface EmbeddingInfo {
  mode: string;
  provider: string | null;
  model: string | null;
  base_url: string | null;
  has_api_key: boolean;
}

interface SettingsResponse {
  providers: ProviderInfo[];
  active_provider: string;
  active_bundle_id: string;
  embeddings: EmbeddingInfo;
}

interface ProviderCatalogEntry {
  id: string;
  name: string;
  provider_type: string;
  default_base_url: string;
  default_model: string;
  default_family: string;
  api_key_hint: string;
  default_tier_cheap: string;
  default_tier_fast: string;
  default_tier_code: string;
  default_tier_powerful: string;
  dashboard_url: string;
  cost_note: string;
}

interface BundleTierView {
  connection_id: string;
  slug: string;
}
interface BundleTiersView {
  cheap?: BundleTierView | null;
  fast?: BundleTierView | null;
  code?: BundleTierView | null;
  powerful?: BundleTierView | null;
}
interface BundleView {
  id: string;
  name: string;
  is_active: boolean;
  tiers: BundleTiersView;
  updated_at: string;
}

interface ModelCatalogEntry {
  slug: string;
  display_name: string;
}

interface BundledStatus {
  downloadedTiers: string[];
  activeTier: string | null;
  totalCacheSizeMb: number;
}

const TIERS = ['cheap', 'fast', 'code', 'powerful'] as const;
type TierKey = (typeof TIERS)[number];
const TIER_LABEL: Record<TierKey, string> = {
  cheap: 'Cheap',
  fast: 'Fast',
  code: 'Code',
  powerful: 'Powerful',
};

interface ProviderForm {
  id: string;
  base_url: string;
  model: string;
  api_key: string;
  family: string;
  context_window_tokens: string;
  isNew: boolean;
  hasKey: boolean;
  tierDefaults?: Record<string, string>;
}

export function PanelModels({ client }: { client: AthenClient }) {
  const settings = useLoad(() => client.get<SettingsResponse>('/settings'), [client]);
  const catalog = useLoad(
    () => client.get<ProviderCatalogEntry[]>('/settings/provider-catalog'),
    [client],
  );

  return (
    <>
      <ConnectionsSection client={client} settings={settings} catalog={catalog.data ?? []} />
      <BundlesSection client={client} providers={settings.data?.providers ?? []} />
      <EmbeddingsSection client={client} settings={settings} />
    </>
  );
}

// ---------------------------------------------------------------------------
// Connections
// ---------------------------------------------------------------------------

function ConnectionsSection({
  client,
  settings,
  catalog,
}: {
  client: AthenClient;
  settings: ReturnType<typeof useLoad<SettingsResponse>>;
  catalog: ProviderCatalogEntry[];
}) {
  const [form, setForm] = useState<ProviderForm | null>(null);
  const [test, setTest] = useState<TestResult | null>(null);
  const act = useAction();

  const openCatalog = (entry: ProviderCatalogEntry) => {
    setTest(null);
    setForm({
      id: entry.id,
      base_url: entry.default_base_url,
      model: entry.default_model,
      api_key: '',
      family: entry.default_family,
      context_window_tokens: '',
      isNew: true,
      hasKey: false,
      tierDefaults: {
        Cheap: entry.default_tier_cheap,
        Fast: entry.default_tier_fast,
        Code: entry.default_tier_code,
        Powerful: entry.default_tier_powerful,
      },
    });
  };

  const openEdit = (p: ProviderInfo) => {
    setTest(null);
    setForm({
      id: p.id,
      base_url: p.base_url,
      model: p.model,
      api_key: '',
      family: p.family,
      context_window_tokens: String(p.context_window_tokens || ''),
      isNew: false,
      hasKey: p.has_api_key,
    });
  };

  const save = async () => {
    if (!form) return;
    const ok = await act.run(() =>
      client.post('/settings/providers', {
        id: form.id.trim(),
        base_url: form.base_url.trim(),
        model: form.model.trim(),
        api_key: form.api_key ? form.api_key : undefined,
        family: form.family || undefined,
        context_window_tokens: form.context_window_tokens
          ? Number(form.context_window_tokens)
          : undefined,
        tier_models: form.isNew ? form.tierDefaults : undefined,
      }),
    );
    if (ok) {
      setForm(null);
      await settings.reload();
    }
  };

  const runTest = async () => {
    if (!form) return;
    setTest(null);
    await act.run(async () => {
      const r = await client.post<TestResult>('/settings/providers/test', {
        id: form.id.trim(),
        base_url: form.base_url.trim(),
        model: form.model.trim(),
        api_key: form.api_key ? form.api_key : undefined,
      });
      setTest(r);
    });
  };

  const remove = async (id: string) => {
    const ok = await act.run(() =>
      client.post(`/settings/providers/${encodeURIComponent(id)}/delete`),
    );
    if (ok) {
      if (form?.id === id) setForm(null);
      await settings.reload();
    }
  };

  const providers = settings.data?.providers ?? [];
  const configured = new Set(providers.map((p) => p.id));
  const addable = catalog.filter((c) => !configured.has(c.id));

  return (
    <Section
      title="Connections"
      hint="LLM provider credentials. Bundles below decide which connection each tier uses."
      actions={
        addable.length > 0 ? (
          <select
            className="st-btn"
            value=""
            onChange={(e) => {
              const entry = addable.find((c) => c.id === e.target.value);
              if (entry) openCatalog(entry);
            }}
          >
            <option value="">+ Add connection…</option>
            {addable.map((c) => (
              <option key={c.id} value={c.id}>
                {c.name}
              </option>
            ))}
          </select>
        ) : undefined
      }
    >
      {settings.loading && <Loading />}
      <ErrorText error={settings.error} />
      {!settings.loading && providers.length === 0 && (
        <div className="st-dim">No connections configured yet.</div>
      )}
      <div className="st-list">
        {providers.map((p) => (
          <div key={p.id} className={`st-item${form?.id === p.id ? ' selected' : ''}`}>
            <div className="st-item-main">
              <div className="st-item-title">
                {p.name}
                {p.is_active && <span className="st-badge coral">primary</span>}
                {p.has_api_key ? (
                  <span className="st-badge green">key set</span>
                ) : (
                  p.provider_type !== 'local' && <span className="st-badge amber">no key</span>
                )}
              </div>
              <div className="st-item-sub st-mono">
                {p.model} · {p.base_url}
              </div>
            </div>
            <div className="st-item-actions">
              <button type="button" className="st-btn small" onClick={() => openEdit(p)}>
                Edit
              </button>
              <ConfirmButton label="Delete" className="small" onConfirm={() => void remove(p.id)} />
            </div>
          </div>
        ))}
      </div>
      {form && (
        <>
          <hr className="st-divider" />
          <div className="st-row">
            <Field label="Connection id" grow>
              <input
                type="text"
                value={form.id}
                readOnly={!form.isNew}
                onChange={(e) => setForm({ ...form, id: e.target.value })}
              />
            </Field>
            <Field label="Base URL" grow>
              <input
                type="url"
                value={form.base_url}
                onChange={(e) => setForm({ ...form, base_url: e.target.value })}
              />
            </Field>
          </div>
          <div className="st-row">
            <Field label="Default model" grow>
              <input
                type="text"
                value={form.model}
                onChange={(e) => setForm({ ...form, model: e.target.value })}
              />
            </Field>
            <Field label="API key" grow>
              <input
                type="password"
                value={form.api_key}
                placeholder={form.hasKey ? 'unchanged' : 'paste API key'}
                onChange={(e) => setForm({ ...form, api_key: e.target.value })}
              />
            </Field>
            <Field label="Context window (tokens)">
              <input
                type="number"
                value={form.context_window_tokens}
                placeholder="auto"
                onChange={(e) => setForm({ ...form, context_window_tokens: e.target.value })}
              />
            </Field>
          </div>
          <div className="st-row">
            <button type="button" className="st-btn primary" disabled={act.pending} onClick={() => void save()}>
              Save connection
            </button>
            <button type="button" className="st-btn" disabled={act.pending} onClick={() => void runTest()}>
              Test
            </button>
            <button type="button" className="st-btn" onClick={() => setForm(null)}>
              Cancel
            </button>
            <TestBadge result={test} />
          </div>
        </>
      )}
      <ErrorText error={act.error} />
    </Section>
  );
}

// ---------------------------------------------------------------------------
// Bundles
// ---------------------------------------------------------------------------

function BundlesSection({
  client,
  providers,
}: {
  client: AthenClient;
  providers: ProviderInfo[];
}) {
  const bundles = useLoad(() => client.get<BundleView[]>('/settings/bundles'), [client]);
  const act = useAction();
  const [newName, setNewName] = useState('');
  const [curated, setCurated] = useState<Record<string, ModelCatalogEntry[]>>({});

  // Pre-fetch curated slugs per configured connection (powers the
  // per-tier datalists; free text remains allowed).
  useEffect(() => {
    let cancelled = false;
    void (async () => {
      for (const p of providers) {
        try {
          const list = await client.get<ModelCatalogEntry[]>(
            `/settings/curated-models?provider_id=${encodeURIComponent(p.id)}`,
          );
          if (cancelled) return;
          setCurated((prev) => ({ ...prev, [p.id]: list }));
        } catch {
          /* curated list is a nicety; the input stays free-text */
        }
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [client, providers]);

  const mutate = useCallback(
    async (fn: () => Promise<unknown>) => {
      const ok = await act.run(fn);
      if (ok) await bundles.reload();
      return ok;
    },
    [act, bundles],
  );

  const create = async () => {
    const name = newName.trim();
    if (!name) return;
    const ok = await mutate(() => client.post('/settings/bundles', { name }));
    if (ok) setNewName('');
  };

  // IMPORTANT: tier edits persist immediately on change — deferred saves
  // get silently reverted by refetch-driven re-renders.
  const setTier = async (b: BundleView, tier: TierKey, value: BundleTierView | null) => {
    const tiers: BundleTiersView = {
      cheap: b.tiers.cheap ?? null,
      fast: b.tiers.fast ?? null,
      code: b.tiers.code ?? null,
      powerful: b.tiers.powerful ?? null,
      [tier]: value,
    };
    await mutate(() =>
      client.post(`/settings/bundles/${encodeURIComponent(b.id)}`, { tiers }),
    );
  };

  return (
    <Section
      title="Bundles"
      hint="Named per-tier model loadouts. The active Bundle decides which connection + slug each tier resolves to."
      actions={
        <>
          <input
            type="text"
            className="st-mono"
            style={{
              font: 'inherit',
              fontSize: 12.5,
              color: 'var(--text)',
              background: 'rgba(0,0,0,.25)',
              border: '1px solid var(--glass-border)',
              borderRadius: 9,
              padding: '6px 10px',
              width: 150,
            }}
            placeholder="New bundle name"
            value={newName}
            onChange={(e) => setNewName(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === 'Enter') void create();
            }}
          />
          <button type="button" className="st-btn" disabled={act.pending || !newName.trim()} onClick={() => void create()}>
            Create
          </button>
        </>
      }
    >
      {bundles.loading && <Loading />}
      <ErrorText error={bundles.error} />
      <div className="st-list">
        {(bundles.data ?? []).map((b) => (
          <div key={b.id} className="st-item" style={{ alignItems: 'stretch', flexDirection: 'column' }}>
            <div className="st-row" style={{ alignItems: 'center', justifyContent: 'space-between' }}>
              <div className="st-item-title">
                {b.name}
                {b.is_active && <span className="st-badge coral">active</span>}
              </div>
              <div className="st-item-actions">
                {!b.is_active && (
                  <button
                    type="button"
                    className="st-btn small"
                    disabled={act.pending}
                    onClick={() =>
                      void mutate(() =>
                        client.post(`/settings/bundles/${encodeURIComponent(b.id)}/activate`),
                      )
                    }
                  >
                    Activate
                  </button>
                )}
                <button
                  type="button"
                  className="st-btn small"
                  disabled={act.pending}
                  onClick={() =>
                    void mutate(() =>
                      client.post(`/settings/bundles/${encodeURIComponent(b.id)}/duplicate`, {
                        new_name: `${b.name} copy`,
                      }),
                    )
                  }
                >
                  Duplicate
                </button>
                <ConfirmButton
                  label="Delete"
                  className="small"
                  disabled={b.is_active}
                  onConfirm={() =>
                    void mutate(() =>
                      client.post(`/settings/bundles/${encodeURIComponent(b.id)}/delete`),
                    )
                  }
                />
              </div>
            </div>
            {TIERS.map((tier) => {
              const cur = b.tiers[tier] ?? null;
              const conn = cur?.connection_id ?? '';
              return (
                <div key={tier} className="st-tier-row">
                  <span className="st-tier-label">{TIER_LABEL[tier]}</span>
                  <select
                    value={conn}
                    onChange={(e) => {
                      const cid = e.target.value;
                      if (!cid) {
                        void setTier(b, tier, null);
                        return;
                      }
                      const slug =
                        cur && cur.connection_id === cid
                          ? cur.slug
                          : (providers.find((p) => p.id === cid)?.model ?? '');
                      void setTier(b, tier, { connection_id: cid, slug });
                    }}
                  >
                    <option value="">— unset —</option>
                    {providers.map((p) => (
                      <option key={p.id} value={p.id}>
                        {p.name}
                      </option>
                    ))}
                  </select>
                  <input
                    type="text"
                    list={conn ? `st-curated-${b.id}-${tier}` : undefined}
                    placeholder="model slug"
                    disabled={!conn}
                    defaultValue={cur?.slug ?? ''}
                    key={`${conn}:${cur?.slug ?? ''}`}
                    onBlur={(e) => {
                      const slug = e.target.value.trim();
                      if (!conn || slug === (cur?.slug ?? '')) return;
                      void setTier(b, tier, { connection_id: conn, slug });
                    }}
                    onKeyDown={(e) => {
                      if (e.key === 'Enter') (e.target as HTMLInputElement).blur();
                    }}
                  />
                  {conn && (
                    <datalist id={`st-curated-${b.id}-${tier}`}>
                      {(curated[conn] ?? []).map((m) => (
                        <option key={m.slug} value={m.slug}>
                          {m.display_name}
                        </option>
                      ))}
                    </datalist>
                  )}
                </div>
              );
            })}
          </div>
        ))}
      </div>
      <ErrorText error={act.error} />
    </Section>
  );
}

// ---------------------------------------------------------------------------
// Embeddings
// ---------------------------------------------------------------------------

const EMBED_MODES = [
  { value: 'Automatic', label: 'Automatic' },
  { value: 'Cloud', label: 'Cloud' },
  { value: 'LocalOnly', label: 'Local only' },
  { value: 'Specific', label: 'Specific provider' },
  { value: 'Bundled', label: 'Bundled (on-device)' },
  { value: 'Off', label: 'Off' },
];

function EmbeddingsSection({
  client,
  settings,
}: {
  client: AthenClient;
  settings: ReturnType<typeof useLoad<SettingsResponse>>;
}) {
  const act = useAction();
  const [test, setTest] = useState<TestResult | null>(null);
  const [form, setForm] = useState({
    mode: 'Automatic',
    provider: '',
    model: '',
    base_url: '',
    api_key: '',
  });
  const [tier, setTier] = useState('light');
  const status = useLoad(
    () => client.get<BundledStatus>('/settings/embeddings/bundled-status'),
    [client],
  );

  const emb = settings.data?.embeddings;
  useEffect(() => {
    if (!emb) return;
    const mode = emb.mode.startsWith('Bundled') ? 'Bundled' : emb.mode;
    setForm((f) => ({
      ...f,
      mode: EMBED_MODES.some((m) => m.value === mode) ? mode : 'Automatic',
      provider: emb.provider ?? '',
      model: emb.model ?? '',
      base_url: emb.base_url ?? '',
    }));
  }, [emb]);

  useEffect(() => {
    if (status.data?.activeTier) setTier(status.data.activeTier);
  }, [status.data]);

  const save = async () => {
    const ok = await act.run(() =>
      client.post('/settings/embeddings', {
        mode: form.mode === 'Bundled' ? `Bundled:${tier}` : form.mode,
        provider: form.provider || undefined,
        model: form.model || undefined,
        base_url: form.base_url || undefined,
        api_key: form.api_key || undefined,
      }),
    );
    if (ok) await settings.reload();
  };

  const runTest = async () => {
    setTest(null);
    await act.run(async () => {
      const r = await client.post<TestResult>('/settings/embeddings/test', {
        provider: form.provider || 'openai',
        model: form.model || undefined,
        base_url: form.base_url || undefined,
        api_key: form.api_key || undefined,
      });
      setTest(r);
    });
  };

  const applyBundled = async () => {
    const ok = await act.run(() =>
      client.post('/settings/embeddings/bundled-mode', { tier }),
    );
    if (ok) {
      await status.reload();
      await settings.reload();
    }
  };

  return (
    <Section title="Embeddings" hint="Powers semantic memory recall.">
      <div className="st-row">
        <Field label="Mode">
          <select value={form.mode} onChange={(e) => setForm({ ...form, mode: e.target.value })}>
            {EMBED_MODES.map((m) => (
              <option key={m.value} value={m.value}>
                {m.label}
              </option>
            ))}
          </select>
        </Field>
        {form.mode === 'Specific' && (
          <>
            <Field label="Provider" grow>
              <input
                type="text"
                value={form.provider}
                placeholder="openai / ollama / …"
                onChange={(e) => setForm({ ...form, provider: e.target.value })}
              />
            </Field>
            <Field label="Model" grow>
              <input
                type="text"
                value={form.model}
                onChange={(e) => setForm({ ...form, model: e.target.value })}
              />
            </Field>
            <Field label="Base URL" grow>
              <input
                type="url"
                value={form.base_url}
                onChange={(e) => setForm({ ...form, base_url: e.target.value })}
              />
            </Field>
            <Field label="API key">
              <input
                type="password"
                value={form.api_key}
                placeholder={emb?.has_api_key ? 'unchanged' : 'API key'}
                onChange={(e) => setForm({ ...form, api_key: e.target.value })}
              />
            </Field>
          </>
        )}
        {form.mode === 'Bundled' && (
          <Field label="Bundled tier">
            <select value={tier} onChange={(e) => setTier(e.target.value)}>
              <option value="light">Light (~270 MB)</option>
              <option value="standard">Standard (~530 MB)</option>
              <option value="high-quality">High quality (~1.2 GB)</option>
            </select>
          </Field>
        )}
      </div>
      <div className="st-row">
        <button type="button" className="st-btn primary" disabled={act.pending} onClick={() => void save()}>
          Save
        </button>
        {form.mode === 'Specific' && (
          <button type="button" className="st-btn" disabled={act.pending} onClick={() => void runTest()}>
            Test
          </button>
        )}
        {form.mode === 'Bundled' && (
          <button type="button" className="st-btn" disabled={act.pending} onClick={() => void applyBundled()}>
            Download &amp; apply tier
          </button>
        )}
        <TestBadge result={test} />
        {status.data && (
          <span className="st-dim">
            {status.data.downloadedTiers.length > 0
              ? `cached: ${status.data.downloadedTiers.join(', ')} (${status.data.totalCacheSizeMb} MB)`
              : 'no bundled models cached'}
          </span>
        )}
      </div>
      <ErrorText error={act.error ?? settings.error} />
    </Section>
  );
}
