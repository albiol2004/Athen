// Shared primitives for the Settings panels: data-loading hook, inline
// error / test-result display, and the 3-second "Sure?" confirm button.
// All visual classes are `st-` prefixed (see settings.css).

import { useCallback, useEffect, useRef, useState } from 'react';
import type { ReactNode } from 'react';
import { ApiError } from '../api/client';

/** Wire shape of every `TestResult` the backend returns. */
export interface TestResult {
  success: boolean;
  message: string;
}

export function errMsg(e: unknown): string {
  if (e instanceof ApiError) return e.message;
  if (e instanceof Error) return e.message;
  return String(e);
}

/**
 * Load data on mount (and whenever `deps` change). Exposes `reload`
 * so panels can refetch after each mutation.
 */
export function useLoad<T>(fn: () => Promise<T>, deps: unknown[] = []) {
  const [data, setData] = useState<T | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  // eslint-disable-next-line react-hooks/exhaustive-deps
  const load = useCallback(fn, deps);
  const reload = useCallback(async () => {
    setLoading(true);
    try {
      setData(await load());
      setError(null);
    } catch (e) {
      setError(errMsg(e));
    } finally {
      setLoading(false);
    }
  }, [load]);
  useEffect(() => {
    void reload();
  }, [reload]);
  return { data, loading, error, reload, setData };
}

/**
 * Run a mutation with pending + inline-error tracking. Returns true on
 * success so callers can chain a refetch / form reset.
 */
export function useAction() {
  const [pending, setPending] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const run = useCallback(async (fn: () => Promise<unknown>): Promise<boolean> => {
    setPending(true);
    setError(null);
    try {
      await fn();
      return true;
    } catch (e) {
      setError(errMsg(e));
      return false;
    } finally {
      setPending(false);
    }
  }, []);
  return { pending, error, run, setError };
}

export function ErrorText({ error }: { error: string | null | undefined }) {
  if (!error) return null;
  return <div className="st-error">{error}</div>;
}

export function Loading() {
  return <div className="st-dim">Loading…</div>;
}

/** Inline green/red test-result badge. */
export function TestBadge({ result }: { result: TestResult | null }) {
  if (!result) return null;
  return (
    <span className={`st-test ${result.success ? 'ok' : 'fail'}`}>{result.message}</span>
  );
}

/** Destructive button that swaps to "Sure?" for 3 s before firing. */
export function ConfirmButton({
  label,
  onConfirm,
  disabled,
  className,
}: {
  label: string;
  onConfirm: () => void;
  disabled?: boolean;
  className?: string;
}) {
  const [armed, setArmed] = useState(false);
  const timer = useRef<number | null>(null);
  useEffect(
    () => () => {
      if (timer.current !== null) window.clearTimeout(timer.current);
    },
    [],
  );
  return (
    <button
      type="button"
      disabled={disabled}
      className={`st-btn st-danger${armed ? ' armed' : ''} ${className ?? ''}`}
      onClick={() => {
        if (armed) {
          if (timer.current !== null) window.clearTimeout(timer.current);
          setArmed(false);
          onConfirm();
        } else {
          setArmed(true);
          timer.current = window.setTimeout(() => setArmed(false), 3000);
        }
      }}
    >
      {armed ? 'Sure?' : label}
    </button>
  );
}

export function Section({
  title,
  hint,
  actions,
  children,
}: {
  title: string;
  hint?: string;
  actions?: ReactNode;
  children: ReactNode;
}) {
  return (
    <section className="st-section">
      <div className="st-section-head">
        <div>
          <h3>{title}</h3>
          {hint && <p className="st-hint">{hint}</p>}
        </div>
        {actions && <div className="st-section-actions">{actions}</div>}
      </div>
      {children}
    </section>
  );
}

export function Field({
  label,
  children,
  grow,
}: {
  label: string;
  children: ReactNode;
  grow?: boolean;
}) {
  return (
    <label className={`st-field${grow ? ' grow' : ''}`}>
      <span>{label}</span>
      {children}
    </label>
  );
}
