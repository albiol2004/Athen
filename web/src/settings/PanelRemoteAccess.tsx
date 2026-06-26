// Remote Access panel (docs/REMOTE_ACCESS.md §7/§8): turn the instance's
// HTTP API on at runtime, set the user+password (HTTP Basic) login, and
// optionally expose a public Cloudflare quick-tunnel link.
//
// Wire shapes: athen-app/src/http_api.rs `/api/remote-access*` routes
// (RemoteAccessInfo / RemoteAccessStatus, snake_case verbatim). Mutating
// remote access from the remote surface is allowed but a documented
// footgun — the desktop app is the primary control surface.

import { useEffect, useState } from 'react';
import type { AthenClient } from '../api/client';
import type { RemoteAccessStatus } from '../api/types';
import { ErrorText, Field, Loading, Section, useAction, useLoad } from './shared';

export function PanelRemoteAccess({ client }: { client: AthenClient }) {
  const info = useLoad(() => client.getRemoteAccess(), [client]);
  const act = useAction();

  // Editable form state, hydrated from the loaded config.
  const [enabled, setEnabled] = useState(false);
  const [port, setPort] = useState(8787);
  const [username, setUsername] = useState('');
  // Empty ⇒ keep the stored password (omit it from the POST body).
  const [password, setPassword] = useState('');
  const [tunnelEnabled, setTunnelEnabled] = useState(false);

  const [status, setStatus] = useState<RemoteAccessStatus | null>(null);

  useEffect(() => {
    if (!info.data) return;
    setEnabled(info.data.enabled);
    setPort(info.data.port);
    setUsername(info.data.username);
    setTunnelEnabled(info.data.tunnel_enabled);
  }, [info.data]);

  const refreshStatus = async () => {
    try {
      setStatus(await client.remoteAccessStatus());
    } catch {
      /* status is best-effort; keep the last good value */
    }
  };

  // Initial + on-change status read.
  useEffect(() => {
    void refreshStatus();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [client]);

  // While the public link is still being created, poll status every 2.5s.
  const pendingTunnel =
    enabled && tunnelEnabled && !!status && !status.tunnel_url && !status.last_error;
  useEffect(() => {
    if (!pendingTunnel) return;
    const id = window.setInterval(() => void refreshStatus(), 2500);
    return () => window.clearInterval(id);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [pendingTunnel]);

  const save = async () => {
    const body: {
      enabled: boolean;
      port: number;
      username: string;
      password?: string;
      tunnel_enabled: boolean;
    } = {
      enabled,
      port,
      username: username.trim(),
      tunnel_enabled: tunnelEnabled,
    };
    // Omit the password to keep the stored one; send it only when typed.
    if (password) body.password = password;
    const ok = await act.run(async () => {
      const fresh = await client.setRemoteAccess(body);
      setStatus(fresh);
    });
    if (ok) {
      setPassword('');
      await info.reload();
    }
  };

  return (
    <Section
      title="Remote Access"
      hint="Turn on the instance's HTTP API so you can reach this Athen from a browser, phone, or the web client — optionally over a public Cloudflare link."
    >
      <div className="st-warn">
        Enabling Remote Access exposes a <strong>shell-capable agent</strong> over the network.
        Anyone with the link and credentials can run commands as you. Keep it off unless you need
        it, and use a strong password.
      </div>

      {info.loading && <Loading />}
      <ErrorText error={info.error} />

      {info.data && (
        <>
          <div className="st-row" style={{ marginBottom: 8 }}>
            <label className="st-check">
              <input
                type="checkbox"
                checked={enabled}
                onChange={(e) => setEnabled(e.target.checked)}
              />
              Enable remote access
            </label>
            <Field label="Port">
              <input
                type="number"
                value={port}
                onChange={(e) => setPort(Number(e.target.value))}
              />
            </Field>
          </div>

          <div className="st-row" style={{ marginBottom: 8 }}>
            <Field label="Username" grow>
              <input
                type="text"
                value={username}
                placeholder="username"
                autoComplete="off"
                onChange={(e) => setUsername(e.target.value)}
              />
            </Field>
            <Field label="Password" grow>
              <input
                type="password"
                value={password}
                placeholder={info.data.has_password ? 'leave blank to keep current' : 'set a password'}
                autoComplete="new-password"
                onChange={(e) => setPassword(e.target.value)}
              />
            </Field>
          </div>

          <div className="st-row" style={{ marginBottom: 8 }}>
            <label className="st-check">
              <input
                type="checkbox"
                checked={tunnelEnabled}
                onChange={(e) => setTunnelEnabled(e.target.checked)}
              />
              Create public link (Cloudflare tunnel)
            </label>
          </div>

          <div className="st-row">
            <button
              type="button"
              className="st-btn primary"
              disabled={act.pending}
              onClick={() => void save()}
            >
              {act.pending ? 'Saving…' : 'Save'}
            </button>
            <span className="st-dim">
              Changes restart the listener live — no app restart needed.
            </span>
          </div>
          <ErrorText error={act.error} />

          <hr className="st-divider" />

          <RemoteAccessStatusBlock status={status} pendingTunnel={pendingTunnel} />
        </>
      )}
    </Section>
  );
}

function RemoteAccessStatusBlock({
  status,
  pendingTunnel,
}: {
  status: RemoteAccessStatus | null;
  pendingTunnel: boolean;
}) {
  if (!status) return <div className="st-dim">Status unavailable.</div>;

  return (
    <div className="st-list">
      <div className="st-item">
        <div className="st-item-main">
          <div className="st-item-title">
            Listener{' '}
            <span className={`st-badge ${status.listening ? 'green' : 'red'}`}>
              {status.listening ? 'running' : 'stopped'}
            </span>
          </div>
          {status.local_url && (
            <div className="st-item-sub st-mono">{status.local_url}</div>
          )}
        </div>
      </div>

      <div className="st-item">
        <div className="st-item-main">
          <div className="st-item-title">Public link</div>
          {status.tunnel_url ? (
            <div className="st-item-sub st-mono">{status.tunnel_url}</div>
          ) : pendingTunnel ? (
            <div className="st-item-sub st-dim">Creating public link…</div>
          ) : (
            <div className="st-item-sub st-dim">No public link.</div>
          )}
        </div>
        {status.tunnel_url && (
          <div className="st-item-actions">
            <button
              type="button"
              className="st-btn small"
              onClick={() => void navigator.clipboard?.writeText(status.tunnel_url ?? '')}
            >
              Copy
            </button>
            <a
              className="st-btn small"
              href={status.tunnel_url}
              target="_blank"
              rel="noreferrer"
            >
              Open
            </a>
          </div>
        )}
      </div>

      <div className="st-item">
        <div className="st-item-main">
          <div className="st-item-title">
            cloudflared{' '}
            <span className={`st-badge ${status.cloudflared_installed ? 'green' : 'amber'}`}>
              {status.cloudflared_installed ? 'installed' : 'not installed'}
            </span>
          </div>
          {!status.cloudflared_installed && (
            <div className="st-item-sub st-dim">
              Installed on demand when you enable the public link.
            </div>
          )}
        </div>
      </div>

      {status.last_error && (
        <div className="st-item">
          <div className="st-item-main">
            <div className="st-item-title">
              Last error <span className="st-badge red">error</span>
            </div>
            <div className="st-item-sub" style={{ color: 'var(--red)', whiteSpace: 'pre-wrap' }}>
              {status.last_error}
            </div>
          </div>
        </div>
      )}
    </div>
  );
}
