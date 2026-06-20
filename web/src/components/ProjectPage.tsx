// Project page — the main-pane view for a single Project, mirroring the
// desktop app. Header (name, back-to-chat, rename, delete, new-arc-in-project)
// + four tabs: Overview (instructions/summary/summary-mode), Arcs (member
// arcs, each opens in chat), Files (workspace folder listing, read-only),
// Memories (project-scoped memories, read-only).
//
// Wire shapes live in api/client.ts (project* methods) and api/types.ts.
// Member arcs = GET /api/arcs filtered by `project_id === project.id`.

import { useCallback, useEffect, useState } from 'react';
import type { AthenClient } from '../api/client';
import type {
  ArcMeta,
  MemoryInfo,
  Project,
  ProjectFileInfo,
  SummaryMode,
} from '../api/types';

function fmtTime(iso: string | null | undefined): string | null {
  if (!iso) return null;
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  return d.toLocaleString();
}

function fmtBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  const units = ['KB', 'MB', 'GB', 'TB'];
  let v = n / 1024;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i += 1;
  }
  return `${v < 10 ? v.toFixed(1) : Math.round(v)} ${units[i]}`;
}

function errMsg(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

type Tab = 'overview' | 'arcs' | 'files' | 'memories';

export function ProjectPage({
  client,
  project,
  onBack,
  onChanged,
  onOpenArc,
  onNewArcInProject,
}: {
  client: AthenClient;
  project: Project;
  /** Return to the chat surface. */
  onBack: () => void;
  /** Refresh the project (and the sidebar list) after a mutation. */
  onChanged: () => void;
  /** Open one of the project's arcs in the chat surface. */
  onOpenArc: (arcId: string) => void;
  /** Create a fresh arc that inherits this project, then leave to chat. */
  onNewArcInProject: (projectId: string) => void;
}) {
  const [tab, setTab] = useState<Tab>('overview');
  const [renaming, setRenaming] = useState(false);
  const [nameDraft, setNameDraft] = useState(project.name);
  const [confirmDelete, setConfirmDelete] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    setNameDraft(project.name);
    setRenaming(false);
    setConfirmDelete(false);
  }, [project.id, project.name]);

  const rename = async () => {
    const n = nameDraft.trim();
    setRenaming(false);
    if (!n || n === project.name) {
      setNameDraft(project.name);
      return;
    }
    setBusy(true);
    setError(null);
    try {
      await client.updateProject(project.id, { name: n });
      onChanged();
    } catch (e) {
      setError(errMsg(e));
      setNameDraft(project.name);
    } finally {
      setBusy(false);
    }
  };

  const remove = async () => {
    setBusy(true);
    setError(null);
    try {
      await client.deleteProject(project.id);
      onBack();
      onChanged();
    } catch (e) {
      setError(errMsg(e));
      setBusy(false);
    }
  };

  return (
    <div className="project-page">
      <div className="project-head">
        <button className="pp-back" onClick={onBack} title="Back to chat">
          <svg width="16" height="16" viewBox="0 0 24 24" fill="none" aria-hidden="true">
            <path d="M15 18l-6-6 6-6" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" />
          </svg>
        </button>
        <svg className="pp-icon" width="18" height="18" viewBox="0 0 24 24" fill="none" aria-hidden="true">
          <path
            d="M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V7Z"
            stroke="currentColor"
            strokeWidth="1.8"
            strokeLinejoin="round"
          />
        </svg>
        {renaming ? (
          <input
            className="pp-name-input"
            autoFocus
            value={nameDraft}
            onChange={(e) => setNameDraft(e.target.value)}
            onBlur={() => void rename()}
            onKeyDown={(e) => {
              if (e.key === 'Enter') void rename();
              if (e.key === 'Escape') {
                setNameDraft(project.name);
                setRenaming(false);
              }
            }}
          />
        ) : (
          <h2 className="pp-name" onDoubleClick={() => setRenaming(true)}>
            {project.name}
          </h2>
        )}
        <span className="pp-slug">{project.folder_slug}</span>
        <div className="pp-head-actions">
          <button className="pp-btn" disabled={busy} onClick={() => setRenaming(true)}>
            Rename
          </button>
          <button className="pp-btn primary" disabled={busy} onClick={() => onNewArcInProject(project.id)}>
            New arc in project
          </button>
          <button
            className={`pp-btn danger${confirmDelete ? ' armed' : ''}`}
            disabled={busy}
            onClick={() => {
              if (!confirmDelete) {
                setConfirmDelete(true);
                setTimeout(() => setConfirmDelete(false), 3000);
                return;
              }
              void remove();
            }}
          >
            {confirmDelete ? 'Sure? Delete' : 'Delete'}
          </button>
        </div>
      </div>

      <nav className="pp-tabs">
        {(['overview', 'arcs', 'files', 'memories'] as Tab[]).map((t) => (
          <button key={t} className={`pp-tab${tab === t ? ' active' : ''}`} onClick={() => setTab(t)}>
            {t[0].toUpperCase() + t.slice(1)}
          </button>
        ))}
      </nav>

      {error && <div className="pp-error">{error}</div>}

      <div className="pp-body">
        {tab === 'overview' && <OverviewTab client={client} project={project} onChanged={onChanged} />}
        {tab === 'arcs' && <ArcsTab client={client} project={project} onOpenArc={onOpenArc} />}
        {tab === 'files' && <FilesTab client={client} project={project} />}
        {tab === 'memories' && <MemoriesTab client={client} project={project} />}
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Overview: editable instructions + summary + summary-mode (global).
// ---------------------------------------------------------------------------

function OverviewTab({
  client,
  project,
  onChanged,
}: {
  client: AthenClient;
  project: Project;
  onChanged: () => void;
}) {
  const [instructions, setInstructions] = useState(project.instructions ?? '');
  const [mode, setMode] = useState<SummaryMode | null>(null);
  const [saving, setSaving] = useState(false);
  const [summarizing, setSummarizing] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    setInstructions(project.instructions ?? '');
  }, [project.id, project.instructions]);

  useEffect(() => {
    let gone = false;
    client
      .summaryMode()
      .then((m) => !gone && setMode(m))
      .catch(() => {});
    return () => {
      gone = true;
    };
  }, [client]);

  const dirty = instructions !== (project.instructions ?? '');

  const save = async () => {
    setSaving(true);
    setError(null);
    try {
      await client.updateProject(project.id, { instructions });
      onChanged();
    } catch (e) {
      setError(errMsg(e));
    } finally {
      setSaving(false);
    }
  };

  const refreshSummary = async () => {
    setSummarizing(true);
    setError(null);
    try {
      await client.updateProjectSummary(project.id);
      onChanged();
    } catch (e) {
      setError(errMsg(e));
    } finally {
      setSummarizing(false);
    }
  };

  const changeMode = async (m: SummaryMode) => {
    setMode(m);
    try {
      await client.setSummaryMode(m);
    } catch (e) {
      setError(errMsg(e));
    }
  };

  return (
    <div className="pp-overview">
      <section className="pp-card">
        <h3>Instructions</h3>
        <p className="pp-dim">Shared context injected into every arc in this project.</p>
        <textarea
          className="pp-textarea"
          rows={6}
          value={instructions}
          placeholder="What should every conversation in this project know?"
          onChange={(e) => setInstructions(e.target.value)}
        />
        <div className="pp-row">
          <button className="pp-btn primary" disabled={saving || !dirty} onClick={() => void save()}>
            {saving ? 'Saving…' : 'Save instructions'}
          </button>
          {dirty && <span className="pp-dim">Unsaved changes</span>}
        </div>
      </section>

      <section className="pp-card">
        <div className="pp-card-head">
          <h3>Summary</h3>
          <div className="pp-row">
            <label className="pp-inline-field">
              <span>Mode</span>
              <select
                value={mode ?? 'auto'}
                disabled={mode === null}
                onChange={(e) => void changeMode(e.target.value as SummaryMode)}
              >
                <option value="auto">Auto</option>
                <option value="manual">Manual</option>
                <option value="off">Off</option>
              </select>
            </label>
            <button className="pp-btn" disabled={summarizing} onClick={() => void refreshSummary()}>
              {summarizing ? 'Updating…' : 'Update summary now'}
            </button>
          </div>
        </div>
        <p className="pp-dim">Summary mode is a global setting shared by all projects.</p>
        {project.summary ? (
          <>
            {fmtTime(project.summary_updated_at) && (
              <div className="pp-dim pp-stamp">Updated {fmtTime(project.summary_updated_at)}</div>
            )}
            <div className="pp-summary">{project.summary}</div>
          </>
        ) : (
          <div className="pp-empty">No summary yet. It builds up as arcs in this project wrap up.</div>
        )}
      </section>
      {error && <div className="pp-error">{error}</div>}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Arcs: member arcs (filter /api/arcs by project_id). Each opens in chat.
// ---------------------------------------------------------------------------

function ArcsTab({
  client,
  project,
  onOpenArc,
}: {
  client: AthenClient;
  project: Project;
  onOpenArc: (arcId: string) => void;
}) {
  const [arcs, setArcs] = useState<ArcMeta[] | null>(null);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(() => {
    setError(null);
    client
      .listArcs()
      .then((all) => setArcs(all.filter((a) => a.project_id === project.id)))
      .catch((e) => setError(errMsg(e)));
  }, [client, project.id]);

  useEffect(() => {
    load();
  }, [load]);

  if (error) return <div className="pp-error">{error}</div>;
  if (arcs === null) return <div className="pp-dim">Loading…</div>;
  if (arcs.length === 0)
    return (
      <div className="pp-empty">
        No arcs in this project yet. Use “New arc in project”, or assign an existing arc from Settings → Projects.
      </div>
    );

  return (
    <div className="pp-list">
      {arcs.map((a) => (
        <button key={a.id} className="pp-arc-row" onClick={() => onOpenArc(a.id)}>
          <div className="pp-arc-main">
            <div className="pp-arc-name">{a.name || 'New conversation'}</div>
            <div className="pp-dim pp-arc-sub">
              {a.source} · {a.entry_count} {a.entry_count === 1 ? 'entry' : 'entries'}
            </div>
          </div>
          <span className="pp-dim">{fmtTime(a.updated_at)}</span>
        </button>
      ))}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Files: read-only listing of the project's workspace folder.
// ---------------------------------------------------------------------------

function FilesTab({ client, project }: { client: AthenClient; project: Project }) {
  const [files, setFiles] = useState<ProjectFileInfo[] | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let gone = false;
    setError(null);
    setFiles(null);
    client
      .projectFiles(project.id)
      .then((f) => !gone && setFiles(f))
      .catch((e) => !gone && setError(errMsg(e)));
    return () => {
      gone = true;
    };
  }, [client, project.id]);

  if (error) return <div className="pp-error">{error}</div>;
  if (files === null) return <div className="pp-dim">Loading…</div>;
  if (files.length === 0)
    return (
      <div className="pp-empty">
        No files yet. Files the agent saves into this project (via <code>save_file</code>) appear here.
      </div>
    );

  return (
    <div className="pp-list">
      {files.map((f) => (
        <div key={f.name} className="pp-file-row">
          <span className="pp-file-icon">
            {f.is_dir ? (
              <svg width="16" height="16" viewBox="0 0 24 24" fill="none" aria-hidden="true">
                <path
                  d="M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V7Z"
                  stroke="currentColor"
                  strokeWidth="1.7"
                  strokeLinejoin="round"
                />
              </svg>
            ) : (
              <svg width="16" height="16" viewBox="0 0 24 24" fill="none" aria-hidden="true">
                <path
                  d="M7 3h7l5 5v13a0 0 0 0 1 0 0H7a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2Z"
                  stroke="currentColor"
                  strokeWidth="1.7"
                  strokeLinejoin="round"
                />
                <path d="M14 3v5h5" stroke="currentColor" strokeWidth="1.7" strokeLinejoin="round" />
              </svg>
            )}
          </span>
          <span className="pp-file-name">{f.name}</span>
          <span className="pp-dim pp-file-size">{f.is_dir ? '—' : fmtBytes(f.size_bytes)}</span>
          <span className="pp-dim pp-file-when">{fmtTime(f.modified) ?? ''}</span>
        </div>
      ))}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Memories: read-only project-scoped memory cards.
// ---------------------------------------------------------------------------

function MemoriesTab({ client, project }: { client: AthenClient; project: Project }) {
  const [mems, setMems] = useState<MemoryInfo[] | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let gone = false;
    setError(null);
    setMems(null);
    client
      .projectMemories(project.id)
      .then((m) => !gone && setMems(m))
      .catch((e) => !gone && setError(errMsg(e)));
    return () => {
      gone = true;
    };
  }, [client, project.id]);

  if (error) return <div className="pp-error">{error}</div>;
  if (mems === null) return <div className="pp-dim">Loading…</div>;
  if (mems.length === 0)
    return (
      <div className="pp-empty">
        No memories scoped to this project yet. Things the agent remembers while working here show up here.
      </div>
    );

  return (
    <div className="pp-list">
      {mems.map((m) => (
        <div key={m.id} className="pp-mem-card">
          <div className="pp-mem-content">{m.content}</div>
          <div className="pp-dim pp-mem-meta">
            <span className="pp-badge">{m.memory_type}</span>
            <span>{m.source}</span>
            {fmtTime(m.timestamp) && <span>· {fmtTime(m.timestamp)}</span>}
          </div>
        </div>
      ))}
    </div>
  );
}
