import { useState } from 'react';
import { AthenClient, type ClientConfig } from '../api/client';

export function Login({ onLogin }: { onLogin: (cfg: ClientConfig) => void }) {
  const [token, setToken] = useState('');
  const [server, setServer] = useState('');
  const [error, setError] = useState<string | null>(null);
  const [pending, setPending] = useState(false);

  const connect = async () => {
    const cfg: ClientConfig = {
      baseUrl: server.trim().replace(/\/+$/, ''),
      token: token.trim(),
    };
    if (!cfg.token) {
      setError('Enter the access token.');
      return;
    }
    setPending(true);
    setError(null);
    try {
      await new AthenClient(cfg).currentArc();
      onLogin(cfg);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      setPending(false);
    }
  };

  return (
    <div className="login-wrap">
      <form
        className="login-card"
        onSubmit={(e) => {
          e.preventDefault();
          void connect();
        }}
      >
        <div className="brand big">
          <svg width="22" height="22" viewBox="0 0 24 24" fill="none" aria-hidden="true">
            <circle cx="12" cy="12" r="9" stroke="currentColor" strokeWidth="2" />
            <path d="M8.5 15.5 12 8l3.5 7.5M9.8 13h4.4" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" />
          </svg>
          Athen
        </div>
        <label>
          Access token
          <input
            type="password"
            value={token}
            onChange={(e) => setToken(e.target.value)}
            placeholder="paste your token"
            autoFocus
            autoComplete="current-password"
          />
        </label>
        <details className="advanced">
          <summary>Server</summary>
          <label>
            Instance URL (empty = this server)
            <input
              type="text"
              value={server}
              onChange={(e) => setServer(e.target.value)}
              placeholder="http://127.0.0.1:8787"
            />
          </label>
        </details>
        {error && <div className="form-error">{error}</div>}
        <button type="submit" disabled={pending}>
          {pending ? 'Connecting…' : 'Connect'}
        </button>
        <p className="hint">
          The token lives in <code>http_token</code> inside the instance's data directory (or{' '}
          <code>ATHEN_HTTP_TOKEN</code>).
        </p>
      </form>
    </div>
  );
}
