import { useCallback, useEffect, useMemo, useReducer, useRef, useState } from 'react';
import { ApiError, AthenClient } from '../api/client';
import { connectEvents, type ConnectionStatus } from '../api/events';
import type {
  ArcMeta,
  DeepResearchDepth,
  DeepResearchDoneEvent,
  DeepResearchMode,
  DeepResearchProgressEvent,
  NotificationInfo,
  Project,
} from '../api/types';
import { chatReducer, initialChat } from '../chat/reducer';
import { SettingsModal } from '../settings/SettingsModal';
import { AgentsPanel } from './AgentsPanel';
import { ArcPickers } from './ArcPickers';
import { Bell } from './Bell';
import { ChangesRail } from './ChangesRail';
import { Chat, type ChatCallbacks, type OutgoingFile, type OutgoingImage } from './Chat';
import { DeepResearchBanner, DeepResearchModal } from './DeepResearch';
import { GoalBanner, type GoalState, type PlanState } from './PlanGoal';
import { ProjectPage } from './ProjectPage';
import { Sidebar } from './Sidebar';
import { errMessage, useToast } from './Toast';
import { Wakeups } from './Wakeups';

function errMsg(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

type Drawer = 'none' | 'agents' | 'changes' | 'wakeups';

export function Shell({ client, onLogout }: { client: AthenClient; onLogout: () => void }) {
  const { toast } = useToast();
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
  const [settingsTab, setSettingsTab] = useState<string | undefined>(undefined);
  const [changesKey, setChangesKey] = useState(0);
  const [projects, setProjects] = useState<Project[]>([]);
  // null = chat surface; a project id = the Project page for that project.
  const [openProject, setOpenProject] = useState<string | null>(null);
  // Deep Research (docs/DEEP_RESEARCH.md): launcher seed text (null = closed),
  // live progress/done banners (scoped to the active arc), POST-in-flight flag.
  const [drQuestion, setDrQuestion] = useState<string | null>(null);
  const [drProgress, setDrProgress] = useState<DeepResearchProgressEvent | null>(null);
  const [drDone, setDrDone] = useState<DeepResearchDoneEvent | null>(null);
  const [drBusy, setDrBusy] = useState(false);

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

  const refreshProjects = useCallback(async () => {
    try {
      setProjects(await client.listProjects());
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
        const [grants, notifs, projs] = await Promise.all([
          client.pendingGrants(),
          client.listNotifications(),
          client.listProjects().catch(() => [] as Project[]),
        ]);
        if (gone) return;
        for (const g of grants) dispatch({ type: 'grant', g });
        setNotifications(notifs);
        setProjects(projs);
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
        onDeepResearchProgress: (e) => {
          if (e.arc_id !== activeArcRef.current) return;
          setDrProgress(e);
          setDrDone(null);
        },
        onDeepResearchDone: (e) => {
          if (e.arc_id !== activeArcRef.current) return;
          setDrProgress(null);
          setDrDone(e);
          void refreshArcs();
        },
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
        // Deep Research banners are arc-scoped — drop any from the old arc.
        setDrProgress(null);
        setDrDone(null);
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

  // ---- project actions ----
  const createProject = useCallback(
    async (name: string) => {
      try {
        const p = await client.createProject(name);
        await refreshProjects();
        setOpenProject(p.id);
      } catch (e) {
        if (!bail(e)) dispatch({ type: 'system', text: `Couldn't create project: ${errMsg(e)}` });
      }
    },
    [client, refreshProjects, bail],
  );

  // Open one of a project's arcs in the chat surface (leaves the project page).
  const openArcFromProject = useCallback(
    (arcId: string) => {
      setOpenProject(null);
      void switchArc(arcId);
    },
    [switchArc],
  );

  // Create a fresh arc that inherits the given project, then drop into chat.
  // Set the active project first so newArc()'s arc picks it up server-side.
  const newArcInProject = useCallback(
    async (projectId: string) => {
      try {
        await client.setActiveProject(projectId);
        setOpenProject(null);
        await createArc();
      } catch (e) {
        if (!bail(e)) dispatch({ type: 'system', text: `Couldn't start arc: ${errMsg(e)}` });
      }
    },
    [client, createArc, bail],
  );

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

  // ---- deep research ----
  // The button only opens the launcher; the POST happens once the user picks
  // depth (+ extend/new). An arc must exist first so the paper has a home.
  const onDeepResearch = useCallback<ChatCallbacks['onDeepResearch']>(
    (question) => {
      if (!activeArcRef.current) {
        toast('Start a conversation first, then run Deep Research in it.', 'info');
        return;
      }
      setDrDone(null);
      setDrQuestion(question);
    },
    [toast],
  );

  const runDeepResearch = useCallback(
    async (question: string, depth: DeepResearchDepth, mode: DeepResearchMode | undefined) => {
      const arcId = activeArcRef.current;
      setDrQuestion(null);
      if (!arcId) return;
      setDrBusy(true);
      setDrDone(null);
      // Seed an immediate "planning" banner so the surface reacts before the
      // first SSE tick lands.
      setDrProgress({
        arc_id: arcId,
        phase: 'planning',
        detail: question,
        workers_total: 0,
        workers_done: 0,
        workers_ok: 0,
      });
      try {
        const res = await client.deepResearch(arcId, question, depth, mode);
        // The done banner is normally driven by SSE; fall back to the response
        // if the event was missed (e.g. stream lag).
        if (arcId === activeArcRef.current) {
          setDrProgress(null);
          setDrDone((cur) =>
            cur ?? {
              arc_id: res.arc_id,
              paper_path: res.paper_path,
              question: res.question,
              workers_ok: res.workers_ok,
              workers_total: res.workers_total,
              sub_questions: res.sub_questions,
              extended: res.extended,
            },
          );
        }
        toast(
          res.extended ? 'Research paper extended.' : 'Research paper ready.',
          'success',
        );
        void refreshArcs();
      } catch (e) {
        if (!bail(e)) {
          if (arcId === activeArcRef.current) setDrProgress(null);
          toast(errMessage(e), 'error');
        }
      } finally {
        setDrBusy(false);
      }
    },
    [client, toast, bail, refreshArcs],
  );

  const openSettings = useCallback((tab?: string) => {
    setSettingsTab(tab);
    setSettingsOpen(true);
  }, []);

  const cb = useMemo<ChatCallbacks>(
    () => ({
      onSend,
      onCancel: cancel,
      onAnswerQuestion,
      onDecideTask,
      onDecideGrant,
      onApprovePlan,
      onDiscardPlan,
      onOpenSettings: openSettings,
      onDeepResearch,
    }),
    [
      onSend,
      cancel,
      onAnswerQuestion,
      onDecideTask,
      onDecideGrant,
      onApprovePlan,
      onDiscardPlan,
      openSettings,
      onDeepResearch,
    ],
  );

  const activeMeta = useMemo(() => arcs.find((a) => a.id === activeArc), [arcs, activeArc]);
  const openProjectMeta = useMemo(
    () => projects.find((p) => p.id === openProject) ?? null,
    [projects, openProject],
  );
  // A project that was open but then deleted/vanished: fall back to chat.
  useEffect(() => {
    if (openProject && !openProjectMeta) setOpenProject(null);
  }, [openProject, openProjectMeta]);

  return (
    <div className="shell">
      <Sidebar
        arcs={arcs}
        activeArc={activeArc}
        unread={unread}
        open={sidebarOpen}
        onClose={() => setSidebarOpen(false)}
        projects={projects}
        activeProject={openProject}
        projectActions={{
          onSelect: (id) => {
            setOpenProject(id);
            setSidebarOpen(false);
          },
          onCreate: (name) => void createProject(name),
        }}
        actions={{
          onSelect: (id) => {
            setOpenProject(null);
            void switchArc(id);
          },
          onNew: () => {
            setOpenProject(null);
            void createArc();
          },
          onRename: (id, name) =>
            void client
              .post(`/arcs/${encodeURIComponent(id)}/rename`, { name })
              .then(refreshArcs, (e) => toast(`Couldn't rename: ${errMessage(e)}`, 'error')),
          onCompact: (id) =>
            void client
              .post<{ compacted?: boolean; tokens_before?: number; tokens_after?: number }>(
                `/arcs/${encodeURIComponent(id)}/compact`,
              )
              .then(async (r) => {
                if (r && r.compacted) {
                  const before = r.tokens_before ?? 0;
                  const after = r.tokens_after ?? 0;
                  dispatch({
                    type: 'system',
                    text: `Conversation compacted (${before} → ${after} tokens).`,
                  });
                  // Refresh the timeline so the new summary entry shows up,
                  // matching the desktop behaviour.
                  if (id === activeArcRef.current) {
                    const entries = await client.arcEntries(id).catch(() => null);
                    if (entries) dispatch({ type: 'reset', entries });
                  }
                } else {
                  dispatch({ type: 'system', text: 'Nothing to compact yet (conversation too short).' });
                }
              })
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
          <span className="arc-title">
            {openProjectMeta ? openProjectMeta.name : activeMeta?.name || 'Athen'}
          </span>
          <span className={`conn ${status}`} title={`Event stream: ${status}`} />
          {!openProjectMeta && activeMeta && (
            <ArcPickers client={client} arc={activeMeta} onChanged={() => void refreshArcs()} />
          )}
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
        {openProjectMeta ? (
          <ProjectPage
            client={client}
            project={openProjectMeta}
            onBack={() => setOpenProject(null)}
            onChanged={() => void refreshProjects()}
            onOpenArc={openArcFromProject}
            onNewArcInProject={(id) => void newArcInProject(id)}
          />
        ) : (
          <>
            {goal && (
              <GoalBanner
                goal={goal}
                onClear={() =>
                  void client
                    .del('/goal')
                    .then(() => setGoal(null), (e) => toast(`Couldn't clear goal: ${errMessage(e)}`, 'error'))
                }
              />
            )}
            <DeepResearchBanner
              progress={drProgress}
              done={drDone}
              onDismiss={() => setDrDone(null)}
            />
            <Chat
              items={chat.items}
              busy={busy}
              researchBusy={drBusy}
              arcKey={activeArc}
              plan={plan}
              client={client}
              cb={cb}
            />
          </>
        )}
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
      {settingsOpen && (
        <SettingsModal
          client={client}
          initialTab={settingsTab}
          onClose={() => {
            setSettingsOpen(false);
            setSettingsTab(undefined);
          }}
        />
      )}
      {drQuestion !== null && (
        <DeepResearchModal
          initialQuestion={drQuestion}
          hasPaper={Boolean(activeMeta?.research_paper_path)}
          priorQuestion={activeMeta?.research_question ?? null}
          onStart={(q, depth, mode) => void runDeepResearch(q, depth, mode)}
          onClose={() => setDrQuestion(null)}
        />
      )}
    </div>
  );
}
