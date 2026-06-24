// Global toast notifications — warm-dark/glass stack pinned bottom-right.
// Mirrors the desktop `frontend/app.js` toast helper so async failures the
// user kicked off (a setting that didn't save, a rename that bounced) surface
// instead of vanishing into an empty `.catch(() => {})`.
//
// Usage:
//   const { toast } = useToast();
//   client.post(...).catch((e) => toast(errMessage(e), 'error'));

import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useRef,
  useState,
  type ReactNode,
} from 'react';
import { ApiError } from '../api/client';

export type ToastKind = 'error' | 'success' | 'info';

interface ToastItem {
  id: number;
  message: string;
  kind: ToastKind;
}

interface ToastApi {
  /** Show a transient toast. `kind` defaults to 'info'. */
  toast: (message: string, kind?: ToastKind) => void;
}

const ToastContext = createContext<ToastApi | null>(null);

/** Cap so a burst of failures can't bury the surface. */
const MAX_TOASTS = 4;
/** Errors linger; success/info clear faster. */
const TTL_MS: Record<ToastKind, number> = {
  error: 8000,
  success: 3500,
  info: 4500,
};

/** Normalize any thrown value to a readable, user-facing string. */
export function errMessage(e: unknown): string {
  if (e instanceof ApiError) return e.message;
  if (e instanceof Error) return e.message;
  if (typeof e === 'string') return e;
  return 'Something went wrong.';
}

export function ToastProvider({ children }: { children: ReactNode }) {
  const [items, setItems] = useState<ToastItem[]>([]);
  const nextId = useRef(1);
  const timers = useRef<Map<number, ReturnType<typeof setTimeout>>>(new Map());

  const dismiss = useCallback((id: number) => {
    setItems((list) => list.filter((t) => t.id !== id));
    const handle = timers.current.get(id);
    if (handle !== undefined) {
      clearTimeout(handle);
      timers.current.delete(id);
    }
  }, []);

  const toast = useCallback(
    (message: string, kind: ToastKind = 'info') => {
      const id = nextId.current++;
      setItems((list) => {
        const next = [...list, { id, message, kind }];
        // Drop the oldest if we'd exceed the cap.
        return next.length > MAX_TOASTS ? next.slice(next.length - MAX_TOASTS) : next;
      });
      const handle = setTimeout(() => dismiss(id), TTL_MS[kind]);
      timers.current.set(id, handle);
    },
    [dismiss],
  );

  useEffect(() => {
    const map = timers.current;
    return () => {
      for (const handle of map.values()) clearTimeout(handle);
      map.clear();
    };
  }, []);

  const api = useMemo<ToastApi>(() => ({ toast }), [toast]);

  return (
    <ToastContext.Provider value={api}>
      {children}
      <div className="toast-stack" role="region" aria-live="polite" aria-label="Notifications">
        {items.map((t) => (
          <div key={t.id} className={`toast toast-${t.kind}`} role="status">
            <span className="toast-msg">{t.message}</span>
            <button
              type="button"
              className="toast-close"
              aria-label="Dismiss"
              onClick={() => dismiss(t.id)}
            >
              ×
            </button>
          </div>
        ))}
      </div>
    </ToastContext.Provider>
  );
}

/**
 * Access the toast API. Safe to call outside a provider — returns a no-op
 * so a stray consumer can't crash the tree (it just won't show anything).
 */
export function useToast(): ToastApi {
  const ctx = useContext(ToastContext);
  return ctx ?? NOOP;
}

const NOOP: ToastApi = { toast: () => {} };
