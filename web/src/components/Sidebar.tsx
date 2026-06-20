import { useEffect, useRef, useState } from 'react';
import type { ArcMeta, Project } from '../api/types';

function timeAgo(iso: string): string {
  const t = Date.parse(iso);
  if (Number.isNaN(t)) return '';
  const s = Math.max(0, (Date.now() - t) / 1000);
  if (s < 60) return 'now';
  if (s < 3600) return `${Math.floor(s / 60)}m`;
  if (s < 86400) return `${Math.floor(s / 3600)}h`;
  return `${Math.floor(s / 86400)}d`;
}

export interface ArcActions {
  onSelect: (id: string) => void;
  onNew: () => void;
  onRename: (id: string, name: string) => void;
  onCompact: (id: string) => void;
  onDelete: (id: string) => void;
}

function ArcRow({
  arc,
  active,
  unread,
  actions,
}: {
  arc: ArcMeta;
  active: boolean;
  unread: boolean;
  actions: ArcActions;
}) {
  const [menu, setMenu] = useState(false);
  const [renaming, setRenaming] = useState(false);
  const [confirmDelete, setConfirmDelete] = useState(false);
  const [name, setName] = useState(arc.name);
  const wrapRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!menu) return;
    const close = (e: MouseEvent) => {
      if (wrapRef.current && !wrapRef.current.contains(e.target as Node)) {
        setMenu(false);
        setConfirmDelete(false);
      }
    };
    document.addEventListener('mousedown', close);
    return () => document.removeEventListener('mousedown', close);
  }, [menu]);

  if (renaming) {
    const commit = () => {
      setRenaming(false);
      const n = name.trim();
      if (n && n !== arc.name) actions.onRename(arc.id, n);
      else setName(arc.name);
    };
    return (
      <div className="arc-row renaming">
        <input
          autoFocus
          value={name}
          onChange={(e) => setName(e.target.value)}
          onBlur={commit}
          onKeyDown={(e) => {
            if (e.key === 'Enter') commit();
            if (e.key === 'Escape') {
              setName(arc.name);
              setRenaming(false);
            }
          }}
        />
      </div>
    );
  }

  return (
    <div className={`arc-row-wrap${active ? ' active' : ''}`} ref={wrapRef}>
      <button className="arc-row" onClick={() => actions.onSelect(arc.id)} onDoubleClick={() => setRenaming(true)}>
        <span className="arc-name">{arc.name || 'New conversation'}</span>
        <span className="arc-meta">
          {unread && <span className="unread-dot" />}
          {timeAgo(arc.updated_at)}
        </span>
      </button>
      <button
        className="arc-menu-btn"
        onClick={(e) => {
          e.stopPropagation();
          setMenu((m) => !m);
        }}
        title="Conversation options"
      >
        ⋯
      </button>
      {menu && (
        <div className="arc-menu">
          <button
            onClick={() => {
              setMenu(false);
              setRenaming(true);
            }}
          >
            Rename
          </button>
          <button
            onClick={() => {
              setMenu(false);
              actions.onCompact(arc.id);
            }}
          >
            Compact
          </button>
          <button
            className="danger"
            onClick={() => {
              if (!confirmDelete) {
                setConfirmDelete(true);
                setTimeout(() => setConfirmDelete(false), 3000);
                return;
              }
              setMenu(false);
              actions.onDelete(arc.id);
            }}
          >
            {confirmDelete ? 'Sure? Delete' : 'Delete'}
          </button>
        </div>
      )}
    </div>
  );
}

export interface ProjectActions {
  onSelect: (id: string) => void;
  onCreate: (name: string) => void;
}

function ProjectsSection({
  projects,
  activeProject,
  actions,
}: {
  projects: Project[];
  activeProject: string | null;
  actions: ProjectActions;
}) {
  const [creating, setCreating] = useState(false);
  const [name, setName] = useState('');

  const commit = () => {
    const n = name.trim();
    setCreating(false);
    setName('');
    if (n) actions.onCreate(n);
  };

  return (
    <div className="proj-section">
      <div className="proj-head">
        <span className="proj-title">Projects</span>
        <button className="proj-add" title="New project" onClick={() => setCreating(true)}>
          <svg width="13" height="13" viewBox="0 0 24 24" fill="none" aria-hidden="true">
            <path d="M12 5v14M5 12h14" stroke="currentColor" strokeWidth="2.2" strokeLinecap="round" />
          </svg>
        </button>
      </div>
      {creating && (
        <input
          className="proj-new-input"
          autoFocus
          value={name}
          placeholder="Project name"
          onChange={(e) => setName(e.target.value)}
          onBlur={commit}
          onKeyDown={(e) => {
            if (e.key === 'Enter') commit();
            if (e.key === 'Escape') {
              setCreating(false);
              setName('');
            }
          }}
        />
      )}
      {projects.map((p) => (
        <button
          key={p.id}
          className={`proj-row${p.id === activeProject ? ' active' : ''}`}
          onClick={() => actions.onSelect(p.id)}
          title={p.name}
        >
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" aria-hidden="true">
            <path
              d="M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V7Z"
              stroke="currentColor"
              strokeWidth="1.7"
              strokeLinejoin="round"
            />
          </svg>
          <span className="proj-name">{p.name}</span>
        </button>
      ))}
      {projects.length === 0 && !creating && <div className="proj-empty">No projects</div>}
    </div>
  );
}

export function Sidebar({
  arcs,
  activeArc,
  unread,
  open,
  actions,
  onClose,
  projects,
  activeProject,
  projectActions,
}: {
  arcs: ArcMeta[];
  activeArc: string | null;
  unread: Set<string>;
  open: boolean;
  actions: ArcActions;
  onClose: () => void;
  projects: Project[];
  activeProject: string | null;
  projectActions: ProjectActions;
}) {
  return (
    <>
      {open && <div className="sidebar-overlay" onClick={onClose} />}
      <aside className={`sidebar${open ? ' open' : ''}`}>
        <div className="brand">
          <svg width="18" height="18" viewBox="0 0 24 24" fill="none" aria-hidden="true">
            <circle cx="12" cy="12" r="9" stroke="currentColor" strokeWidth="2" />
            <path d="M8.5 15.5 12 8l3.5 7.5M9.8 13h4.4" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" />
          </svg>
          Athen
        </div>
        <ProjectsSection projects={projects} activeProject={activeProject} actions={projectActions} />
        <button className="new-chat" onClick={actions.onNew}>
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" aria-hidden="true">
            <path d="M12 5v14M5 12h14" stroke="currentColor" strokeWidth="2.2" strokeLinecap="round" />
          </svg>
          New chat
        </button>
        <nav className="arc-list">
          {arcs.map((a) => (
            <ArcRow key={a.id} arc={a} active={a.id === activeArc} unread={unread.has(a.id)} actions={actions} />
          ))}
          {arcs.length === 0 && <div className="arc-empty">No conversations yet</div>}
        </nav>
      </aside>
    </>
  );
}
