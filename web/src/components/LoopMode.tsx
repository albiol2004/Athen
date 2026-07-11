// Loop Mode web UI (docs/LOOP_MODE.md) — the trigger button, start modal, and
// in-thread progress card, mirroring the Deep Research surface (DeepResearch.tsx)
// so a running loop reads like an agent working in the conversation. Purely
// presentational: state lives in Shell.tsx (`loopRuns`), this file only renders
// props and calls the client on user actions.

import { useState } from 'react';
import type { AthenClient } from '../api/client';
import { useInspector } from './Inspector';
import { Spinner } from './Spinner';
import { errMessage, useToast } from './Toast';
import type { LoopDoneEvent, LoopProgressEvent, LoopState } from '../api/types';

/**
 * Per-arc Loop Mode run state backing the in-thread card. Populated by SSE
 * (`loop-progress`/`loop-done`), by the optimistic seed in `runLoop`, and by
 * boot re-hydration from `loopStatus`. `progress`/`done` mirror the DR card's
 * two-phase shape; `loopRoot` (from the start ack or status) enables the mailbox
 * buttons even before the first progress tick.
 */
export interface LoopRun {
  goal: string | null;
  progress: LoopProgressEvent | null;
  done: LoopDoneEvent | null;
  /** Resolved loop root — presence gates the mailbox buttons. */
  loopRoot: string | null;
}

const MAILBOX_FILES = ['GOAL.md', 'PLAN.md', 'REPORT.md', 'VERDICT.md'] as const;

/** Human copy for each terminal `stop` reason. */
const TERMINAL: Record<Exclude<LoopState, 'running'>, { label: string; tone: string }> = {
  ship: { label: 'Ready to ship', tone: 'ship' },
  blocked: { label: 'Blocked — needs a human decision', tone: 'blocked' },
  blocked_double_iterate: { label: 'Blocked — the loop is stuck', tone: 'blocked' },
  cap_reached: { label: 'Iteration cap reached', tone: 'cap' },
  stopped: { label: 'Stopped', tone: 'stopped' },
  error: { label: 'Errored', tone: 'error' },
};

function LoopIcon() {
  return (
    <svg width="16" height="16" viewBox="0 0 24 24" fill="none" aria-hidden="true">
      <path
        d="M4 9a8 8 0 0 1 13.7-4M20 15A8 8 0 0 1 6.3 19"
        stroke="currentColor"
        strokeWidth="2"
        strokeLinecap="round"
      />
      <path d="M17 3v4h-4M7 21v-4h4" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" />
    </svg>
  );
}

function CheckIcon() {
  return (
    <svg width="11" height="11" viewBox="0 0 24 24" fill="none" aria-hidden="true">
      <path d="M5 13l4 4L19 7" stroke="currentColor" strokeWidth="3" strokeLinecap="round" strokeLinejoin="round" />
    </svg>
  );
}

/** Composer trigger, next to the Deep Research button. */
export function LoopModeButton({ busy, onClick }: { busy: boolean; onClick: () => void }) {
  return (
    <button
      type="button"
      className="loop-trigger"
      title="Loop Mode — run a Lead→Evaluator loop toward a goal"
      onClick={onClick}
      disabled={busy}
    >
      {busy ? <Spinner /> : <LoopIcon />}
    </button>
  );
}

/**
 * Start dialog. Collects the goal, optional acceptance criteria (folded into the
 * goal text under a heading — the backend takes a single `goal` string), and a
 * max-iterations cap (default 10).
 */
export function LoopModeModal({
  onStart,
  onClose,
}: {
  onStart: (goal: string, maxIterations: number) => void;
  onClose: () => void;
}) {
  const [goal, setGoal] = useState('');
  const [criteria, setCriteria] = useState('');
  const [maxIterations, setMaxIterations] = useState(10);

  const trimmed = goal.trim();

  const begin = () => {
    if (!trimmed) return;
    const crit = criteria.trim();
    const fullGoal = crit ? `${trimmed}\n\n## Acceptance criteria\n${crit}` : trimmed;
    const cap = Number.isFinite(maxIterations)
      ? Math.min(50, Math.max(1, Math.round(maxIterations)))
      : 10;
    onStart(fullGoal, cap);
  };

  return (
    <div
      className="confirm-overlay"
      onClick={(e) => {
        if (e.target === e.currentTarget) onClose();
      }}
    >
      <div className="confirm-dialog dr-modal" role="dialog" aria-modal="true" aria-label="Loop Mode">
        <h3>Loop Mode</h3>
        <p className="dr-modal-sub">
          Athen runs a Lead→Evaluator loop toward your goal, iterating until the Evaluator
          rules SHIP or BLOCKED (or the iteration cap is hit).
        </p>
        <label className="dr-field-label" htmlFor="loop-goal">
          Goal
        </label>
        <textarea
          id="loop-goal"
          className="dr-question"
          rows={3}
          autoFocus
          placeholder="e.g. Add a --dry-run flag to the export command, with tests."
          value={goal}
          onChange={(e) => setGoal(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === 'Enter' && (e.metaKey || e.ctrlKey)) {
              e.preventDefault();
              begin();
            }
          }}
        />
        <label className="dr-field-label" htmlFor="loop-criteria">
          Acceptance criteria <span className="loop-optional">(optional)</span>
        </label>
        <textarea
          id="loop-criteria"
          className="dr-question"
          rows={2}
          placeholder="How the Evaluator should judge done — commands, behaviors, checks."
          value={criteria}
          onChange={(e) => setCriteria(e.target.value)}
        />
        <label className="dr-field-label" htmlFor="loop-max">
          Max iterations
        </label>
        <input
          id="loop-max"
          className="loop-max-input"
          type="number"
          min={1}
          max={50}
          value={maxIterations}
          onChange={(e) => setMaxIterations(e.target.valueAsNumber)}
        />
        <div className="confirm-buttons">
          <button type="button" className="confirm-cancel" onClick={onClose}>
            Cancel
          </button>
          <button type="button" className="confirm-ok" disabled={!trimmed} onClick={begin}>
            Start loop
          </button>
        </div>
      </div>
    </div>
  );
}

