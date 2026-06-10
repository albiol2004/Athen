import { useMemo, useState } from 'react';
import { AthenClient, type ClientConfig } from './api/client';
import { Login } from './components/Login';
import { Shell } from './components/Shell';

const STORAGE_KEY = 'athen.web.auth';

function loadSaved(): ClientConfig | null {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (!raw) return null;
    const v = JSON.parse(raw) as ClientConfig;
    return typeof v.token === 'string' && typeof v.baseUrl === 'string' ? v : null;
  } catch {
    return null;
  }
}

export function App() {
  const [cfg, setCfg] = useState<ClientConfig | null>(loadSaved);
  const client = useMemo(() => (cfg ? new AthenClient(cfg) : null), [cfg]);

  if (!client) {
    return (
      <Login
        onLogin={(c) => {
          localStorage.setItem(STORAGE_KEY, JSON.stringify(c));
          setCfg(c);
        }}
      />
    );
  }
  return (
    <Shell
      client={client}
      onLogout={() => {
        localStorage.removeItem(STORAGE_KEY);
        setCfg(null);
      }}
    />
  );
}
