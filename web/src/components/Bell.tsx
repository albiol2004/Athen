import { useEffect, useRef, useState } from 'react';
import type { NotificationInfo } from '../api/types';

export function Bell({
  notifications,
  onMarkAllRead,
  onOpenArc,
}: {
  notifications: NotificationInfo[];
  onMarkAllRead: () => void;
  onOpenArc: (arcId: string) => void;
}) {
  const [open, setOpen] = useState(false);
  const wrapRef = useRef<HTMLDivElement>(null);
  const unread = notifications.filter((n) => !n.is_read).length;

  useEffect(() => {
    if (!open) return;
    const close = (e: MouseEvent) => {
      if (wrapRef.current && !wrapRef.current.contains(e.target as Node)) setOpen(false);
    };
    document.addEventListener('mousedown', close);
    return () => document.removeEventListener('mousedown', close);
  }, [open]);

  return (
    <div className="bell-wrap" ref={wrapRef}>
      <button className="icon-btn" onClick={() => setOpen((o) => !o)} title="Notifications">
        <svg width="17" height="17" viewBox="0 0 24 24" fill="none" aria-hidden="true">
          <path
            d="M6 9a6 6 0 0 1 12 0c0 5 2 6 2 6H4s2-1 2-6Zm4.5 9a1.8 1.8 0 0 0 3 0"
            stroke="currentColor"
            strokeWidth="1.8"
            strokeLinecap="round"
            strokeLinejoin="round"
          />
        </svg>
        {unread > 0 && <span className="bell-badge">{unread > 99 ? '99+' : unread}</span>}
      </button>
      {open && (
        <div className="bell-panel">
          <div className="bell-head">
            <span>Notifications</span>
            {unread > 0 && (
              <button className="link-btn" onClick={onMarkAllRead}>
                Mark all read
              </button>
            )}
          </div>
          <div className="bell-list">
            {notifications.length === 0 && <div className="bell-empty">Nothing yet</div>}
            {notifications.slice(0, 50).map((n) => (
              <button
                key={n.id}
                className={`bell-item${n.is_read ? '' : ' unread'}`}
                onClick={() => {
                  if (n.arc_id) {
                    onOpenArc(n.arc_id);
                    setOpen(false);
                  }
                }}
              >
                <span className="bell-title">{n.title}</span>
                {n.body && <span className="bell-body">{n.body}</span>}
              </button>
            ))}
          </div>
        </div>
      )}
    </div>
  );
}
