import { useCallback, useEffect, useMemo, useReducer, useRef, useState } from 'react';
import { ApiError, AthenClient } from '../api/client';
import { connectEvents, type ConnectionStatus } from '../api/events';
import type { ArcMeta, NotificationInfo } from '../api/types';
import { chatReducer, initialChat } from '../chat/reducer';
import { SettingsModal } from '../settings/SettingsModal';
import { AgentsPanel } from './AgentsPanel';
import { ArcPickers } from './ArcPickers';
import { Bell } from './Bell';
import { ChangesRail } from './ChangesRail';
import { Chat, type ChatCallbacks, type OutgoingFile, type OutgoingImage } from './Chat';
import { GoalBanner, type GoalState, type PlanState } from './PlanGoal';
import { Sidebar } from './Sidebar';
import { Wakeups } from './Wakeups';

function errMsg(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

type Drawer = 'none' | 'agents' | 'changes' | 'wakeups';

export function Shell({ client, onLogout }: { client: AthenClient; onLogout: () => void }) {
  const [arcs, setArcs] = useState<ArcMeta[]>([]);
  const [activeArc, setActiveArc] = useState<string | null>(null);
  const [unread, setUnread] = useState<Set<string>>(new Set());
  const [status, setStatus] = useState<ConnectionStatus>('connecting');
  const [busy, setBusy] = useState(false);
  const [notifications, setNotifications] = useState<NotificationInfo[]>([]);
  const [sidebarOpen, setSidebarOpen] = useState(false);
  const [chat, dispatch] = useReducer(chatReducer, initialChat);
  const [goal, setGoal] = useState<GoalState | null>(null);
  const [plan, setPlan] = useState<PlanState | null>(null);
  const [drawer, setDrawer] = useState<Drawer>('none');
  const [settingsOpen, setSettingsOpen] = useState(false);
  const [changesKey, setChangesKey] = useState(0);

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

  const refreshGoalPlan = useCallback(async () => {
    try {
      const [g, p] = await Promise.all([
        client.get<GoalState | null>('/goal'),
        client.get<PlanState | null>('/plan'),
      ]);
      setGoal(g && g.goal ? g : null);
      setPlan(p);
    } catch {
      /* non-critical */
    }
  }, [client]);

  // ---- boot ----
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
        const [grants, notifs] = await Promise.all([client.pendingGrants(), client.listNotifications()]);
        if (gone) return;
        for (const g of grants) dispatch({ type: 'grant', g });
        setNotifications(notifs);
        void refreshGoalPlan();
      } catch (e) {
        if (!bail(e)) dispatch({ type: 'system', text: `Couldn't load: ${errMsg(e)}` });
      }
    })();
    return () => {
      gone = true;
    };
  }, [client, bail, refreshGoalPlan]);

  // ---- live events ----
  useEffect(
    () =>
      connectEvents(client, {
        onStatus: setStatus,
        onStream: (e) => {
          const active = activeArcRef.current;
          if (e.arc_id && active && e.arc_id !== active) {
            if (e.is_final) {
              setUnread((s) => new Set(s).add(e.arc_id as string));
              void refreshArcs();
            }
            return;
          }
          // Only real content counts — a bare is_final (no delta) must
          // not suppress the long-poll reply fallback.
          if (!e.is_thinking && e.delta) streamSeenRef.current = true;
          dispatch({ type: 'stream', e });
        },
        onProgress: (e) => {
          const active = activeArcRef.current;
          if (e.arc_id && active && e.arc_id !== active) return;
          dispatch({ type: 'progress', e });
          if (e.status === 'Completed' && (e.tool_name === 'edit' || e.tool_name === 'write')) {
            setChangesKey((k) => k + 1);
          }
        },
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
        onArcUpdated: () => {
          void refreshArcs();
          void refreshGoalPlan();
        },
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
    [client, refreshArcs, refreshGoalPlan],
  );

  // ---- arc actions ----
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
        void refreshGoalPlan();
        void refreshArcs();
      } catch (e) {
        if (!bail(e)) dispatch({ type: 'system', text: `Couldn't switch: ${errMsg(e)}` });
      }
    },
    [client, bail, refreshGoalPlan, refreshArcs],
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
    async (text: string, images: OutgoingImage[], files: OutgoingFile[]) => {
      const label =
        text ||
        [...images.map((i) => i.name), ...files.map((f) => f.name)].join(', ') ||
        '(attachment)';
      dispatch({ type: 'user', text: label });
      const arcId = activeArcRef.current ?? undefined;

      if (busyRef.current) {
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
        const resp = await client.sendMessage(
          text,
          arcId,
          images.length
            ? images.map((i) => ({ mime_type: i.mime_type, data: { kind: 'base64' as const, data: i.base64 } }))
            : undefined,
          files.length ? files : undefined,
        );
        if (resp?.pending_approval) dispatch({ type: 'task', t: resp.pending_approval });
        else if (resp?.content && !streamSeenRef.current) dispatch({ type: 'agent', text: resp.content });
        if (!arcId) {
          const cur = await client.currentArc();
          if (cur.arc_id) setActiveArc(cur.arc_id);
        }
        void refreshArcs();
        void refreshGoalPlan();
      } catch (e) {
        if (!bail(e)) dispatch({ type: 'system', text: `Send failed: ${errMsg(e)}` });
      } finally {
        busyRef.current = false;
        setBusy(false);
      }
    },
    [client, refreshArcs, refreshGoalPlan, bail],
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

  // ---- chat callbacks (stabilized so the memo'd timeline doesn't churn) ----
  // Every handler below closes only over stable values: `client` (singleton),
  // `dispatch` (stable from useReducer), and already-memoized callbacks
  // (`send`, `cancel`, `refreshGoalPlan`). `setPlan` is a stable state setter.
  const onSend = useCallback(
    (t: string, imgs: OutgoingImage[], fls: OutgoingFile[]) => void send(t, imgs, fls),
    [send],
  );
  const onAnswerQuestion = useCallback<ChatCallbacks['onAnswerQuestion']>(
    async (q, c) => {
      await client.answerQuestion(q.id, c.key);
      dispatch({ type: 'resolve', card: 'question', refId: q.id, label: c.label || c.key });
    },
    [client],
  );
  const onDecideTask = useCallback<ChatCallbacks['onDecideTask']>(
    async (t, approved) => {
      await client.approveTask(t.task_id, approved);
      dispatch({
        type: 'resolve',
        card: 'task',
        refId: t.task_id,
        label: approved ? 'Approved' : 'Denied',
      });
    },
    [client],
  );
  const onDecideGrant = useCallback<ChatCallbacks['onDecideGrant']>(
    async (g, decision, label) => {
      await client.resolveGrant(g.id, decision);
      dispatch({ type: 'resolve', card: 'grant', refId: g.id, label });
    },
    [client],
  );
  const onApprovePlan = useCallback<ChatCallbacks['onApprovePlan']>(async () => {
    await client.post('/plan/approve');
    await refreshGoalPlan();
    void send('Execute the plan step by step.', [], []);
  }, [client, refreshGoalPlan, send]);
  const onDiscardPlan = useCallback<ChatCallbacks['onDiscardPlan']>(async () => {
    await client.post('/plan/clear');
    setPlan(null);
  }, [client]);

  const cb = useMemo<ChatCallbacks>(
    () => ({
      onSend,
      onCancel: cancel,
      onAnswerQuestion,
      onDecideTask,
      onDecideGrant,
      onApprovePlan,
      onDiscardPlan,
    }),
    [onSend, cancel, onAnswerQuestion, onDecideTask, onDecideGrant, onApprovePlan, onDiscardPlan],
  );

  const activeMeta = useMemo(() => arcs.find((a) => a.id === activeArc), [arcs, activeArc]);

  return (
    <div className="shell">
      <Sidebar
        arcs={arcs}
        activeArc={activeArc}
        unread={unread}
        open={sidebarOpen}
        onClose={() => setSidebarOpen(false)}
        actions={{
          onSelect: (id) => void switchArc(id),
          onNew: () => void createArc(),
          onRename: (id, name) =>
            void client.post(`/arcs/${encodeURIComponent(id)}/rename`, { name }).then(refreshArcs, () => {}),
          onCompact: (id) =>
            void client
              .post(`/arcs/${encodeURIComponent(id)}/compact`)
              .then(() => dispatch({ type: 'system', text: 'Conversation compacted.' }))
              .catch((e) => dispatch({ type: 'system', text: `Compact failed: ${errMsg(e)}` })),
          onDelete: (id) =>
            void client
              .post<{ active_arc_id: string }>(`/arcs/${encodeURIComponent(id)}/delete`)
              .then(async (r) => {
                await refreshArcs();
                if (id === activeArcRef.current) await switchArc(r.active_arc_id);
              })
              .catch((e) => dispatch({ type: 'system', text: `Delete failed: ${errMsg(e)}` })),
        }}
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
          {activeMeta && <ArcPickers client={client} arc={activeMeta} onChanged={() => void refreshArcs()} />}
          <div className="topbar-right">
            <button
              className="icon-btn"
              title="Active agents"
              onClick={() => setDrawer((d) => (d === 'agents' ? 'none' : 'agents'))}
            >
              <svg width="16" height="16" viewBox="0 0 24 24" fill="none" aria-hidden="true">
                <circle cx="12" cy="12" r="3" stroke="currentColor" strokeWidth="1.8" />
                <path
                  d="M12 5V3M12 21v-2M5 12H3m18 0h-2M6.3 6.3 4.9 4.9m14.2 14.2-1.4-1.4M6.3 17.7l-1.4 1.4M19.1 4.9l-1.4 1.4"
                  stroke="currentColor"
                  strokeWidth="1.8"
                  strokeLinecap="round"
                />
              </svg>
            </button>
            <button
              className="icon-btn"
              title="File changes (revert)"
              onClick={() => setDrawer((d) => (d === 'changes' ? 'none' : 'changes'))}
            >
              <svg width="16" height="16" viewBox="0 0 24 24" fill="none" aria-hidden="true">
                <path
                  d="M3 12a9 9 0 1 0 3-6.7M3 4v5h5"
                  stroke="currentColor"
                  strokeWidth="1.8"
                  strokeLinecap="round"
                  strokeLinejoin="round"
                />
              </svg>
            </button>
            <button
              className="icon-btn"
              title="Scheduled wake-ups"
              onClick={() => setDrawer((d) => (d === 'wakeups' ? 'none' : 'wakeups'))}
            >
              <svg width="16" height="16" viewBox="0 0 24 24" fill="none" aria-hidden="true">
                <circle cx="12" cy="13" r="7" stroke="currentColor" strokeWidth="1.8" />
                <path d="M12 10v3l2 2M5 4 3 6m16-2 2 2" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" />
              </svg>
            </button>
            <Bell
              notifications={notifications}
              onMarkAllRead={() => void markAllRead()}
              onOpenArc={(id) => void switchArc(id)}
            />
            <button className="icon-btn" title="Settings" onClick={() => setSettingsOpen(true)}>
              <svg width="16" height="16" viewBox="0 0 24 24" fill="none" aria-hidden="true">
                <circle cx="12" cy="12" r="3" stroke="currentColor" strokeWidth="1.8" />
                <path
                  d="M19.4 15a1.7 1.7 0 0 0 .34 1.87l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06a1.7 1.7 0 0 0-1.87-.34 1.7 1.7 0 0 0-1 1.55V21a2 2 0 1 1-4 0v-.09a1.7 1.7 0 0 0-1-1.55 1.7 1.7 0 0 0-1.87.34l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06a1.7 1.7 0 0 0 .34-1.87 1.7 1.7 0 0 0-1.55-1H3a2 2 0 1 1 0-4h.09a1.7 1.7 0 0 0 1.55-1 1.7 1.7 0 0 0-.34-1.87l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06a1.7 1.7 0 0 0 1.87.34h.01a1.7 1.7 0 0 0 1-1.55V3a2 2 0 1 1 4 0v.09a1.7 1.7 0 0 0 1 1.55 1.7 1.7 0 0 0 1.87-.34l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06a1.7 1.7 0 0 0-.34 1.87v.01a1.7 1.7 0 0 0 1.55 1H21a2 2 0 1 1 0 4h-.09a1.7 1.7 0 0 0-1.55 1Z"
                  stroke="currentColor"
                  strokeWidth="1.5"
                />
              </svg>
            </button>
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
        {goal && (
          <GoalBanner
            goal={goal}
            onClear={() => void client.del('/goal').then(() => setGoal(null), () => {})}
          />
        )}
        <Chat
          items={chat.items}
          busy={busy}
          arcKey={activeArc}
          plan={plan}
          client={client}
          cb={cb}
        />
      </div>
      {drawer === 'agents' && (
        <AgentsPanel client={client} onClose={() => setDrawer('none')} onOpenArc={(id) => void switchArc(id)} />
      )}
      {drawer === 'changes' && activeArc && (
        <ChangesRail
          client={client}
          arcId={activeArc}
          refreshKey={changesKey}
          onClose={() => setDrawer('none')}
          onReverted={() => {
            setChangesKey((k) => k + 1);
            void switchArc(activeArc);
          }}
        />
      )}
      {drawer === 'wakeups' && <Wakeups client={client} onClose={() => setDrawer('none')} />}
      {settingsOpen && <SettingsModal client={client} onClose={() => setSettingsOpen(false)} />}
    </div>
  );
}
