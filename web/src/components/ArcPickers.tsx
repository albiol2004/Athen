// Per-arc override pickers (profile / reasoning effort / model tier /
// security mode) — fire-and-forget selects, same semantics as the
// desktop header toolbar.

import { useEffect, useState } from 'react';
import type { AthenClient } from '../api/client';
import type { ArcMeta } from '../api/types';
import { errMessage, useToast } from './Toast';

interface ProfileRow {
  id: string;
  name: string;
  [k: string]: unknown;
}

const EFFORTS = ['default', 'off', 'minimal', 'low', 'medium', 'high', 'max'];
const TIERS = ['auto', 'Judges', 'Fast', 'Code', 'Powerful'];
const MODES = ['global', 'bunker', 'assistant', 'yolo'];

export function ArcPickers({
  client,
  arc,
  onChanged,
}: {
  client: AthenClient;
  arc: ArcMeta;
  onChanged: () => void;
}) {
  const { toast } = useToast();
  const [profiles, setProfiles] = useState<ProfileRow[]>([]);
  useEffect(() => {
    client
      .get<ProfileRow[]>('/profiles')
      .then(setProfiles)
      .catch(() => {});
  }, [client]);

  const set = (path: string, value: string | null) =>
    void client
      .post(`/arcs/${encodeURIComponent(arc.id)}/${path}`, { value })
      .then(onChanged, (e) => toast(`Couldn't update ${path}: ${errMessage(e)}`, 'error'));

  const meta = arc as unknown as Record<string, unknown>;
  const effort = (meta.reasoning_effort_override as string | null) ?? 'default';
  // Normalize the legacy "Cheap" override to the renamed "Judges" tier so an
  // arc pinned before the rename still selects the matching option.
  const rawTier = (meta.tier_override as string | null) ?? 'auto';
  const tier = rawTier === 'Cheap' ? 'Judges' : rawTier;
  const mode = (meta.security_mode_override as string | null) ?? 'global';
  const codeMode = Boolean(arc.code_mode);
  // When non-null, the "enter a repo path" dialog is showing.
  const [pathPrompt, setPathPrompt] = useState<string | null>(null);

  const setCodeMode = (enabled: boolean, root: string | null) =>
    void client
      .setArcCodeMode(arc.id, enabled, root)
      .then(onChanged, (e) => toast(`Couldn't update Code Mode: ${errMessage(e)}`, 'error'));

  return (
    <div className="arc-pickers">
      <button
        type="button"
        className={`arc-codemode-toggle${codeMode ? ' active' : ''}`}
        title={
          codeMode
            ? `Code Mode on — rooted at ${arc.code_mode_root ?? '?'}. Click to turn off.`
            : 'Code Mode — work in-place in a real git repository'
        }
        onClick={() => {
          if (codeMode) setCodeMode(false, null);
          else setPathPrompt(arc.code_mode_root ?? '');
        }}
      >
        {'</>'} Code
      </button>
      <select
        title="Agent profile for this conversation"
        value={arc.active_profile_id ?? ''}
        onChange={(e) => set('profile', e.target.value || null)}
      >
        <option value="">Default profile</option>
        {profiles.map((p) => (
          <option key={p.id} value={p.id}>
            {p.name}
          </option>
        ))}
      </select>
      <select
        title="Reasoning effort"
        value={effort}
        onChange={(e) => set('effort', e.target.value === 'default' ? null : e.target.value)}
      >
        {EFFORTS.map((x) => (
          <option key={x} value={x}>
            {x === 'default' ? 'Effort: default' : `Effort: ${x}`}
          </option>
        ))}
      </select>
      <select
        title="Model tier"
        value={tier}
        onChange={(e) => set('tier', e.target.value === 'auto' ? null : e.target.value)}
      >
        {TIERS.map((x) => (
          <option key={x} value={x}>
            {x === 'auto' ? 'Tier: auto' : `Tier: ${x}`}
          </option>
        ))}
      </select>
      <select
        title="Security mode for this conversation"
        value={mode}
        onChange={(e) => set('security', e.target.value === 'global' ? null : e.target.value)}
      >
        {MODES.map((x) => (
          <option key={x} value={x}>
            {x === 'global' ? 'Security: global' : `Security: ${x}`}
          </option>
        ))}
      </select>
      {pathPrompt !== null && (
        <CodeModePathDialog
          initial={pathPrompt}
          onConfirm={(root) => {
            setPathPrompt(null);
            setCodeMode(true, root);
          }}
          onCancel={() => setPathPrompt(null)}
        />
      )}
    </div>
  );
}

/**
 * Small "enter the absolute repo path" dialog for enabling Code Mode. There is
 * no native folder picker in the browser, so a text input for the path is the
 * correct surface here (the backend validates the dir exists). Chrome mirrors
 * ConfirmDialog (`confirm-overlay`/`confirm-dialog`).
 */
function CodeModePathDialog({
  initial,
  onConfirm,
  onCancel,
}: {
  initial: string;
  onConfirm: (root: string) => void;
  onCancel: () => void;
}) {
  const [path, setPath] = useState(initial);
  const trimmed = path.trim();

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') onCancel();
    };
    document.addEventListener('keydown', onKey);
    return () => document.removeEventListener('keydown', onKey);
  }, [onCancel]);

  return (
    <div
      className="confirm-overlay"
      onClick={(e) => {
        if (e.target === e.currentTarget) onCancel();
      }}
    >
      <div className="confirm-dialog" role="dialog" aria-modal="true" aria-label="Enable Code Mode">
        <h3>Enable Code Mode</h3>
        <p>
          Enter the absolute path to the git repository this conversation should work in. The agent
          works in-place in that repo and Athen recognizes its real git state.
        </p>
        <input
          type="text"
          className="codemode-path-input"
          autoFocus
          spellCheck={false}
          placeholder="/home/you/projects/my-repo"
          value={path}
          onChange={(e) => setPath(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === 'Enter' && trimmed) {
              e.preventDefault();
              onConfirm(trimmed);
            }
          }}
        />
        <div className="confirm-buttons">
          <button type="button" className="confirm-cancel" onClick={onCancel}>
            Cancel
          </button>
          <button
            type="button"
            className="confirm-ok"
            disabled={!trimmed}
            onClick={() => onConfirm(trimmed)}
          >
            Enable
          </button>
        </div>
      </div>
    </div>
  );
}
