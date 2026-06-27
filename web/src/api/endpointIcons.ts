// Module-level cache of registered-endpoint logos (name → data: URL), used
// to brand `http_request` tool cards in the chat view without a per-card
// network call. Loaded once by the Shell on mount; tool cards read it
// synchronously. Logos are cached locally by the backend, so this is a
// single cheap `/endpoints` read, not a favicon fetch.

import type { AthenClient } from './client';

interface EndpointLite {
  name: string;
  icon?: string | null;
}

let iconByName: Record<string, string> = {};

/** Fetch the endpoint list once and (re)build the name→logo map. */
export async function loadEndpointIcons(client: AthenClient): Promise<void> {
  try {
    const list = await client.get<EndpointLite[]>('/endpoints');
    const next: Record<string, string> = {};
    for (const e of list ?? []) {
      if (e?.name && e.icon) next[e.name.toLowerCase()] = e.icon;
    }
    iconByName = next;
  } catch {
    /* best-effort — tool cards fall back to the globe glyph */
  }
}

/** Cached logo for a registered endpoint name, or null. */
export function endpointIcon(name?: string | null): string | null {
  if (!name) return null;
  return iconByName[name.toLowerCase()] ?? null;
}
