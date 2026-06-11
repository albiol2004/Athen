// Agents & Tools panel: agent profiles, skills, identity store, MCP
// servers. Body shapes: commands.rs (AgentProfileInput, SkillInput,
// IdentityCategoryInput, IdentityEntryInput, McpAddBody…).

import { useState } from 'react';
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

// ---- wire shapes ----

type ProfileTag = 'Always' | { Profile: string } | { NotProfile: string };

interface AgentProfile {
  id: string;
  display_name: string;
  description: string;
  custom_persona_addendum: string | null;
  tool_selection: unknown;
  primary_groups: string[];
  expertise: unknown;
  model_profile_hint: string | null;
  github_identity: 'none' | 'bot' | 'user';
  builtin: boolean;
}

interface ProfileTokenEstimate {
  approx_tokens: number;
  tool_count_available: number;
  tool_count_revealed: number;
}

interface Skill {
  slug: string;
  name: string;
  description: string;
  applies_to: ProfileTag[];
  source: string;
}

interface SkillDetail extends Skill {
  body: string;
}

interface IdentityCategory {
  name: string;
  description: string;
  default_applies_to: ProfileTag[];
  sort_order: number;
  is_seed: boolean;
}

interface IdentityEntry {
  id: string;
  category: string;
  body: string;
  applies_to: ProfileTag[];
  pinned: boolean;
  proposed_by_agent: boolean;
}

interface IdentityEstimate {
  entry_count: number;
  approx_tokens: number;
}

interface CatalogEntryView {
  id: string;
  display_name: string;
  description: string;
  enabled: boolean;
  config: unknown;
}

interface McpCatalogEntry {
  id: string;
  display_name: string;
  description: string;
  icon: string | null;
  config_schema: unknown;
  source: McpSource;
  base_risk?: string;
  tool_risks?: Record<string, string>;
}

type McpSource =
  | { kind: 'bundled'; binary_name: string }
  | { kind: 'download'; url: string; binary_name: string }
  | {
      kind: 'process';
      command: string;
      args?: string[];
      env?: { key: string; value: { kind: 'plain'; value: string } | { kind: 'vault'; scope: string; key: string } }[];
      working_dir?: string | null;
    };

interface McpToolView {
  name: string;
  description: string | null;
  base_risk: string;
}

interface McpTestSpawnResult {
  tool_count: number;
  tool_names: string[];
}

type SubTab = 'profiles' | 'skills' | 'identity' | 'mcp';

export function PanelAgents({ client }: { client: AthenClient }) {
  const [sub, setSub] = useState<SubTab>('profiles');
  return (
    <>
      <div className="st-subtabs">
        {(
          [
            ['profiles', 'Profiles'],
            ['skills', 'Skills'],
            ['identity', 'Identity'],
            ['mcp', 'MCP servers'],
          ] as [SubTab, string][]
        ).map(([id, label]) => (
          <button
            key={id}
            type="button"
            className={`st-subtab${sub === id ? ' active' : ''}`}
            onClick={() => setSub(id)}
          >
            {label}
          </button>
        ))}
      </div>
      {sub === 'profiles' && <ProfilesSection client={client} />}
      {sub === 'skills' && <SkillsSection client={client} />}
      {sub === 'identity' && <IdentitySection client={client} />}
      {sub === 'mcp' && <McpSection client={client} />}
    </>
  );
}

// ---------------------------------------------------------------------------
// Profiles
// ---------------------------------------------------------------------------

interface ProfileForm {
  id: string;
  display_name: string;
  description: string;
  custom_persona_addendum: string;
  model_profile_hint: string;
  github_identity: 'none' | 'bot' | 'user';
  isNew: boolean;
  /** Pass-through fields preserved on update. */
  base: AgentProfile | null;
}

