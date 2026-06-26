import { useState } from 'react';
import { AthenClient, type ClientConfig } from '../api/client';

export function Login({ onLogin }: { onLogin: (cfg: ClientConfig) => void }) {
  const [username, setUsername] = useState('');
  const [token, setToken] = useState('');
  const [server, setServer] = useState('');
  const [error, setError] = useState<string | null>(null);
  const [pending, setPending] = useState(false);

  const connect = async () => {
    const baseUrl = server.trim().replace(/\/+$/, '');
    const user = username.trim();
    const secret = token.trim();
    // Non-empty username ⇒ HTTP Basic (username + password). Empty
    // username ⇒ the existing token mode (the field holds the token).
    const cfg: ClientConfig = user
      ? { baseUrl, token: '', username: user, password: secret }
      : { baseUrl, token: secret };
    if (!secret) {
      setError(user ? 'Enter the password.' : 'Enter the access token.');
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
          Username (leave blank for token mode)
          <input
            type="text"
            value={username}
            onChange={(e) => setUsername(e.target.value)}
            placeholder="username"
            autoFocus
            autoComplete="username"
          />
        </label>
        <label>
          Password or token
          <input
            type="password"
            value={token}
            onChange={(e) => setToken(e.target.value)}
            placeholder="password or token"
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
          Sign in with the username + password set in Settings → Remote Access, or leave the
          username blank and paste the access token from <code>http_token</code> in the instance's
          data directory (or <code>ATHEN_HTTP_TOKEN</code>).
        </p>
      </form>
    </div>
  );
}