/**
 * In-thread progress card. Running → live role/iteration + Stop; terminal →
 * final ruling + dismiss. Mailbox buttons open the loop's GOAL/PLAN/REPORT/
 * VERDICT in the Inspector; a not-yet-written file surfaces as an info toast,
 * never a raw error.
 */
export function LoopThreadCard({
  client,
  run,
  onStop,
  onDismiss,
}: {
  client: AthenClient;
  run: LoopRun;
  onStop: () => void;
  onDismiss: () => void;
}) {
  const inspector = useInspector();
  const { toast } = useToast();
  const [loadingFile, setLoadingFile] = useState<string | null>(null);

  const { progress, done, loopRoot } = run;
  const arcId = done?.arc_id ?? progress?.arc_id;
  const running = !done && !!progress;

  const viewFile = async (name: string) => {
    if (!inspector || !arcId) return;
    setLoadingFile(name);
    try {
      const md = await client.getLoopFile(arcId, name);
      inspector.open({ kind: 'paper', title: name, markdown: md });
    } catch (e) {
      // The mailbox endpoint returns "file not found" for a file a role has not
      // written yet (e.g. VERDICT.md before the first Evaluator run). Surface
      // that as a gentle info note, not a scary error.
      const msg = errMessage(e);
      if (/not found|not written/i.test(msg)) {
        toast(`${name} hasn't been written yet.`, 'info');
      } else {
        toast(msg, 'error');
      }
    } finally {
      setLoadingFile(null);
    }
  };

  const mailbox = loopRoot ? (
    <div className="loop-mailbox" role="group" aria-label="Loop mailbox">
      {MAILBOX_FILES.map((name) => (
        <button
          key={name}
          type="button"
          className="loop-mailbox-btn"
          disabled={loadingFile !== null}
          onClick={() => viewFile(name)}
        >
          {loadingFile === name ? <Spinner /> : name.replace(/\.md$/, '')}
        </button>
      ))}
    </div>
  ) : null;

  if (done) {
    const term = TERMINAL[done.stop as Exclude<LoopState, 'running'>] ?? {
      label: done.stop,
      tone: 'stopped',
    };
    const iters =
      done.iterations_run != null && done.max_iterations != null
        ? `${done.iterations_run} / ${done.max_iterations} iterations`
        : done.iterations_run != null
          ? `${done.iterations_run} iterations`
          : null;
    return (
      <div className={`loop-thread-card done ${term.tone}`}>
        <div className="loop-thread-head">
          <span className="loop-thread-icon done">
            <CheckIcon />
          </span>
          <span className="loop-thread-label">Loop Mode — {term.label}</span>
          <button type="button" className="loop-thread-close" aria-label="Dismiss" onClick={onDismiss}>
            ×
          </button>
        </div>
        {iters && <div className="loop-thread-stat">{iters}</div>}
        {done.last_verdict && (
          <div className="loop-thread-stat">Final verdict: {done.last_verdict}</div>
        )}
        {done.verdict_source_disagreement && (
          <div className="loop-thread-detail">
            Note: the Evaluator's structured verdict disagreed with loop/VERDICT.md; the file was
            used as truth.
          </div>
        )}
        {mailbox}
      </div>
    );
  }

  if (!progress) return null;

  const role =
    progress.phase === 'lead'
      ? 'Lead working…'
      : progress.phase === 'evaluator'
        ? 'Evaluator verifying…'
        : 'Wrapping up…';

  return (
    <div className="loop-thread-card running">
      <div className="loop-thread-head">
        <span className="loop-live-dot" />
        <span className="loop-thread-label">Loop Mode</span>
        <span className="loop-thread-tag">
          {progress.iteration} / {progress.max_iterations}
        </span>
      </div>
      <div className="loop-thread-role">{role}</div>
      {progress.detail && <div className="loop-thread-detail">{progress.detail}</div>}
      {progress.last_verdict && (
        <div className="loop-thread-stat">Last verdict: {progress.last_verdict}</div>
      )}
      {mailbox}
      {running && (
        <div className="loop-thread-actions">
          <button type="button" className="loop-stop" onClick={onStop}>
            Stop loop
          </button>
          <span className="loop-stop-hint">Stops at the next role boundary, not instantly.</span>
        </div>
      )}
    </div>
  );
}
