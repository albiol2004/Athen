// Per-tool inline SVG icon map for the Athen web UI.
// Mirrors the desktop frontend's icon set. Dependency-free.

function svg(inner: string): string {
  return `<svg viewBox="0 0 24 24" width="100%" height="100%" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${inner}</svg>`;
}

// Icon inner-markup constants.
const FILE_TEXT =
  '<path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z"/><path d="M14 2v6h6"/><line x1="9" y1="13" x2="15" y2="13"/><line x1="9" y1="17" x2="15" y2="17"/>';
const FOLDER =
  '<path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z"/>';
const FILE_SEARCH =
  '<path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h7"/><path d="M14 2v6h6"/><circle cx="17.5" cy="17.5" r="2.5"/><path d="M21 21l-1.5-1.5"/>';
const PEN_DOC =
  '<path d="M11 4H6a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h14a2 2 0 0 0 2-2v-5"/><path d="M18.5 2.5a2.121 2.121 0 0 1 3 3L12 15l-4 1 1-4 9.5-9.5z"/>';
const TERMINAL =
  '<polyline points="4 17 10 11 4 5"/><line x1="12" y1="19" x2="20" y2="19"/>';
const STOP = '<rect x="6" y="6" width="12" height="12" rx="2"/>';
const LOGS =
  '<line x1="4" y1="6" x2="20" y2="6"/><line x1="4" y1="12" x2="20" y2="12"/><line x1="4" y1="18" x2="14" y2="18"/>';
const GLOBE =
  '<circle cx="12" cy="12" r="10"/><line x1="2" y1="12" x2="22" y2="12"/><path d="M12 2a15.3 15.3 0 0 1 4 10 15.3 15.3 0 0 1-4 10 15.3 15.3 0 0 1-4-10 15.3 15.3 0 0 1 4-10z"/>';
const SEARCH =
  '<circle cx="11" cy="11" r="7"/><line x1="21" y1="21" x2="16.65" y2="16.65"/>';
const BOOKMARK =
  '<path d="M19 21l-7-5-7 5V5a2 2 0 0 1 2-2h10a2 2 0 0 1 2 2z"/>';
const SPARKLES =
  '<path d="M12 3l1.5 4.5L18 9l-4.5 1.5L12 15l-1.5-4.5L6 9l4.5-1.5z"/><path d="M5.2 17l.6 1.8L7.6 19.4 5.8 20l-.6 1.8L4.6 20 2.8 19.4 4.6 18.8z"/>';
const CALENDAR =
  '<rect x="3" y="4" width="18" height="18" rx="2" ry="2"/><line x1="16" y1="2" x2="16" y2="6"/><line x1="8" y1="2" x2="8" y2="6"/><line x1="3" y1="10" x2="21" y2="10"/>';
const CAL_PLUS =
  '<rect x="3" y="4" width="18" height="18" rx="2"/><line x1="16" y1="2" x2="16" y2="6"/><line x1="8" y1="2" x2="8" y2="6"/><line x1="3" y1="10" x2="21" y2="10"/><line x1="12" y1="14" x2="12" y2="18"/><line x1="10" y1="16" x2="14" y2="16"/>';
const TRASH =
  '<polyline points="3 6 5 6 21 6"/><path d="M19 6l-1 14a2 2 0 0 1-2 2H8a2 2 0 0 1-2-2L5 6"/><line x1="10" y1="11" x2="10" y2="17"/><line x1="14" y1="11" x2="14" y2="17"/><path d="M9 6V4a2 2 0 0 1 2-2h2a2 2 0 0 1 2 2v2"/>';
const USERS =
  '<path d="M16 21v-2a4 4 0 0 0-4-4H5a4 4 0 0 0-4 4v2"/><circle cx="8.5" cy="7" r="4"/><path d="M22 21v-2a4 4 0 0 0-3-3.87"/><path d="M16 3.13a4 4 0 0 1 0 7.75"/>';
const USER_SEARCH =
  '<circle cx="9" cy="7" r="4"/><path d="M3 21v-2a4 4 0 0 1 4-4h4"/><circle cx="17" cy="17" r="3"/><line x1="21" y1="21" x2="19" y2="19"/>';
const USER_PLUS =
  '<path d="M16 21v-2a4 4 0 0 0-4-4H5a4 4 0 0 0-4 4v2"/><circle cx="8.5" cy="7" r="4"/><line x1="20" y1="8" x2="20" y2="14"/><line x1="17" y1="11" x2="23" y2="11"/>';
const USER =
  '<path d="M20 21v-2a4 4 0 0 0-4-4H8a4 4 0 0 0-4 4v2"/><circle cx="12" cy="7" r="4"/>';
const FOLDER_PLUS =
  '<path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z"/><line x1="12" y1="11" x2="12" y2="17"/><line x1="9" y1="14" x2="15" y2="14"/>';
const INFO =
  '<circle cx="12" cy="12" r="10"/><line x1="12" y1="16" x2="12" y2="12"/><line x1="12" y1="8" x2="12.01" y2="8"/>';
const CHECK =
  '<path d="M22 11.08V12a10 10 0 1 1-5.93-9.14"/><polyline points="22 4 12 14.01 9 11.01"/>';
const DELEGATE =
  '<circle cx="6" cy="8" r="3"/><path d="M2 21v-2a4 4 0 0 1 4-4h0"/><circle cx="18" cy="8" r="3"/><path d="M14 21v-2a4 4 0 0 1 4-4h0"/><line x1="9" y1="12" x2="15" y2="12"/><polyline points="13 10 15 12 13 14"/>';
