// Deep Research UI (docs/DEEP_RESEARCH.md) for the web frontend — at parity
// with the desktop surface. Three pieces, all driven from Shell:
//   - DeepResearchButton: lives in the composer, opens the launcher.
//   - DeepResearchModal:  question (if the composer was empty) + depth picker,
//     then (when the active arc already has a paper) a ConfirmDialog for
//     extend-vs-new before the POST.
//   - DeepResearchThreadCard: in-thread progress stepper + completion card
//     (rendered inside the chat thread by <Chat>, so a run reads like an agent
//     working in the conversation), driven by the `deep-research-progress` /
//     `deep-research-done` SSE events. Arc-scoped by Shell's per-arc run map.
//
// Backend contract: POST /api/arcs/{id}/deep-research { question, depth?, mode? }
// (AthenClient.deepResearch) to launch; GET /api/arcs/{id}/research-paper
// (AthenClient.getResearchPaper, body = the Markdown string) to read the
// finished paper. The completion card's "View paper" fetches that Markdown and
// renders it inline in a modal via the shared <Markdown> renderer.

import { Fragment, useEffect, useState } from 'react';
import type { AthenClient } from '../api/client';
import { ConfirmDialog } from './ConfirmDialog';
import { Markdown } from './Markdown';
import { Spinner } from './Spinner';
import { errMessage, useToast } from './Toast';
import type {
  DeepResearchDepth,
  DeepResearchDoneEvent,
  DeepResearchMode,
  DeepResearchProgressEvent,
} from '../api/types';

const DEPTHS: { value: DeepResearchDepth; label: string; hint: string }[] = [
  { value: 'quick', label: 'Quick', hint: '3 sub-topics · fast' },
  { value: 'standard', label: 'Standard', hint: '5 sub-topics · balanced' },
  { value: 'deep', label: 'Deep', hint: '8 sub-topics · thorough' },
];

/** Last path segment of a workspace-relative paper path. */
export function paperBasename(path: string): string {
  const parts = path.split(/[\\/]/).filter(Boolean);
  return parts[parts.length - 1] ?? path;
}

const ResearchIcon = ({ size = 15 }: { size?: number }) => (
  <svg width={size} height={size} viewBox="0 0 24 24" fill="none" aria-hidden="true">
    <circle cx="11" cy="11" r="6.5" stroke="currentColor" strokeWidth="1.8" />
    <path d="m16 16 4.5 4.5" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" />
    <path d="M11 8v6M8 11h6" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" />
  </svg>
);

const CheckIcon = () => (
  <svg width="11" height="11" viewBox="0 0 24 24" fill="none" aria-hidden="true">
    <polyline
      points="20 6 9 17 4 12"
      stroke="currentColor"
      strokeWidth="3"
      strokeLinecap="round"
      strokeLinejoin="round"
    />
  </svg>
);

/** Composer button that opens the Deep Research launcher. */
export function DeepResearchButton({
  busy,
  onClick,
}: {
  busy: boolean;
  onClick: () => void;
}) {
  return (
    <button
      type="button"
      className="dr-trigger"
      title="Deep Research — research a question into a cited paper"
      onClick={onClick}
      disabled={busy}
    >
      {busy ? <Spinner /> : <ResearchIcon />}
    </button>
  );
}

/**
 * Launch flow. `initialQuestion` is the composer text (may be empty → we ask
 * for one). When `hasPaper` is true the user picks extend-vs-new via a
 * ConfirmDialog before the run begins. `onStart` performs the POST.
 */
