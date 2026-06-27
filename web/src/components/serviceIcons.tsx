// Service icons for the Settings panels (Cloud APIs + custom MCP servers).
//
// Monochrome 24×24 stroke glyphs (currentColor) that match the chat tool
// icons, so provider rows read as part of the same icon system rather than
// a pasted-in logo wall. Each curated HTTP provider gets a category-true
// glyph; anything unrecognized falls back to a neutral cloud. A custom MCP
// icon may instead be a user-uploaded image (data: / http(s) URL), which
// renders as an <img>.
//
// Kept in lockstep with the desktop equivalents in frontend/app.js
// (SVC_GLYPHS / CLOUD_API_ICONS / mcpEntryIconHtml / fileToIconDataUrl).

import { useState } from 'react';

function svg(inner: string): string {
  return `<svg viewBox="0 0 24 24" width="18" height="18" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${inner}</svg>`;
}

const GLYPHS: Record<string, string> = {
  search: '<circle cx="11" cy="11" r="7"/><line x1="21" y1="21" x2="16.65" y2="16.65"/>',
  shield: '<path d="M12 2 4 5v6c0 5 3.4 8.4 8 11 4.6-2.6 8-6 8-11V5z"/>',
  flame:
    '<path d="M12 2C9 5 7 7 7 11a5 5 0 0 0 10 0c0-2-1-3.6-2-5-.5 1.5-1.5 2-2.5 2C13 8 13 5 12 2z"/>',
  reader:
    '<path d="M14 3H6a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V9z"/><path d="M14 3v6h6"/><line x1="8" y1="13" x2="16" y2="13"/><line x1="8" y1="17" x2="13" y2="17"/>',
  atSign:
    '<circle cx="12" cy="12" r="4"/><path d="M16 12v1.5a2.5 2.5 0 0 0 5 0V12a9 9 0 1 0-3.6 7.2"/>',
  users:
    '<path d="M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2"/><circle cx="9" cy="7" r="4"/><path d="M22 21v-2a4 4 0 0 0-3-3.87"/><path d="M16 3.13a4 4 0 0 1 0 7.75"/>',
  database:
    '<ellipse cx="12" cy="5" rx="9" ry="3"/><path d="M3 5v14a9 3 0 0 0 18 0V5"/><path d="M3 12a9 3 0 0 0 18 0"/>',
  languages:
    '<path d="M4 5h7"/><path d="M9 3v2c0 4.5-2.2 7.5-5 9"/><path d="M5 9c0 3 3.5 5 6 6"/><path d="m13 20 4-9 4 9"/><path d="M14.5 17h5"/>',
  newspaper:
    '<path d="M4 22a2 2 0 0 1-2-2V6a1 1 0 0 1 1-1h13a1 1 0 0 1 1 1v14a2 2 0 0 0 2 2H4z"/><path d="M18 9h2a1 1 0 0 1 1 1v9a2 2 0 0 1-2 2"/><line x1="6" y1="9" x2="14" y2="9"/><line x1="6" y1="13" x2="14" y2="13"/><line x1="6" y1="17" x2="11" y2="17"/>',
  cloudSun:
    '<path d="M12 2v2"/><path d="m4.9 4.9 1.4 1.4"/><path d="M20 12h2"/><path d="m17.7 6.3 1.4-1.4"/><path d="M15.9 12.6a4 4 0 1 0-5.9-4.1"/><path d="M13 22H7a5 5 0 1 1 4.9-6H13a3 3 0 0 1 0 6z"/>',
  banknote:
    '<rect x="2" y="6" width="20" height="12" rx="2"/><circle cx="12" cy="12" r="2.5"/><path d="M6 12h.01M18 12h.01"/>',
  mapPin: '<path d="M20 10c0 6-8 12-8 12s-8-6-8-12a8 8 0 0 1 16 0z"/><circle cx="12" cy="10" r="3"/>',
  audioBars:
    '<line x1="4" y1="9" x2="4" y2="15"/><line x1="8" y1="6" x2="8" y2="18"/><line x1="12" y1="4" x2="12" y2="20"/><line x1="16" y1="7" x2="16" y2="17"/><line x1="20" y1="10" x2="20" y2="14"/>',
  speaker: '<path d="M11 5 6 9H2v6h4l5 4z"/><path d="M15.5 8.5a5 5 0 0 1 0 7"/><path d="M19 5a9 9 0 0 1 0 14"/>',
  mic: '<rect x="9" y="2" width="6" height="11" rx="3"/><path d="M5 10a7 7 0 0 0 14 0"/><line x1="12" y1="19" x2="12" y2="22"/>',
  route:
    '<circle cx="6" cy="19" r="3"/><path d="M9 19h8.5a3.5 3.5 0 0 0 0-7h-11a3.5 3.5 0 0 1 0-7H15"/><circle cx="18" cy="5" r="3"/>',
  zap: '<path d="M13 2 3 14h7l-1 8 10-12h-7l1-8z"/>',
  phone:
    '<path d="M22 16.9v3a2 2 0 0 1-2.2 2 19.8 19.8 0 0 1-8.6-3.1 19.5 19.5 0 0 1-6-6A19.8 19.8 0 0 1 2.1 4.2 2 2 0 0 1 4.1 2h3a2 2 0 0 1 2 1.7c.1 1 .4 1.9.7 2.8a2 2 0 0 1-.5 2.1L8.1 9.9a16 16 0 0 0 6 6l1.3-1.3a2 2 0 0 1 2.1-.4c.9.3 1.8.6 2.8.7a2 2 0 0 1 1.7 2z"/>',
  cloud: '<path d="M17.5 19a4.5 4.5 0 0 0 0-9 6 6 0 0 0-11.6 1.5A4 4 0 0 0 6 19z"/>',
  server:
    '<rect x="2" y="3" width="20" height="8" rx="2"/><rect x="2" y="13" width="20" height="8" rx="2"/><line x1="6" y1="7" x2="6.01" y2="7"/><line x1="6" y1="17" x2="6.01" y2="17"/>',
  // Built-in MCP icon names (parity with frontend mcpIconSvg).
  folder: '<path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z"/>',
  globe:
    '<circle cx="12" cy="12" r="10"/><line x1="2" y1="12" x2="22" y2="12"/><path d="M12 2a15.3 15.3 0 0 1 4 10 15.3 15.3 0 0 1-4 10 15.3 15.3 0 0 1-4-10 15.3 15.3 0 0 1 4-10z"/>',
  terminal: '<polyline points="4 17 10 11 4 5"/><line x1="12" y1="19" x2="20" y2="19"/>',
  mail:
    '<path d="M4 4h16c1.1 0 2 .9 2 2v12c0 1.1-.9 2-2 2H4c-1.1 0-2-.9-2-2V6c0-1.1.9-2 2-2z"/><polyline points="22,6 12,13 2,6"/>',
  calendar:
    '<rect x="3" y="4" width="18" height="18" rx="2" ry="2"/><line x1="16" y1="2" x2="16" y2="6"/><line x1="8" y1="2" x2="8" y2="6"/><line x1="3" y1="10" x2="21" y2="10"/>',
};

