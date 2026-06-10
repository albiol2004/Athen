import { useCallback, useEffect, useReducer, useRef, useState } from 'react';
import { ApiError, AthenClient } from '../api/client';
import { connectEvents, type ConnectionStatus } from '../api/events';
import type { NotificationInfo } from '../api/types';
import { chatReducer, initialChat } from '../chat/reducer';
import { Bell } from './Bell';
import { Chat } from './Chat';
import { Sidebar } from './Sidebar';
import type { ArcMeta } from '../api/types';

function errMsg(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

export function Shell({ client, onLogout }: { client: AthenClient; onLogout: () => void }) {
  const [arcs, setArcs] = useState<ArcMeta[]>([]);
  const [activeArc, setActiveArc] = useState<string | null>(null);
  const [unread, setUnread] = useState<Set<string>>(new Set());
  const [status, setStatus] = useState<ConnectionStatus>('connecting');
  const [busy, setBusy] = useState(false);
  const [notifications, setNotifications] = useState<NotificationInfo[]>([]);
  const [sidebarOpen, setSidebarOpen] = useState(false);
  const [chat, dispatch] = useReducer(chatReducer, initialChat);

  // Refs mirror state the SSE handlers need — the subscription outlives
  // any single render, so closures must not capture stale values.
  const activeArcRef = useRef<string | null>(null);
  activeArcRef.current = activeArc;
  const busyRef = useRef(false);
  const streamSeenRef = useRef(false);

  const bail = useCallback(
    (e: unknown) => {
      if (e instanceof ApiError && e.status === 401) {
        onLogout();
        return true;
      }
      return false;
    },
    [onLogout],
  );

  const refreshArcs = useCallback(async () => {
    try {
      setArcs(await client.listArcs());
    } catch (e) {
      bail(e);
    }
  }, [client, bail]);

  // ---- boot: arcs + active history + parked grants + notifications ----
  useEffect(() => {
    let gone = false;
    (async () => {
      try {
        const [arcList, cur] = await Promise.all([client.listArcs(), client.currentArc()]);
        if (gone) return;
        setArcs(arcList);
        if (cur.arc_id) {
          setActiveArc(cur.arc_id);
          const entries = await client.arcEntries(cur.arc_id);
          if (gone) return;
          dispatch({ type: 'reset', entries });
        }
        const [grants, notifs] = await Promise.all([
          client.pendingGrants(),
          client.listNotifications(),
        ]);
        if (gone) return;
        for (const g of grants) dispatch({ type: 'grant', g });
        setNotifications(notifs);
      } catch (e) {
        if (!bail(e)) dispatch({ type: 'system', text: `Couldn't load: ${errMsg(e)}` });
      }
    })();
    return () => {
      gone = true;
    };
  }, [client, bail]);

  // ---- live events ----
  useEffect(
    () =>
      connectEvents(client, {
        onStatus: setStatus,
        onStream: (e) => {
          // Background-arc output marks the arc unread instead of
          // rendering into whatever is on screen (desktop rule).
          const active = activeArcRef.current;
          if (e.arc_id && active && e.arc_id !== active) {
            if (e.is_final) {
              setUnread((s) => new Set(s).add(e.arc_id as string));
              void refreshArcs();
            }
            return;
          }
          // Only real content counts — the executor emits a bare
          // is_final (no delta) at end of non-streamed turns, and that
          // must not suppress the long-poll reply fallback.
          if (!e.is_thinking && e.delta) streamSeenRef.current = true;
          dispatch({ type: 'stream', e });
        },
        onProgress: (e) => {
          const active = activeArcRef.current;
          if (e.arc_id && active && e.arc_id !== active) return;
          dispatch({ type: 'progress', e });
        },
        // Approval cards render regardless of arc — the user must always
        // be able to answer a blocked agent from wherever they are.
        onQuestion: (q) => dispatch({ type: 'question', q }),
        onApprovalResolved: (p) => {
          if (p.task_id)
            dispatch({
              type: 'resolve',
              card: 'task',
              refId: p.task_id,
              label: p.approved ? 'Approved' : 'Denied',
            });
        },
        onGrant: (g) => dispatch({ type: 'grant', g }),
        onGrantResolvedElsewhere: (id) =>
          dispatch({ type: 'resolve', card: 'grant', refId: String(id), label: 'Resolved elsewhere' }),
        onArcUpdated: () => void refreshArcs(),
        onNotification: (n) => setNotifications((l) => [n, ...l]),
        onLagged: () => {
          dispatch({ type: 'system', text: 'Event stream lagged — some output may be missing.' });
          const active = activeArcRef.current;
          if (active) {
            client
              .arcEntries(active)
              .then((entries) => dispatch({ type: 'reset', entries }))
              .catch(() => {});
          }
        },
      }),
    [client, refreshArcs],
  );

  // ---- actions ----
  const switchArc = useCallback(
    async (id: string) => {
      try {
        const entries = await client.selectArc(id);
        setActiveArc(id);
        dispatch({ type: 'reset', entries });
        setUnread((s) => {
          if (!s.has(id)) return s;
          const n = new Set(s);
          n.delete(id);
          return n;
        });
        setSidebarOpen(false);
      } catch (e) {
        if (!bail(e)) dispatch({ type: 'system', text: `Couldn't switch: ${errMsg(e)}` });
      }
    },
    [client, bail],
  );

  const createArc = useCallback(async () => {
    try {
      const { arc_id } = await client.newArc();
      await refreshArcs();
      await switchArc(arc_id);
    } catch (e) {
      if (!bail(e)) dispatch({ type: 'system', text: `Couldn't create arc: ${errMsg(e)}` });
    }
  }, [client, refreshArcs, switchArc, bail]);

  const send = useCallback(
    async (text: string) => {
      dispatch({ type: 'user', text });
      const arcId = activeArcRef.current ?? undefined;

      if (busyRef.current) {
        // A turn is running: queue the input into it.
        if (!arcId) return;
        try {
          await client.queueMessage(arcId, text);
        } catch (e) {
          if (!bail(e)) dispatch({ type: 'system', text: `Couldn't queue: ${errMsg(e)}` });
        }
        return;
      }

      busyRef.current = true;
      setBusy(true);
      streamSeenRef.current = false;
      try {
        // Long-poll: resolves at end of turn. Streaming renders via SSE
        // meanwhile; the response matters for the risk-gate card and as
        // a no-stream fallback.
        const resp = await client.sendMessage(text, arcId);
        if (resp?.pending_approval) dispatch({ type: 'task', t: resp.pending_approval });
        else if (resp?.content && !streamSeenRef.current) dispatch({ type: 'agent', text: resp.content });
        if (!arcId) {
          const cur = await client.currentArc();
          if (cur.arc_id) setActiveArc(cur.arc_id);
        }
        void refreshArcs();
      } catch (e) {
        if (!bail(e)) dispatch({ type: 'system', text: `Send failed: ${errMsg(e)}` });
      } finally {
        busyRef.current = false;
        setBusy(false);
      }
    },
    [client, refreshArcs, bail],
  );

  const cancel = useCallback(() => {
    client.cancelAll().catch((e) => {
      if (!bail(e)) dispatch({ type: 'system', text: `Cancel failed: ${errMsg(e)}` });
    });
  }, [client, bail]);

  const markAllRead = useCallback(async () => {
    try {
      await client.markAllNotificationsRead();
      setNotifications(await client.listNotifications());
    } catch (e) {
      bail(e);
    }
  }, [client, bail]);

  const activeMeta = arcs.find((a) => a.id === activeArc);

  return (
    <div className="shell">
      <Sidebar
        arcs={arcs}
        activeArc={activeArc}
        unread={unread}
        open={sidebarOpen}
        onSelect={(id) => void switchArc(id)}
        onNew={() => void createArc()}
        onClose={() => setSidebarOpen(false)}
      />
      <div className="main">
        <header className="topbar">
          <button className="icon-btn menu-btn" onClick={() => setSidebarOpen(true)} title="Conversations">
            <svg width="17" height="17" viewBox="0 0 24 24" fill="none" aria-hidden="true">
              <path d="M4 7h16M4 12h16M4 17h16" stroke="currentColor" strokeWidth="2" strokeLinecap="round" />
            </svg>
          </button>
          <span className="arc-title">{activeMeta?.name || 'Athen'}</span>
          <span className={`conn ${status}`} title={`Event stream: ${status}`} />
          <div className="topbar-right">
            <Bell
              notifications={notifications}
              onMarkAllRead={() => void markAllRead()}
              onOpenArc={(id) => void switchArc(id)}
            />
            <button className="icon-btn" onClick={onLogout} title="Disconnect">
              <svg width="16" height="16" viewBox="0 0 24 24" fill="none" aria-hidden="true">
                <path
                  d="M15 4h3a2 2 0 0 1 2 2v12a2 2 0 0 1-2 2h-3M10 17l5-5-5-5M15 12H3"
                  stroke="currentColor"
                  strokeWidth="1.8"
                  strokeLinecap="round"
                  strokeLinejoin="round"
                />
              </svg>
            </button>
          </div>
        </header>
        <Chat
          items={chat.items}
          busy={busy}
          arcKey={activeArc}
          cb={{
            onSend: (t) => void send(t),
            onCancel: cancel,
            onAnswerQuestion: async (q, c) => {
              await client.answerQuestion(q.id, c.key);
              dispatch({ type: 'resolve', card: 'question', refId: q.id, label: c.label || c.key });
            },
            onDecideTask: async (t, approved) => {
              await client.approveTask(t.task_id, approved);
              dispatch({
                type: 'resolve',
                card: 'task',
                refId: t.task_id,
                label: approved ? 'Approved' : 'Denied',
              });
            },
            onDecideGrant: async (g, decision, label) => {
              await client.resolveGrant(g.id, decision);
              dispatch({ type: 'resolve', card: 'grant', refId: g.id, label });
            },
          }}
        />
      </div>
    </div>
  );
}