export function DeepResearchModal({
  initialQuestion,
  hasPaper,
  priorQuestion,
  onStart,
  onClose,
}: {
  initialQuestion: string;
  hasPaper: boolean;
  priorQuestion: string | null;
  onStart: (question: string, depth: DeepResearchDepth, mode: DeepResearchMode | undefined) => void;
  onClose: () => void;
}) {
  const [question, setQuestion] = useState(initialQuestion);
  const [depth, setDepth] = useState<DeepResearchDepth>('standard');
  // When set, the extend-vs-new ConfirmDialog is showing for this question.
  const [pendingQuestion, setPendingQuestion] = useState<string | null>(null);

  const trimmed = question.trim();

  const begin = () => {
    if (!trimmed) return;
    if (hasPaper) {
      setPendingQuestion(trimmed);
      return;
    }
    onStart(trimmed, depth, undefined);
  };

  if (pendingQuestion !== null) {
    const subject = priorQuestion ? `"${priorQuestion}"` : 'the existing paper';
    return (
      <ConfirmDialog
        title="Extend or start fresh?"
        body={`This arc already has a research paper on ${subject}. Extend it with the new findings, or start a separate paper?`}
        confirmLabel="Extend"
        cancelLabel="New paper"
        onConfirm={() => onStart(pendingQuestion, depth, 'extend')}
        onCancel={() => onStart(pendingQuestion, depth, 'new')}
      />
    );
  }

  return (
    <div
      className="confirm-overlay"
      onClick={(e) => {
        if (e.target === e.currentTarget) onClose();
      }}
    >
      <div className="confirm-dialog dr-modal" role="dialog" aria-modal="true" aria-label="Deep Research">
        <h3>Deep Research</h3>
        <p className="dr-modal-sub">
          Athen decomposes the question, reads many sources in parallel, and writes a cited paper.
        </p>
        <label className="dr-field-label" htmlFor="dr-question">
          Question
        </label>
        <textarea
          id="dr-question"
          className="dr-question"
          rows={3}
          autoFocus
          placeholder="e.g. What's the state of EU right-to-repair law vs California's?"
          value={question}
          onChange={(e) => setQuestion(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === 'Enter' && (e.metaKey || e.ctrlKey)) {
              e.preventDefault();
              begin();
            }
          }}
        />
        <div className="dr-field-label">Depth</div>
        <div className="dr-depths" role="radiogroup" aria-label="Research depth">
          {DEPTHS.map((d) => (
            <button
              key={d.value}
              type="button"
              role="radio"
              aria-checked={depth === d.value}
              className={`dr-depth-chip${depth === d.value ? ' active' : ''}`}
              onClick={() => setDepth(d.value)}
            >
              <span className="dr-depth-label">{d.label}</span>
              <span className="dr-depth-hint">{d.hint}</span>
            </button>
          ))}
        </div>
        <div className="confirm-buttons">
          <button type="button" className="confirm-cancel" onClick={onClose}>
            Cancel
          </button>
          <button type="button" className="confirm-ok" disabled={!trimmed} onClick={begin}>
            Start research
          </button>
        </div>
      </div>
    </div>
  );
}

type Phase = DeepResearchProgressEvent['phase'];

const PHASE_LABEL: Record<Phase, string> = {
  planning: 'Decomposing the question…',
  reading: 'Researching sources',
  refining: 'Reviewing findings for gaps…',
  synthesizing: 'Writing the paper…',
};

// Pipeline shown in the stepper. "Refining" only lights up on deep runs (a
// `refining` event arrives between reading rounds); otherwise it's skipped.
const DR_STEPS: { key: Phase; label: string }[] = [
  { key: 'planning', label: 'Planning' },
  { key: 'reading', label: 'Researching' },
  { key: 'refining', label: 'Refining' },
  { key: 'synthesizing', label: 'Synthesizing' },
];
const DR_STEP_INDEX: Record<Phase, number> = {
  planning: 0,
  reading: 1,
  refining: 2,
  synthesizing: 3,
};

/** Four-step phase ticker. Completed steps show a check, the current one is
 *  active (pulsing), and "Refining" greys out (skipped) once a non-deep run
 *  moves past it. */
function DeepResearchStepper({
  current,
  refiningSeen,
  finished,
}: {
  current: Phase;
  refiningSeen: boolean;
  finished: boolean;
}) {
  const curIdx = finished ? DR_STEPS.length : DR_STEP_INDEX[current];
  return (
    <div className="dr-stepper">
      {DR_STEPS.map((step, i) => {
        let state: 'done' | 'active' | 'pending' | 'skipped';
        if (step.key === 'refining' && !refiningSeen && !finished) {
          state = curIdx > 2 ? 'skipped' : 'pending';
        } else if (i < curIdx) {
          state = 'done';
        } else if (i === curIdx) {
          state = 'active';
        } else {
          state = 'pending';
        }
        return (
          <Fragment key={step.key}>
            {i > 0 && <span className={`dr-step-conn${i <= curIdx ? ' filled' : ''}`} />}
            <div className={`dr-step dr-step-${state}`}>
              <span className="dr-step-marker">{state === 'done' ? <CheckIcon /> : null}</span>
              <span className="dr-step-name">{step.label}</span>
            </div>
          </Fragment>
        );
      })}
    </div>
  );
}

/**
 * Modal that fetches the arc's research paper Markdown via
 * `client.getResearchPaper` and renders it with the shared <Markdown>
 * component (same renderer the chat uses for assistant messages). Spinner
 * while pending, toast on failure (then closes). Overlay/dialog chrome mirrors
 * ConfirmDialog/DeepResearchModal (`confirm-overlay`/`confirm-dialog`).
 */