const CLOUD_API_ICONS: Record<string, string> = {
  jinaai: GLYPHS.reader,
  firecrawl: GLYPHS.flame,
  bravesearch: GLYPHS.shield,
  serpapi: GLYPHS.search,
  hunter: GLYPHS.atSign,
  apollo: GLYPHS.users,
  peopledatalabs: GLYPHS.database,
  deepl: GLYPHS.languages,
  newsapiorg: GLYPHS.newspaper,
  openmeteo: GLYPHS.cloudSun,
  frankfurter: GLYPHS.banknote,
  opencage: GLYPHS.mapPin,
  elevenlabs: GLYPHS.audioBars,
  cartesia: GLYPHS.speaker,
  deepgram: GLYPHS.mic,
  openrouter: GLYPHS.route,
  groq: GLYPHS.zap,
  twilio: GLYPHS.phone,
};

// Named built-in MCP icons (folder, globe, …). Aliases mirror mcpIconSvg.
const NAMED_MCP_ICONS: Record<string, string> = {
  folder: GLYPHS.folder,
  files: GLYPHS.folder,
  globe: GLYPHS.globe,
  web: GLYPHS.globe,
  terminal: GLYPHS.terminal,
  shell: GLYPHS.terminal,
  database: GLYPHS.database,
  db: GLYPHS.database,
  mail: GLYPHS.mail,
  email: GLYPHS.mail,
  calendar: GLYPHS.calendar,
};

function normalizeProviderKey(provider: string): string {
  return (provider || '').toLowerCase().replace(/[^a-z0-9]/g, '');
}

