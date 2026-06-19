// Projects panel: the ChatGPT/Claude-style container that groups many
// arcs around common work. Create / edit / delete projects, set
// per-project instructions, drive the project summary (mode + manual
// refresh), and assign arcs to a project.
//
// Wire shapes: athen-app/src/commands.rs (ProjectInput / project_*_core)
// + http_api.rs routes under /api/projects. A Project owns a workspace
// folder (folder_slug) that is preserved on delete.

import { useState } from 'react';
import type { AthenClient } from '../api/client';
import type { ArcMeta } from '../api/types';
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

interface Project {
  id: string;
  name: string;
  folder_slug: string;
  instructions: string | null;
  summary: string | null;
  summary_updated_at: string | null;
  created_at: string;
  updated_at: string;
}

type SummaryMode = 'auto' | 'manual' | 'off';

interface ProjectForm {
  id: string;
  name: string;
  instructions: string;
  isNew: boolean;
}

function fmtTime(iso: string | null): string | null {
  if (!iso) return null;
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  return d.toLocaleString();
}

export function PanelProjects({ client }: { client: AthenClient }) {
  const projects = useLoad(() => client.get<Project[]>('/projects'), [client]);
  const mode = useLoad(() => client.get<SummaryMode>('/projects/summary-mode'), [client]);
  const act = useAction();

  const [form, setForm] = useState<ProjectForm | null>(null);
  // Per-project pending state for the (possibly slow) summary refresh.
  const [summarizing, setSummarizing] = useState<string | null>(null);

  const open = (p: Project | null) => {
    if (!p) {
      setForm({ id: '', name: '', instructions: '', isNew: true });
      return;
    }
    setForm({ id: p.id, name: p.name, instructions: p.instructions ?? '', isNew: false });
  };

  const save = async () => {
    if (!form) return;
    const name = form.name.trim();
    if (!name) return;
    const instr = form.instructions.trim();
    let ok: boolean;
    if (form.isNew) {
      ok = await act.run(() => client.post('/projects', { name, instructions: instr || undefined }));
    } else {
      // Empty box ⇒ clear instructions; otherwise update them.
      const body = instr
        ? { name, instructions: instr }
        : { name, clear_instructions: true };
      ok = await act.run(() => client.post(`/projects/${encodeURIComponent(form.id)}`, body));
    }
    if (ok) {
      setForm(null);
      await projects.reload();
    }
  };

  const remove = async (id: string) => {
    const ok = await act.run(() => client.post(`/projects/${encodeURIComponent(id)}/delete`));
    if (ok) {
      if (form?.id === id) setForm(null);
      await projects.reload();
    }
  };

  const refreshSummary = async (id: string) => {
    setSummarizing(id);
    const ok = await act.run(() => client.post(`/projects/${encodeURIComponent(id)}/summary`));
    setSummarizing(null);
    if (ok) await projects.reload();
  };

  const setMode = async (m: SummaryMode) => {
    const ok = await act.run(() => client.post('/projects/summary-mode', { mode: m }));
    if (ok) await mode.reload();
  };

  return (
    <>
      <Section
        title="Projects"
        hint="Containers that group many arcs around common work — shared instructions, a workspace folder, and a maintained summary."
        actions={
          <button type="button" className="st-btn" onClick={() => open(null)}>
            + New project
          </button>
        }
      >
        <div className="st-row" style={{ alignItems: 'center', gap: 10, marginBottom: 8 }}>
          <Field label="Summary mode">
            <select
              value={mode.data ?? 'auto'}
              disabled={mode.loading || act.pending}
              onChange={(e) => void setMode(e.target.value as SummaryMode)}
            >
              <option value="auto">Auto (maintained on arc switch)</option>
              <option value="manual">Manual (refresh on demand)</option>
              <option value="off">Off</option>
            </select>
          </Field>
        </div>

        {projects.loading && <Loading />}
        <ErrorText error={projects.error ?? mode.error} />
        {!projects.loading && (projects.data ?? []).length === 0 && (
          <div className="st-dim">No projects yet. Create one to group related arcs.</div>
        )}

        <div className="st-list">
          {(projects.data ?? []).map((p) => (
            <div key={p.id} className={`st-item${form?.id === p.id ? ' selected' : ''}`}>
              <div className="st-item-main">
                <div className="st-item-title">{p.name}</div>
                <div className="st-item-sub st-mono">{p.folder_slug}</div>
                {p.instructions && (
                  <div
                    className="st-item-sub"
                    style={{ color: 'var(--text)', whiteSpace: 'pre-wrap', marginTop: 4 }}
                  >
                    {p.instructions}
                  </div>
                )}
                {p.summary && (
                  <div className="st-item-sub" style={{ marginTop: 6 }}>
                    <span className="st-badge">summary</span>{' '}
                    {fmtTime(p.summary_updated_at) && (
                      <span className="st-dim">· {fmtTime(p.summary_updated_at)}</span>
                    )}
                    <div style={{ whiteSpace: 'pre-wrap', marginTop: 3 }}>{p.summary}</div>
                  </div>
                )}
              </div>
              <div className="st-item-actions">
                <button type="button" className="st-btn small" onClick={() => open(p)}>
                  Edit
                </button>
                <button
                  type="button"
                  className="st-btn small"
                  disabled={act.pending}
                  onClick={() => void refreshSummary(p.id)}
                >
                  {summarizing === p.id ? 'Summarizing…' : 'Update summary'}
                </button>
                <button
                  type="button"
                  className="st-btn small"
                  disabled={act.pending}
                  onClick={() => void act.run(() => client.post('/active-project', { value: p.id }))}
                >
                  Set active
                </button>
                <ConfirmButton
                  label="Delete"
                  className="small"
                  onConfirm={() => void remove(p.id)}
                />
              </div>
            </div>
          ))}
        </div>

        {form && (
          <>
            <hr className="st-divider" />
            <Field label="Project name" grow>
              <input
                type="text"
                value={form.name}
                placeholder="Q3 launch"
                onChange={(e) => setForm({ ...form, name: e.target.value })}
              />
            </Field>
            <Field label="Instructions (shared context for every arc in this project)" grow>
              <textarea
                rows={5}
                value={form.instructions}
                onChange={(e) => setForm({ ...form, instructions: e.target.value })}
              />
            </Field>
            <div className="st-row">
              <button
                type="button"
                className="st-btn primary"
                disabled={act.pending || !form.name.trim()}
                onClick={() => void save()}
              >
                {form.isNew ? 'Create project' : 'Save project'}
              </button>
              <button type="button" className="st-btn" onClick={() => setForm(null)}>
                Cancel
              </button>
              {!form.isNew && (
                <span className="st-dim">
                  Clearing the instructions box and saving removes them. Deleting a project keeps its
                  workspace folder on disk.
                </span>
              )}
            </div>
          </>
        )}
        <ErrorText error={act.error} />
      </Section>

      <ArcAssignment client={client} projects={projects.data ?? []} />
    </>
  );
}

