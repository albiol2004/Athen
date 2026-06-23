// Full-screen Settings modal: left tab rail + per-area panels.
// Every panel talks to the instance HTTP API through the generic
// client verbs; endpoint map lives in athen-app/src/http_api.rs
// (`full_surface_router`).

import { useEffect, useState } from 'react';
import type { AthenClient } from '../api/client';
import './settings.css';
import { PanelModels } from './PanelModels';
import { PanelAgents } from './PanelAgents';
import { PanelConnections } from './PanelConnections';
import { PanelSecurity } from './PanelSecurity';
import { PanelContacts } from './PanelContacts';
import { PanelMemory } from './PanelMemory';
import { PanelProjects } from './PanelProjects';

export type TabId = 'models' | 'agents' | 'connections' | 'security' | 'contacts' | 'memory' | 'projects';

const stroke = { stroke: 'currentColor', strokeWidth: 1.7, strokeLinecap: 'round', strokeLinejoin: 'round', fill: 'none' } as const;

const ICONS: Record<TabId, JSX.Element> = {
  models: (
    <svg width="15" height="15" viewBox="0 0 24 24" aria-hidden="true">
      <path {...stroke} d="M12 3 4 7.5v9L12 21l8-4.5v-9L12 3Z" />
      <path {...stroke} d="M4 7.5 12 12l8-4.5M12 12v9" />
    </svg>
  ),
  agents: (
    <svg width="15" height="15" viewBox="0 0 24 24" aria-hidden="true">
      <circle {...stroke} cx="12" cy="8" r="3.4" />
      <path {...stroke} d="M5 20c.8-3.6 3.6-5.4 7-5.4s6.2 1.8 7 5.4" />
    </svg>
  ),
  connections: (
    <svg width="15" height="15" viewBox="0 0 24 24" aria-hidden="true">
      <path {...stroke} d="M9.5 14.5 5.6 18.4a3 3 0 0 1-4.2-4.2l3.9-3.9a3 3 0 0 1 4.2 0M14.5 9.5l3.9-3.9a3 3 0 0 1 4.2 4.2l-3.9 3.9a3 3 0 0 1-4.2 0" transform="translate(0.2 -0.2)" />
      <path {...stroke} d="m9.5 14.5 5-5" />
    </svg>
  ),
  security: (
    <svg width="15" height="15" viewBox="0 0 24 24" aria-hidden="true">
      <path {...stroke} d="M12 3 5 6v5c0 4.4 3 8.2 7 10 4-1.8 7-5.6 7-10V6l-7-3Z" />
      <path {...stroke} d="m9 11.8 2.2 2.2L15.4 9.6" />
    </svg>
  ),
  contacts: (
    <svg width="15" height="15" viewBox="0 0 24 24" aria-hidden="true">
      <circle {...stroke} cx="9" cy="9" r="3" />
      <path {...stroke} d="M3.5 19.5c.7-3 2.8-4.5 5.5-4.5s4.8 1.5 5.5 4.5M16 5.5a3 3 0 0 1 0 7M17.7 15.3c1.7.6 2.7 1.9 3.1 4.2" />
    </svg>
  ),
  memory: (
    <svg width="15" height="15" viewBox="0 0 24 24" aria-hidden="true">
      <path {...stroke} d="M8.5 5.5A3.5 3.5 0 0 1 12 9v9.5a3 3 0 1 1-6-.4 3.4 3.4 0 0 1-1.6-5.8A3.4 3.4 0 0 1 5.5 7a3.4 3.4 0 0 1 3-1.5ZM15.5 5.5A3.5 3.5 0 0 0 12 9" />
      <path {...stroke} d="M15.5 5.5a3.4 3.4 0 0 1 3 1.5 3.4 3.4 0 0 1 1.1 5.3 3.4 3.4 0 0 1-1.6 5.8 3 3 0 1 1-6-.4" />
    </svg>
  ),
  projects: (
    <svg width="15" height="15" viewBox="0 0 24 24" aria-hidden="true">
      <path {...stroke} d="M3.5 7.5a2 2 0 0 1 2-2h3l2 2h6a2 2 0 0 1 2 2v7a2 2 0 0 1-2 2h-11a2 2 0 0 1-2-2V7.5Z" />
    </svg>
  ),
};

