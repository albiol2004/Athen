// Goal banner (below the topbar) + plan drafting card + plan progress
// banner — ports of the desktop's goal/plan surfaces.

import { useState } from 'react';
import { Spinner } from './Spinner';

export interface GoalState {
  goal?: string | null;
  criteria?: string | null;
  [k: string]: unknown;
}
export interface PlanStep {
  description: string;
  completed?: boolean;
  [k: string]: unknown;
}
export interface PlanState {
  goal?: string | null;
  acceptance_criteria?: string | null;
  steps?: PlanStep[];
  status?: string;
  [k: string]: unknown;
}

export function GoalBanner({ goal, onClear }: { goal: GoalState; onClear: () => void }) {
  if (!goal.goal) return null;
  return (
    <div className="goal-banner">
      <span className="goal-dot" />
      <span className="goal-text">
        {goal.goal}
        {goal.criteria && <span className="goal-criteria"> — done when: {goal.criteria}</span>}
      </span>
      <button className="goal-clear" onClick={onClear} title="Clear goal">
        ×
      </button>
    </div>
  );
}

export function PlanCard({
  plan,
  onApprove,
  onDiscard,
}: {
  plan: PlanState;
  onApprove: () => Promise<void>;
  onDiscard: () => Promise<void>;
}) {
  const [pendingLabel, setPendingLabel] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const steps = plan.steps ?? [];
  const drafting = (plan.status ?? 'Drafting') === 'Drafting';
  const pending = pendingLabel !== null;
  const run = (label: string, f: () => Promise<void>) => async () => {
    if (pending) return; // re-entry guard
    setPendingLabel(label);
    setError(null);
    try {
      await f();
    } catch (e) {
      // Surface the failure inline and re-enable so the user can retry.
      setError((e as Error).message);
    } finally {
      setPendingLabel(null);
    }
  };
  return (
    <div className="plan-card">
      <h4>
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" aria-hidden="true">
          <path d="M4 6h16M4 12h10M4 18h7" stroke="currentColor" strokeWidth="2" strokeLinecap="round" />
        </svg>
        {plan.goal || 'Plan'}
      </h4>
      {plan.acceptance_criteria && <p className="plan-criteria">Done when: {plan.acceptance_criteria}</p>}
      <ol className="plan-steps">
        {steps.map((s, i) => (
          <li key={i} className={s.completed ? 'done' : ''}>
            {s.description}
          </li>
        ))}
      </ol>
      {drafting ? (
        <>
          <div className="actions">
            <button className="approve" disabled={pending} onClick={run('approve', onApprove)}>
              {pendingLabel === 'approve' && <Spinner />}
              Approve &amp; execute
            </button>
            <button disabled={pending} onClick={run('discard', onDiscard)}>
              {pendingLabel === 'discard' && <Spinner />}
              Discard
            </button>
          </div>
          {error && <div className="card-error">{error}</div>}
        </>
      ) : (
        <div className="plan-progress">
          Step {Math.min(steps.filter((s) => s.completed).length + 1, steps.length)} of {steps.length} —{' '}
          {plan.status}
        </div>
      )}
    </div>
  );
}
