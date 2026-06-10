import type { ArcMeta } from '../api/types';

function timeAgo(iso: string): string {
  const t = Date.parse(iso);
  if (Number.isNaN(t)) return '';
  const s = Math.max(0, (Date.now() - t) / 1000);
  if (s < 60) return 'now';
  if (s < 3600) return `${Math.floor(s / 60)}m`;
  if (s < 86400) return `${Math.floor(s / 3600)}h`;
  return `${Math.floor(s / 86400)}d`;
}

export function Sidebar({
  arcs,
  activeArc,
  unread,
  open,
  onSelect,
  onNew,
  onClose,
}: {
  arcs: ArcMeta[];
  activeArc: string | null;
  unread: Set<string>;
  open: boolean;
  onSelect: (id: string) => void;
  onNew: () => void;
  onClose: () => void;
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
        <button className="new-chat" onClick={onNew}>
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" aria-hidden="true">
            <path d="M12 5v14M5 12h14" stroke="currentColor" strokeWidth="2.2" strokeLinecap="round" />
          </svg>
          New chat
        </button>
        <nav className="arc-list">
          {arcs.map((a) => (
            <button
              key={a.id}
              className={`arc-row${a.id === activeArc ? ' active' : ''}`}
              onClick={() => onSelect(a.id)}
            >
              <span className="arc-name">{a.name || 'New conversation'}</span>
              <span className="arc-meta">
                {unread.has(a.id) && <span className="unread-dot" />}
                {timeAgo(a.updated_at)}
              </span>
            </button>
          ))}
          {arcs.length === 0 && <div className="arc-empty">No conversations yet</div>}
        </nav>
      </aside>
    </>
  );
}