function ProfilesSection({ client }: { client: AthenClient }) {
  const profiles = useLoad(() => client.get<AgentProfile[]>('/profiles'), [client]);
  const act = useAction();
  const [form, setForm] = useState<ProfileForm | null>(null);
  const [tokens, setTokens] = useState<ProfileTokenEstimate | null>(null);

  const open = (p: AgentProfile | null) => {
    setTokens(null);
    if (!p) {
      setForm({
        id: '',
        display_name: '',
        description: '',
        custom_persona_addendum: '',
        model_profile_hint: '',
        github_identity: 'none',
        isNew: true,
        base: null,
      });
      return;
    }
    setForm({
      id: p.id,
      display_name: p.display_name,
      description: p.description,
      custom_persona_addendum: p.custom_persona_addendum ?? '',
      model_profile_hint: p.model_profile_hint ?? '',
      github_identity: p.github_identity ?? 'none',
      isNew: false,
      base: p,
    });
    void client
      .get<ProfileTokenEstimate>(`/profiles/${encodeURIComponent(p.id)}/tokens`)
      .then(setTokens)
      .catch(() => setTokens(null));
  };

  const save = async () => {
    if (!form) return;
    const input = {
      id: form.id.trim(),
      display_name: form.display_name.trim(),
      description: form.description,
      custom_persona_addendum: form.custom_persona_addendum || null,
      tool_selection: form.base?.tool_selection ?? undefined,
      primary_groups: form.base?.primary_groups ?? [],
      expertise: form.base?.expertise ?? undefined,
      model_profile_hint: form.model_profile_hint || null,
      github_identity: form.github_identity,
    };
    const ok = await act.run(() =>
      client.post(form.isNew ? '/profiles' : '/profiles/update', input),
    );
    if (ok) {
      setForm(null);
      await profiles.reload();
    }
  };

  const remove = async (id: string) => {
    const ok = await act.run(() => client.post(`/profiles/${encodeURIComponent(id)}/delete`));
    if (ok) {
      if (form?.id === id) setForm(null);
      await profiles.reload();
    }
  };

  const restore = async (id: string) => {
    const ok = await act.run(() => client.post(`/profiles/${encodeURIComponent(id)}/restore`));
    if (ok) await profiles.reload();
  };

  return (
    <Section
      title="Agent profiles"
      hint="Who the agent is for a given arc: persona, tool prominence, model hint."
      actions={
        <button type="button" className="st-btn" onClick={() => open(null)}>
          + New profile
        </button>
      }
    >
      {profiles.loading && <Loading />}
      <ErrorText error={profiles.error} />
      <div className="st-list">
        {(profiles.data ?? []).map((p) => (
          <div key={p.id} className={`st-item${form?.id === p.id ? ' selected' : ''}`}>
            <div className="st-item-main">
              <div className="st-item-title">
                {p.display_name}
                {p.builtin && <span className="st-badge">built-in</span>}
              </div>
              <div className="st-item-sub">{p.description}</div>
            </div>
            <div className="st-item-actions">
              <button type="button" className="st-btn small" onClick={() => open(p)}>
                Edit
              </button>
              {p.builtin ? (
                <button
                  type="button"
                  className="st-btn small"
                  disabled={act.pending}
                  onClick={() => void restore(p.id)}
                >
                  Restore
                </button>
              ) : (
                <ConfirmButton label="Delete" className="small" onConfirm={() => void remove(p.id)} />
              )}
            </div>
          </div>
        ))}
      </div>
      {form && (
        <>
          <hr className="st-divider" />
          <div className="st-row">
            <Field label="Profile id" grow>
              <input
                type="text"
                value={form.id}
                readOnly={!form.isNew}
                placeholder="my_profile"
                onChange={(e) => setForm({ ...form, id: e.target.value })}
              />
            </Field>
            <Field label="Display name" grow>
              <input
                type="text"
                value={form.display_name}
                onChange={(e) => setForm({ ...form, display_name: e.target.value })}
              />
            </Field>
            <Field label="Model hint">
              <select
                value={form.model_profile_hint}
                onChange={(e) => setForm({ ...form, model_profile_hint: e.target.value })}
              >
                <option value="">(none)</option>
                <option value="Cheap">Cheap</option>
                <option value="Fast">Fast</option>
                <option value="Code">Code</option>
                <option value="Powerful">Powerful</option>
              </select>
            </Field>
            <Field label="GitHub identity">
              <select
                value={form.github_identity}
                onChange={(e) =>
                  setForm({ ...form, github_identity: e.target.value as ProfileForm['github_identity'] })
                }
              >
                <option value="none">None</option>
                <option value="bot">Bot</option>
                <option value="user">User</option>
              </select>
            </Field>
          </div>
          <Field label="Description" grow>
            <input
              type="text"
              value={form.description}
              onChange={(e) => setForm({ ...form, description: e.target.value })}
            />
          </Field>
          <Field label="Persona addendum (appended to the system prompt)" grow>
            <textarea
              rows={5}
              value={form.custom_persona_addendum}
              onChange={(e) => setForm({ ...form, custom_persona_addendum: e.target.value })}
            />
          </Field>
          <div className="st-row">
            <button type="button" className="st-btn primary" disabled={act.pending} onClick={() => void save()}>
              {form.isNew ? 'Create profile' : 'Save profile'}
            </button>
            <button type="button" className="st-btn" onClick={() => setForm(null)}>
              Cancel
            </button>
            {tokens && (
              <span className="st-dim">
                static prefix ≈ {tokens.approx_tokens.toLocaleString()} tokens ·{' '}
                {tokens.tool_count_revealed}/{tokens.tool_count_available} tools revealed
              </span>
            )}
          </div>
        </>
      )}
      <ErrorText error={act.error} />
    </Section>
  );
}