export function ResearchPaperModal({
  client,
  arcId,
  title,
  onClose,
}: {
  client: AthenClient;
  arcId: string;
  title: string;
  onClose: () => void;
}) {
  const { toast } = useToast();
  const [markdown, setMarkdown] = useState<string | null>(null);

  useEffect(() => {
    let alive = true;
    client.getResearchPaper(arcId).then(
      (md) => {
        if (alive) setMarkdown(md);
      },
      (e: unknown) => {
        if (!alive) return;
        toast(errMessage(e), 'error');
        onClose();
      },
    );
    return () => {
      alive = false;
    };
  }, [client, arcId, toast, onClose]);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') onClose();
    };
    document.addEventListener('keydown', onKey);
    return () => document.removeEventListener('keydown', onKey);
  }, [onClose]);

  return (
    <div
      className="confirm-overlay"
      onClick={(e) => {
        if (e.target === e.currentTarget) onClose();
      }}
    >
      <div
        className="confirm-dialog dr-paper-modal"
        role="dialog"
        aria-modal="true"
        aria-label="Research paper"
      >
        <div className="dr-paper-head">
          <h3 title={title}>{title}</h3>
          <button
            type="button"
            className="dr-thread-close"
            aria-label="Close"
            onClick={onClose}
          >
            ×
          </button>
        </div>
        <div className="dr-paper-body">
          {markdown === null ? (
            <div className="dr-paper-loading">
              <Spinner />
              <span>Loading paper…</span>
            </div>
          ) : (
            <Markdown text={markdown} />
          )}
        </div>
      </div>
    </div>
  );
}

/**
 * In-thread Deep Research card. Rendered inside the chat message thread (by
 * <Chat>) so the run reads like an agent working in the conversation. While
 * `progress` is set (and `done` is not) it shows the phase stepper + a bar
 * during the reading phase; once `done` arrives it becomes the result card.
 * `question` is the launch question (shown during progress; the done event
 * carries its own). All inputs are scoped to the active arc by Shell.
 */
export function DeepResearchThreadCard({
  client,
  question,
  progress,
  done,
  refiningSeen,
  onDismiss,
}: {
  client: AthenClient;
  question: string | null;
  progress: DeepResearchProgressEvent | null;
  done: DeepResearchDoneEvent | null;
  refiningSeen: boolean;
  onDismiss: () => void;
}) {
  const [viewing, setViewing] = useState(false);

  if (done) {
    const basename = paperBasename(done.paper_path);
    return (
      <div className="dr-thread-card done" role="status">
        <div className="dr-thread-head">
          <span className="dr-thread-icon done">
            <CheckIcon />
          </span>
          <span className="dr-thread-label">Research paper ready</span>
          <span className="dr-thread-tag">
            {done.extended ? 'Extended existing paper' : 'New paper'}
          </span>
          <button
            type="button"
            className="dr-thread-close"
            aria-label="Dismiss"
            onClick={onDismiss}
          >
            ×
          </button>
        </div>
        {done.question && <div className="dr-thread-question">{done.question}</div>}
        <div className="dr-thread-file" title={done.paper_path}>
          {basename}
        </div>
        <div className="dr-thread-stat">
          {done.workers_ok}/{done.workers_total} sub-topics covered
        </div>
        {done.sub_questions.length > 0 && (
          <details className="dr-thread-subq">
            <summary>
              {done.sub_questions.length} sub-question
              {done.sub_questions.length === 1 ? '' : 's'} investigated
            </summary>
            <ul>
              {done.sub_questions.map((q, i) => (
                <li key={i}>{q}</li>
              ))}
            </ul>
          </details>
        )}
        <div className="dr-thread-actions">
          <button type="button" className="dr-view-paper" onClick={() => setViewing(true)}>
            View paper
          </button>
        </div>
        {viewing && (
          <ResearchPaperModal
            client={client}
            arcId={done.arc_id}
            title={basename}
            onClose={() => setViewing(false)}
          />
        )}
      </div>
    );
  }

  if (!progress) return null;

  const reading = progress.phase === 'reading';
  const hasBar = reading && progress.workers_total > 0;
  const pct = hasBar ? Math.round((progress.workers_done / progress.workers_total) * 100) : 0;

  return (
    <div className="dr-thread-card running" role="status" aria-live="polite">
      <div className="dr-thread-head">
        <span className="dr-thread-icon">
          <ResearchIcon />
        </span>
        <span className="dr-thread-label">Deep Research</span>
        <span className="dr-live-dot" aria-hidden="true" />
      </div>
      {question && <div className="dr-thread-question">{question}</div>}
      <DeepResearchStepper current={progress.phase} refiningSeen={refiningSeen} finished={false} />
      <div className="dr-thread-status">{PHASE_LABEL[progress.phase]}</div>
      {hasBar && (
        <>
          <div className="dr-thread-bar" aria-hidden="true">
            <div className="dr-thread-fill" style={{ width: `${pct}%` }} />
          </div>
          <div className="dr-thread-stat">
            {progress.workers_done}/{progress.workers_total} researchers reporting ·{' '}
            {progress.workers_ok} succeeded
          </div>
        </>
      )}
      {progress.detail && <div className="dr-thread-detail">{progress.detail}</div>}
    </div>
  );
}
