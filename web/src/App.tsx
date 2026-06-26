import { useEffect, useMemo, useState } from 'react';
import { AthenClient, type ClientConfig } from './api/client';
import { Login } from './components/Login';
import { Shell } from './components/Shell';

const STORAGE_KEY = 'athen.web.auth';

/** Path prefix when served through the admin-panel gateway
 * (`/i/{instance}/…`). There the panel session cookie authenticates us
 * and the proxy injects the instance bearer — no token, no login screen,
 * just the prefix on every API call. */
function gatewayBase(): string | null {
  const m = window.location.pathname.match(/^(\/i\/[^/]+)(\/|$)/);
  return m ? m[1] : null;
}

function loadSaved(): ClientConfig | null {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (!raw) return null;
    const v = JSON.parse(raw) as Partial<ClientConfig>;
    if (typeof v.token !== 'string' || typeof v.baseUrl !== 'string') return null;
    const cfg: ClientConfig = { baseUrl: v.baseUrl, token: v.token };
    // HTTP Basic credentials (docs/REMOTE_ACCESS.md §4), when present.
    if (typeof v.username === 'string') cfg.username = v.username;
    if (typeof v.password === 'string') cfg.password = v.password;
    return cfg;
  } catch {
    return null;
  }
}

/** Serialize only the fields we persist (baseUrl/token + optional Basic creds). */
function saveCfg(c: ClientConfig): void {
  const out: ClientConfig = { baseUrl: c.baseUrl, token: c.token };
  if (c.username) out.username = c.username;
  if (c.password) out.password = c.password;
  localStorage.setItem(STORAGE_KEY, JSON.stringify(out));
}

export function App() {
  const gw = useMemo(gatewayBase, []);
  const [cfg, setCfg] = useState<ClientConfig | null>(gw ? null : loadSaved);

  useEffect(() => {
    if (!gw) return;
    const c: ClientConfig = { baseUrl: gw, token: '' };
    new AthenClient(c)
      .currentArc()
      .then(() => setCfg(c))
      .catch(() => {
        // No live panel session — the panel login page sorts it out.
        window.location.href = '/';
      });
  }, [gw]);

  const client = useMemo(() => (cfg ? new AthenClient(cfg) : null), [cfg]);

  if (!client) {
    if (gw) return null; // gateway probe in flight (or redirecting)
    return (
      <Login
        onLogin={(c) => {
          saveCfg(c);
          setCfg(c);
        }}
      />
    );
  }
  return (
    <Shell
      client={client}
      onLogout={() => {
        if (gw) {
          window.location.href = '/';
          return;
        }
        localStorage.removeItem(STORAGE_KEY);
        setCfg(null);
      }}
    />
  );
}