const TABS: { id: TabId; label: string }[] = [
  { id: 'models', label: 'Models' },
  { id: 'agents', label: 'Agents & Tools' },
  { id: 'connections', label: 'Connections' },
  { id: 'security', label: 'Security' },
  { id: 'contacts', label: 'Contacts' },
  { id: 'memory', label: 'Memory' },
  { id: 'projects', label: 'Projects' },
];

export function SettingsModal({
  client,
  onClose,
  initialTab,
}: {
  client: AthenClient;
  onClose: () => void;
  initialTab?: string;
}): JSX.Element {
  const isTabId = (t: string | undefined): t is TabId =>
    t === 'models' || t === 'agents' || t === 'connections' || t === 'security'
    || t === 'contacts' || t === 'memory' || t === 'projects';
  const [tab, setTab] = useState<TabId>(isTabId(initialTab) ? initialTab : 'models');

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') onClose();
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [onClose]);

  return (
    <div
      className="st-overlay"
      onMouseDown={(e) => {
        if (e.target === e.currentTarget) onClose();
      }}
    >
      <div className="st-modal" role="dialog" aria-modal="true" aria-label="Settings">
        <nav className="st-nav">
          <div className="st-nav-title">
            <svg width="16" height="16" viewBox="0 0 24 24" fill="none" aria-hidden="true">
              <circle cx="12" cy="12" r="3.2" stroke="currentColor" strokeWidth="1.7" />
              <path
                d="M19.2 13.4a7.4 7.4 0 0 0 0-2.8l2-1.5-2-3.4-2.3 1a7.5 7.5 0 0 0-2.4-1.4L14 2.8h-4l-.5 2.5a7.5 7.5 0 0 0-2.4 1.4l-2.3-1-2 3.4 2 1.5a7.4 7.4 0 0 0 0 2.8l-2 1.5 2 3.4 2.3-1a7.5 7.5 0 0 0 2.4 1.4l.5 2.5h4l.5-2.5a7.5 7.5 0 0 0 2.4-1.4l2.3 1 2-3.4-2-1.5Z"
                stroke="currentColor"
                strokeWidth="1.5"
                strokeLinejoin="round"
              />
            </svg>
            Settings
          </div>
          {TABS.map((t) => (
            <button
              key={t.id}
              type="button"
              className={`st-tab${tab === t.id ? ' active' : ''}`}
              onClick={() => setTab(t.id)}
            >
              {ICONS[t.id]}
              {t.label}
            </button>
          ))}
        </nav>
        <div className="st-body">
          <div className="st-head">
            <h2>{TABS.find((t) => t.id === tab)?.label}</h2>
            <button type="button" className="st-close" onClick={onClose} aria-label="Close settings">
              <svg width="15" height="15" viewBox="0 0 24 24" aria-hidden="true">
                <path
                  d="m6 6 12 12M18 6 6 18"
                  stroke="currentColor"
                  strokeWidth="1.8"
                  strokeLinecap="round"
                />
              </svg>
            </button>
          </div>
          <div className="st-content" key={tab}>
            {tab === 'models' && <PanelModels client={client} />}
            {tab === 'agents' && <PanelAgents client={client} />}
            {tab === 'connections' && <PanelConnections client={client} />}
            {tab === 'security' && <PanelSecurity client={client} />}
            {tab === 'contacts' && <PanelContacts client={client} />}
            {tab === 'memory' && <PanelMemory client={client} />}
            {tab === 'projects' && <PanelProjects client={client} />}
          </div>
        </div>
      </div>
    </div>
  );
}