// ---------------------------------------------------------------------------
// Arc assignment
//
// Shell.tsx (the arc rail) is owned by a concurrent process, so per-arc
// project assignment lives here: pick a project, then assign any arc to
// it. Membership can't be read back from `GET /api/arcs` (ArcMeta has no
// project_id yet), so this is a one-way "assign" surface; richer
// in-rail assignment is a follow-up.
// ---------------------------------------------------------------------------

function ArcAssignment({ client, projects }: { client: AthenClient; projects: Project[] }) {
  const arcs = useLoad(() => client.listArcs(), [client]);
  const act = useAction();
  const [target, setTarget] = useState<string>('');

  const assign = async (arcId: string, value: string | null) => {
    await act.run(() => client.post(`/arcs/${encodeURIComponent(arcId)}/project`, { value }));
  };

  return (
    <Section
      title="Assign arcs"
      hint="Add an existing arc to a project, or detach it. New arcs inherit the active project automatically."
      actions={
        <button type="button" className="st-btn" disabled={arcs.loading} onClick={() => void arcs.reload()}>
          Refresh
        </button>
      }
    >
      <div className="st-row" style={{ alignItems: 'center', marginBottom: 8 }}>
        <Field label="Project">
          <select value={target} onChange={(e) => setTarget(e.target.value)}>
            <option value="">(choose a project)</option>
            {projects.map((p) => (
              <option key={p.id} value={p.id}>
                {p.name}
              </option>
            ))}
          </select>
        </Field>
      </div>

      {arcs.loading && <Loading />}
      <ErrorText error={arcs.error} />
      {!arcs.loading && (arcs.data ?? []).length === 0 && (
        <div className="st-dim">No arcs yet.</div>
      )}

      <div className="st-list">
        {((arcs.data ?? []) as ArcMeta[]).map((a) => (
          <div key={a.id} className="st-item">
            <div className="st-item-main">
              <div className="st-item-title">{a.name || '(untitled arc)'}</div>
              <div className="st-item-sub st-mono">{a.source}</div>
            </div>
            <div className="st-item-actions">
              <button
                type="button"
                className="st-btn small primary"
                disabled={act.pending || !target}
                onClick={() => void assign(a.id, target)}
              >
                Add to project
              </button>
              <button
                type="button"
                className="st-btn small"
                disabled={act.pending}
                onClick={() => void assign(a.id, null)}
              >
                Detach
              </button>
            </div>
          </div>
        ))}
      </div>
      <ErrorText error={act.error} />
    </Section>
  );
}
