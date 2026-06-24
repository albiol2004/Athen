// Per-arc override pickers (profile / reasoning effort / model tier /
// security mode) — fire-and-forget selects, same semantics as the
// desktop header toolbar.

import { useEffect, useState } from 'react';
import type { AthenClient } from '../api/client';
import type { ArcMeta } from '../api/types';

interface ProfileRow {
  id: string;
  name: string;
  [k: string]: unknown;
}

const EFFORTS = ['default', 'off', 'minimal', 'low', 'medium', 'high', 'max'];
const TIERS = ['auto', 'Cheap', 'Fast', 'Code', 'Powerful'];
// Display label per tier wire value. "Cheap" shows as "Judges" (the
// auxiliary triage/judges tier); the stored override value stays "Cheap".
const tierLabel = (x: string): string => (x === 'Cheap' ? 'Judges' : x);
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
  const [profiles, setProfiles] = useState<ProfileRow[]>([]);
  useEffect(() => {
    client
      .get<ProfileRow[]>('/profiles')
      .then(setProfiles)
      .catch(() => {});
  }, [client]);

  const set = (path: string, value: string | null) =>
    void client.post(`/arcs/${encodeURIComponent(arc.id)}/${path}`, { value }).then(onChanged, () => {});

  const meta = arc as unknown as Record<string, unknown>;
  const effort = (meta.reasoning_effort_override as string | null) ?? 'default';
  const tier = (meta.tier_override as string | null) ?? 'auto';
  const mode = (meta.security_mode_override as string | null) ?? 'global';

  return (
    <div className="arc-pickers">
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
            {x === 'auto' ? 'Tier: auto' : `Tier: ${tierLabel(x)}`}
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
    </div>
  );
}
