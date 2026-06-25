// Deep Research UI (docs/DEEP_RESEARCH.md) for the web frontend — at parity
// with the desktop surface. Three pieces, all driven from Shell:
//   - DeepResearchButton: lives in the composer, opens the launcher.
//   - DeepResearchModal:  question (if the composer was empty) + depth picker,
//     then (when the active arc already has a paper) a ConfirmDialog for
//     extend-vs-new before the POST.
//   - DeepResearchBanner: non-blocking progress ticker + completion card,
//     driven by the `deep-research-progress` / `deep-research-done` SSE events.
//
// Backend contract: POST /api/arcs/{id}/deep-research { question, depth?, mode? }
// (AthenClient.deepResearch) to launch; GET /api/arcs/{id}/research-paper
// (AthenClient.getResearchPaper, body = the Markdown string) to read the
// finished paper. The completion card's "View paper" fetches that Markdown and
// renders it inline in a modal via the shared <Markdown> renderer.

import { useEffect, useState } from 'react';
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

const DocIcon = () => (
  <svg width="18" height="18" viewBox="0 0 24 24" fill="none" aria-hidden="true">
    <path
      d="M14 3H7a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2V8l-5-5Zm0 0v5h5"
      stroke="currentColor"
      strokeWidth="1.8"
      strokeLinejoin="round"
    />
    <path d="M9 13h6M9 16h4" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" />
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

const PHASE_LABEL: Record<DeepResearchProgressEvent['phase'], string> = {
  planning: 'Planning research…',
  reading: 'Researching sources…',
  synthesizing: 'Writing the paper…',
};

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
            className="dr-banner-close"
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
 * Non-blocking banner. While `progress` is set (and `done` is not) it shows the
 * phase ticker + a bar during the reading phase. Once `done` arrives it shows
 * the completion card. Both are scoped to the active arc by Shell.
 */
export function DeepResearchBanner({
  client,
  progress,
  done,
  onDismiss,
}: {
  client: AthenClient;
  progress: DeepResearchProgressEvent | null;
  done: DeepResearchDoneEvent | null;
  onDismiss: () => void;
}) {
  const [viewing, setViewing] = useState(false);

  if (done) {
    const basename = paperBasename(done.paper_path);
    return (
      <div className="dr-banner dr-done" role="status">
        <span className="dr-done-icon">
          <DocIcon />
        </span>
        <div className="dr-done-body">
          <div className="dr-done-title">
            Research paper ready
            {done.extended && <span className="dr-done-tag"> (extended)</span>}
          </div>
          <div className="dr-done-file" title={done.paper_path}>
            {basename}
          </div>
          <div className="dr-done-meta">
            {done.workers_ok}/{done.workers_total} sub-topics covered · or ask in chat — I can read it.
          </div>
          <div className="dr-done-actions">
            <button type="button" className="dr-view-paper" onClick={() => setViewing(true)}>
              View paper
            </button>
          </div>
        </div>
        <button type="button" className="dr-banner-close" aria-label="Dismiss" onClick={onDismiss}>
          ×
        </button>
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
  const pct =
    reading && progress.workers_total > 0
      ? Math.round((progress.workers_done / progress.workers_total) * 100)
      : 0;
  const label =
    reading && progress.workers_total > 0
      ? `${PHASE_LABEL.reading} (${progress.workers_done}/${progress.workers_total}, ${progress.workers_ok} ok)`
      : PHASE_LABEL[progress.phase];

  return (
    <div className="dr-banner dr-progress" role="status" aria-live="polite">
      <span className="dr-progress-spin">
        <Spinner />
      </span>
      <div className="dr-progress-body">
        <div className="dr-progress-label">{label}</div>
        {progress.detail && <div className="dr-progress-detail">{progress.detail}</div>}
        {reading && progress.workers_total > 0 && (
          <div className="dr-progress-bar" aria-hidden="true">
            <div className="dr-progress-fill" style={{ width: `${pct}%` }} />
          </div>
        )}
      </div>
    </div>
  );
}