const MAIL =
  '<path d="M4 4h16a2 2 0 0 1 2 2v12a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V6a2 2 0 0 1 2-2z"/><polyline points="22,6 12,13 2,6"/>';
const PAPER_PLANE =
  '<line x1="22" y1="2" x2="11" y2="13"/><polygon points="22 2 15 22 11 13 2 9 22 2"/>';
const PHONE =
  '<path d="M22 16.92v3a2 2 0 0 1-2.18 2 19.79 19.79 0 0 1-8.63-3.07 19.5 19.5 0 0 1-6-6 19.79 19.79 0 0 1-3.07-8.67A2 2 0 0 1 4.11 2h3a2 2 0 0 1 2 1.72c.13.96.36 1.9.7 2.81a2 2 0 0 1-.45 2.11L8.09 9.91a16 16 0 0 0 6 6l1.27-1.27a2 2 0 0 1 2.11-.45c.9.34 1.85.57 2.81.7A2 2 0 0 1 22 16.92z"/>';
const ALARM =
  '<circle cx="12" cy="13" r="8"/><polyline points="12 9 12 13 14.5 15"/><line x1="5" y1="3" x2="2" y2="6"/><line x1="22" y1="6" x2="19" y2="3"/>';
const IDENTITY =
  '<circle cx="12" cy="8" r="4"/><path d="M4 21v-1a8 8 0 0 1 16 0v1"/><line x1="9" y1="13" x2="15" y2="13"/>';
const SKILL =
  '<path d="M3 5a2 2 0 0 1 2-2h5a2 2 0 0 1 2 2v15"/><path d="M21 5a2 2 0 0 0-2-2h-5a2 2 0 0 0-2 2v15"/><path d="M3 5v15h8"/><path d="M21 5v15h-8"/>';
const PLAN =
  '<rect x="5" y="2" width="14" height="20" rx="2"/><line x1="9" y1="8" x2="15" y2="8"/><line x1="9" y1="12" x2="15" y2="12"/><line x1="9" y1="16" x2="13" y2="16"/>';
const GEAR_FALLBACK =
  '<circle cx="12" cy="12" r="3"/><path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06a1.65 1.65 0 0 0 1.82.33H9a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06a1.65 1.65 0 0 0-.33 1.82V9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z"/>';

// Tool key -> wrapped SVG markup.
const ICONS: Record<string, string> = {
  read: svg(FILE_TEXT),
  list_directory: svg(FOLDER),
  grep: svg(FILE_SEARCH),
  write: svg(PEN_DOC),
  edit: svg(PEN_DOC),
  delete_file: svg(TRASH),
  shell_execute: svg(TERMINAL),
  shell_spawn: svg(TERMINAL),
  shell_kill: svg(STOP),
  shell_logs: svg(LOGS),
  web_search: svg(SEARCH),
  web_fetch: svg(GLOBE),
  email_send: svg(MAIL),
  send_telegram: svg(PAPER_PLANE),
  place_call: svg(PHONE),
  memory_store: svg(BOOKMARK),
  memory_recall: svg(SPARKLES),
  calendar_list: svg(CALENDAR),
  calendar_create: svg(CAL_PLUS),
  calendar_update: svg(CALENDAR),
  calendar_delete: svg(TRASH),
  contacts_list: svg(USERS),
  contacts_search: svg(USER_SEARCH),
  contacts_create: svg(USER_PLUS),
  contacts_update: svg(USER),
  contacts_delete: svg(TRASH),
  delegate_to_agent: svg(DELEGATE),
  install_package: svg(FOLDER_PLUS),
  uninstall_package: svg(TRASH),
  list_installed_packages: svg(BOOKMARK),
  create_wakeup: svg(ALARM),
  identity_add: svg(IDENTITY),
  load_skill: svg(SKILL),
  http_request: svg(GLOBE),
  athen_docs: svg(INFO),
  submit_plan: svg(PLAN),
  complete_step: svg(CHECK),
  update_plan: svg(PLAN),
  setup_email: svg(MAIL),
  setup_calendar_connect: svg(CALENDAR),
  setup_calendar_configure: svg(CAL_PLUS),
  setup_telegram: svg(PAPER_PLANE),
  setup_owner_info: svg(USER),
  setup_search_key: svg(SEARCH),
};

// Suffix aliases applied to MCP-prefixed tool suffixes before retrying.
const SUFFIX_ALIASES: Record<string, string> = {
  read_file: "read",
  write_file: "write",
  list_dir: "list_directory",
  list_files: "list_directory",
  search_files: "grep",
};

function resolveKey(name: string): string | undefined {
  if (name in ICONS) {
    return name;
  }
  const sep = name.indexOf("__");
  if (sep !== -1) {
    const suffix = name.slice(sep + 2);
    if (suffix in ICONS) {
      return suffix;
    }
    const aliased = SUFFIX_ALIASES[suffix];
    if (aliased !== undefined && aliased in ICONS) {
      return aliased;
    }
  }
  return undefined;
}

// Returns complete inline <svg>…</svg> markup for a tool name.
// Always returns something — unknown/MCP tools fall back to a generic glyph.
export function toolIconSvg(name: string): string {
  const key = resolveKey(name);
  if (key !== undefined) {
    return ICONS[key];
  }
  return svg(GEAR_FALLBACK);
}
