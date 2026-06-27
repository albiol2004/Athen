// Interactive timeline pieces: tool chips, thinking blocks, and the
// three approval-card flavors (coordinator question, risk-gate task,
// file-permission grant). Wire shapes mirror chat.html / app.js.

import { useState } from 'react';
import { Spinner } from './Spinner';
import type {
  ApprovalChoice,
  ApprovalQuestion,
  GrantDecision,
  GrantRequest,
  PendingApproval,
} from '../api/types';

export function ToolChip({
  name,
  status,
  detail,
  failed,
}: {
  name: string;
  status: string;
  detail?: string;
  failed?: boolean;
}) {
  const done = status === 'Completed' || status === 'completed' || status === 'done';
  return (
    <div
      className={`chip${done ? ' done' : ''}${failed ? ' failed' : ''}`}
      title={detail || undefined}
    >
      <span className="dot" />
      <span className="label">
        {name}
        {!done && !failed ? ` — ${status}` : ''}
      </span>
    </div>
  );
}

export function ThinkingBlock({ content, done }: { content: string; done: boolean }) {
  // While live: forced open. Once done the attribute is dropped, the
  // block collapses, and the user can re-expand it freely.
  return (
    <details className={`thinking${done ? '' : ' thinking-live'}`} open={done ? undefined : true}>
      <summary>
        {!done && <span className="tool-pulse-dot" />}
        {done ? 'Thought process' : 'Thinking…'}
      </summary>
      <pre>{content}</pre>
    </details>
  );
}

/** Shared async-button row: disables siblings while a choice is in flight. */
function ActionRow({
  resolved,
  actions,
}: {
  resolved?: string;
  actions: { label: string; primary?: boolean; run: () => Promise<void> }[];
}) {
  // Track WHICH action is in flight so only the clicked button spins.
  // A non-null value also doubles as the re-entry guard (all buttons
  // disabled while any one is running).
  const [pendingLabel, setPendingLabel] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  if (resolved) return <div className="resolved">{resolved}</div>;
  const pending = pendingLabel !== null;
  return (
    <>
      <div className="actions">
        {actions.map((a) => (
          <button
            key={a.label}
            className={a.primary ? 'approve' : undefined}
            disabled={pending}
            onClick={async () => {
              if (pending) return; // re-entry guard
              setPendingLabel(a.label);
              setError(null);
              try {
                await a.run();
                // On success the card resolves and this row unmounts, so we
                // intentionally leave the button locked.
              } catch (e) {
                setError((e as Error).message);
                setPendingLabel(null); // re-enable so the user can retry
              }
            }}
          >
            {pendingLabel === a.label && <Spinner />}
            {a.label}
          </button>
        ))}
      </div>
      {error && <div className="card-error">{error}</div>}
    </>
  );
}

export function QuestionCard({
  q,
  resolved,
  onAnswer,
}: {
  q: ApprovalQuestion;
  resolved?: string;
  onAnswer: (choice: ApprovalChoice) => Promise<void>;
}) {
  const choices: ApprovalChoice[] =
    q.choices && q.choices.length
      ? q.choices
      : [
          { key: 'approve', label: 'Approve', kind: 'approve' },
          { key: 'deny', label: 'Deny', kind: 'deny' },
        ];
  return (
    <div className="card">
      <h4>{q.prompt || 'Approval needed'}</h4>
      {q.description && <p>{q.description}</p>}
      <ActionRow
        resolved={resolved}
        actions={choices.map((c) => ({
          label: c.label || c.key,
          primary: c.kind === 'approve',
          run: () => onAnswer(c),
        }))}
      />
    </div>
  );
}

export function TaskCard({
  t,
  resolved,
  onDecide,
}: {
  t: PendingApproval;
  resolved?: string;
  onDecide: (approved: boolean) => Promise<void>;
}) {
  const risk = `${t.risk_level || '?'}${t.risk_score != null ? ` ${t.risk_score}` : ''}`;
  return (
    <div className="card">
      <h4>Approval needed (risk: {risk})</h4>
      <p>{t.description || t.summary || 'Athen wants to run a risky task.'}</p>
      <ActionRow
        resolved={resolved}
        actions={[
          { label: 'Approve', primary: true, run: () => onDecide(true) },
          { label: 'Deny', run: () => onDecide(false) },
        ]}
      />
    </div>
  );
}

export function GrantCard({
  g,
  resolved,
  onDecide,
}: {
  g: GrantRequest;
  resolved?: string;
  onDecide: (decision: GrantDecision, label: string) => Promise<void>;
}) {
  const actions: { label: string; primary?: boolean; run: () => Promise<void> }[] = [];
  if (g.detected_root) {
    const label = `Allow ${g.detected_root.pathDisplay || g.detected_root.path}${
      g.detected_root.marker ? ` (${g.detected_root.marker})` : ''
    }`;
    const path = g.detected_root.path;
    actions.push({ label, primary: true, run: () => onDecide({ AllowProjectRoot: path }, label) });
  }
  actions.push(
    { label: 'Allow once', primary: !g.detected_root, run: () => onDecide('Allow', 'Allowed once') },
    { label: 'Allow always', run: () => onDecide('AllowAlways', 'Allowed always') },
    { label: 'Deny', run: () => onDecide('Deny', 'Denied') },
  );
  return (
    <div className="card">
      <h4>
        File permission: {g.access || 'access'} via {g.tool || 'tool'}
      </h4>
      <p className="paths">{(g.paths || []).join(', ')}</p>
      <ActionRow resolved={resolved} actions={actions} />
    </div>
  );
}