function isImageUrl(v: string): boolean {
  return /^(data:|https?:\/\/)/i.test(v);
}

// Real brand favicon for a host, via DuckDuckGo's icon service (follows
// each site's declared icon, normalizes size). Keyed on the registrable
// domain (last two labels) so api.* subdomains resolve the marketing-site
// logo. Returns null for unparseable URLs.
function faviconUrl(url: string): string | null {
  try {
    const host = new URL(url).hostname;
    const parts = host.split('.');
    const domain = parts.length > 2 ? parts.slice(-2).join('.') : host;
    return domain ? `https://icons.duckduckgo.com/ip3/${domain}.ico` : null;
  } catch {
    return null;
  }
}

/**
 * Icon chip for a curated HTTP provider: the provider's real logo
 * (favicon), falling back to a category glyph (neutral cloud for unknown)
 * when the logo can't load — so every row stays recognizable, online or
 * off.
 */
export function CloudApiIcon({ provider, baseUrl }: { provider: string; baseUrl?: string }) {
  const [failed, setFailed] = useState(false);
  const fav = baseUrl ? faviconUrl(baseUrl) : null;
  if (fav && !failed) {
    return (
      <span className="st-item-icon">
        <img
          className="st-item-icon-img"
          src={fav}
          alt=""
          loading="lazy"
          onError={() => setFailed(true)}
        />
      </span>
    );
  }
  const inner = CLOUD_API_ICONS[normalizeProviderKey(provider)] ?? GLYPHS.cloud;
  return <span className="st-item-icon" dangerouslySetInnerHTML={{ __html: svg(inner) }} />;
}

/**
 * Icon chip for an MCP server. A custom `icon` may be a data:/http(s)
 * image, a built-in icon name, or absent → default server glyph. When
 * `onPick` is supplied the chip is a button that opens an image picker.
 */
export function McpIcon({
  icon,
  onPick,
}: {
  icon?: string | null;
  onPick?: () => void;
}) {
  const v = (icon ?? '').trim();
  let body: React.ReactNode;
  if (v && isImageUrl(v)) {
    body = <img className="st-item-icon-img" src={v} alt="" draggable={false} />;
  } else {
    const inner = (v && NAMED_MCP_ICONS[v.toLowerCase()]) || GLYPHS.server;
    body = <span dangerouslySetInnerHTML={{ __html: svg(inner) }} />;
  }
  if (onPick) {
    return (
      <button
        type="button"
        className="st-item-icon st-item-icon-btn"
        title="Set a custom icon"
        onClick={onPick}
      >
        {body}
      </button>
    );
  }
  return <span className="st-item-icon">{body}</span>;
}

/**
 * Read a user-picked image file into a small icon data URL. SVGs ride
 * through as-is (vector, tiny); rasters are downscaled to ≤64px so the
 * stored definition stays a few KB regardless of source resolution.
 */
export function fileToIconDataUrl(file: File): Promise<string> {
  return new Promise((resolve, reject) => {
    if (!file) return reject(new Error('No file'));
    if (file.size > 2 * 1024 * 1024) return reject(new Error('Image too large (max 2 MB)'));
    const reader = new FileReader();
    reader.onerror = () => reject(new Error('Could not read file'));
    if (file.type === 'image/svg+xml') {
      reader.onload = () => resolve(String(reader.result));
      reader.readAsDataURL(file);
      return;
    }
    reader.onload = () => {
      const img = new Image();
      img.onload = () => {
        const max = 64;
        const scale = Math.min(1, max / Math.max(img.width, img.height));
        const w = Math.max(1, Math.round(img.width * scale));
        const h = Math.max(1, Math.round(img.height * scale));
        const canvas = document.createElement('canvas');
        canvas.width = w;
        canvas.height = h;
        const ctx = canvas.getContext('2d');
        if (!ctx) return reject(new Error('Canvas unavailable'));
        ctx.drawImage(img, 0, 0, w, h);
        try {
          resolve(canvas.toDataURL('image/png'));
        } catch {
          reject(new Error('Could not encode image'));
        }
      };
      img.onerror = () => reject(new Error('Could not decode image'));
      img.src = String(reader.result);
    };
    reader.readAsDataURL(file);
  });
}

/** True when the icon is a user-uploaded image (so a "Reset" affordance applies). */
export function isCustomImageIcon(icon?: string | null): boolean {
  const v = (icon ?? '').trim();
  return !!v && isImageUrl(v);
}