// ---------------------------------------------------------------------------
// Skills
// ---------------------------------------------------------------------------

interface SkillForm {
  slug: string;
  name: string;
  description: string;
  body: string;
  applies_to: ProfileTag[];
  isNew: boolean;
}

function SkillsSection({ client }: { client: AthenClient }) {
  const skills = useLoad(() => client.get<Skill[]>('/skills'), [client]);
  const act = useAction();
  const [form, setForm] = useState<SkillForm | null>(null);

  const openEdit = async (slug: string) => {
    await act.run(async () => {
      const d = await client.get<SkillDetail | null>(`/skills/${encodeURIComponent(slug)}`);
      if (!d) throw new Error('Skill not found on disk — try Sync.');
      setForm({
        slug: d.slug,
        name: d.name,
        description: d.description,
        body: d.body,
        applies_to: d.applies_to,
        isNew: false,
      });
    });
  };

  const save = async () => {
    if (!form) return;
    const ok = await act.run(() =>
      client.post('/skills', {
        slug: form.slug.trim(),
        name: form.name.trim(),
        description: form.description.trim(),
        applies_to: form.applies_to,
        body: form.body,
      }),
    );
    if (ok) {
      setForm(null);
      await skills.reload();
    }
  };

  const remove = async (slug: string) => {
    const ok = await act.run(() => client.post(`/skills/${encodeURIComponent(slug)}/delete`));
    if (ok) {
      if (form?.slug === slug) setForm(null);
      await skills.reload();
    }
  };

  return (
    <Section
      title="Skills"
      hint="Procedural playbooks the agent loads on demand via load_skill."
      actions={
        <>
          <button
            type="button"
            className="st-btn"
            disabled={act.pending}
            onClick={() => void act.run(() => client.post('/skills/sync')).then((ok) => { if (ok) void skills.reload(); })}
          >
            Sync from disk
          </button>
          <button
            type="button"
            className="st-btn"
            onClick={() =>
              setForm({ slug: '', name: '', description: '', body: '', applies_to: ['Always'], isNew: true })
            }
          >
            + New skill
          </button>
        </>
      }
    >
      {skills.loading && <Loading />}
      <ErrorText error={skills.error} />
      {!skills.loading && (skills.data ?? []).length === 0 && (
        <div className="st-dim">No skills yet.</div>
      )}
      <div className="st-list">
        {(skills.data ?? []).map((s) => (
          <div key={s.slug} className={`st-item${form?.slug === s.slug ? ' selected' : ''}`}>
            <div className="st-item-main">
              <div className="st-item-title">
                {s.name}
                <span className="st-badge">{s.source}</span>
              </div>
              <div className="st-item-sub">
                <span className="st-mono">{s.slug}</span> — {s.description}
              </div>
            </div>
            <div className="st-item-actions">
              <button type="button" className="st-btn small" onClick={() => void openEdit(s.slug)}>
                Edit
              </button>
              <ConfirmButton label="Delete" className="small" onConfirm={() => void remove(s.slug)} />
            </div>
          </div>
        ))}
      </div>
      {form && (
        <>
          <hr className="st-divider" />
          <div className="st-row">
            <Field label="Slug (folder name)" grow>
              <input
                type="text"
                value={form.slug}
                readOnly={!form.isNew}
                placeholder="my-skill"
                onChange={(e) => setForm({ ...form, slug: e.target.value })}
              />
            </Field>
            <Field label="Name" grow>
              <input
                type="text"
                value={form.name}
                onChange={(e) => setForm({ ...form, name: e.target.value })}
              />
            </Field>
          </div>
          <Field label="Description (one sentence — what the model sees)" grow>
            <input
              type="text"
              value={form.description}
              onChange={(e) => setForm({ ...form, description: e.target.value })}
            />
          </Field>
          <Field label="Body (SKILL.md markdown)" grow>
            <textarea
              rows={10}
              className="st-mono"
              value={form.body}
              onChange={(e) => setForm({ ...form, body: e.target.value })}
            />
          </Field>
          <div className="st-row">
            <button type="button" className="st-btn primary" disabled={act.pending} onClick={() => void save()}>
              Save skill
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

// ---------------------------------------------------------------------------
// Identity
// ---------------------------------------------------------------------------

function IdentitySection({ client }: { client: AthenClient }) {
  const cats = useLoad(() => client.get<IdentityCategory[]>('/identity/categories'), [client]);
  const entries = useLoad(() => client.get<IdentityEntry[]>('/identity/entries'), [client]);
  const estimate = useLoad(() => client.get<IdentityEstimate>('/identity/estimate'), [client]);
  const act = useAction();
  const [newCat, setNewCat] = useState('');
  const [drafts, setDrafts] = useState<Record<string, string>>({});
  const [editing, setEditing] = useState<{ id: string; body: string } | null>(null);

  const refresh = async () => {
    await Promise.all([cats.reload(), entries.reload(), estimate.reload()]);
  };

  const addCategory = async () => {
    const name = newCat.trim();
    if (!name) return;
    const maxSort = Math.max(0, ...(cats.data ?? []).map((c) => c.sort_order));
    const ok = await act.run(() =>
      client.post('/identity/categories', {
        name,
        description: '',
        default_applies_to: ['Always'],
        sort_order: maxSort + 1,
      }),
    );
    if (ok) {
      setNewCat('');
      await refresh();
    }
  };

  const addEntry = async (cat: IdentityCategory) => {
    const body = (drafts[cat.name] ?? '').trim();
    if (!body) return;
    const applies = cat.default_applies_to.length > 0 ? cat.default_applies_to : ['Always' as ProfileTag];
    const ok = await act.run(() =>
      client.post('/identity/entries', { category: cat.name, body, applies_to: applies, pinned: false }),
    );
    if (ok) {
      setDrafts((d) => ({ ...d, [cat.name]: '' }));
      await refresh();
    }
  };

  const saveEdit = async (entry: IdentityEntry) => {
    if (!editing) return;
    const ok = await act.run(() =>
      client.post('/identity/entries', {
        id: entry.id,
        category: entry.category,
        body: editing.body,
        applies_to: entry.applies_to,
        pinned: entry.pinned,
      }),
    );
    if (ok) {
      setEditing(null);
      await refresh();
    }
  };

  const removeEntry = async (id: string) => {
    const ok = await act.run(() => client.post(`/identity/entries/${encodeURIComponent(id)}/delete`));
    if (ok) await refresh();
  };

  const removeCategory = async (name: string) => {
    const ok = await act.run(() =>
      client.post(`/identity/categories/${encodeURIComponent(name)}/delete`),
    );
    if (ok) await refresh();
  };

  const byCat = new Map<string, IdentityEntry[]>();
  for (const e of entries.data ?? []) {
    const list = byCat.get(e.category) ?? [];
    list.push(e);
    byCat.set(e.category, list);
  }

  return (
    <Section
      title="Identity"
      hint="Who Athen is — always-on facts and rules injected into every agent's static prefix."
      actions={
        <>
          {estimate.data && (
            <span className="st-dim">
              {estimate.data.entry_count} entries ≈ {estimate.data.approx_tokens.toLocaleString()} tokens
            </span>
          )}
          <input
            type="text"
            placeholder="New category"
            value={newCat}
            onChange={(e) => setNewCat(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === 'Enter') void addCategory();
            }}
            style={{
              font: 'inherit',
              fontSize: 12.5,
              color: 'var(--text)',
              background: 'rgba(0,0,0,.25)',
              border: '1px solid var(--glass-border)',
              borderRadius: 9,
              padding: '6px 10px',
              width: 130,
            }}
          />
          <button type="button" className="st-btn" disabled={!newCat.trim() || act.pending} onClick={() => void addCategory()}>
            Add
          </button>
        </>
      }
    >
      {(cats.loading || entries.loading) && <Loading />}
      <ErrorText error={cats.error ?? entries.error} />
      {(cats.data ?? [])
        .slice()
        .sort((a, b) => a.sort_order - b.sort_order)
        .map((cat) => (
          <div key={cat.name}>
            <div className="st-row" style={{ alignItems: 'center', justifyContent: 'space-between' }}>
              <div className="st-item-title">
                {cat.name}
                {cat.is_seed && <span className="st-badge">seed</span>}
              </div>
              {!cat.is_seed && (
                <ConfirmButton
                  label="Delete category"
                  className="small"
                  onConfirm={() => void removeCategory(cat.name)}
                />
              )}
            </div>
            <div className="st-list" style={{ marginTop: 6 }}>
              {(byCat.get(cat.name) ?? []).map((e) => (
                <div key={e.id} className="st-item">
                  {editing?.id === e.id ? (
                    <>
                      <div className="st-item-main">
                        <textarea
                          rows={3}
                          style={{
                            width: '100%',
                            font: 'inherit',
                            color: 'var(--text)',
                            background: 'rgba(0,0,0,.25)',
                            border: '1px solid var(--glass-border)',
                            borderRadius: 9,
                            padding: '7px 10px',
                          }}
                          value={editing.body}
                          onChange={(ev) => setEditing({ id: e.id, body: ev.target.value })}
                        />
                      </div>
                      <div className="st-item-actions">
                        <button type="button" className="st-btn small primary" disabled={act.pending} onClick={() => void saveEdit(e)}>
                          Save
                        </button>
                        <button type="button" className="st-btn small" onClick={() => setEditing(null)}>
                          Cancel
                        </button>
                      </div>
                    </>
                  ) : (
                    <>
                      <div className="st-item-main">
                        <div className="st-item-sub" style={{ color: 'var(--text)', whiteSpace: 'pre-wrap' }}>
                          {e.body}
                        </div>
                        {e.proposed_by_agent && <span className="st-badge amber">agent-proposed</span>}
                      </div>
                      <div className="st-item-actions">
                        <button type="button" className="st-btn small" onClick={() => setEditing({ id: e.id, body: e.body })}>
                          Edit
                        </button>
                        <ConfirmButton label="Delete" className="small" onConfirm={() => void removeEntry(e.id)} />
                      </div>
                    </>
                  )}
                </div>
              ))}
              <div className="st-row">
                <Field label={`Add to ${cat.name}`} grow>
                  <textarea
                    rows={2}
                    value={drafts[cat.name] ?? ''}
                    onChange={(e) => setDrafts((d) => ({ ...d, [cat.name]: e.target.value }))}
                  />
                </Field>
                <button
                  type="button"
                  className="st-btn"
                  disabled={act.pending || !(drafts[cat.name] ?? '').trim()}
                  onClick={() => void addEntry(cat)}
                >
                  Add entry
                </button>
              </div>
            </div>
            <hr className="st-divider" />
          </div>
        ))}
      <ErrorText error={act.error} />
    </Section>
  );
}

// ---------------------------------------------------------------------------
// MCP
// ---------------------------------------------------------------------------

interface McpForm {
  id: string;
  display_name: string;
  command: string;
  args: string;
  env: string;
  enable_now: boolean;
}

function buildEntry(form: McpForm): { entry: McpCatalogEntry; env_secrets: Record<string, string> } {
  const env_secrets: Record<string, string> = {};
  const bindings: NonNullable<Extract<McpSource, { kind: 'process' }>['env']> = [];
  for (const line of form.env.split('\n')) {
    const t = line.trim();
    if (!t) continue;
    const eq = t.indexOf('=');
    const key = eq === -1 ? t : t.slice(0, eq).trim();
    const value = eq === -1 ? '' : t.slice(eq + 1);
    if (!key) continue;
    // Treat all values as secrets → vault-backed bindings; the backend
    // routes the raw value through env_secrets into the vault (same
    // shape the desktop frontend sends, scope filled server-side).
    bindings.push({ key, value: { kind: 'vault', scope: '', key } });
    env_secrets[key] = value;
  }
  const entry: McpCatalogEntry = {
    id: form.id.trim(),
    display_name: form.display_name.trim() || form.id.trim(),
    description: '',
    icon: null,
    config_schema: {},
    source: {
      kind: 'process',
      command: form.command.trim(),
      args: form.args.trim() ? form.args.trim().split(/\s+/) : [],
      env: bindings,
    },
    base_risk: 'WritePersist',
    tool_risks: {},
  };
  return { entry, env_secrets };
}

function McpSection({ client }: { client: AthenClient }) {
  const catalog = useLoad(() => client.get<CatalogEntryView[]>('/mcp/catalog'), [client]);
  const custom = useLoad(() => client.get<McpCatalogEntry[]>('/mcp/custom'), [client]);
  const act = useAction();
  const [form, setForm] = useState<McpForm | null>(null);
  const [spawnResult, setSpawnResult] = useState<McpTestSpawnResult | null>(null);
  const [tools, setTools] = useState<{ id: string; list: McpToolView[] } | null>(null);

  const refresh = async () => {
    await Promise.all([catalog.reload(), custom.reload()]);
  };

  const toggleBuiltin = async (e: CatalogEntryView) => {
    const ok = await act.run(() =>
      e.enabled
        ? client.post(`/mcp/${encodeURIComponent(e.id)}/disable`)
        : client.post(`/mcp/${encodeURIComponent(e.id)}/enable`, { config: e.config ?? {} }),
    );
    if (ok) await refresh();
  };

  const addCustom = async () => {
    if (!form) return;
    const { entry, env_secrets } = buildEntry(form);
    const ok = await act.run(() =>
      client.post('/mcp/custom', { entry, env_secrets, enable_now: form.enable_now }),
    );
    if (ok) {
      setForm(null);
      setSpawnResult(null);
      await refresh();
    }
  };

  const testSpawn = async () => {
    if (!form) return;
    setSpawnResult(null);
    const { entry, env_secrets } = buildEntry(form);
    await act.run(async () => {
      const r = await client.post<McpTestSpawnResult>('/mcp/test-spawn', { entry, env_secrets });
      setSpawnResult(r);
    });
  };

  const removeCustom = async (id: string) => {
    const ok = await act.run(() => client.post(`/mcp/custom/${encodeURIComponent(id)}/remove`));
    if (ok) await refresh();
  };

  const showTools = async (id: string) => {
    if (tools?.id === id) {
      setTools(null);
      return;
    }
    await act.run(async () => {
      const list = await client.get<McpToolView[]>(`/mcp/${encodeURIComponent(id)}/tools`);
      setTools({ id, list });
    });
  };

  return (
    <>
      <Section title="Built-in MCPs" hint="Bundled tool servers shipped with Athen.">
        {catalog.loading && <Loading />}
        <ErrorText error={catalog.error} />
        <div className="st-list">
          {(catalog.data ?? []).map((e) => (
            <div key={e.id} className="st-item">
              <div className="st-item-main">
                <div className="st-item-title">
                  {e.display_name}
                  {e.enabled && <span className="st-badge green">enabled</span>}
                </div>
                <div className="st-item-sub">{e.description}</div>
              </div>
              <div className="st-item-actions">
                <button type="button" className="st-btn small" disabled={act.pending} onClick={() => void toggleBuiltin(e)}>
                  {e.enabled ? 'Disable' : 'Enable'}
                </button>
              </div>
            </div>
          ))}
        </div>
      </Section>
      <Section
        title="Custom MCP servers"
        hint="Bring-your-own stdio MCP servers (Claude Desktop / Cursor compatible)."
        actions={
          <button
            type="button"
            className="st-btn"
            onClick={() => {
              setSpawnResult(null);
              setForm({ id: '', display_name: '', command: '', args: '', env: '', enable_now: true });
            }}
          >
            + Add server
          </button>
        }
      >
        {custom.loading && <Loading />}
        <ErrorText error={custom.error} />
        {!custom.loading && (custom.data ?? []).length === 0 && (
          <div className="st-dim">No custom servers.</div>
        )}
        <div className="st-list">
          {(custom.data ?? []).map((e) => (
            <div key={e.id} className="st-item">
              <div className="st-item-main">
                <div className="st-item-title">{e.display_name}</div>
                <div className="st-item-sub st-mono">
                  {e.source.kind === 'process'
                    ? `${e.source.command} ${(e.source.args ?? []).join(' ')}`
                    : e.source.kind}
                </div>
                {tools?.id === e.id && (
                  <div className="st-item-sub" style={{ marginTop: 6 }}>
                    {tools.list.length === 0
                      ? 'No tools advertised.'
                      : tools.list.map((t) => (
                          <div key={t.name}>
                            <span className="st-mono">{t.name}</span>
                            {t.description ? ` — ${t.description}` : ''}{' '}
                            <span className="st-badge">{t.base_risk}</span>
                          </div>
                        ))}
                  </div>
                )}
              </div>
              <div className="st-item-actions">
                <button type="button" className="st-btn small" disabled={act.pending} onClick={() => void showTools(e.id)}>
                  {tools?.id === e.id ? 'Hide tools' : 'Tools'}
                </button>
                <ConfirmButton label="Remove" className="small" onConfirm={() => void removeCustom(e.id)} />
              </div>
            </div>
          ))}
        </div>
        {form && (
          <>
            <hr className="st-divider" />
            <div className="st-row">
              <Field label="Id" grow>
                <input
                  type="text"
                  value={form.id}
                  placeholder="github"
                  onChange={(e) => setForm({ ...form, id: e.target.value })}
                />
              </Field>
              <Field label="Display name" grow>
                <input
                  type="text"
                  value={form.display_name}
                  onChange={(e) => setForm({ ...form, display_name: e.target.value })}
                />
              </Field>
            </div>
            <div className="st-row">
              <Field label="Command" grow>
                <input
                  type="text"
                  className="st-mono"
                  value={form.command}
                  placeholder="npx"
                  onChange={(e) => setForm({ ...form, command: e.target.value })}
                />
              </Field>
              <Field label="Arguments (space-separated)" grow>
                <input
                  type="text"
                  className="st-mono"
                  value={form.args}
                  placeholder="-y @modelcontextprotocol/server-github"
                  onChange={(e) => setForm({ ...form, args: e.target.value })}
                />
              </Field>
            </div>
            <Field label="Environment secrets (KEY=value per line, stored in the vault)" grow>
              <textarea
                rows={3}
                className="st-mono"
                value={form.env}
                onChange={(e) => setForm({ ...form, env: e.target.value })}
              />
            </Field>
            <div className="st-row">
              <label className="st-check">
                <input
                  type="checkbox"
                  checked={form.enable_now}
                  onChange={(e) => setForm({ ...form, enable_now: e.target.checked })}
                />
                Enable immediately
              </label>
              <button type="button" className="st-btn" disabled={act.pending || !form.command.trim()} onClick={() => void testSpawn()}>
                Test spawn
              </button>
              <button
                type="button"
                className="st-btn primary"
                disabled={act.pending || !form.id.trim() || !form.command.trim()}
                onClick={() => void addCustom()}
              >
                Save server
              </button>
              <button type="button" className="st-btn" onClick={() => setForm(null)}>
                Cancel
              </button>
              {spawnResult && (
                <span className="st-test ok">
                  {spawnResult.tool_count} tools: {spawnResult.tool_names.slice(0, 8).join(', ')}
                  {spawnResult.tool_names.length > 8 ? '…' : ''}
                </span>
              )}
            </div>
          </>
        )}
        <ErrorText error={act.error} />
      </Section>
    </>
  );
}
