// ─── Tauri Initialization ───

let invoke;

// Container for tool execution cards during the current request.
let currentToolContainer = null;

// ─── Streaming State ───

// Tracks the currently streaming assistant message bubble so that
// incoming `agent-stream` deltas can be appended to it progressively.
let streamingBubble = null;
// Accumulates the full text received via streaming so the final
// message can be rendered with full markdown once complete.
let streamingText = '';
// Whether we received any streaming chunks for the current request.
let didReceiveStreamChunks = false;
// Whether a request is currently being processed by the agent.
let isProcessing = false;
// Tracks the thinking/reasoning block for thinking models.
let thinkingBlock = null;
let thinkingContent = null;
let thinkingText = '';

// ─── Arc State ───

// The currently active arc ID.
let activeArcId = null;
// Whether the first user message in this arc has been sent
// (used to auto-name the arc).
let arcHasMessages = false;
// Arcs with unread background activity (e.g. Telegram responses).
const arcsWithNotifications = new Set();

// ─── Error Retry State ───

// Stores the last user message so we can retry on transient errors.
let lastMessage = null;

function retryLastMessage() {
    if (lastMessage) {
        inputEl.value = lastMessage;
        formEl.requestSubmit();
    }
}

// Schedule a non-critical callback for an idle slice, with a setTimeout
// fallback for environments lacking requestIdleCallback (older WebKitGTK).
function scheduleIdle(fn) {
    if (typeof requestIdleCallback === 'function') {
        requestIdleCallback(fn, { timeout: 2000 });
    } else {
        setTimeout(fn, 100);
    }
}

// ─── Built-in tool icons ───
// Maps Athen built-in tool names to inline SVG markup so the chat UI can
// show an icon + short label instead of the raw tool identifier. Tools not
// in this map (user-installed MCPs, unknown names) render their raw name.
function toolSvg(inner, w) {
    const size = w || 14;
    return `<svg viewBox="0 0 24 24" width="${size}" height="${size}" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${inner}</svg>`;
}
const ICON_FILE_TEXT   = toolSvg('<path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z"/><path d="M14 2v6h6"/><line x1="9" y1="13" x2="15" y2="13"/><line x1="9" y1="17" x2="15" y2="17"/>');
const ICON_FOLDER      = toolSvg('<path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z"/>');
const ICON_FILE_SEARCH = toolSvg('<path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h7"/><path d="M14 2v6h6"/><circle cx="17.5" cy="17.5" r="2.5"/><path d="M21 21l-1.5-1.5"/>');
const ICON_PEN_DOC     = toolSvg('<path d="M11 4H6a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h14a2 2 0 0 0 2-2v-5"/><path d="M18.5 2.5a2.121 2.121 0 0 1 3 3L12 15l-4 1 1-4 9.5-9.5z"/>');
const ICON_TERMINAL    = toolSvg('<polyline points="4 17 10 11 4 5"/><line x1="12" y1="19" x2="20" y2="19"/>');
const ICON_STOP        = toolSvg('<rect x="6" y="6" width="12" height="12" rx="2"/>');
const ICON_LOGS        = toolSvg('<line x1="4" y1="6" x2="20" y2="6"/><line x1="4" y1="12" x2="20" y2="12"/><line x1="4" y1="18" x2="14" y2="18"/>');
const ICON_GLOBE       = toolSvg('<circle cx="12" cy="12" r="10"/><line x1="2" y1="12" x2="22" y2="12"/><path d="M12 2a15.3 15.3 0 0 1 4 10 15.3 15.3 0 0 1-4 10 15.3 15.3 0 0 1-4-10 15.3 15.3 0 0 1 4-10z"/>');
const ICON_SEARCH      = toolSvg('<circle cx="11" cy="11" r="7"/><line x1="21" y1="21" x2="16.65" y2="16.65"/>');
const ICON_BOOKMARK    = toolSvg('<path d="M19 21l-7-5-7 5V5a2 2 0 0 1 2-2h10a2 2 0 0 1 2 2z"/>');
const ICON_SPARKLES    = toolSvg('<path d="M12 3l1.5 4.5L18 9l-4.5 1.5L12 15l-1.5-4.5L6 9l4.5-1.5z"/><path d="M5.2 17l.6 1.8L7.6 19.4 5.8 20l-.6 1.8L4.6 20 2.8 19.4 4.6 18.8z"/>');
const ICON_CALENDAR    = toolSvg('<rect x="3" y="4" width="18" height="18" rx="2" ry="2"/><line x1="16" y1="2" x2="16" y2="6"/><line x1="8" y1="2" x2="8" y2="6"/><line x1="3" y1="10" x2="21" y2="10"/>');
const ICON_CAL_PLUS    = toolSvg('<rect x="3" y="4" width="18" height="18" rx="2"/><line x1="16" y1="2" x2="16" y2="6"/><line x1="8" y1="2" x2="8" y2="6"/><line x1="3" y1="10" x2="21" y2="10"/><line x1="12" y1="14" x2="12" y2="18"/><line x1="10" y1="16" x2="14" y2="16"/>');
const ICON_TRASH       = toolSvg('<polyline points="3 6 5 6 21 6"/><path d="M19 6l-1 14a2 2 0 0 1-2 2H8a2 2 0 0 1-2-2L5 6"/><line x1="10" y1="11" x2="10" y2="17"/><line x1="14" y1="11" x2="14" y2="17"/><path d="M9 6V4a2 2 0 0 1 2-2h2a2 2 0 0 1 2 2v2"/>');
const ICON_USERS       = toolSvg('<path d="M16 21v-2a4 4 0 0 0-4-4H5a4 4 0 0 0-4 4v2"/><circle cx="8.5" cy="7" r="4"/><path d="M22 21v-2a4 4 0 0 0-3-3.87"/><path d="M16 3.13a4 4 0 0 1 0 7.75"/>');
const ICON_USER_SEARCH = toolSvg('<circle cx="9" cy="7" r="4"/><path d="M3 21v-2a4 4 0 0 1 4-4h4"/><circle cx="17" cy="17" r="3"/><line x1="21" y1="21" x2="19" y2="19"/>');
const ICON_USER_PLUS   = toolSvg('<path d="M16 21v-2a4 4 0 0 0-4-4H5a4 4 0 0 0-4 4v2"/><circle cx="8.5" cy="7" r="4"/><line x1="20" y1="8" x2="20" y2="14"/><line x1="17" y1="11" x2="23" y2="11"/>');
const ICON_USER        = toolSvg('<path d="M20 21v-2a4 4 0 0 0-4-4H8a4 4 0 0 0-4 4v2"/><circle cx="12" cy="7" r="4"/>');
const ICON_FOLDER_PLUS = toolSvg('<path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z"/><line x1="12" y1="11" x2="12" y2="17"/><line x1="9" y1="14" x2="15" y2="14"/>');
const ICON_ARROW_RIGHT = toolSvg('<line x1="5" y1="12" x2="19" y2="12"/><polyline points="12 5 19 12 12 19"/>');
const ICON_INFO        = toolSvg('<circle cx="12" cy="12" r="10"/><line x1="12" y1="16" x2="12" y2="12"/><line x1="12" y1="8" x2="12.01" y2="8"/>');
const ICON_CHECK       = toolSvg('<path d="M22 11.08V12a10 10 0 1 1-5.93-9.14"/><polyline points="22 4 12 14.01 9 11.01"/>');

const BUILTIN_TOOL_ICONS = {
    'read': ICON_FILE_TEXT, 'list_directory': ICON_FOLDER, 'grep': ICON_FILE_SEARCH,
    'write': ICON_PEN_DOC, 'edit': ICON_PEN_DOC,
    'shell_execute': ICON_TERMINAL, 'shell_spawn': ICON_TERMINAL,
    'shell_kill': ICON_STOP, 'shell_logs': ICON_LOGS,
    'web_search': ICON_SEARCH, 'web_fetch': ICON_GLOBE,
    'memory_store': ICON_BOOKMARK, 'memory_recall': ICON_SPARKLES,
    'calendar_list': ICON_CALENDAR, 'calendar_create': ICON_CAL_PLUS,
    'calendar_update': ICON_CALENDAR, 'calendar_delete': ICON_TRASH,
    'contacts_list': ICON_USERS, 'contacts_search': ICON_USER_SEARCH,
    'contacts_create': ICON_USER_PLUS, 'contacts_update': ICON_USER,
    'contacts_delete': ICON_TRASH,
    // mcp-filesystem (matched via the suffix lookup in _normalizedToolKey)
    'delete_path': ICON_TRASH, 'append_file': ICON_PEN_DOC,
    'create_dir': ICON_FOLDER_PLUS, 'move_path': ICON_ARROW_RIGHT,
    'exists': ICON_CHECK, 'stat': ICON_INFO,
};

const BUILTIN_TOOL_LABELS = {
    'read': 'Read', 'list_directory': 'List', 'grep': 'Search files',
    'write': 'Write', 'edit': 'Edit',
    'shell_execute': 'Run', 'shell_spawn': 'Spawn',
    'shell_kill': 'Stop', 'shell_logs': 'Logs',
    'web_search': 'Search web', 'web_fetch': 'Fetch',
    'memory_store': 'Save', 'memory_recall': 'Recall',
    'calendar_list': 'Events', 'calendar_create': 'Create event',
    'calendar_update': 'Update event', 'calendar_delete': 'Delete event',
    'contacts_list': 'Contacts', 'contacts_search': 'Find contact',
    'contacts_create': 'Add contact', 'contacts_update': 'Update contact',
    'contacts_delete': 'Delete contact',
    'delete_path': 'Delete', 'append_file': 'Append',
    'create_dir': 'Create folder', 'move_path': 'Move',
    'exists': 'Check', 'stat': 'Info',
};

// MCP-prefixed tools (e.g. `files__read_file`) — strip prefix and try common
// suffix aliases so MCP filesystem tools pick up the same icons as built-ins.
const MCP_SUFFIX_ALIASES = {
    'read_file': 'read', 'write_file': 'write',
    'list_dir': 'list_directory', 'list_files': 'list_directory',
    'search_files': 'grep',
};

function _normalizedToolKey(toolName) {
    if (!toolName) return null;
    if (BUILTIN_TOOL_ICONS[toolName]) return toolName;
    const sep = toolName.indexOf('__');
    if (sep > 0) {
        const suffix = toolName.slice(sep + 2);
        if (BUILTIN_TOOL_ICONS[suffix]) return suffix;
        const alias = MCP_SUFFIX_ALIASES[suffix];
        if (alias && BUILTIN_TOOL_ICONS[alias]) return alias;
    }
    return null;
}
function builtinToolIcon(toolName)  { const k = _normalizedToolKey(toolName); return k ? BUILTIN_TOOL_ICONS[k] : null; }
function builtinToolLabel(toolName) { const k = _normalizedToolKey(toolName); return k ? BUILTIN_TOOL_LABELS[k] : ''; }

function registerTauriEventListeners() {
    if (!(window.__TAURI__.event && window.__TAURI__.event.listen)) return;

    window.__TAURI__.event.listen('agent-progress', (event) => {
        const { step, tool_name, status, detail } = event.payload;

        // Update status bar as before.
        setStatus('working', `Step ${step}: ${tool_name} (${status})`);

        // Skip non-tool steps (e.g. "Evaluating risk...", "Task completed").
        if (step === 0 || tool_name === 'Task completed') return;

        // Create tool container if it does not exist yet.
        if (!currentToolContainer) {
            currentToolContainer = document.createElement('div');
            currentToolContainer.className = 'tool-steps-container';
            messagesEl.appendChild(currentToolContainer);
        }

        // Build the tool execution card.
        const card = document.createElement('div');
        const statusClass = status === 'Completed' ? 'completed' :
                            status === 'Failed' ? 'failed' : 'in-progress';
        const builtinIcon = builtinToolIcon(tool_name);
        const builtinClass = builtinIcon ? ' builtin' : '';
        card.className = `tool-execution-card ${statusClass}${builtinClass}`;
        card.setAttribute('title', tool_name);

        const statusIcon = status === 'Completed' ? '&#10003;' :
                           status === 'Failed' ? '&#10007;' : '&#9679;';

        let detailHtml = '';
        if (detail) {
            const truncated = detail.length > 80 ? detail.substring(0, 80) + '...' : detail;
            detailHtml = `<span class="tool-detail">${escapeHtml(truncated)}</span>`;
        }

        const labelText = builtinIcon ? builtinToolLabel(tool_name) : tool_name;
        const iconMarkup = builtinIcon
            ? `<span class="tool-builtin-icon">${builtinIcon}</span>`
            : '';

        card.innerHTML =
            `<span class="tool-status-icon">${statusIcon}</span>` +
            iconMarkup +
            `<span class="tool-name">${escapeHtml(labelText)}</span>` +
            detailHtml;

        currentToolContainer.appendChild(card);

        // Scroll to keep latest card visible.
        requestAnimationFrame(() => {
            messagesEl.parentElement.scrollTo({
                top: messagesEl.parentElement.scrollHeight,
                behavior: 'smooth'
            });
        });
    });

    // Listen for streaming text chunks from the agent executor.
    // Each event carries { delta, is_final, arc_id }.
    // If the stream belongs to a different arc, show a notification
    // dot on that arc in the sidebar instead of rendering a bubble.
    window.__TAURI__.event.listen('agent-stream', (event) => {
        const { delta, is_final, arc_id, is_thinking } = event.payload;

        // Check if this stream belongs to the currently visible arc.
        const isActiveArc = !arc_id || arc_id === activeArcId;

        if (is_final) {
            if (isActiveArc && streamingBubble && streamingText) {
                streamingBubble.innerHTML = renderMarkdown(streamingText);
                streamingBubble.classList.remove('streaming');
            }
            // Close the thinking block so it's collapsed by default
            // but still expandable by the user.
            if (isActiveArc && thinkingBlock && thinkingText) {
                thinkingContent.textContent = thinkingText;
                thinkingBlock.open = false;
            }
            // If it was a background arc, show a notification dot
            // and refresh the sidebar.
            if (!isActiveArc && arc_id) {
                markArcWithNotification(arc_id);
                loadArcs();
            }
            streamingBubble = null;
            streamingText = '';
            thinkingBlock = null;
            thinkingContent = null;
            thinkingText = '';
            return;
        }

        if (!delta) return;

        // For background arcs, silently accumulate but don't render.
        if (!isActiveArc) return;

        didReceiveStreamChunks = true;

        if (is_thinking) {
            thinkingText += delta;

            // Create the thinking block on the first thinking chunk.
            if (!thinkingBlock) {
                // Remove welcome message if present.
                const welcome = messagesEl.querySelector('.welcome-message');
                if (welcome) welcome.remove();

                // Ensure we have a message row to attach the thinking block to.
                let row = messagesEl.querySelector('#streaming-message');
                if (!row) {
                    row = document.createElement('div');
                    row.className = 'message-row assistant';
                    row.id = 'streaming-message';

                    const avatar = document.createElement('div');
                    avatar.className = 'message-avatar';
                    avatar.textContent = 'A';

                    const wrap = document.createElement('div');
                    wrap.className = 'message-content-wrap';

                    row.appendChild(avatar);
                    row.appendChild(wrap);
                    messagesEl.appendChild(row);
                }

                const wrap = row.querySelector('.message-content-wrap');

                thinkingBlock = document.createElement('details');
                thinkingBlock.className = 'thinking-block';
                thinkingBlock.open = true;

                const summary = document.createElement('summary');
                summary.textContent = 'Thinking...';
                thinkingBlock.appendChild(summary);

                thinkingContent = document.createElement('div');
                thinkingContent.className = 'thinking-content';
                thinkingBlock.appendChild(thinkingContent);

                wrap.appendChild(thinkingBlock);
            }

            thinkingContent.textContent = thinkingText;
        } else {
            streamingText += delta;

            // Create the streaming bubble on the first content chunk.
            if (!streamingBubble) {
                // Remove welcome message if present.
                const welcome = messagesEl.querySelector('.welcome-message');
                if (welcome) welcome.remove();

                let row = messagesEl.querySelector('#streaming-message');
                if (!row) {
                    row = document.createElement('div');
                    row.className = 'message-row assistant';
                    row.id = 'streaming-message';

                    const avatar = document.createElement('div');
                    avatar.className = 'message-avatar';
                    avatar.textContent = 'A';

                    const wrap = document.createElement('div');
                    wrap.className = 'message-content-wrap';

                    row.appendChild(avatar);
                    row.appendChild(wrap);
                    messagesEl.appendChild(row);
                }

                const wrap = row.querySelector('.message-content-wrap');

                streamingBubble = document.createElement('div');
                streamingBubble.className = 'message-bubble streaming';

                wrap.appendChild(streamingBubble);
            }

            streamingBubble.textContent = streamingText;
        }

        requestAnimationFrame(() => {
            messagesEl.parentElement.scrollTo({
                top: messagesEl.parentElement.scrollHeight,
                behavior: 'auto'
            });
        });
    });

    // Listen for arc updates (e.g. Telegram auto-execution).
    window.__TAURI__.event.listen('arc-updated', () => {
        loadArcs();
    });

    // Listen for notifications from the agent
    window.__TAURI__.event.listen('notification', (event) => {
        const data = event.payload;
        showNotificationToast(data);
        updateNotifBadge();
        // If the user is viewing the notifications tab, refresh the list
        if (notificationsView && !notificationsView.classList.contains('hidden')) {
            loadNotifications();
        }
    });

    // Listen for path-grant requests from the file gate.
    window.__TAURI__.event.listen('grant-requested', (event) => {
        enqueueGrantRequest(event.payload);
    });

    // Listen for sense events (email, calendar, messaging, etc.)
    window.__TAURI__.event.listen('sense-event', (event) => {
        const { source, from, subject, body_preview,
                relevance, reason, suggested_action, arc_id } = event.payload;
        showSenseNotification(source, from, subject, body_preview,
                              relevance, reason, suggested_action, arc_id);
    });
}

function initTauri() {
    performance.mark('athen-init-start');
    if (window.__TAURI__ && window.__TAURI__.core) {
        invoke = window.__TAURI__.core.invoke;

        // Synchronous, lightweight: just registers .listen() handlers.
        // Must run before any task could fire (e.g. agent-stream).
        registerTauriEventListeners();

        setStatus('idle', 'Ready');

        // Yield to the renderer for first paint before kicking off any
        // IPC. WebKitGTK 2.52's WebProcess watchdog crashes if a sync
        // IPC isn't answered within 10s of document load -- spreading
        // startup work across frames keeps the main thread responsive.
        requestAnimationFrame(() => requestAnimationFrame(() => {
            // Onboarding check runs in parallel with normal data loads.
            // The overlay sits on top so any partial UI behind it stays
            // hidden, and skipping/completing onboarding reveals the
            // already-loaded main UI immediately.
            maybeRunOnboarding();
            startInitialDataLoads();
        }));
    } else {
        setStatus('working', 'Waiting for Tauri...');
        setTimeout(initTauri, 100);
    }
}

function startInitialDataLoads() {
    performance.mark('athen-init-data-load');

    // Critical path: arc + history needed for first usable view.
    invoke('get_current_arc').then((sid) => {
        activeArcId = sid;
        loadArcs();
        loadHistory();
    }).catch(() => {
        loadHistory();
    }).finally(() => {
        performance.mark('athen-init-done');
        try {
            const t0 = performance.getEntriesByName('athen-init-start')[0];
            const t1 = performance.getEntriesByName('athen-init-data-load')[0];
            const t2 = performance.getEntriesByName('athen-init-done')[0];
            if (t0 && t1 && t2) {
                console.log(
                    `[athen] init: paint-yield=${(t1.startTime - t0.startTime).toFixed(1)}ms, ` +
                    `critical-load=${(t2.startTime - t1.startTime).toFixed(1)}ms, ` +
                    `total=${(t2.startTime - t0.startTime).toFixed(1)}ms`
                );
            }
        } catch (_) { /* ignore */ }
    });

    // Non-critical: defer to idle slices so they can't contend with first paint.
    scheduleIdle(() => updateNotifBadge());
    scheduleIdle(() => recoverPendingGrants());
}

// ─── DOM References ───

const messagesEl = document.getElementById('messages');
const inputEl = document.getElementById('message-input');
const formEl = document.getElementById('input-form');
const statusDot = document.getElementById('status-dot');
const statusText = document.getElementById('status-text');
const sendBtn = document.getElementById('send-btn');
const stopBtn = document.getElementById('stop-btn');
const sessionListEl = document.getElementById('session-list');
const sidebarEl = document.getElementById('sidebar');
const sidebarOverlay = document.getElementById('sidebar-overlay');
const sidebarToggle = document.getElementById('sidebar-toggle');

// ─── Sidebar Logic ───

async function loadArcs() {
    if (!invoke) return;
    try {
        const arcs = await invoke('list_arcs');
        renderArcList(arcs || []);
    } catch (err) {
        console.error('Failed to load arcs:', err);
    }
}

function getSourceIcon(source) {
    switch (source) {
        case 'Email': return '<span class="arc-source-icon" title="Email">&#x1f4e7;</span>';
        case 'Calendar': return '<span class="arc-source-icon" title="Calendar">&#x1f4c5;</span>';
        case 'Messaging': return '<span class="arc-source-icon" title="Message">&#x1f4ac;</span>';
        case 'System': return '<span class="arc-source-icon" title="System">&#9881;</span>';
        default: return '<span class="arc-source-icon" title="Chat">&#x1f4ac;</span>';
    }
}

// Build a single arc DOM item. Extracted so renderArcList can stream
// items across frames without duplicating the construction logic.
function buildArcItem(arc) {
    const item = document.createElement('div');
        item.className = 'session-item';
        if (arc.id === activeArcId) {
            item.classList.add('active');
        }
        item.dataset.arcId = arc.id;

        const content = document.createElement('div');
        content.className = 'session-item-content';

        // Notification dot for arcs with unread background activity.
        if (arcsWithNotifications.has(arc.id)) {
            const dot = document.createElement('span');
            dot.className = 'arc-notification-dot';
            content.appendChild(dot);
        }

        const nameEl = document.createElement('div');
        nameEl.className = 'session-item-name';
        nameEl.textContent = arc.name;
        content.appendChild(nameEl);

        const metaEl = document.createElement('div');
        metaEl.className = 'session-item-meta';

        // Source icon
        const sourceIconSpan = document.createElement('span');
        sourceIconSpan.innerHTML = getSourceIcon(arc.source);
        metaEl.appendChild(sourceIconSpan);

        // Branch indicator
        if (arc.parent_arc_id) {
            const branchBadge = document.createElement('span');
            branchBadge.className = 'arc-branch-badge';
            branchBadge.textContent = '\u21b3';
            metaEl.appendChild(branchBadge);
        }

        const dateEl = document.createElement('span');
        dateEl.className = 'session-item-date';
        dateEl.textContent = formatSessionDate(arc.updated_at);
        metaEl.appendChild(dateEl);

        if (arc.entry_count > 0) {
            const countEl = document.createElement('span');
            countEl.className = 'session-item-count';
            countEl.textContent = arc.entry_count;
            metaEl.appendChild(countEl);
        }

        content.appendChild(metaEl);
        item.appendChild(content);

        // Action buttons (rename + branch + delete)
        const actions = document.createElement('div');
        actions.className = 'session-item-actions';

        const renameBtn = document.createElement('button');
        renameBtn.className = 'session-action-btn';
        renameBtn.title = 'Rename';
        renameBtn.innerHTML = '&#9998;'; // pencil
        renameBtn.addEventListener('click', (e) => {
            e.stopPropagation();
            startRenameArc(item, arc.id, arc.name);
        });
        actions.appendChild(renameBtn);

        const branchBtn = document.createElement('button');
        branchBtn.className = 'session-action-btn';
        branchBtn.title = 'Branch';
        branchBtn.textContent = '\u21b3';
        branchBtn.addEventListener('click', (e) => {
            e.stopPropagation();
            branchFromArc(arc.id, arc.name);
        });
        actions.appendChild(branchBtn);

        const deleteBtn = document.createElement('button');
        deleteBtn.className = 'session-action-btn delete';
        deleteBtn.title = 'Delete';
        deleteBtn.innerHTML = '&#10005;'; // x mark
        deleteBtn.addEventListener('click', (e) => {
            e.stopPropagation();
            handleDeleteArc(arc.id);
        });
        actions.appendChild(deleteBtn);

        item.appendChild(actions);

        // Click to switch arc
        item.addEventListener('click', () => {
            handleSwitchArc(arc.id);
        });

        // Double-click to rename
        nameEl.addEventListener('dblclick', (e) => {
            e.stopPropagation();
            startRenameArc(item, arc.id, arc.name);
        });

    return item;
}

// Render the arc sidebar. The first ARC_EAGER_COUNT visible arcs are
// rendered synchronously (they're above the fold). Any remaining arcs
// are appended on idle slices so initial paint isn't blocked when the
// user has hundreds of conversations.
const ARC_EAGER_COUNT = 10;
function renderArcList(arcs) {
    sessionListEl.innerHTML = '';

    if (!arcs || arcs.length === 0) {
        sessionListEl.innerHTML = '<div class="session-list-empty">No conversations yet</div>';
        return;
    }

    // Filter out merged arcs once so the chunking math below is honest.
    const visible = arcs.filter((a) => a.status !== 'Merged');

    const eager = visible.slice(0, ARC_EAGER_COUNT);
    const rest = visible.slice(ARC_EAGER_COUNT);

    for (const arc of eager) {
        sessionListEl.appendChild(buildArcItem(arc));
    }

    if (rest.length === 0) return;

    let idx = 0;
    function appendChunk() {
        // Two arcs per idle slice keeps frame budget under ~4ms.
        const end = Math.min(idx + 2, rest.length);
        for (; idx < end; idx++) {
            sessionListEl.appendChild(buildArcItem(rest[idx]));
        }
        if (idx < rest.length) scheduleIdle(appendChunk);
    }
    scheduleIdle(appendChunk);
}

/// Mark an arc as having unread background activity.
function markArcWithNotification(arcId) {
    arcsWithNotifications.add(arcId);
    // Try to update the existing sidebar item immediately.
    const item = sessionListEl.querySelector(`[data-arc-id="${arcId}"]`);
    if (item && !item.querySelector('.arc-notification-dot')) {
        const dot = document.createElement('span');
        dot.className = 'arc-notification-dot';
        item.querySelector('.session-item-content')?.prepend(dot);
    }
}

function formatSessionDate(dateStr) {
    try {
        const date = new Date(dateStr);
        const now = new Date();
        const diffMs = now - date;
        const diffDays = Math.floor(diffMs / (1000 * 60 * 60 * 24));

        if (diffDays === 0) return 'Today';
        if (diffDays === 1) return 'Yesterday';
        if (diffDays < 7) return `${diffDays}d ago`;

        return date.toLocaleDateString([], { month: 'short', day: 'numeric' });
    } catch {
        return '';
    }
}

function startRenameArc(itemEl, arcId, currentName) {
    const nameEl = itemEl.querySelector('.session-item-name');
    if (!nameEl) return;

    // Replace name text with an input.
    const input = document.createElement('input');
    input.type = 'text';
    input.className = 'session-rename-input';
    input.value = currentName;
    nameEl.textContent = '';
    nameEl.appendChild(input);
    input.focus();
    input.select();

    const finishRename = async (save) => {
        const newName = input.value.trim();
        if (save && newName && newName !== currentName) {
            try {
                await invoke('rename_arc', { arcId, name: newName });
                nameEl.textContent = newName;
            } catch (err) {
                console.error('Rename failed:', err);
                nameEl.textContent = currentName;
            }
        } else {
            nameEl.textContent = currentName;
        }
    };

    input.addEventListener('keydown', (e) => {
        if (e.key === 'Enter') {
            e.preventDefault();
            finishRename(true);
        } else if (e.key === 'Escape') {
            finishRename(false);
        }
    });

    input.addEventListener('blur', () => {
        finishRename(true);
    });
}

async function handleSwitchArc(arcId) {
    if (!invoke || arcId === activeArcId) return;

    try {
        const entries = await invoke('switch_arc', { arcId });
        activeArcId = arcId;

        // Clear notification dot for this arc.
        arcsWithNotifications.delete(arcId);

        // Check if the arc has entries already (for auto-naming).
        arcHasMessages = entries && entries.length > 0;

        // Clear the chat UI and render the loaded entries.
        clearChatUI();
        renderEntries(entries);

        // Update active highlight in sidebar.
        document.querySelectorAll('.session-item').forEach((el) => {
            el.classList.toggle('active', el.dataset.arcId === arcId);
        });

        // Close sidebar on mobile.
        closeSidebar();

        inputEl.focus();
    } catch (err) {
        console.error('Switch arc failed:', err);
    }
}

async function handleDeleteArc(arcId) {
    if (!invoke) return;
    if (!confirm('Delete this Arc and all its entries?')) return;

    try {
        const newActiveId = await invoke('delete_arc', { arcId });

        // If the deleted arc was the active one, the backend switched us.
        if (arcId === activeArcId) {
            activeArcId = newActiveId;
            // Reload entries for the new active arc.
            try {
                const entries = await invoke('get_arc_history');
                clearChatUI();
                renderEntries(entries);
                arcHasMessages = !!(entries && entries.length > 0);
            } catch (err2) {
                console.error('Failed to load history after delete:', err2);
                clearChatUI();
            }
        }

        // Refresh the sidebar list.
        await loadArcs();
    } catch (err) {
        console.error('Delete arc failed:', err);
    }
}

function clearChatUI() {
    messagesEl.innerHTML = `
        <div class="welcome-message">
            <div class="welcome-icon">A</div>
            <p>Hello! I'm <strong>Athen</strong>, your universal AI agent. I can execute shell commands, read and write files, manage tasks, and more.</p>
            <p class="welcome-hint">Type a message below to get started.</p>
        </div>
    `;
    currentToolContainer = null;
    streamingBubble = null;
    streamingText = '';
    didReceiveStreamChunks = false;
}

// ─── Sidebar Toggle (mobile) ───

function openSidebar() {
    sidebarEl.classList.add('open');
    sidebarOverlay.classList.add('visible');
}

function closeSidebar() {
    sidebarEl.classList.remove('open');
    sidebarOverlay.classList.remove('visible');
}

if (sidebarToggle) {
    sidebarToggle.addEventListener('click', () => {
        if (sidebarEl.classList.contains('open')) {
            closeSidebar();
        } else {
            openSidebar();
        }
    });
}

if (sidebarOverlay) {
    sidebarOverlay.addEventListener('click', closeSidebar);
}

// ─── Auto-name arc ───

async function autoNameArc(message) {
    if (!invoke || !activeArcId || arcHasMessages) return;
    arcHasMessages = true;

    // Truncate the first message to ~30 characters for the arc name.
    let name = message.trim();
    if (name.length > 30) {
        // Cut at last word boundary within 30 chars.
        name = name.substring(0, 30);
        const lastSpace = name.lastIndexOf(' ');
        if (lastSpace > 15) {
            name = name.substring(0, lastSpace);
        }
        name += '...';
    }

    try {
        await invoke('rename_arc', { arcId: activeArcId, name });
        // Update the sidebar item in place.
        const item = sessionListEl.querySelector(
            `.session-item[data-arc-id="${activeArcId}"] .session-item-name`
        );
        if (item) {
            item.textContent = name;
        }
    } catch (err) {
        console.error('Auto-name arc failed:', err);
    }
}

// ─── Markdown Renderer ───

function parseTableRow(line) {
    return line.trim()
        .replace(/^\|/, '')
        .replace(/\|$/, '')
        .split('|')
        .map((c) => c.trim());
}
function parseTableSeparator(line) {
    if (!/^\s*\|?[\s:|-]+\|[\s:|-]+\|?\s*$/.test(line)) return null;
    const cells = parseTableRow(line);
    if (cells.length === 0) return null;
    const aligns = [];
    for (const c of cells) {
        if (!/^:?-{3,}:?$/.test(c) && !/^:?-+:?$/.test(c)) return null;
        const left = c.startsWith(':');
        const right = c.endsWith(':');
        aligns.push(left && right ? 'center' : right ? 'right' : left ? 'left' : null);
    }
    return aligns;
}

function renderMarkdown(text) {
    const codeBlocks = [];
    let processed = text.replace(/```(\w*)\n([\s\S]*?)```/g, (_match, lang, code) => {
        const idx = codeBlocks.length;
        const langLabel = lang ? `<div class="code-lang">${escapeHtml(lang)}</div>` : '';
        const body = escapeHtml(code.replace(/\n$/, ''));
        codeBlocks.push(`<div class="code-block">${langLabel}<span class="code-body">${body}</span></div>`);
        return `\x00CODEBLOCK_${idx}\x00`;
    });

    const inlineCodes = [];
    processed = processed.replace(/`([^`\n]+)`/g, (_match, code) => {
        const idx = inlineCodes.length;
        inlineCodes.push(`<span class="inline-code">${escapeHtml(code)}</span>`);
        return `\x00INLINE_${idx}\x00`;
    });

    // Split into lines for block-level processing
    const lines = processed.split('\n');
    const result = [];
    let i = 0;

    while (i < lines.length) {
        const line = lines[i];

        // Headers
        const headerMatch = line.match(/^(#{1,3})\s+(.+)$/);
        if (headerMatch) {
            const level = headerMatch[1].length;
            result.push(`<h${level}>${renderInline(headerMatch[2])}</h${level}>`);
            i++;
            continue;
        }

        // Unordered list
        if (/^[\s]*[-*]\s+/.test(line)) {
            const items = [];
            while (i < lines.length && /^[\s]*[-*]\s+/.test(lines[i])) {
                items.push(`<li>${renderInline(lines[i].replace(/^[\s]*[-*]\s+/, ''))}</li>`);
                i++;
            }
            result.push(`<ul>${items.join('')}</ul>`);
            continue;
        }

        // Ordered list
        if (/^[\s]*\d+\.\s+/.test(line)) {
            const items = [];
            while (i < lines.length && /^[\s]*\d+\.\s+/.test(lines[i])) {
                items.push(`<li>${renderInline(lines[i].replace(/^[\s]*\d+\.\s+/, ''))}</li>`);
                i++;
            }
            result.push(`<ol>${items.join('')}</ol>`);
            continue;
        }

        // GitHub-flavored markdown tables: header row + separator + data rows.
        // The separator decides whether the lines are actually a table; without
        // it we fall through and treat them as a paragraph (so a stray pipe in
        // prose isn't promoted to a table).
        if (/^\s*\|.*\|\s*$/.test(line) && i + 1 < lines.length) {
            const aligns = parseTableSeparator(lines[i + 1]);
            if (aligns) {
                const headers = parseTableRow(line);
                if (headers.length === aligns.length) {
                    i += 2;
                    const rows = [];
                    while (i < lines.length && /^\s*\|.*\|\s*$/.test(lines[i])) {
                        rows.push(parseTableRow(lines[i]));
                        i++;
                    }
                    const cellStyle = (idx) => {
                        const a = aligns[idx];
                        return a ? ` style="text-align:${a}"` : '';
                    };
                    const thead = `<thead><tr>${headers.map((h, idx) =>
                        `<th${cellStyle(idx)}>${renderInline(h)}</th>`
                    ).join('')}</tr></thead>`;
                    const tbody = `<tbody>${rows.map(r =>
                        `<tr>${r.map((c, idx) =>
                            `<td${cellStyle(idx)}>${renderInline(c)}</td>`
                        ).join('')}</tr>`
                    ).join('')}</tbody>`;
                    result.push(`<div class="md-table-wrap"><table class="md-table">${thead}${tbody}</table></div>`);
                    continue;
                }
            }
        }

        // Empty line — paragraph break
        if (line.trim() === '') {
            result.push('');
            i++;
            continue;
        }

        // Standalone code-block placeholder — emit as block, do not wrap in <p>
        if (/^\x00CODEBLOCK_\d+\x00$/.test(line)) {
            result.push(line);
            i++;
            continue;
        }

        // Regular text — collect consecutive lines into a paragraph
        const paraLines = [];
        while (i < lines.length && lines[i].trim() !== '' &&
               !/^#{1,3}\s+/.test(lines[i]) &&
               !/^[\s]*[-*]\s+/.test(lines[i]) &&
               !/^[\s]*\d+\.\s+/.test(lines[i]) &&
               !/^\s*\|.*\|\s*$/.test(lines[i]) &&
               !/^\x00CODEBLOCK_\d+\x00$/.test(lines[i])) {
            paraLines.push(lines[i]);
            i++;
        }
        if (paraLines.length > 0) {
            const text = paraLines.map(l => renderInline(l)).join('<br>');
            result.push(`<p>${text}</p>`);
        }
    }

    let html = result.filter(l => l !== '').join('\n');

    codeBlocks.forEach((block, idx) => {
        html = html.replaceAll(`\x00CODEBLOCK_${idx}\x00`, block);
    });

    inlineCodes.forEach((code, idx) => {
        html = html.replaceAll(`\x00INLINE_${idx}\x00`, code);
    });

    return html;
}

function renderInline(text) {
    // Bold
    text = text.replace(/\*\*(.+?)\*\*/g, '<strong>$1</strong>');
    // Italic (but not inside bold markers)
    text = text.replace(/(?<!\*)\*([^*]+?)\*(?!\*)/g, '<em>$1</em>');
    // Links
    text = text.replace(/\[([^\]]+)\]\(([^)]+)\)/g, '<a href="$2" target="_blank" rel="noopener">$1</a>');
    return text;
}

function escapeHtml(text) {
    const div = document.createElement('div');
    div.textContent = text;
    return div.innerHTML;
}

// ─── Sense Notifications ───

function showSenseNotification(source, from, subject, bodyPreview,
                                relevance, reason, suggestedAction, arcId) {
    const container = document.getElementById('messages');
    if (!container) return;

    const welcome = container.querySelector('.welcome-message');
    if (welcome) welcome.remove();

    const card = document.createElement('div');
    const urgencyClass = relevance === 'high' ? 'email-high' : 'email-medium';
    card.className = 'email-card ' + urgencyClass;

    const preview = bodyPreview
        ? '<div class="email-card-body">' + escapeHtml(bodyPreview) + '</div>'
        : '';

    const relevanceBadge = relevance === 'high'
        ? '<span class="email-badge email-badge-high">Urgent</span>'
        : '<span class="email-badge email-badge-medium">Important</span>';

    const sourceIcon = source === 'email' ? '\u{1f4e7}' :
                       source === 'calendar' ? '\u{1f4c5}' :
                       source === 'message' ? '\u{1f4ac}' : '\u{2699}\u{fe0f}';
    const sourceLabel = source.charAt(0).toUpperCase() + source.slice(1);

    const reasonHtml = reason
        ? '<div class="email-card-reason">' + escapeHtml(reason) + '</div>'
        : '';

    // Build action buttons based on source and suggested_action.
    let actionsHtml = '';

    if (source === 'calendar') {
        // Calendar-specific actions
        actionsHtml += '<button class="email-action-btn email-action-primary" onclick="askAboutSenseEvent(this, \'prepare\')">What should I prepare?</button>';
    } else {
        // Email / messaging actions
        actionsHtml += '<button class="email-action-btn" onclick="askAboutSenseEvent(this, \'summarize\')">Summarize</button>';
        if (suggestedAction === 'reply' || suggestedAction === 'urgent') {
            actionsHtml += '<button class="email-action-btn email-action-primary" onclick="askAboutSenseEvent(this, \'reply\')">Draft Reply</button>';
        }
        if (suggestedAction === 'calendar') {
            actionsHtml += '<button class="email-action-btn" onclick="askAboutSenseEvent(this, \'calendar\')">Add to Calendar</button>';
        }
    }

    // Open Arc button — always present, switches context to the event's arc
    if (arcId) {
        actionsHtml += '<button class="email-action-btn" onclick="handleSwitchArc(\'' + escapeHtml(arcId) + '\')">Open Arc</button>';
    }

    card.innerHTML =
        '<div class="email-card-header">' +
            '<span class="email-card-icon">' + sourceIcon + '</span>' +
            relevanceBadge +
            '<span class="email-card-label">' + sourceLabel + '</span>' +
            '<span class="email-card-time">' + formatTime(new Date()) + '</span>' +
        '</div>' +
        '<div class="email-card-from">' + escapeHtml(from) + '</div>' +
        '<div class="email-card-subject">' + escapeHtml(subject) + '</div>' +
        reasonHtml +
        preview +
        '<div class="email-card-actions">' + actionsHtml + '</div>';

    container.appendChild(card);

    requestAnimationFrame(() => {
        container.parentElement.scrollTo({
            top: container.parentElement.scrollHeight,
            behavior: 'smooth'
        });
    });

    // Refresh the arc list since a new arc may have been created.
    loadArcs();
}

// Handle sense event action buttons — sends a message to the agent.
async function askAboutSenseEvent(btn, action) {
    const card = btn.closest('.email-card');
    if (!card) return;
    const from = card.querySelector('.email-card-from')?.textContent || '';
    const subject = card.querySelector('.email-card-subject')?.textContent || '';
    const body = card.querySelector('.email-card-body')?.textContent || '';

    // Find the arc_id from the Open Arc button in the same card.
    const arcBtn = card.querySelector('.email-action-btn[onclick*="handleSwitchArc"]');
    const arcIdMatch = arcBtn?.getAttribute('onclick')?.match(/handleSwitchArc\('([^']+)'\)/);
    const arcId = arcIdMatch ? arcIdMatch[1] : null;

    // Switch to the event's Arc first so the agent has full context.
    if (arcId && arcId !== activeArcId) {
        await handleSwitchArc(arcId);
    }

    let prompt;
    if (action === 'summarize') {
        prompt = 'Summarize this for me briefly.';
    } else if (action === 'reply') {
        prompt = 'Draft a professional reply to this.';
    } else if (action === 'calendar') {
        prompt = 'Extract the event details and tell me what to add to my calendar.';
    } else if (action === 'prepare') {
        prompt = 'What should I prepare or know about for this upcoming event?';
    }

    if (prompt && inputEl) {
        inputEl.value = prompt;
        formEl.requestSubmit();
    }
}

// ─── Time Formatting ───

function formatTime(date) {
    return date.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
}

// ─── Message Rendering ───

function addMessage(role, content, meta) {
    // Remove welcome message on first real message
    const welcome = messagesEl.querySelector('.welcome-message');
    if (welcome) welcome.remove();

    const row = document.createElement('div');
    row.className = `message-row ${role}`;

    const avatar = document.createElement('div');
    avatar.className = 'message-avatar';
    avatar.textContent = role === 'user' ? 'Y' : 'A';

    const wrap = document.createElement('div');
    wrap.className = 'message-content-wrap';

    // Tool calls go above the bubble
    if (meta && meta.toolCallsHtml) {
        const toolsDiv = document.createElement('div');
        toolsDiv.className = 'tool-calls-container';
        toolsDiv.innerHTML = meta.toolCallsHtml;
        wrap.appendChild(toolsDiv);
    }

    const bubble = document.createElement('div');
    bubble.className = 'message-bubble';

    if (role === 'user') {
        // User messages: escape HTML to prevent XSS
        bubble.textContent = content;
    } else if (meta && meta.isError) {
        bubble.className = 'message-bubble error-message';

        // Build structured error card with icon, message, and optional action.
        const errorIcon = document.createElement('span');
        errorIcon.className = 'error-icon';
        errorIcon.innerHTML = '&#9888;'; // warning triangle

        const errorText = document.createElement('span');
        errorText.className = 'error-text';
        errorText.textContent = content;

        bubble.appendChild(errorIcon);
        bubble.appendChild(errorText);

        // Determine error category for actionable buttons.
        const errorStr = content.toLowerCase();
        const isRetryable = errorStr.includes('took too long')
            || errorStr.includes('could not connect')
            || errorStr.includes('rate limit')
            || errorStr.includes('try again');
        const isAuthError = errorStr.includes('api key')
            || errorStr.includes('authentication');

        if (isRetryable && lastMessage) {
            const retryBtn = document.createElement('button');
            retryBtn.className = 'error-retry-btn';
            retryBtn.textContent = 'Retry';
            retryBtn.addEventListener('click', () => {
                retryLastMessage();
            });
            bubble.appendChild(retryBtn);
        }

        if (isAuthError) {
            const settingsLink = document.createElement('button');
            settingsLink.className = 'error-settings-link';
            settingsLink.textContent = 'Open Settings';
            settingsLink.addEventListener('click', () => {
                const settingsBtn = document.getElementById('settings-btn');
                if (settingsBtn) settingsBtn.click();
            });
            bubble.appendChild(settingsLink);
        }
    } else {
        // Assistant messages: render markdown
        bubble.innerHTML = renderMarkdown(content);
    }

    wrap.appendChild(bubble);

    // Meta line (time, risk badge, domain)
    const metaRow = document.createElement('div');
    metaRow.className = 'message-meta';
    let metaHtml = `<span class="message-time">${formatTime(new Date())}</span>`;
    if (meta && meta.riskHtml) {
        metaHtml += meta.riskHtml;
    }
    if (meta && meta.domain) {
        metaHtml += `<span class="domain-label">${escapeHtml(meta.domain)}</span>`;
    }
    metaRow.innerHTML = metaHtml;
    wrap.appendChild(metaRow);

    row.appendChild(avatar);
    row.appendChild(wrap);
    messagesEl.appendChild(row);

    // Smooth scroll to bottom
    requestAnimationFrame(() => {
        messagesEl.parentElement.scrollTo({
            top: messagesEl.parentElement.scrollHeight,
            behavior: 'smooth'
        });
    });
}

/// Finalize a streaming message bubble by adding meta information
/// (time, risk badge, domain) and removing the streaming class.
function finalizeStreamingMessage(meta) {
    const streamRow = document.getElementById('streaming-message');
    if (!streamRow) return;

    // Remove the temporary id and streaming class.
    streamRow.removeAttribute('id');
    const bubble = streamRow.querySelector('.message-bubble');
    if (bubble) {
        bubble.classList.remove('streaming');
        // If the is_final handler already rendered markdown, the bubble has
        // innerHTML set.  If not (race condition), render from streamingText.
        if (streamingText) {
            bubble.innerHTML = renderMarkdown(streamingText);
        }
    }

    // Add meta row to the content wrap.
    const wrap = streamRow.querySelector('.message-content-wrap');
    if (wrap) {
        const metaRow = document.createElement('div');
        metaRow.className = 'message-meta';
        let metaHtml = `<span class="message-time">${formatTime(new Date())}</span>`;
        if (meta && meta.riskHtml) {
            metaHtml += meta.riskHtml;
        }
        if (meta && meta.domain) {
            metaHtml += `<span class="domain-label">${escapeHtml(meta.domain)}</span>`;
        }
        metaRow.innerHTML = metaHtml;
        wrap.appendChild(metaRow);
    }
}

// ─── Status Management ───

function setStatus(state, text) {
    statusDot.className = `status-dot ${state}`;
    statusText.textContent = text;
}

function setInputEnabled(enabled) {
    inputEl.disabled = !enabled;
    isProcessing = !enabled;
    if (enabled) {
        // Show send button, hide stop button.
        sendBtn.classList.remove('hidden');
        sendBtn.disabled = false;
        stopBtn.classList.add('hidden');
    } else {
        // Hide send button, show stop button.
        sendBtn.classList.add('hidden');
        stopBtn.classList.remove('hidden');
    }
}

// ─── Textarea Auto-Resize ───

function autoResize() {
    inputEl.style.height = 'auto';
    const newHeight = Math.min(inputEl.scrollHeight, 150);
    inputEl.style.height = newHeight + 'px';
}

inputEl.addEventListener('input', autoResize);

// ─── Keyboard Handling ───

inputEl.addEventListener('keydown', (e) => {
    if (e.key === 'Enter' && !e.shiftKey) {
        e.preventDefault();
        formEl.requestSubmit();
    }
});

// ─── Approval Dialog ───

function addApprovalDialog(approval) {
    // Remove welcome message if present
    const welcome = messagesEl.querySelector('.welcome-message');
    if (welcome) welcome.remove();

    const row = document.createElement('div');
    row.className = 'message-row assistant';
    row.id = `approval-${approval.task_id}`;

    const avatar = document.createElement('div');
    avatar.className = 'message-avatar';
    avatar.textContent = 'A';

    const wrap = document.createElement('div');
    wrap.className = 'message-content-wrap';

    const bubble = document.createElement('div');
    bubble.className = 'message-bubble approval-bubble';

    const riskClass = approval.risk_level === 'Critical' ? 'danger' :
                      approval.risk_level === 'Danger' ? 'danger' :
                      approval.risk_level === 'Caution' ? 'caution' : 'safe';

    bubble.innerHTML = `
        <div class="approval-header">
            <span class="approval-icon">&#9888;</span>
            <span class="approval-title">This action requires approval</span>
        </div>
        <div class="approval-details">
            <div class="approval-risk">
                <span class="risk-badge ${riskClass}">${escapeHtml(approval.risk_level)}</span>
                <span class="approval-score">Risk score: ${Math.round(approval.risk_score)}</span>
            </div>
            <div class="approval-description">${escapeHtml(approval.description)}</div>
        </div>
        <div class="approval-actions">
            <button class="btn-approve" data-task-id="${approval.task_id}">Approve</button>
            <button class="btn-deny" data-task-id="${approval.task_id}">Deny</button>
        </div>
    `;

    // Wire up buttons via event listeners (safer than inline onclick)
    wrap.appendChild(bubble);

    const metaRow = document.createElement('div');
    metaRow.className = 'message-meta';
    metaRow.innerHTML = `<span class="message-time">${formatTime(new Date())}</span>`;
    wrap.appendChild(metaRow);

    row.appendChild(avatar);
    row.appendChild(wrap);
    messagesEl.appendChild(row);

    // Attach click handlers after adding to DOM
    bubble.querySelector('.btn-approve').addEventListener('click', () => {
        handleApproval(approval.task_id, true);
    });
    bubble.querySelector('.btn-deny').addEventListener('click', () => {
        handleApproval(approval.task_id, false);
    });

    requestAnimationFrame(() => {
        messagesEl.parentElement.scrollTo({
            top: messagesEl.parentElement.scrollHeight,
            behavior: 'smooth'
        });
    });
}

async function handleApproval(taskId, approved) {
    if (!invoke) return;

    // Disable the approval buttons immediately.
    const approvalEl = document.getElementById(`approval-${taskId}`);
    if (approvalEl) {
        const buttons = approvalEl.querySelectorAll('button');
        buttons.forEach(btn => { btn.disabled = true; });
    }

    setInputEnabled(false);
    setStatus('working', approved ? 'Executing approved action...' : 'Cancelling...');

    // Reset tool container and streaming state for approval execution.
    currentToolContainer = null;
    streamingBubble = null;
    streamingText = '';
    didReceiveStreamChunks = false;

    try {
        const response = await invoke('approve_task', {
            taskId: taskId,
            approved: approved
        });

        // Remove the approval dialog
        if (approvalEl) {
            approvalEl.remove();
        }

        if (!approved) {
            addMessage('assistant', 'Action denied by user.', {
                riskHtml: '<span class="risk-badge safe">Denied</span>'
            });
        } else {
            const meta = {};

            if (response.risk_level) {
                const riskClass = response.risk_level === 'Safe' ? 'safe' :
                                 response.risk_level === 'Caution' ? 'caution' : 'danger';
                meta.riskHtml = `<span class="risk-badge ${riskClass}">${escapeHtml(response.risk_level)}</span>`;
            }
            if (response.domain) {
                meta.domain = response.domain;
            }

            if (didReceiveStreamChunks && streamingBubble) {
                // The response was already streamed progressively.
                // Finalize the streaming bubble with meta info.
                finalizeStreamingMessage(meta);
            } else {
                // No streaming happened (e.g. non-streaming provider,
                // or the response was a failure message). Show normally.
                addMessage('assistant', response.content || '', meta);
            }
        }

        setStatus('idle', 'Ready');
    } catch (err) {
        console.error('Approval error:', err);
        addMessage('assistant', `Error: ${err}`, { isError: true });
        setStatus('error', 'Error');
    }

    // Reset streaming state.
    streamingBubble = null;
    streamingText = '';
    didReceiveStreamChunks = false;
    currentToolContainer = null;

    setInputEnabled(true);
    inputEl.focus();

    // Refresh sidebar to update message counts.
    loadArcs();
}

// ─── Form Submission ───

formEl.addEventListener('submit', async (e) => {
    e.preventDefault();

    const message = inputEl.value.trim();
    if (!message) return;

    if (!invoke) {
        addMessage('assistant', 'Tauri backend not connected. Is the app running inside Tauri?', { isError: true });
        return;
    }

    // Auto-name the arc from the first message.
    autoNameArc(message);

    // Store for potential retry on transient errors.
    lastMessage = message;

    // Show user message
    addMessage('user', message);
    inputEl.value = '';
    inputEl.style.height = 'auto';

    // Disable input while processing
    setInputEnabled(false);
    setStatus('working', 'Thinking...');

    // Reset tool container and streaming state for this new request.
    currentToolContainer = null;
    streamingBubble = null;
    streamingText = '';
    didReceiveStreamChunks = false;
    thinkingBlock = null;
    thinkingContent = null;
    thinkingText = '';

    try {
        // Call Tauri backend. While this awaits, `agent-stream` events
        // may arrive and progressively build the streaming bubble.
        const response = await invoke('send_message', { message });

        // If the response contains a pending approval, show the approval dialog.
        if (response.pending_approval) {
            addApprovalDialog(response.pending_approval);
            setStatus('working', 'Awaiting approval');
            // Keep input disabled while awaiting approval decision.
            return;
        }

        // Build meta info
        const meta = {};

        if (response.risk_level) {
            const riskClass = response.risk_level === 'Safe' ? 'safe' :
                             response.risk_level === 'Caution' ? 'caution' : 'danger';
            meta.riskHtml = `<span class="risk-badge ${riskClass}">${escapeHtml(response.risk_level)}</span>`;
        }

        if (response.domain) {
            meta.domain = response.domain;
        }

        // Build tool calls HTML
        if (response.tool_calls && response.tool_calls.length > 0) {
            let toolsHtml = '';
            for (const tc of response.tool_calls) {
                const rawName = tc.name || '';
                const summary = escapeHtml(tc.summary || '');
                const builtinIcon = builtinToolIcon(rawName);
                if (builtinIcon) {
                    const label = escapeHtml(builtinToolLabel(rawName));
                    const titleAttr = escapeHtml(rawName);
                    toolsHtml += `<div class="tool-call builtin" title="${titleAttr}">
                        <span class="tool-call-icon">${builtinIcon}</span>
                        <span class="tool-call-name">${label}</span>
                        <span class="tool-call-summary">${summary}</span>
                    </div>`;
                } else {
                    const name = escapeHtml(rawName);
                    toolsHtml += `<div class="tool-call">
                        <span class="tool-call-name">${name}</span>
                        <span class="tool-call-summary">${summary}</span>
                    </div>`;
                }
            }
            meta.toolCallsHtml = toolsHtml;
        }

        // Check if streaming already rendered the response. The bubble
        // reference may have been cleared by `is_final`, but the DOM element
        // (`#streaming-message`) still exists.
        const streamedRow = messagesEl.querySelector('#streaming-message');
        if (didReceiveStreamChunks && streamedRow) {
            // Re-acquire the bubble reference for finalization.
            streamingBubble = streamedRow.querySelector('.message-bubble');
            finalizeStreamingMessage(meta);
        } else {
            // No streaming happened -- render the full response at once.
            addMessage('assistant', response.content || '', meta);
        }

        currentToolContainer = null;
        setStatus('idle', 'Ready');
    } catch (err) {
        console.error('Tauri invoke error:', err);
        addMessage('assistant', `Error: ${err}`, { isError: true });
        currentToolContainer = null;
        setStatus('error', 'Error');
    }

    // Reset streaming state for the next request.
    streamingBubble = null;
    streamingText = '';
    didReceiveStreamChunks = false;

    setInputEnabled(true);
    inputEl.focus();

    // Refresh sidebar to update message counts.
    loadArcs();
});

// ─── Cancel / Stop Button ───

stopBtn.addEventListener('click', () => {
    if (!invoke || !isProcessing) return;
    invoke('cancel_task').catch((err) => {
        console.error('Failed to cancel task:', err);
    });
    setStatus('working', 'Cancelling...');
});

document.addEventListener('keydown', (e) => {
    if (e.key === 'Escape' && isProcessing && invoke) {
        invoke('cancel_task').catch((err) => {
            console.error('Failed to cancel task:', err);
        });
        setStatus('working', 'Cancelling...');
    }
});

// ─── History Restoration ───

function parseEntryMetadata(metadata) {
    if (!metadata) return null;
    if (typeof metadata !== 'string') return metadata;
    try { return JSON.parse(metadata); } catch { return null; }
}

// Group consecutive tool_call entries into a single render unit so they can
// be displayed as one collapsible dropdown attached to the assistant
// message of the same turn. Other entry types are rendered as before.
function buildRenderUnits(entries) {
    const units = [];
    let buffer = [];
    const flush = () => {
        if (buffer.length > 0) {
            units.push({ kind: 'tool_group', entries: buffer });
            buffer = [];
        }
    };
    for (const entry of entries) {
        if (entry.entry_type === 'tool_call') {
            buffer.push(entry);
        } else {
            flush();
            units.push({ kind: 'entry', entry });
        }
    }
    flush();
    return units;
}

function renderRenderUnit(unit) {
    if (unit.kind === 'tool_group') {
        renderToolGroup(unit.entries);
    } else {
        renderHistoryEntry(unit.entry);
    }
}

function renderEntries(entries) {
    if (!entries) return;
    for (const unit of buildRenderUnits(entries)) renderRenderUnit(unit);
}

// Render a collapsed group of tool_call entries: a clickable strip showing
// each tool's icon, expanding to reveal one card per invocation with its
// status, label, and short result summary.
function renderToolGroup(toolCalls) {
    if (!toolCalls || toolCalls.length === 0) return;

    const details = document.createElement('details');
    details.className = 'tool-group-history';

    const summary = document.createElement('summary');
    summary.className = 'tool-group-summary';

    const icons = document.createElement('span');
    icons.className = 'tool-group-icons';
    for (const tc of toolCalls) {
        const meta = parseEntryMetadata(tc.metadata) || {};
        const toolName = meta.tool || tc.content || '';
        const icon = builtinToolIcon(toolName);
        const slot = document.createElement('span');
        slot.className = 'tool-group-icon-slot';
        slot.title = toolName;
        if (icon) {
            slot.innerHTML = icon;
        } else {
            slot.textContent = toolName.slice(0, 2);
            slot.classList.add('fallback');
        }
        const status = meta.status || 'Completed';
        if (status === 'Failed') slot.classList.add('failed');
        icons.appendChild(slot);
    }
    summary.appendChild(icons);

    const count = document.createElement('span');
    count.className = 'tool-group-count';
    count.textContent = `${toolCalls.length} tool${toolCalls.length === 1 ? '' : 's'}`;
    summary.appendChild(count);

    details.appendChild(summary);

    const body = document.createElement('div');
    body.className = 'tool-group-body tool-steps-container';
    for (const tc of toolCalls) {
        const meta = parseEntryMetadata(tc.metadata) || {};
        const toolName = meta.tool || tc.content || '';
        const status = meta.status || 'Completed';
        const summaryText = meta.summary || '';
        const icon = builtinToolIcon(toolName);

        const card = document.createElement('div');
        const statusClass = status === 'Completed' ? 'completed'
                          : status === 'Failed' ? 'failed' : 'in-progress';
        const builtinClass = icon ? ' builtin' : '';
        card.className = `tool-execution-card ${statusClass}${builtinClass}`;
        card.title = toolName;

        const statusIcon = status === 'Completed' ? '&#10003;'
                         : status === 'Failed' ? '&#10007;' : '&#9679;';
        const labelText = icon ? builtinToolLabel(toolName) : toolName;
        const iconMarkup = icon ? `<span class="tool-builtin-icon">${icon}</span>` : '';
        let detailHtml = '';
        if (summaryText) {
            const truncated = summaryText.length > 80 ? summaryText.substring(0, 80) + '...' : summaryText;
            detailHtml = `<span class="tool-detail">${escapeHtml(truncated)}</span>`;
        }
        card.innerHTML =
            `<span class="tool-status-icon">${statusIcon}</span>` +
            iconMarkup +
            `<span class="tool-name">${escapeHtml(labelText)}</span>` +
            detailHtml;
        body.appendChild(card);
    }
    details.appendChild(body);

    messagesEl.appendChild(details);
}

// Render a single non-tool-call history entry. tool_call entries should be
// routed through renderToolGroup via buildRenderUnits, not here.
function renderHistoryEntry(entry) {
    if (entry.entry_type === 'message') {
        addMessage(entry.source, entry.content);
    } else if (entry.entry_type === 'email_event') {
        const meta = parseEntryMetadata(entry.metadata) || {};
        addEmailEntry(entry.content, meta);
    } else if (entry.entry_type === 'tool_call') {
        // Fallback for callers that didn't go through buildRenderUnits.
        renderToolGroup([entry]);
    }
}

async function loadHistory() {
    if (!invoke) return;
    try {
        const entries = await invoke('get_arc_history');
        if (entries && entries.length > 0) {
            arcHasMessages = true;
            // Remove the welcome message since we have history.
            const welcome = messagesEl.querySelector('.welcome-message');
            if (welcome) welcome.remove();

            // Group tool_calls into their dropdowns up-front, then stream
            // the units across idle slices to avoid stalling WebKit on long
            // conversations.
            const units = buildRenderUnits(entries);
            const eagerCount = Math.min(2, units.length);
            for (let i = 0; i < eagerCount; i++) renderRenderUnit(units[i]);

            if (units.length > eagerCount) {
                let idx = eagerCount;
                const appendChunk = () => {
                    // Two units per slice — markdown render dominates cost.
                    const end = Math.min(idx + 2, units.length);
                    for (; idx < end; idx++) renderRenderUnit(units[idx]);
                    if (idx < units.length) scheduleIdle(appendChunk);
                };
                scheduleIdle(appendChunk);
            }
        }
    } catch (err) {
        console.error('Failed to load history:', err);
    }
}

function addEmailEntry(content, meta) {
    const row = document.createElement('div');
    row.className = 'message-row system';
    row.innerHTML = '<div class="email-inline-entry">&#x1f4e7; ' + escapeHtml(content) + '</div>';
    messagesEl.appendChild(row);
}

function addSystemEntry(content, type) {
    const row = document.createElement('div');
    row.className = 'message-row system';
    const icon = type === 'tool' ? '&#128295;' : '&#9881;';
    row.innerHTML = '<div class="system-inline-entry">' + icon + ' ' + escapeHtml(content) + '</div>';
    messagesEl.appendChild(row);
}

// ─── New Arc (both sidebar button and header button) ───

async function newArc() {
    if (!invoke) return;
    try {
        const newId = await invoke('new_arc');
        activeArcId = newId;
        arcHasMessages = false;
        clearChatUI();
        closeSidebar();
        await loadArcs();
        inputEl.focus();
    } catch (err) {
        console.error('Failed to create arc:', err);
    }
}

async function branchFromArc(parentArcId, parentName) {
    if (!invoke) return;
    const branchName = prompt('Name for the new branch:', parentName + ' (branch)');
    if (!branchName) return;
    try {
        const newId = await invoke('branch_arc', { parentArcId, name: branchName });
        activeArcId = newId;
        arcHasMessages = false;
        clearChatUI();
        closeSidebar();
        await loadArcs();
        inputEl.focus();
    } catch (err) {
        console.error('Failed to branch arc:', err);
    }
}

const newChatBtn = document.getElementById('new-chat-btn');
if (newChatBtn) {
    newChatBtn.addEventListener('click', newArc);
}

const sidebarNewChatBtn = document.getElementById('sidebar-new-chat-btn');
if (sidebarNewChatBtn) {
    sidebarNewChatBtn.addEventListener('click', newArc);
}

// ─── Settings ───

const settingsView = document.getElementById('settings-view');
const settingsBtn = document.getElementById('settings-btn');
const settingsBack = document.getElementById('settings-back');
const appView = document.getElementById('app');
const timelineView = document.getElementById('timeline-view');
const providerListEl = document.getElementById('provider-list');
const addProviderBtn = document.getElementById('add-provider-btn');
const providerTemplates = document.getElementById('provider-templates');
const securityModeEl = document.getElementById('security-mode');
const securityHintEl = document.getElementById('security-hint');
const saveSecurityBtn = document.getElementById('save-security-btn');

const PROVIDER_DEFAULTS = {
    deepseek:  { name: 'DeepSeek',        base_url: 'https://api.deepseek.com',  model: 'deepseek-chat',           type: 'cloud' },
    openai:    { name: 'OpenAI',           base_url: 'https://api.openai.com',    model: 'gpt-4o',                 type: 'cloud' },
    anthropic: { name: 'Anthropic',        base_url: 'https://api.anthropic.com', model: 'claude-sonnet-4-20250514', type: 'cloud' },
    ollama:    { name: 'Ollama',           base_url: 'http://localhost:11434',     model: 'llama3',                 type: 'local' },
    llamacpp:  { name: 'llama.cpp',        base_url: 'http://localhost:8080',      model: 'default',                type: 'local' },
    custom:    { name: 'Custom Provider',  base_url: '',                           model: '',                       type: 'cloud' },
};

const SECURITY_HINTS = {
    assistant: 'Standard risk evaluation. The agent asks for approval on risky actions.',
    bunker:    'Maximum caution. Everything above read-only requires your approval.',
    yolo:      'Minimal friction. Only critical actions (financial, system config) need approval.',
};

function showSettings() {
    appView.style.display = 'none';
    timelineView?.classList.add('hidden');
    calendarView?.classList.add('hidden');
    contactsView?.classList.add('hidden');
    notificationsView?.classList.add('hidden');
    document.getElementById('memory-view')?.classList.add('hidden');
    document.getElementById('sidebar').style.display = '';
    if (timelineRefreshInterval) { clearInterval(timelineRefreshInterval); timelineRefreshInterval = null; }
    settingsView.classList.remove('hidden');
    closeSidebar();
    loadSettings();
}

function showChat() {
    settingsView.classList.add('hidden');
    timelineView?.classList.add('hidden');
    calendarView?.classList.add('hidden');
    contactsView?.classList.add('hidden');
    notificationsView?.classList.add('hidden');
    document.getElementById('memory-view')?.classList.add('hidden');
    document.getElementById('sidebar').style.display = '';
    if (timelineRefreshInterval) { clearInterval(timelineRefreshInterval); timelineRefreshInterval = null; }
    appView.style.display = 'flex';
    inputEl.focus();
}

async function loadSettings() {
    if (!invoke) return;
    try {
        const settings = await invoke('get_settings');
        renderProviders(settings.providers);
        securityModeEl.value = settings.security_mode;
        securityHintEl.textContent = SECURITY_HINTS[settings.security_mode] || '';

        // Populate email settings
        if (settings.email) {
            document.getElementById('email-enabled').checked = settings.email.enabled;
            document.getElementById('email-imap-server').value = settings.email.imap_server || '';
            document.getElementById('email-imap-port').value = settings.email.imap_port || 993;
            document.getElementById('email-username').value = settings.email.username || '';
            document.getElementById('email-use-tls').checked = settings.email.use_tls !== false;
            document.getElementById('email-folders').value = settings.email.folders || 'INBOX';
            document.getElementById('email-poll-interval').value = settings.email.poll_interval_secs || 60;
            document.getElementById('email-lookback').value = settings.email.lookback_hours || 24;
            // Don't populate password - show placeholder if has_password
            const pwField = document.getElementById('email-password');
            if (settings.email.has_password) {
                pwField.placeholder = '••••••••  (saved)';
            }
            toggleEmailFields(settings.email.enabled);
        }

        // Populate telegram settings
        if (settings.telegram) {
            document.getElementById('telegram-enabled').checked = settings.telegram.enabled;
            const ownerIdEl = document.getElementById('telegram-owner-id');
            if (settings.telegram.owner_user_id) {
                ownerIdEl.value = settings.telegram.owner_user_id;
            } else {
                ownerIdEl.value = '';
            }
            const chatIdsEl = document.getElementById('telegram-chat-ids');
            chatIdsEl.value = settings.telegram.allowed_chat_ids.length > 0
                ? settings.telegram.allowed_chat_ids.join(', ')
                : '';
            document.getElementById('telegram-poll-interval').value = settings.telegram.poll_interval_secs || 5;

            // Populate token field so the user can test connection again
            const tokenField = document.getElementById('telegram-bot-token');
            if (settings.telegram.bot_token) {
                tokenField.value = settings.telegram.bot_token;
            }
            toggleTelegramFields(settings.telegram.enabled);
        }

        // Populate notification settings
        if (settings.notifications) {
            document.getElementById('notif-escalation-timeout').value = settings.notifications.escalation_timeout_secs || 300;
            document.getElementById('notif-quiet-hours-enabled').checked = settings.notifications.quiet_hours_enabled;
            if (settings.notifications.quiet_hours_enabled) {
                document.getElementById('notif-quiet-start-hour').value = settings.notifications.quiet_start_hour;
                document.getElementById('notif-quiet-start-minute').value = settings.notifications.quiet_start_minute;
                document.getElementById('notif-quiet-end-hour').value = settings.notifications.quiet_end_hour;
                document.getElementById('notif-quiet-end-minute').value = settings.notifications.quiet_end_minute;
                document.getElementById('notif-quiet-allow-critical').checked = settings.notifications.quiet_allow_critical;
                document.getElementById('quiet-hours-fields').style.display = 'block';
            } else {
                document.getElementById('quiet-hours-fields').style.display = 'none';
            }
            renderChannelOrder(settings.notifications.preferred_channels || []);
        }

        // Populate embedding settings
        if (settings.embeddings) {
            const modeEl = document.getElementById('embedding-mode');
            if (modeEl) modeEl.value = settings.embeddings.mode || 'Automatic';
            const providerEl = document.getElementById('embedding-provider');
            if (providerEl && settings.embeddings.provider) providerEl.value = settings.embeddings.provider;
            const modelEl = document.getElementById('embedding-model');
            if (modelEl && settings.embeddings.model) modelEl.value = settings.embeddings.model;
            const urlEl = document.getElementById('embedding-base-url');
            if (urlEl && settings.embeddings.base_url) urlEl.value = settings.embeddings.base_url;
            const keyEl = document.getElementById('embedding-api-key');
            if (keyEl && settings.embeddings.api_key_hint) {
                keyEl.placeholder = settings.embeddings.api_key_hint + '  (saved)';
            }
            const hintEl = document.getElementById('embedding-mode-hint');
            if (hintEl) {
                const hints = {
                    'Automatic': 'Auto-detects the best available provider for generating embeddings.',
                    'Cloud': 'Uses a cloud provider (requires API key) for highest quality embeddings.',
                    'LocalOnly': 'Forces local-only embedding generation. No data leaves your machine.',
                    'Off': 'Disables memory and embeddings entirely.',
                };
                hintEl.textContent = hints[settings.embeddings.mode] || '';
            }
            // If mode is Specific, auto-expand advanced
            if (settings.embeddings.mode === 'Specific') {
                const adv = document.getElementById('embedding-advanced');
                const arrow = document.querySelector('#embedding-advanced-toggle .advanced-arrow');
                if (adv) adv.style.display = 'block';
                if (arrow) arrow.textContent = '\u25BC';
            }
        }
        await loadMcpCatalog();
        await loadGrants();
    } catch (err) {
        console.error('Failed to load settings:', err);
        showToast('Failed to load settings: ' + err, 'error');
    }
}

// ─── MCP catalog ──────────────────────────────────────────────────────

async function loadMcpCatalog() {
    const listEl = document.getElementById('mcp-list');
    if (!listEl) return;
    try {
        const entries = await invoke('list_mcp_catalog');
        renderMcpCatalog(entries);
    } catch (err) {
        console.error('Failed to load MCP catalog:', err);
        listEl.innerHTML = '<p class="setting-hint">Failed to load tools.</p>';
    }
}

function renderMcpCatalog(entries) {
    const listEl = document.getElementById('mcp-list');
    listEl.innerHTML = '';
    if (!entries || entries.length === 0) {
        listEl.innerHTML = '<p class="setting-hint">No tools available.</p>';
        return;
    }
    for (const entry of entries) {
        listEl.appendChild(createMcpCard(entry));
    }
}

function createMcpCard(entry) {
    const card = document.createElement('div');
    card.className = 'mcp-card' + (entry.enabled ? ' enabled' : '');
    card.dataset.mcpId = entry.id;

    const header = document.createElement('div');
    header.className = 'mcp-card-header';

    const titleWrap = document.createElement('div');
    titleWrap.className = 'mcp-card-title';
    if (entry.icon) {
        const icon = document.createElement('span');
        icon.className = 'mcp-card-icon';
        icon.textContent = entry.icon;
        titleWrap.appendChild(icon);
    }
    const nameWrap = document.createElement('div');
    nameWrap.className = 'mcp-card-name-wrap';
    const name = document.createElement('div');
    name.className = 'mcp-card-name';
    name.textContent = entry.display_name;
    const desc = document.createElement('div');
    desc.className = 'mcp-card-desc';
    desc.textContent = entry.description;
    nameWrap.appendChild(name);
    nameWrap.appendChild(desc);
    titleWrap.appendChild(nameWrap);

    const toggle = document.createElement('label');
    toggle.className = 'mcp-toggle';
    const checkbox = document.createElement('input');
    checkbox.type = 'checkbox';
    checkbox.checked = entry.enabled;
    const slider = document.createElement('span');
    slider.className = 'mcp-toggle-slider';
    toggle.appendChild(checkbox);
    toggle.appendChild(slider);

    header.appendChild(titleWrap);
    header.appendChild(toggle);
    card.appendChild(header);

    const body = document.createElement('div');
    body.className = 'mcp-card-body';
    body.style.display = entry.enabled ? 'block' : 'none';

    const fields = renderJsonSchemaFields(entry.config_schema, entry.config || {});
    if (fields) {
        body.appendChild(fields);

        const actions = document.createElement('div');
        actions.className = 'setting-actions';
        const saveBtn = document.createElement('button');
        saveBtn.className = 'btn-primary';
        saveBtn.textContent = 'Save Configuration';
        saveBtn.addEventListener('click', () => handleSaveMcpConfig(card, entry));
        actions.appendChild(saveBtn);
        body.appendChild(actions);
    }
    card.appendChild(body);

    checkbox.addEventListener('change', async (e) => {
        const willEnable = e.target.checked;
        try {
            if (willEnable) {
                const config = readMcpConfigFromCard(card, entry.config_schema);
                await invoke('enable_mcp', { mcpId: entry.id, config });
                card.classList.add('enabled');
                body.style.display = 'block';
                showToast(`${entry.display_name} enabled`, 'success');
            } else {
                await invoke('disable_mcp', { mcpId: entry.id });
                card.classList.remove('enabled');
                body.style.display = 'none';
                showToast(`${entry.display_name} disabled`, 'success');
            }
        } catch (err) {
            console.error('Failed to toggle MCP:', err);
            e.target.checked = !willEnable;
            showToast('Failed: ' + err, 'error');
        }
    });

    return card;
}

function renderJsonSchemaFields(schema, currentValues) {
    if (!schema || schema.type !== 'object' || !schema.properties) return null;
    const props = schema.properties;
    const required = new Set(schema.required || []);
    const keys = Object.keys(props);
    if (keys.length === 0) return null;

    const container = document.createElement('div');
    container.className = 'mcp-config-fields';

    for (const key of keys) {
        const prop = props[key];
        const row = document.createElement('div');
        row.className = 'setting-row';

        const label = document.createElement('label');
        label.textContent = prop.title || key + (required.has(key) ? ' *' : '');
        label.htmlFor = `mcp-field-${key}`;
        row.appendChild(label);

        const input = document.createElement(
            prop.type === 'boolean' ? 'input' :
            (prop.enum ? 'select' : 'input')
        );
        input.id = `mcp-field-${key}`;
        input.dataset.fieldKey = key;
        input.dataset.fieldType = prop.type || 'string';
        input.className = 'settings-input';

        const currentValue = currentValues[key] !== undefined ? currentValues[key] : prop.default;

        if (prop.type === 'boolean') {
            input.type = 'checkbox';
            input.checked = !!currentValue;
        } else if (prop.enum) {
            for (const opt of prop.enum) {
                const o = document.createElement('option');
                o.value = opt;
                o.textContent = opt;
                if (opt === currentValue) o.selected = true;
                input.appendChild(o);
            }
        } else if (prop.type === 'integer' || prop.type === 'number') {
            input.type = 'number';
            if (currentValue !== undefined && currentValue !== null) input.value = currentValue;
            if (prop.minimum !== undefined) input.min = prop.minimum;
            if (prop.maximum !== undefined) input.max = prop.maximum;
        } else {
            input.type = 'text';
            if (currentValue !== undefined && currentValue !== null) input.value = currentValue;
            if (prop.description) input.placeholder = prop.description;
        }

        row.appendChild(input);

        if (prop.description) {
            const hint = document.createElement('p');
            hint.className = 'setting-hint';
            hint.textContent = prop.description;
            row.appendChild(hint);
        }

        container.appendChild(row);
    }

    return container;
}

function readMcpConfigFromCard(card, schema) {
    const config = {};
    if (!schema || !schema.properties) return config;
    const inputs = card.querySelectorAll('[data-field-key]');
    for (const input of inputs) {
        const key = input.dataset.fieldKey;
        const type = input.dataset.fieldType;
        if (type === 'boolean') {
            config[key] = input.checked;
        } else if (type === 'integer') {
            const v = input.value.trim();
            if (v !== '') config[key] = parseInt(v, 10);
        } else if (type === 'number') {
            const v = input.value.trim();
            if (v !== '') config[key] = parseFloat(v);
        } else {
            const v = input.value.trim();
            if (v !== '') config[key] = v;
        }
    }
    return config;
}

async function handleSaveMcpConfig(card, entry) {
    try {
        const config = readMcpConfigFromCard(card, entry.config_schema);
        await invoke('enable_mcp', { mcpId: entry.id, config });
        showToast(`${entry.display_name} configuration saved`, 'success');
    } catch (err) {
        console.error('Failed to save MCP config:', err);
        showToast('Failed: ' + err, 'error');
    }
}

function renderProviders(providers) {
    providerListEl.innerHTML = '';
    for (const p of providers) {
        providerListEl.appendChild(createProviderCard(p));
    }
}

function createProviderCard(provider) {
    const card = document.createElement('div');
    card.className = 'provider-card' + (provider.is_active ? ' active' : '');
    card.dataset.providerId = provider.id;

    const header = document.createElement('div');
    header.className = 'provider-card-header';

    const titleArea = document.createElement('div');
    titleArea.className = 'provider-card-title';

    const dot = document.createElement('span');
    dot.className = 'provider-status-dot ' + (provider.has_api_key || provider.provider_type === 'local' ? 'active' : 'inactive');

    const name = document.createElement('span');
    name.className = 'provider-name';
    name.textContent = provider.name;

    const subtitle = document.createElement('span');
    subtitle.className = 'provider-subtitle';
    subtitle.textContent = provider.model + ' \u00B7 ' + provider.base_url.replace(/^https?:\/\//, '');

    titleArea.appendChild(dot);
    titleArea.appendChild(name);
    titleArea.appendChild(subtitle);
    header.appendChild(titleArea);

    const rightSide = document.createElement('div');
    rightSide.style.display = 'flex';
    rightSide.style.alignItems = 'center';
    rightSide.style.gap = '8px';

    if (provider.is_active) {
        const badge = document.createElement('span');
        badge.className = 'provider-active-badge';
        badge.textContent = 'Active';
        rightSide.appendChild(badge);
    } else {
        const setActiveBtn = document.createElement('button');
        setActiveBtn.className = 'btn-set-active';
        setActiveBtn.textContent = 'Set Active';
        setActiveBtn.addEventListener('click', (e) => {
            e.stopPropagation();
            handleSetActiveProvider(provider.id, provider.name);
        });
        rightSide.appendChild(setActiveBtn);
    }

    const chevron = document.createElement('span');
    chevron.className = 'provider-card-chevron';
    chevron.innerHTML = '&#9654;';
    rightSide.appendChild(chevron);
    header.appendChild(rightSide);

    // Toggle expand
    header.addEventListener('click', () => {
        card.classList.toggle('expanded');
    });

    // Body
    const body = document.createElement('div');
    body.className = 'provider-card-body';

    const isLocal = provider.provider_type === 'local';

    body.innerHTML = `
        <div class="provider-field">
            <label>Base URL</label>
            <input type="text" class="provider-url" value="${escapeHtml(provider.base_url)}" placeholder="https://api.example.com">
        </div>
        <div class="provider-field">
            <label>Model</label>
            <input type="text" class="provider-model" value="${escapeHtml(provider.model)}" placeholder="model-name">
        </div>
        ${!isLocal ? `
        <div class="provider-field">
            <label>API Key</label>
            <div class="api-key-wrapper">
                <input type="password" class="provider-api-key" placeholder="${provider.has_api_key ? 'Key is set (leave blank to keep)' : 'Enter API key'}" autocomplete="off">
                <button class="api-key-toggle" title="Show/hide key">&#128065;</button>
            </div>
            ${provider.api_key_hint ? `<div class="api-key-hint">Current: ${escapeHtml(provider.api_key_hint)}</div>` : ''}
        </div>
        ` : ''}
        <div class="provider-card-actions">
            <button class="btn-secondary test-btn">Test Connection</button>
            <button class="btn-primary save-btn">Save</button>
            <button class="btn-danger delete-btn">Delete</button>
            <span class="test-result"></span>
        </div>
    `;

    card.appendChild(header);
    card.appendChild(body);

    // Wire up events after adding to DOM (we do it immediately since innerHTML is set).
    const apiKeyInput = body.querySelector('.provider-api-key');
    const toggleBtn = body.querySelector('.api-key-toggle');
    if (toggleBtn && apiKeyInput) {
        toggleBtn.addEventListener('click', () => {
            apiKeyInput.type = apiKeyInput.type === 'password' ? 'text' : 'password';
        });
    }

    body.querySelector('.test-btn').addEventListener('click', () => {
        handleTestProvider(card, provider.id);
    });

    body.querySelector('.save-btn').addEventListener('click', () => {
        handleSaveProvider(card, provider.id);
    });

    body.querySelector('.delete-btn').addEventListener('click', () => {
        handleDeleteProvider(provider.id);
    });

    return card;
}

async function handleSaveProvider(card, id) {
    if (!invoke) return;

    const baseUrl = card.querySelector('.provider-url').value.trim();
    const model = card.querySelector('.provider-model').value.trim();
    const apiKeyInput = card.querySelector('.provider-api-key');
    // null means "don't change", empty string means "remove"
    let apiKey = null;
    if (apiKeyInput) {
        const val = apiKeyInput.value;
        if (val !== '') {
            apiKey = val;
        }
    }

    const saveBtn = card.querySelector('.save-btn');
    saveBtn.disabled = true;
    saveBtn.textContent = 'Saving...';

    try {
        const msg = await invoke('save_provider', {
            id: id,
            baseUrl: baseUrl,
            model: model,
            apiKey: apiKey,
        });
        showToast(msg, 'success');
        // Reload to reflect changes.
        await loadSettings();
    } catch (err) {
        showToast('Failed to save: ' + err, 'error');
    }

    saveBtn.disabled = false;
    saveBtn.textContent = 'Save';
}

async function handleDeleteProvider(id) {
    if (!invoke) return;
    if (!confirm('Delete provider "' + id + '"? You can re-add it later.')) return;

    try {
        const msg = await invoke('delete_provider', { id: id });
        showToast(msg, 'success');
        await loadSettings();
    } catch (err) {
        showToast('Failed to delete: ' + err, 'error');
    }
}

async function handleSetActiveProvider(id, name) {
    if (!invoke) return;

    try {
        const msg = await invoke('set_active_provider', { id: id });
        showToast(msg, 'success');
        // Refresh the settings page to update active badges.
        await loadSettings();
    } catch (err) {
        showToast('Failed to switch provider: ' + err, 'error');
    }
}

async function handleTestProvider(card, id) {
    if (!invoke) return;

    const baseUrl = card.querySelector('.provider-url').value.trim();
    const model = card.querySelector('.provider-model').value.trim();
    const apiKeyInput = card.querySelector('.provider-api-key');
    let apiKey = null;
    if (apiKeyInput && apiKeyInput.value !== '') {
        apiKey = apiKeyInput.value;
    }

    const testBtn = card.querySelector('.test-btn');
    const resultEl = card.querySelector('.test-result');
    testBtn.disabled = true;
    testBtn.textContent = 'Testing...';
    resultEl.textContent = '';
    resultEl.className = 'test-result';

    try {
        const result = await invoke('test_provider', {
            id: id,
            baseUrl: baseUrl,
            model: model,
            apiKey: apiKey,
        });
        resultEl.textContent = result.message;
        resultEl.className = 'test-result ' + (result.success ? 'success' : 'error');
    } catch (err) {
        resultEl.textContent = 'Error: ' + err;
        resultEl.className = 'test-result error';
    }

    testBtn.disabled = false;
    testBtn.textContent = 'Test Connection';
}

// Add provider template buttons
if (addProviderBtn) {
    addProviderBtn.addEventListener('click', () => {
        providerTemplates.classList.toggle('hidden');
    });
}

document.querySelectorAll('.template-btn').forEach(btn => {
    btn.addEventListener('click', () => {
        const providerId = btn.dataset.provider;
        const defaults = PROVIDER_DEFAULTS[providerId];
        providerTemplates.classList.add('hidden');

        // Check if this provider already exists in the list.
        const existingCard = providerListEl.querySelector(
            `.provider-card[data-provider-id="${providerId}"]`
        );
        if (existingCard) {
            existingCard.classList.add('expanded');
            existingCard.scrollIntoView({ behavior: 'smooth', block: 'center' });
            return;
        }

        // Create a new card with template defaults.
        const newProvider = {
            id: providerId,
            name: defaults.name,
            provider_type: defaults.type,
            base_url: defaults.base_url,
            model: defaults.model,
            has_api_key: false,
            api_key_hint: '',
            is_active: false,
        };

        const card = createProviderCard(newProvider);
        card.classList.add('expanded');
        providerListEl.appendChild(card);
        card.scrollIntoView({ behavior: 'smooth', block: 'center' });
    });
});

// Security mode change hint
if (securityModeEl) {
    securityModeEl.addEventListener('change', () => {
        securityHintEl.textContent = SECURITY_HINTS[securityModeEl.value] || '';
    });
}

// Save security settings
if (saveSecurityBtn) {
    saveSecurityBtn.addEventListener('click', async () => {
        if (!invoke) return;
        saveSecurityBtn.disabled = true;
        saveSecurityBtn.textContent = 'Saving...';

        try {
            const msg = await invoke('save_settings', {
                securityMode: securityModeEl.value,
            });
            showToast(msg, 'success');
        } catch (err) {
            showToast('Failed to save: ' + err, 'error');
        }

        saveSecurityBtn.disabled = false;
        saveSecurityBtn.textContent = 'Save Security Settings';
    });
}

// Settings navigation
if (settingsBtn) {
    settingsBtn.addEventListener('click', showSettings);
}
if (settingsBack) {
    settingsBack.addEventListener('click', showChat);
}

// ─── Toast Notification ───

function showToast(message, type) {
    // Remove existing toasts.
    document.querySelectorAll('.toast').forEach(t => t.remove());

    const toast = document.createElement('div');
    toast.className = 'toast ' + (type || '');
    toast.textContent = message;
    document.body.appendChild(toast);

    setTimeout(() => {
        toast.style.opacity = '0';
        setTimeout(() => toast.remove(), 300);
    }, 4000);
}

// ─── Notification Toasts ───

function showNotificationToast(data) {
    const toast = document.createElement('div');
    toast.className = 'notification-toast';

    const urgencyClass = 'urgency-' + (data.urgency || 'Medium').toLowerCase();
    toast.classList.add(urgencyClass);

    const icons = { Low: '\u2139\uFE0F', Medium: '\uD83D\uDCEC', High: '\u26A0\uFE0F', Critical: '\uD83D\uDEA8' };
    const icon = icons[data.urgency] || '\uD83D\uDCEC';

    const headerDiv = document.createElement('div');
    headerDiv.className = 'toast-header';

    const iconSpan = document.createElement('span');
    iconSpan.className = 'toast-icon';
    iconSpan.textContent = icon;

    const closeBtn = document.createElement('button');
    closeBtn.className = 'toast-close';
    closeBtn.textContent = '\u00D7';
    closeBtn.addEventListener('click', (e) => {
        e.stopPropagation();
        toast.remove();
    });

    if (data.title) {
        // Structured: title + body
        const titleSpan = document.createElement('span');
        titleSpan.className = 'toast-title';
        titleSpan.textContent = data.title;
        headerDiv.appendChild(iconSpan);
        headerDiv.appendChild(titleSpan);
        headerDiv.appendChild(closeBtn);
        toast.appendChild(headerDiv);

        if (data.body) {
            const bodyDiv = document.createElement('div');
            bodyDiv.className = 'toast-body';
            bodyDiv.textContent = data.body;
            toast.appendChild(bodyDiv);
        }
    } else {
        // Humanized: body only, shown as the main content
        headerDiv.appendChild(iconSpan);
        const msgSpan = document.createElement('span');
        msgSpan.className = 'toast-title';
        msgSpan.textContent = data.body || 'Notification';
        headerDiv.appendChild(msgSpan);
        headerDiv.appendChild(closeBtn);
        toast.appendChild(headerDiv);
    }

    // Click to open the related arc
    if (data.arc_id) {
        toast.style.cursor = 'pointer';
        toast.addEventListener('click', (e) => {
            if (!e.target.classList.contains('toast-close')) {
                handleSwitchArc(data.arc_id);
                toast.remove();
                if (data.id && invoke) {
                    invoke('mark_notification_seen', { id: data.id }).then(() => updateNotifBadge()).catch(() => {});
                }
            }
        });
    }

    // Auto-dismiss after 10s for Low/Medium, stay for High/Critical
    const autoDismiss = !data.urgency || data.urgency === 'Low' || data.urgency === 'Medium';
    if (autoDismiss) {
        setTimeout(() => toast.remove(), 10000);
    }

    // Add to toast container
    let container = document.getElementById('toast-container');
    if (!container) {
        container = document.createElement('div');
        container.id = 'toast-container';
        document.body.appendChild(container);
    }
    container.appendChild(toast);
}

// ─── Email Settings ───

function toggleEmailFields(enabled) {
    const fields = document.getElementById('email-settings-fields');
    if (fields) {
        fields.style.opacity = enabled ? '1' : '0.5';
        fields.style.pointerEvents = enabled ? 'auto' : 'none';
    }
}

document.getElementById('email-enabled')?.addEventListener('change', function() {
    toggleEmailFields(this.checked);
});

document.getElementById('save-email-btn')?.addEventListener('click', async function() {
    const password = document.getElementById('email-password').value;
    try {
        const result = await window.__TAURI__.core.invoke('save_email_settings', {
            enabled: document.getElementById('email-enabled').checked,
            imapServer: document.getElementById('email-imap-server').value,
            imapPort: parseInt(document.getElementById('email-imap-port').value) || 993,
            username: document.getElementById('email-username').value,
            password: password || null,
            useTls: document.getElementById('email-use-tls').checked,
            folders: document.getElementById('email-folders').value,
            pollIntervalSecs: parseInt(document.getElementById('email-poll-interval').value) || 60,
            lookbackHours: parseInt(document.getElementById('email-lookback').value) || 24,
        });
        showEmailTestResult(true, result);
    } catch (e) {
        showEmailTestResult(false, e.toString());
    }
});

document.getElementById('test-email-btn')?.addEventListener('click', async function() {
    const btn = this;
    btn.disabled = true;
    btn.textContent = 'Testing...';
    try {
        const result = await window.__TAURI__.core.invoke('test_email_connection', {
            imapServer: document.getElementById('email-imap-server').value,
            imapPort: parseInt(document.getElementById('email-imap-port').value) || 993,
            username: document.getElementById('email-username').value,
            password: document.getElementById('email-password').value,
            useTls: document.getElementById('email-use-tls').checked,
        });
        showEmailTestResult(result.success, result.message);
    } catch (e) {
        showEmailTestResult(false, e.toString());
    } finally {
        btn.disabled = false;
        btn.textContent = 'Test Connection';
    }
});

function showEmailTestResult(success, message) {
    const el = document.getElementById('email-test-result');
    if (!el) return;
    el.className = 'test-result ' + (success ? 'test-success' : 'test-error');
    el.textContent = message;
    el.classList.remove('hidden');
    setTimeout(() => el.classList.add('hidden'), 5000);
}

// ─── Telegram Settings ───

function toggleTelegramFields(enabled) {
    const fields = document.getElementById('telegram-settings-fields');
    if (fields) {
        fields.style.opacity = enabled ? '1' : '0.5';
        fields.style.pointerEvents = enabled ? 'auto' : 'none';
    }
}

document.getElementById('telegram-enabled')?.addEventListener('change', function() {
    toggleTelegramFields(this.checked);
});

document.getElementById('telegram-token-toggle')?.addEventListener('click', function() {
    const input = document.getElementById('telegram-bot-token');
    if (input) {
        input.type = input.type === 'password' ? 'text' : 'password';
    }
});

document.getElementById('save-telegram-btn')?.addEventListener('click', async function() {
    const token = document.getElementById('telegram-bot-token').value;
    const ownerIdStr = document.getElementById('telegram-owner-id').value;
    const chatIdsStr = document.getElementById('telegram-chat-ids').value;
    const pollInterval = parseInt(document.getElementById('telegram-poll-interval').value);

    const allowedChatIds = chatIdsStr
        ? chatIdsStr.split(',').map(s => parseInt(s.trim())).filter(n => !isNaN(n))
        : [];

    try {
        const result = await window.__TAURI__.core.invoke('save_telegram_settings', {
            enabled: document.getElementById('telegram-enabled').checked,
            botToken: token || null,
            ownerUserId: ownerIdStr ? parseInt(ownerIdStr) : null,
            allowedChatIds: allowedChatIds,
            pollIntervalSecs: !isNaN(pollInterval) ? pollInterval : null,
        });
        showTelegramTestResult(true, result);
    } catch (e) {
        showTelegramTestResult(false, e.toString());
    }
});

document.getElementById('test-telegram-btn')?.addEventListener('click', async function() {
    const btn = this;
    btn.disabled = true;
    btn.textContent = 'Testing...';
    try {
        const result = await window.__TAURI__.core.invoke('test_telegram_connection', {
            botToken: document.getElementById('telegram-bot-token').value,
        });
        showTelegramTestResult(result.success, result.message);
    } catch (e) {
        showTelegramTestResult(false, e.toString());
    } finally {
        btn.disabled = false;
        btn.textContent = 'Test Connection';
    }
});

function showTelegramTestResult(success, message) {
    const el = document.getElementById('telegram-test-result');
    if (!el) return;
    el.className = 'test-result ' + (success ? 'test-success' : 'test-error');
    el.textContent = message;
    el.classList.remove('hidden');
    setTimeout(() => el.classList.add('hidden'), 5000);
}

// ─── Notification Settings ───

function renderChannelOrder(channels) {
    const container = document.getElementById('notif-channel-order');
    if (!container) return;
    container.innerHTML = '';
    const allChannels = ['InApp', 'Telegram'];
    const ordered = channels.length > 0 ? channels : allChannels;

    ordered.forEach((ch, i) => {
        const item = document.createElement('div');
        item.className = 'channel-order-item';

        const nameSpan = document.createElement('span');
        nameSpan.className = 'channel-name';
        nameSpan.textContent = ch === 'InApp' ? '\uD83D\uDDA5\uFE0F In-App' : '\uD83D\uDCF1 Telegram';

        const buttonsDiv = document.createElement('div');
        buttonsDiv.className = 'channel-order-buttons';

        const upBtn = document.createElement('button');
        upBtn.textContent = '\u25B2';
        upBtn.disabled = i === 0;
        upBtn.addEventListener('click', () => moveChannel(i, -1));

        const downBtn = document.createElement('button');
        downBtn.textContent = '\u25BC';
        downBtn.disabled = i === ordered.length - 1;
        downBtn.addEventListener('click', () => moveChannel(i, 1));

        buttonsDiv.appendChild(upBtn);
        buttonsDiv.appendChild(downBtn);
        item.appendChild(nameSpan);
        item.appendChild(buttonsDiv);
        container.appendChild(item);
    });

    // Store the current order on the container for retrieval
    container.dataset.order = JSON.stringify(ordered);
}

function moveChannel(index, direction) {
    const container = document.getElementById('notif-channel-order');
    if (!container) return;
    const order = JSON.parse(container.dataset.order || '["InApp","Telegram"]');
    const newIndex = index + direction;
    if (newIndex < 0 || newIndex >= order.length) return;
    const temp = order[index];
    order[index] = order[newIndex];
    order[newIndex] = temp;
    renderChannelOrder(order);
}

function getChannelOrder() {
    const container = document.getElementById('notif-channel-order');
    if (!container || !container.dataset.order) return ['InApp', 'Telegram'];
    return JSON.parse(container.dataset.order);
}

document.getElementById('notif-quiet-hours-enabled')?.addEventListener('change', function() {
    document.getElementById('quiet-hours-fields').style.display = this.checked ? 'block' : 'none';
});

document.getElementById('save-notif-btn')?.addEventListener('click', async function() {
    try {
        const channels = getChannelOrder();
        const quietEnabled = document.getElementById('notif-quiet-hours-enabled').checked;

        await window.__TAURI__.core.invoke('save_notification_settings', {
            preferredChannels: channels,
            escalationTimeoutSecs: parseInt(document.getElementById('notif-escalation-timeout').value) || 300,
            quietHoursEnabled: quietEnabled,
            quietStartHour: quietEnabled ? parseInt(document.getElementById('notif-quiet-start-hour').value) || 22 : null,
            quietStartMinute: quietEnabled ? parseInt(document.getElementById('notif-quiet-start-minute').value) || 0 : null,
            quietEndHour: quietEnabled ? parseInt(document.getElementById('notif-quiet-end-hour').value) || 8 : null,
            quietEndMinute: quietEnabled ? parseInt(document.getElementById('notif-quiet-end-minute').value) || 0 : null,
            quietAllowCritical: quietEnabled ? document.getElementById('notif-quiet-allow-critical').checked : null,
        });

        showToast('Notification settings saved', 'success');
    } catch (e) {
        console.error('Failed to save notification settings:', e);
        showToast('Failed to save notification settings: ' + e, 'error');
    }
});

// ─── Embedding Settings ───

const EMBEDDING_MODE_HINTS = {
    'Automatic': 'Auto-detects the best available provider for generating embeddings.',
    'Cloud': 'Uses a cloud provider (requires API key) for highest quality embeddings.',
    'LocalOnly': 'Forces local-only embedding generation. No data leaves your machine.',
    'Off': 'Disables memory and embeddings entirely.',
};

document.getElementById('embedding-mode')?.addEventListener('change', function() {
    const hint = document.getElementById('embedding-mode-hint');
    if (hint) hint.textContent = EMBEDDING_MODE_HINTS[this.value] || '';
});

document.getElementById('embedding-advanced-toggle')?.addEventListener('click', function() {
    const adv = document.getElementById('embedding-advanced');
    const arrow = this.querySelector('.advanced-arrow');
    if (!adv) return;
    if (adv.style.display === 'none') {
        adv.style.display = 'block';
        if (arrow) arrow.textContent = '\u25BC';
    } else {
        adv.style.display = 'none';
        if (arrow) arrow.textContent = '\u25B6';
    }
});

document.getElementById('embedding-key-toggle')?.addEventListener('click', function() {
    const input = document.getElementById('embedding-api-key');
    if (input) {
        input.type = input.type === 'password' ? 'text' : 'password';
    }
});

document.getElementById('save-embedding-btn')?.addEventListener('click', async function() {
    const btn = this;
    btn.disabled = true;
    btn.textContent = 'Saving...';

    const advVisible = document.getElementById('embedding-advanced')?.style.display !== 'none';
    const provider = document.getElementById('embedding-provider')?.value || null;
    const model = document.getElementById('embedding-model')?.value || null;
    const baseUrl = document.getElementById('embedding-base-url')?.value || null;
    const apiKey = document.getElementById('embedding-api-key')?.value || null;

    // If advanced is expanded and a specific provider is chosen, use Specific mode.
    let mode = document.getElementById('embedding-mode')?.value || 'Automatic';
    if (advVisible && provider) {
        mode = 'Specific';
    }

    try {
        const result = await window.__TAURI__.core.invoke('save_embedding_settings', {
            mode: mode,
            provider: provider || null,
            model: model || null,
            baseUrl: baseUrl || null,
            apiKey: apiKey || null,
        });
        showToast(result, 'success');
    } catch (e) {
        showToast('Failed to save embedding settings: ' + e, 'error');
    } finally {
        btn.disabled = false;
        btn.textContent = 'Save Embedding Settings';
    }
});

document.getElementById('test-embedding-btn')?.addEventListener('click', async function() {
    const btn = this;
    btn.disabled = true;
    btn.textContent = 'Testing...';
    try {
        const result = await window.__TAURI__.core.invoke('test_embedding_provider', {
            provider: document.getElementById('embedding-provider')?.value || 'ollama',
            model: document.getElementById('embedding-model')?.value || null,
            baseUrl: document.getElementById('embedding-base-url')?.value || null,
            apiKey: document.getElementById('embedding-api-key')?.value || null,
        });
        showEmbeddingTestResult(result.success, result.message);
    } catch (e) {
        showEmbeddingTestResult(false, e.toString());
    } finally {
        btn.disabled = false;
        btn.textContent = 'Test Connection';
    }
});

function showEmbeddingTestResult(success, message) {
    const el = document.getElementById('embedding-test-result');
    if (!el) return;
    el.className = 'test-result ' + (success ? 'test-success' : 'test-error');
    el.textContent = message;
    el.classList.remove('hidden');
    setTimeout(() => el.classList.add('hidden'), 5000);
}

// ─── Arc Timeline ───

let timelineRefreshInterval = null;

const timelineToggleBtn = document.getElementById('timeline-toggle-btn');
const timelineBackBtn = document.getElementById('timeline-back');

function showTimeline() {
    appView.style.display = 'none';
    settingsView.classList.add('hidden');
    calendarView?.classList.add('hidden');
    contactsView?.classList.add('hidden');
    notificationsView?.classList.add('hidden');
    document.getElementById('memory-view')?.classList.add('hidden');
    document.getElementById('sidebar').style.display = 'none';
    timelineView.classList.remove('hidden');
    renderTimeline();
    // Auto-refresh every 30s
    timelineRefreshInterval = setInterval(renderTimeline, 30000);
}

function hideTimeline() {
    timelineView.classList.add('hidden');
    document.getElementById('sidebar').style.display = '';
    appView.style.display = 'flex';
    if (timelineRefreshInterval) {
        clearInterval(timelineRefreshInterval);
        timelineRefreshInterval = null;
    }
    inputEl.focus();
}

if (timelineToggleBtn) {
    timelineToggleBtn.addEventListener('click', showTimeline);
}

if (timelineBackBtn) {
    timelineBackBtn.addEventListener('click', hideTimeline);
}

document.getElementById('timeline-new-arc')?.addEventListener('click', async () => {
    hideTimeline();
    if (typeof newArc === 'function') await newArc();
});

async function renderTimeline() {
    const canvas = document.getElementById('timeline-canvas');
    if (!canvas || !invoke) return;

    try {
        const timelineArcs = await invoke('get_timeline_data');
        if (!timelineArcs || timelineArcs.length === 0) {
            canvas.innerHTML = '<div class="tl-empty">No arcs yet. Start a conversation to see the timeline.</div>';
            return;
        }

        // Sort arcs by most recently updated (rightmost = most recent)
        const sorted = [...timelineArcs].sort((a, b) =>
            new Date(a.updated_at) - new Date(b.updated_at)
        );

        // Collect ALL entries across all arcs with their arc index
        let allEntries = [];
        sorted.forEach((arc, colIdx) => {
            (arc.entries || []).forEach(entry => {
                allEntries.push({ ...entry, arcIdx: colIdx, arcId: arc.id });
            });
        });

        // Sort entries by created_at descending (newest first)
        allEntries.sort((a, b) => new Date(b.created_at) - new Date(a.created_at));

        // Build time slots — group entries by time proximity (within 2 minutes = same row)
        const timeSlots = [];
        allEntries.forEach(entry => {
            const entryTime = new Date(entry.created_at).getTime();
            const existing = timeSlots.find(slot =>
                Math.abs(slot.time - entryTime) < 120000 // 2 min window
            );
            if (existing) {
                existing.entries.push(entry);
            } else {
                timeSlots.push({ time: entryTime, entries: [entry] });
            }
        });

        // Sort time slots newest first
        timeSlots.sort((a, b) => b.time - a.time);

        const numCols = sorted.length;

        // Build HTML
        let html = '<div class="tl-graph">';

        // Arc headers (column headers)
        html += '<div class="tl-header-row">';
        html += '<div class="tl-time-label"></div>'; // empty corner
        sorted.forEach((arc, i) => {
            const color = getTlColor(i);
            const icon = getTlSourceIcon(arc.source);
            const statusCls = arc.status === 'Merged' ? ' tl-merged' : arc.status === 'Archived' ? ' tl-archived' : '';
            html += '<div class="tl-col-header' + statusCls + '" style="border-bottom-color: ' + color + '" data-arc-id="' + arc.id + '" title="Click to open">';
            html += '<span class="tl-col-icon">' + icon + '</span>';
            html += '<span class="tl-col-name">' + escapeHtml(arc.name) + '</span>';
            html += '<span class="tl-col-count">' + arc.entry_count + '</span>';
            html += '</div>';
        });
        html += '</div>';

        // Time rows
        timeSlots.forEach(slot => {
            const timeStr = formatTimelineTime(slot.time);
            html += '<div class="tl-row">';
            html += '<div class="tl-time-label">' + timeStr + '</div>';

            // One cell per arc column
            for (let col = 0; col < numCols; col++) {
                const entriesInCol = slot.entries.filter(e => e.arcIdx === col);
                const color = getTlColor(col);

                html += '<div class="tl-cell">';
                if (entriesInCol.length > 0) {
                    entriesInCol.forEach(entry => {
                        const nodeColor = getTlEntryColor(entry.entry_type);
                        const typeIcon = getTlEntryIcon(entry.entry_type);
                        const preview = entry.content.length > 120
                            ? entry.content.substring(0, 120) + '...'
                            : entry.content;
                        const tooltipTime = new Date(entry.created_at).toLocaleString();

                        html += '<div class="tl-node" style="background: ' + nodeColor + '" ';
                        html += 'data-tooltip="' + escapeAttr(typeIcon + ' ' + entry.source + '\n' + preview + '\n' + tooltipTime) + '">';
                        html += '</div>';
                    });
                }
                // Vertical rail line (always present for active arcs)
                html += '<div class="tl-rail" style="background: ' + color + '"></div>';
                html += '</div>';
            }
            html += '</div>';
        });

        // If no entries at all but arcs exist, show just headers
        if (timeSlots.length === 0) {
            html += '<div class="tl-row"><div class="tl-time-label">-</div>';
            for (let col = 0; col < numCols; col++) {
                html += '<div class="tl-cell"><div class="tl-rail" style="background: ' + getTlColor(col) + '"></div></div>';
            }
            html += '</div>';
        }

        html += '</div>';
        canvas.innerHTML = html;

        // Event listeners
        canvas.querySelectorAll('.tl-col-header').forEach(header => {
            header.addEventListener('click', () => {
                const arcId = header.dataset.arcId;
                hideTimeline();
                if (typeof handleSwitchArc === 'function') handleSwitchArc(arcId);
            });
        });

        // Tooltip handling via mouseover
        canvas.querySelectorAll('.tl-node').forEach(node => {
            node.addEventListener('mouseenter', (e) => showTlTooltip(e, node.dataset.tooltip));
            node.addEventListener('mouseleave', hideTlTooltip);
        });

    } catch (e) {
        canvas.innerHTML = '<div class="tl-empty">Failed to load timeline: ' + escapeHtml(e.toString()) + '</div>';
    }
}

function showTlTooltip(event, text) {
    let tooltip = document.getElementById('tl-tooltip');
    if (!tooltip) {
        tooltip = document.createElement('div');
        tooltip.id = 'tl-tooltip';
        tooltip.className = 'tl-tooltip';
        document.body.appendChild(tooltip);
    }
    tooltip.textContent = text;
    tooltip.style.display = 'block';

    // Position near the node
    const rect = event.target.getBoundingClientRect();
    tooltip.style.left = (rect.right + 8) + 'px';
    tooltip.style.top = (rect.top - 10) + 'px';

    // Keep on screen
    const tooltipRect = tooltip.getBoundingClientRect();
    if (tooltipRect.right > window.innerWidth - 10) {
        tooltip.style.left = (rect.left - tooltipRect.width - 8) + 'px';
    }
    if (tooltipRect.bottom > window.innerHeight - 10) {
        tooltip.style.top = (window.innerHeight - tooltipRect.height - 10) + 'px';
    }
}

function hideTlTooltip() {
    const tooltip = document.getElementById('tl-tooltip');
    if (tooltip) tooltip.style.display = 'none';
}

function escapeAttr(s) {
    return s.replace(/&/g, '&amp;').replace(/"/g, '&quot;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
}

function formatTimelineTime(timestamp) {
    const now = Date.now();
    const diff = now - timestamp;
    const secs = Math.floor(diff / 1000);
    if (secs < 60) return 'Now';
    const mins = Math.floor(secs / 60);
    if (mins < 60) return mins + 'm ago';
    const hours = Math.floor(mins / 60);
    if (hours < 24) return hours + 'h ago';
    const days = Math.floor(hours / 24);
    if (days === 1) return 'Yesterday';
    if (days < 7) return days + 'd ago';
    return new Date(timestamp).toLocaleDateString();
}

function getTlColor(idx) {
    const colors = ['#7aa2f7', '#9ece6a', '#e0af68', '#f7768e', '#bb9af7', '#7dcfff', '#ff9e64', '#c0caf5'];
    return colors[idx % colors.length];
}

function getTlSourceIcon(source) {
    switch (source) {
        case 'Email': return '\u{1f4e7}';
        case 'Calendar': return '\u{1f4c5}';
        case 'Messaging': return '\u{1f4ac}';
        case 'System': return '\u2699\ufe0f';
        default: return '\u{1f4ac}';
    }
}

function getTlEntryColor(type) {
    switch (type) {
        case 'message': return '#7aa2f7';
        case 'tool_call': return '#e0af68';
        case 'email_event': return '#bb9af7';
        case 'calendar_event': return '#9ece6a';
        case 'system_event': return '#565f89';
        default: return '#7aa2f7';
    }
}

function getTlEntryIcon(type) {
    switch (type) {
        case 'message': return '\u{1f4ac}';
        case 'tool_call': return '\u{1f527}';
        case 'email_event': return '\u{1f4e7}';
        case 'calendar_event': return '\u{1f4c5}';
        case 'system_event': return '\u2699\ufe0f';
        default: return '\u25cf';
    }
}

// ─── Calendar ───

let calCurrentDate = new Date();
let calViewMode = 'month';
let calEvents = [];

const calendarView = document.getElementById('calendar-view');
const calendarBtn = document.getElementById('calendar-btn');
const calendarBack = document.getElementById('calendar-back');
const calTitle = document.getElementById('cal-title');
const calGrid = document.getElementById('calendar-grid');
const calViewSelect = document.getElementById('cal-view-select');
const calModalOverlay = document.getElementById('cal-modal-overlay');

const CATEGORY_COLORS = {
    meeting: '#7aa2f7',
    birthday: '#bb9af7',
    deadline: '#f7768e',
    reminder: '#e0af68',
    personal: '#9ece6a',
    work: '#73daca',
    other: '#ff9e64',
};

const MONTH_NAMES = [
    'January', 'February', 'March', 'April', 'May', 'June',
    'July', 'August', 'September', 'October', 'November', 'December'
];

const DAY_NAMES = ['Mon', 'Tue', 'Wed', 'Thu', 'Fri', 'Sat', 'Sun'];

function showCalendar() {
    appView.style.display = 'none';
    settingsView.classList.add('hidden');
    timelineView?.classList.add('hidden');
    contactsView?.classList.add('hidden');
    notificationsView?.classList.add('hidden');
    document.getElementById('memory-view')?.classList.add('hidden');
    document.getElementById('sidebar').style.display = '';
    if (timelineRefreshInterval) { clearInterval(timelineRefreshInterval); timelineRefreshInterval = null; }
    calendarView.classList.remove('hidden');
    closeSidebar();
    loadCalendarEvents();
}

function hideCalendar() {
    calendarView.classList.add('hidden');
    document.getElementById('sidebar').style.display = '';
    appView.style.display = 'flex';
    inputEl.focus();
}

function updateCalTitle() {
    if (calViewMode === 'month') {
        calTitle.textContent = MONTH_NAMES[calCurrentDate.getMonth()] + ' ' + calCurrentDate.getFullYear();
    } else {
        const start = getWeekStart(calCurrentDate);
        const end = new Date(start);
        end.setDate(end.getDate() + 6);
        const fmt = (d) => d.getDate() + ' ' + MONTH_NAMES[d.getMonth()].substring(0, 3);
        calTitle.textContent = fmt(start) + ' - ' + fmt(end) + ' ' + end.getFullYear();
    }
}

function getWeekStart(date) {
    const d = new Date(date);
    const day = d.getDay();
    const diff = (day === 0 ? -6 : 1) - day; // Monday start
    d.setDate(d.getDate() + diff);
    d.setHours(0, 0, 0, 0);
    return d;
}

async function loadCalendarEvents() {
    if (!invoke) {
        renderCalendar();
        return;
    }

    let start, end;
    if (calViewMode === 'month') {
        start = new Date(calCurrentDate.getFullYear(), calCurrentDate.getMonth(), 1);
        start.setDate(start.getDate() - 7); // include prev month overlap
        end = new Date(calCurrentDate.getFullYear(), calCurrentDate.getMonth() + 1, 0);
        end.setDate(end.getDate() + 7); // include next month overlap
    } else {
        start = getWeekStart(calCurrentDate);
        end = new Date(start);
        end.setDate(end.getDate() + 7);
    }

    try {
        calEvents = await invoke('list_calendar_events', {
            start: start.toISOString(),
            end: end.toISOString(),
        });
    } catch (err) {
        console.error('Failed to load calendar events:', err);
        calEvents = [];
    }
    renderCalendar();
}

function renderCalendar() {
    updateCalTitle();
    if (calViewMode === 'month') {
        renderMonthView();
    } else {
        renderWeekView();
    }
}

function getEventsForDate(year, month, day) {
    return calEvents.filter(ev => {
        const d = new Date(ev.start_time || ev.start || ev.date);
        return d.getFullYear() === year && d.getMonth() === month && d.getDate() === day;
    });
}

function getEventColor(ev) {
    if (ev.color) return ev.color;
    return CATEGORY_COLORS[ev.category] || CATEGORY_COLORS.other;
}

function formatDateStr(year, month, day) {
    return year + '-' + String(month + 1).padStart(2, '0') + '-' + String(day).padStart(2, '0');
}

function renderMonthView() {
    const year = calCurrentDate.getFullYear();
    const month = calCurrentDate.getMonth();
    const firstDay = new Date(year, month, 1);
    const lastDay = new Date(year, month + 1, 0);
    const today = new Date();
    today.setHours(0, 0, 0, 0);

    // Day of week for the 1st (0=Sun). Adjust for Monday start.
    let startDow = firstDay.getDay();
    startDow = startDow === 0 ? 6 : startDow - 1;

    const totalDays = lastDay.getDate();

    let html = '<div class="cal-month-grid">';

    // Weekday headers
    for (const name of DAY_NAMES) {
        html += '<div class="cal-weekday-header">' + name + '</div>';
    }

    // Previous month fill
    const prevMonthLast = new Date(year, month, 0).getDate();
    for (let i = startDow - 1; i >= 0; i--) {
        const d = prevMonthLast - i;
        const pm = month === 0 ? 11 : month - 1;
        const py = month === 0 ? year - 1 : year;
        const events = getEventsForDate(py, pm, d);
        html += buildDayCell(py, pm, d, events, true, false);
    }

    // Current month days
    for (let d = 1; d <= totalDays; d++) {
        const isToday = year === today.getFullYear() && month === today.getMonth() && d === today.getDate();
        const events = getEventsForDate(year, month, d);
        html += buildDayCell(year, month, d, events, false, isToday);
    }

    // Next month fill to complete grid
    const cellsRendered = startDow + totalDays;
    const remaining = (Math.ceil(cellsRendered / 7) * 7) - cellsRendered;
    for (let d = 1; d <= remaining; d++) {
        const nm = month === 11 ? 0 : month + 1;
        const ny = month === 11 ? year + 1 : year;
        const events = getEventsForDate(ny, nm, d);
        html += buildDayCell(ny, nm, d, events, true, false);
    }

    html += '</div>';
    calGrid.innerHTML = html;

    // Attach click handlers
    calGrid.querySelectorAll('.cal-day').forEach(cell => {
        cell.addEventListener('click', (e) => {
            if (e.target.closest('.cal-event-item') || e.target.closest('.cal-event-more')) return;
            const dateStr = cell.dataset.date;
            if (dateStr) showEventModal(null, dateStr);
        });
    });

    calGrid.querySelectorAll('.cal-event-item').forEach(pill => {
        pill.addEventListener('click', (e) => {
            e.stopPropagation();
            const evId = pill.dataset.eventId;
            const ev = calEvents.find(ev => ev.id === evId);
            if (ev) showEventModal(ev);
        });
    });
}

function buildDayCell(year, month, day, events, isOther, isToday) {
    const classes = ['cal-day'];
    if (isOther) classes.push('other-month');
    if (isToday) classes.push('today');
    const dateStr = formatDateStr(year, month, day);

    let html = '<div class="' + classes.join(' ') + '" data-date="' + dateStr + '">';
    html += '<div class="cal-day-number">' + day + '</div>';
    html += '<div class="cal-day-events">';

    const maxShow = 3;
    const shown = events.slice(0, maxShow);
    for (const ev of shown) {
        const color = getEventColor(ev);
        const title = escapeHtml(ev.title || 'Untitled');
        html += '<div class="cal-event-item" data-event-id="' + ev.id + '" style="background:' + color + '">' + title + '</div>';
    }
    if (events.length > maxShow) {
        html += '<div class="cal-event-more">+' + (events.length - maxShow) + ' more</div>';
    }
    html += '</div></div>';
    return html;
}

function renderWeekView() {
    const weekStart = getWeekStart(calCurrentDate);
    const today = new Date();
    today.setHours(0, 0, 0, 0);

    let html = '<div class="cal-week-grid">';

    // Header row
    html += '<div class="cal-week-header-cell"></div>'; // time column corner
    for (let i = 0; i < 7; i++) {
        const d = new Date(weekStart);
        d.setDate(d.getDate() + i);
        const isToday = d.getTime() === today.getTime();
        html += '<div class="cal-week-header-cell' + (isToday ? ' today' : '') + '">';
        html += '<span>' + DAY_NAMES[i] + '</span>';
        html += '<span class="cal-week-day-num">' + d.getDate() + '</span>';
        html += '</div>';
    }

    // All-day row
    html += '<div class="cal-week-allday-label">all-day</div>';
    for (let i = 0; i < 7; i++) {
        const d = new Date(weekStart);
        d.setDate(d.getDate() + i);
        const dayEvents = getEventsForDate(d.getFullYear(), d.getMonth(), d.getDate())
            .filter(ev => ev.all_day);
        html += '<div class="cal-week-allday-cell">';
        for (const ev of dayEvents) {
            const color = getEventColor(ev);
            html += '<div class="cal-event-item" data-event-id="' + ev.id + '" style="background:' + color + '">' + escapeHtml(ev.title || 'Untitled') + '</div>';
        }
        html += '</div>';
    }

    // Hour rows (0-23)
    for (let h = 0; h < 24; h++) {
        const label = String(h).padStart(2, '0') + ':00';
        html += '<div class="cal-week-time-label">' + label + '</div>';
        for (let i = 0; i < 7; i++) {
            const d = new Date(weekStart);
            d.setDate(d.getDate() + i);
            const dateStr = formatDateStr(d.getFullYear(), d.getMonth(), d.getDate());
            html += '<div class="cal-week-day-col" data-date="' + dateStr + '" data-hour="' + h + '"></div>';
        }
    }

    html += '</div>';
    calGrid.innerHTML = html;

    // Place timed events as positioned blocks
    const weekDays = [];
    for (let i = 0; i < 7; i++) {
        const d = new Date(weekStart);
        d.setDate(d.getDate() + i);
        weekDays.push(d);
    }

    // Get all day columns for positioning
    const dayCols = calGrid.querySelectorAll('.cal-week-day-col');
    // dayCols is a flat list: 24 rows * 7 columns = 168 elements
    // Index formula: row * 7 + col

    calEvents.forEach(ev => {
        if (ev.all_day) return;
        const evStart = new Date(ev.start_time || ev.start || ev.date);
        const evEnd = (ev.end_time || ev.end) ? new Date(ev.end_time || ev.end) : new Date(evStart.getTime() + 3600000);

        // Find which day column
        const evDate = new Date(evStart);
        evDate.setHours(0, 0, 0, 0);
        const colIdx = weekDays.findIndex(d => d.getTime() === evDate.getTime());
        if (colIdx < 0) return;

        const startHour = evStart.getHours() + evStart.getMinutes() / 60;
        const endHour = evEnd.getHours() + evEnd.getMinutes() / 60;
        const duration = Math.max(endHour - startHour, 0.5);

        const hourRow = Math.floor(startHour);
        const cellIdx = hourRow * 7 + colIdx;
        const targetCell = dayCols[cellIdx];
        if (!targetCell) return;

        const topOffset = (startHour - hourRow) * 48; // 48px per hour row
        const height = duration * 48;

        const color = getEventColor(ev);
        const block = document.createElement('div');
        block.className = 'cal-week-event';
        block.style.background = color;
        block.style.top = topOffset + 'px';
        block.style.height = Math.max(height, 20) + 'px';
        block.dataset.eventId = ev.id;
        block.textContent = ev.title || 'Untitled';

        block.addEventListener('click', (e) => {
            e.stopPropagation();
            showEventModal(ev);
        });

        targetCell.appendChild(block);
    });

    // Click on empty cell to create event
    calGrid.querySelectorAll('.cal-week-day-col').forEach(cell => {
        cell.addEventListener('click', (e) => {
            if (e.target.closest('.cal-week-event')) return;
            const dateStr = cell.dataset.date;
            const hour = cell.dataset.hour;
            showEventModal(null, dateStr, hour);
        });
    });

    // Click on all-day event
    calGrid.querySelectorAll('.cal-week-allday-cell .cal-event-item').forEach(pill => {
        pill.addEventListener('click', (e) => {
            e.stopPropagation();
            const evId = pill.dataset.eventId;
            const ev = calEvents.find(ev => ev.id === evId);
            if (ev) showEventModal(ev);
        });
    });
}

// ─── Event Modal ───

function showEventModal(event, defaultDate, defaultHour) {
    const modal = calModalOverlay;
    const titleEl = document.getElementById('cal-modal-title');
    const deleteBtn = document.getElementById('cal-modal-delete');

    // Reset form
    document.getElementById('cal-event-id').value = '';
    document.getElementById('cal-event-title').value = '';
    document.getElementById('cal-event-date').value = '';
    document.getElementById('cal-event-start').value = '09:00';
    document.getElementById('cal-event-end').value = '10:00';
    document.getElementById('cal-event-allday').checked = false;
    document.getElementById('cal-event-location').value = '';
    document.getElementById('cal-event-description').value = '';
    document.getElementById('cal-event-category').value = 'meeting';
    document.getElementById('cal-event-reminder').value = '15';
    document.getElementById('cal-event-recurrence').value = 'none';
    document.getElementById('cal-time-fields').classList.remove('hidden');

    // Reset color picker
    calModalOverlay.querySelectorAll('.cal-color-dot').forEach(d => d.classList.remove('selected'));
    const defaultColorDot = calModalOverlay.querySelector('.cal-color-dot[data-color="#7aa2f7"]');
    if (defaultColorDot) defaultColorDot.classList.add('selected');

    if (event) {
        // Edit mode
        titleEl.textContent = 'Edit Event';
        deleteBtn.classList.remove('hidden');
        document.getElementById('cal-event-id').value = event.id;
        document.getElementById('cal-event-title').value = event.title || '';
        document.getElementById('cal-event-location').value = event.location || '';
        document.getElementById('cal-event-description').value = event.description || '';
        document.getElementById('cal-event-category').value = event.category || 'other';
        // reminder_minutes is an array like [15], pick first or 'none'
        const reminderVal = (event.reminder_minutes && event.reminder_minutes.length > 0)
            ? String(event.reminder_minutes[0]) : 'none';
        document.getElementById('cal-event-reminder').value = reminderVal;
        document.getElementById('cal-event-recurrence').value = event.recurrence || 'none';

        const evStart = new Date(event.start_time || event.start || event.date);
        document.getElementById('cal-event-date').value = formatDateStr(evStart.getFullYear(), evStart.getMonth(), evStart.getDate());

        if (event.all_day) {
            document.getElementById('cal-event-allday').checked = true;
            document.getElementById('cal-time-fields').classList.add('hidden');
        } else {
            document.getElementById('cal-event-start').value = String(evStart.getHours()).padStart(2, '0') + ':' + String(evStart.getMinutes()).padStart(2, '0');
            const endStr = event.end_time || event.end;
            if (endStr) {
                const evEnd = new Date(endStr);
                document.getElementById('cal-event-end').value = String(evEnd.getHours()).padStart(2, '0') + ':' + String(evEnd.getMinutes()).padStart(2, '0');
            }
        }

        // Set color
        const color = getEventColor(event);
        calModalOverlay.querySelectorAll('.cal-color-dot').forEach(d => d.classList.remove('selected'));
        const matchDot = calModalOverlay.querySelector('.cal-color-dot[data-color="' + color + '"]');
        if (matchDot) matchDot.classList.add('selected');

        // Set category color as default if color matches
        const catColor = CATEGORY_COLORS[event.category];
        if (!event.color && catColor) {
            const catDot = calModalOverlay.querySelector('.cal-color-dot[data-color="' + catColor + '"]');
            if (catDot) {
                calModalOverlay.querySelectorAll('.cal-color-dot').forEach(d => d.classList.remove('selected'));
                catDot.classList.add('selected');
            }
        }
    } else {
        // Create mode
        titleEl.textContent = 'New Event';
        deleteBtn.classList.add('hidden');

        if (defaultDate) {
            document.getElementById('cal-event-date').value = defaultDate;
        } else {
            const now = new Date();
            document.getElementById('cal-event-date').value = formatDateStr(now.getFullYear(), now.getMonth(), now.getDate());
        }

        if (defaultHour !== undefined) {
            const h = parseInt(defaultHour, 10);
            document.getElementById('cal-event-start').value = String(h).padStart(2, '0') + ':00';
            document.getElementById('cal-event-end').value = String(h + 1).padStart(2, '0') + ':00';
        }
    }

    modal.classList.remove('hidden');
    document.getElementById('cal-event-title').focus();
}

function hideEventModal() {
    calModalOverlay.classList.add('hidden');
}

function getSelectedColor() {
    const sel = calModalOverlay.querySelector('.cal-color-dot.selected');
    return sel ? sel.dataset.color : '#7aa2f7';
}

async function saveCalendarEvent() {
    const id = document.getElementById('cal-event-id').value;
    const title = document.getElementById('cal-event-title').value.trim();
    if (!title) {
        document.getElementById('cal-event-title').focus();
        return;
    }

    const dateStr = document.getElementById('cal-event-date').value;
    if (!dateStr) {
        document.getElementById('cal-event-date').focus();
        return;
    }
    const allDay = document.getElementById('cal-event-allday').checked;
    const startTime = document.getElementById('cal-event-start').value || '09:00';
    const endTime = document.getElementById('cal-event-end').value || '10:00';
    const location = document.getElementById('cal-event-location').value.trim();
    const description = document.getElementById('cal-event-description').value.trim();
    const category = document.getElementById('cal-event-category').value;
    const color = getSelectedColor();
    const reminder = document.getElementById('cal-event-reminder').value;
    const recurrence = document.getElementById('cal-event-recurrence').value;

    let start, end;
    if (allDay) {
        start = dateStr + 'T00:00:00';
        end = dateStr + 'T23:59:59';
    } else {
        start = dateStr + 'T' + startTime + ':00';
        end = dateStr + 'T' + endTime + ':00';
    }

    // Convert local times to ISO (with timezone offset)
    const startDate = new Date(start);
    const endDate = new Date(end);

    const now = new Date().toISOString();
    const reminderMinutes = (reminder === 'none' || !reminder) ? [] : [parseInt(reminder, 10)];

    const eventData = {
        id: id || crypto.randomUUID(),
        title,
        description: description || null,
        start_time: startDate.toISOString(),
        end_time: endDate.toISOString(),
        all_day: allDay,
        location: location || null,
        recurrence: recurrence === 'none' ? null : recurrence,
        reminder_minutes: reminderMinutes,
        color: color || null,
        category: category || null,
        created_by: 'User',
        arc_id: null,
        created_at: now,
        updated_at: now,
    };

    if (!invoke) {
        // Offline / demo mode: manage locally
        if (id) {
            calEvents = calEvents.map(ev => ev.id === id ? eventData : ev);
        } else {
            calEvents.push(eventData);
        }
        hideEventModal();
        renderCalendar();
        return;
    }

    try {
        if (id) {
            await invoke('update_calendar_event', { event: eventData });
        } else {
            await invoke('create_calendar_event', { event: eventData });
        }
        hideEventModal();
        await loadCalendarEvents();
        showToast('Event saved', 'success');
    } catch (err) {
        console.error('Failed to save event:', err);
        showToast('Failed to save event: ' + err, 'error');
    }
}

async function deleteCalendarEvent() {
    const id = document.getElementById('cal-event-id').value;
    if (!id) return;

    if (!confirm('Delete this event?')) return;

    if (!invoke) {
        calEvents = calEvents.filter(ev => ev.id !== id);
        hideEventModal();
        renderCalendar();
        return;
    }

    try {
        await invoke('delete_calendar_event', { id });
        hideEventModal();
        await loadCalendarEvents();
        showToast('Event deleted', 'success');
    } catch (err) {
        console.error('Failed to delete event:', err);
        showToast('Failed to delete event: ' + err, 'error');
    }
}

// Calendar event listeners
if (calendarBtn) {
    calendarBtn.addEventListener('click', showCalendar);
}

if (calendarBack) {
    calendarBack.addEventListener('click', hideCalendar);
}

document.getElementById('cal-prev')?.addEventListener('click', () => {
    if (calViewMode === 'month') {
        calCurrentDate.setMonth(calCurrentDate.getMonth() - 1);
    } else {
        calCurrentDate.setDate(calCurrentDate.getDate() - 7);
    }
    loadCalendarEvents();
});

document.getElementById('cal-next')?.addEventListener('click', () => {
    if (calViewMode === 'month') {
        calCurrentDate.setMonth(calCurrentDate.getMonth() + 1);
    } else {
        calCurrentDate.setDate(calCurrentDate.getDate() + 7);
    }
    loadCalendarEvents();
});

document.getElementById('cal-today-btn')?.addEventListener('click', () => {
    calCurrentDate = new Date();
    loadCalendarEvents();
});

if (calViewSelect) {
    calViewSelect.addEventListener('change', (e) => {
        calViewMode = e.target.value;
        loadCalendarEvents();
    });
}

// Modal event listeners
document.getElementById('cal-modal-close')?.addEventListener('click', hideEventModal);
document.getElementById('cal-modal-cancel')?.addEventListener('click', hideEventModal);
document.getElementById('cal-modal-save')?.addEventListener('click', saveCalendarEvent);
document.getElementById('cal-modal-delete')?.addEventListener('click', deleteCalendarEvent);

// Close modal on overlay click
calModalOverlay?.addEventListener('click', (e) => {
    if (e.target === calModalOverlay) hideEventModal();
});

// All-day toggle
document.getElementById('cal-event-allday')?.addEventListener('change', (e) => {
    const tf = document.getElementById('cal-time-fields');
    if (e.target.checked) {
        tf.classList.add('hidden');
    } else {
        tf.classList.remove('hidden');
    }
});

// Color picker
document.getElementById('cal-color-options')?.addEventListener('click', (e) => {
    const dot = e.target.closest('.cal-color-dot');
    if (!dot) return;
    calModalOverlay.querySelectorAll('.cal-color-dot').forEach(d => d.classList.remove('selected'));
    dot.classList.add('selected');
});

// Category change updates default color
document.getElementById('cal-event-category')?.addEventListener('change', (e) => {
    const catColor = CATEGORY_COLORS[e.target.value];
    if (catColor) {
        calModalOverlay.querySelectorAll('.cal-color-dot').forEach(d => d.classList.remove('selected'));
        const dot = calModalOverlay.querySelector('.cal-color-dot[data-color="' + catColor + '"]');
        if (dot) dot.classList.add('selected');
    }
});

// ─── Notifications ───

const notificationsView = document.getElementById('notifications-view');
const notificationsBtn = document.getElementById('notifications-btn');
const notificationsBack = document.getElementById('notifications-back');

function showNotifications() {
    appView.style.display = 'none';
    settingsView.classList.add('hidden');
    timelineView?.classList.add('hidden');
    calendarView?.classList.add('hidden');
    contactsView?.classList.add('hidden');
    document.getElementById('memory-view')?.classList.add('hidden');
    document.getElementById('sidebar').style.display = '';
    if (timelineRefreshInterval) { clearInterval(timelineRefreshInterval); timelineRefreshInterval = null; }
    notificationsView.classList.remove('hidden');
    closeSidebar();
    loadNotifications();
}

function hideNotifications() {
    notificationsView.classList.add('hidden');
    document.getElementById('sidebar').style.display = '';
    appView.style.display = 'flex';
    inputEl.focus();
}

async function loadNotifications() {
    if (!invoke) return;
    try {
        const notifications = await invoke('list_notifications');
        renderNotificationList(notifications);
    } catch (e) {
        console.error('Failed to load notifications:', e);
    }
}

function renderNotificationList(notifications) {
    const container = document.getElementById('notifications-list');

    if (!notifications || notifications.length === 0) {
        container.innerHTML = '<p class="empty-state">No notifications yet</p>';
        return;
    }

    container.innerHTML = '';

    for (const notif of notifications) {
        const item = document.createElement('div');
        item.className = 'notification-item' + (notif.is_read ? ' read' : ' unread');

        const urgencyIcons = { Low: '\u2139\uFE0F', Medium: '\uD83D\uDCEC', High: '\u26A0\uFE0F', Critical: '\uD83D\uDEA8' };
        const icon = urgencyIcons[notif.urgency] || '\uD83D\uDCEC';

        const timeAgo = formatTimelineTime(new Date(notif.created_at).getTime());

        // Build the item
        const header = document.createElement('div');
        header.className = 'notification-item-header';

        const iconSpan = document.createElement('span');
        iconSpan.className = 'notif-icon';
        iconSpan.textContent = icon;

        const textDiv = document.createElement('div');
        textDiv.className = 'notif-text';

        if (notif.title) {
            const titleEl = document.createElement('div');
            titleEl.className = 'notif-title';
            titleEl.textContent = notif.title;
            textDiv.appendChild(titleEl);
        }

        const bodyEl = document.createElement('div');
        bodyEl.className = 'notif-body';
        bodyEl.textContent = notif.body;
        textDiv.appendChild(bodyEl);

        const timeEl = document.createElement('span');
        timeEl.className = 'notif-time';
        timeEl.textContent = timeAgo;

        header.appendChild(iconSpan);
        header.appendChild(textDiv);
        header.appendChild(timeEl);
        item.appendChild(header);

        // Actions row
        const actions = document.createElement('div');
        actions.className = 'notif-actions';

        if (notif.arc_id) {
            const openBtn = document.createElement('button');
            openBtn.className = 'notif-action-btn';
            openBtn.textContent = 'Open';
            openBtn.addEventListener('click', () => {
                handleSwitchArc(notif.arc_id);
                hideNotifications();
            });
            actions.appendChild(openBtn);
        }

        if (!notif.is_read) {
            const readBtn = document.createElement('button');
            readBtn.className = 'notif-action-btn';
            readBtn.textContent = 'Mark read';
            readBtn.addEventListener('click', async () => {
                await invoke('mark_notification_read', { id: notif.id });
                item.classList.remove('unread');
                item.classList.add('read');
                readBtn.remove();
                updateNotifBadge();
            });
            actions.appendChild(readBtn);
        }

        const deleteBtn = document.createElement('button');
        deleteBtn.className = 'notif-action-btn notif-delete-btn';
        deleteBtn.textContent = 'Delete';
        deleteBtn.addEventListener('click', async () => {
            await invoke('delete_notification', { id: notif.id });
            item.remove();
            updateNotifBadge();
        });
        actions.appendChild(deleteBtn);

        item.appendChild(actions);
        container.appendChild(item);
    }
}

async function updateNotifBadge() {
    try {
        const notifications = await invoke('list_notifications');
        const unreadCount = notifications.filter(n => !n.is_read).length;
        const badge = document.getElementById('notif-badge');
        if (unreadCount > 0) {
            badge.textContent = unreadCount;
            badge.style.display = 'inline-flex';
        } else {
            badge.style.display = 'none';
        }
    } catch (e) {
        // Ignore errors during badge updates
    }
}

async function markAllNotificationsRead() {
    try {
        await invoke('mark_all_notifications_read');
        loadNotifications();
        updateNotifBadge();
    } catch (e) {
        console.error('Failed to mark all as read:', e);
    }
}

async function deleteReadNotifications() {
    try {
        await invoke('delete_read_notifications');
        loadNotifications();
        updateNotifBadge();
    } catch (e) {
        console.error('Failed to delete read notifications:', e);
    }
}

// Notifications event listeners
if (notificationsBtn) {
    notificationsBtn.addEventListener('click', showNotifications);
}

if (notificationsBack) {
    notificationsBack.addEventListener('click', hideNotifications);
}

// ─── Contacts ───

const contactsView = document.getElementById('contacts-view');
const contactsBtn = document.getElementById('contacts-btn');
const contactsBack = document.getElementById('contacts-back');
const contactsListEl = document.getElementById('contacts-list');
const contactsEmptyEl = document.getElementById('contacts-empty');
const contactsTitle = document.getElementById('contacts-title');

function showContacts() {
    appView.style.display = 'none';
    settingsView.classList.add('hidden');
    timelineView?.classList.add('hidden');
    calendarView?.classList.add('hidden');
    notificationsView?.classList.add('hidden');
    document.getElementById('memory-view')?.classList.add('hidden');
    document.getElementById('sidebar').style.display = '';
    if (timelineRefreshInterval) { clearInterval(timelineRefreshInterval); timelineRefreshInterval = null; }
    contactsView.classList.remove('hidden');
    closeSidebar();
    loadContacts();
}

function hideContacts() {
    contactsView.classList.add('hidden');
    document.getElementById('sidebar').style.display = '';
    appView.style.display = 'flex';
    inputEl.focus();
}

async function loadContacts() {
    if (!invoke) return;
    try {
        const contacts = await invoke('list_contacts');
        renderContactList(contacts);
    } catch (err) {
        console.error('Failed to load contacts:', err);
        showToast('Failed to load contacts: ' + err, 'error');
    }
}

function renderContactList(contacts) {
    if (!contacts || contacts.length === 0) {
        contactsListEl.innerHTML = '';
        contactsEmptyEl.classList.remove('hidden');
        contactsTitle.textContent = 'Contacts';
        return;
    }

    contactsEmptyEl.classList.add('hidden');
    contactsTitle.textContent = 'Contacts (' + contacts.length + ')';
    contactsListEl.innerHTML = '';

    // Sort: blocked last, then by name
    contacts.sort((a, b) => {
        if (a.blocked !== b.blocked) return a.blocked ? 1 : -1;
        return a.name.localeCompare(b.name);
    });

    contacts.forEach(contact => {
        const card = document.createElement('div');
        card.className = 'contact-card' + (contact.blocked ? ' blocked' : '');
        card.dataset.contactId = contact.id;

        const trustClass = 'trust-' + contact.trust_level.toLowerCase();
        const blockedBadge = contact.blocked ? '<span class="contact-badge blocked-badge">Blocked</span>' : '';

        // Interactions text
        const interactionsText = contact.interaction_count > 0
            ? contact.interaction_count + ' interaction' + (contact.interaction_count !== 1 ? 's' : '')
            : 'No interactions';

        // Header (clickable to expand)
        const headerEl = document.createElement('div');
        headerEl.className = 'contact-card-header';
        headerEl.innerHTML =
            '<span class="contact-name">' + escapeHtml(contact.name) + '</span>' +
            blockedBadge +
            '<span class="contact-badge ' + trustClass + '">' + escapeHtml(contact.trust_level) + '</span>' +
            '<span class="contact-interactions">' + interactionsText + '</span>' +
            '<button class="contact-edit-btn" title="Edit contact">&#9998;</button>';

        headerEl.querySelector('.contact-edit-btn').addEventListener('click', (e) => {
            e.stopPropagation();
            showEditContactModal(contact);
        });

        headerEl.addEventListener('click', () => {
            card.classList.toggle('expanded');
        });

        // Details (shown when expanded)
        const detailsEl = document.createElement('div');
        detailsEl.className = 'contact-details';

        // Identifiers
        let identifiersHtml = '';
        if (contact.identifiers && contact.identifiers.length > 0) {
            identifiersHtml = '<div class="contact-identifiers">';
            contact.identifiers.forEach(ident => {
                identifiersHtml +=
                    '<div class="contact-identifier">' +
                    '<span class="contact-identifier-kind">' + escapeHtml(ident.kind) + '</span>' +
                    '<span>' + escapeHtml(ident.value) + '</span>' +
                    '</div>';
            });
            identifiersHtml += '</div>';
        }

        // Meta info
        let metaHtml = '<div class="contact-meta">';
        if (contact.last_interaction) {
            const lastDate = new Date(contact.last_interaction);
            metaHtml += 'Last interaction: ' + lastDate.toLocaleDateString() + ' ' + lastDate.toLocaleTimeString([], {hour: '2-digit', minute:'2-digit'});
        } else {
            metaHtml += 'No interactions yet';
        }
        if (contact.trust_manual_override) {
            metaHtml += ' &middot; <em>Trust manually set</em>';
        }
        metaHtml += '</div>';

        // Actions
        let actionsHtml = '<div class="contact-actions">';

        // Trust level dropdown (don't show AuthUser)
        actionsHtml += '<select class="contact-trust-select" data-contact-id="' + contact.id + '">';
        ['Unknown', 'Neutral', 'Known', 'Trusted'].forEach(level => {
            const selected = contact.trust_level === level ? ' selected' : '';
            actionsHtml += '<option value="' + level + '"' + selected + '>' + level + '</option>';
        });
        actionsHtml += '</select>';

        // Block/Unblock button
        if (contact.blocked) {
            actionsHtml += '<button class="contact-action-btn btn-unblock" data-action="unblock" data-contact-id="' + contact.id + '">Unblock</button>';
        } else {
            actionsHtml += '<button class="contact-action-btn btn-block" data-action="block" data-contact-id="' + contact.id + '">Block</button>';
        }

        // Delete button
        actionsHtml += '<button class="contact-action-btn btn-delete" data-action="delete" data-contact-id="' + contact.id + '">Delete</button>';

        if (contact.trust_manual_override) {
            actionsHtml += '<span class="contact-override-hint">Manual override active</span>';
        }

        actionsHtml += '</div>';

        detailsEl.innerHTML = identifiersHtml + metaHtml + actionsHtml;

        card.appendChild(headerEl);
        card.appendChild(detailsEl);
        contactsListEl.appendChild(card);
    });

    // Attach event listeners for actions within cards
    contactsListEl.querySelectorAll('.contact-trust-select').forEach(select => {
        select.addEventListener('change', async (e) => {
            const contactId = e.target.dataset.contactId;
            const level = e.target.value;
            await setContactTrust(contactId, level);
        });
    });

    contactsListEl.querySelectorAll('.contact-action-btn').forEach(btn => {
        btn.addEventListener('click', async (e) => {
            const contactId = e.target.dataset.contactId;
            const action = e.target.dataset.action;
            if (action === 'block') {
                await toggleBlockContact(contactId, true);
            } else if (action === 'unblock') {
                await toggleBlockContact(contactId, false);
            } else if (action === 'delete') {
                await deleteContact(contactId);
            }
        });
    });
}

async function setContactTrust(id, level) {
    if (!invoke) return;
    try {
        await invoke('set_contact_trust', { id, trustLevel: level });
        showToast('Trust level updated', 'success');
        await loadContacts();
    } catch (err) {
        console.error('Failed to set trust level:', err);
        showToast('Failed to set trust level: ' + err, 'error');
    }
}

async function toggleBlockContact(id, block) {
    if (!invoke) return;
    try {
        if (block) {
            await invoke('block_contact', { id });
            showToast('Contact blocked', 'success');
        } else {
            await invoke('unblock_contact', { id });
            showToast('Contact unblocked', 'success');
        }
        await loadContacts();
    } catch (err) {
        console.error('Failed to ' + (block ? 'block' : 'unblock') + ' contact:', err);
        showToast(err, 'error');
    }
}

async function deleteContact(id) {
    if (!confirm('Are you sure you want to delete this contact? This cannot be undone.')) {
        return;
    }
    if (!invoke) return;
    try {
        await invoke('delete_contact', { id });
        showToast('Contact deleted', 'success');
        await loadContacts();
    } catch (err) {
        console.error('Failed to delete contact:', err);
        showToast(err, 'error');
    }
}

// ─── Contact Modal ───

function getIdentifierPlaceholder(kind) {
    switch(kind) {
        case 'Email': return 'user@example.com';
        case 'Phone': return '+1 234 567 8900';
        case 'Telegram': return '@username or user ID';
        case 'WhatsApp': return '+1 234 567 8900';
        case 'IMessage': return 'email or phone';
        case 'Signal': return '+1 234 567 8900';
        case 'Discord': return 'username#1234';
        case 'Slack': return '@username';
        case 'Twitter': return '@handle';
        case 'Username': return 'username';
        default: return 'identifier';
    }
}

const identifierKinds = ['Email', 'Phone', 'Telegram', 'WhatsApp', 'IMessage', 'Signal', 'Discord', 'Slack', 'Twitter', 'Username', 'Other'];

function addIdentifierRow(kind, value) {
    const list = document.getElementById('contact-identifiers-list');
    const row = document.createElement('div');
    row.className = 'identifier-row';

    const select = document.createElement('select');
    identifierKinds.forEach(k => {
        const opt = document.createElement('option');
        opt.value = k;
        opt.textContent = k;
        if (kind && k === kind) opt.selected = true;
        select.appendChild(opt);
    });

    const input = document.createElement('input');
    input.type = 'text';
    input.placeholder = getIdentifierPlaceholder(kind || 'Email');
    if (value) input.value = value;

    select.addEventListener('change', () => {
        input.placeholder = getIdentifierPlaceholder(select.value);
    });

    const removeBtn = document.createElement('button');
    removeBtn.className = 'remove-identifier-btn';
    removeBtn.textContent = '\u00d7';
    removeBtn.title = 'Remove';
    removeBtn.addEventListener('click', () => row.remove());

    row.appendChild(select);
    row.appendChild(input);
    row.appendChild(removeBtn);
    list.appendChild(row);
}

function showNewContactModal() {
    document.getElementById('contact-edit-id').value = '';
    document.getElementById('contact-name').value = '';
    document.getElementById('contact-trust-modal-select').value = 'Neutral';
    document.getElementById('contact-identifiers-list').innerHTML = '';
    document.getElementById('contact-modal-title').textContent = 'New Contact';
    addIdentifierRow();
    document.getElementById('contact-modal-overlay').style.display = '';
}

function showEditContactModal(contact) {
    document.getElementById('contact-edit-id').value = contact.id;
    document.getElementById('contact-name').value = contact.name || '';
    document.getElementById('contact-trust-modal-select').value = contact.trust_level || 'Neutral';
    document.getElementById('contact-identifiers-list').innerHTML = '';
    document.getElementById('contact-modal-title').textContent = 'Edit Contact';

    if (contact.identifiers && contact.identifiers.length > 0) {
        contact.identifiers.forEach(ident => {
            addIdentifierRow(ident.kind, ident.value);
        });
    } else {
        addIdentifierRow();
    }

    document.getElementById('contact-modal-overlay').style.display = '';
}

function hideContactModal() {
    document.getElementById('contact-modal-overlay').style.display = 'none';
}

async function saveContact() {
    const id = document.getElementById('contact-edit-id').value;
    const name = document.getElementById('contact-name').value.trim();
    const trustLevel = document.getElementById('contact-trust-modal-select').value;

    if (!name) {
        showToast('Name is required', 'error');
        return;
    }

    const rows = document.querySelectorAll('#contact-identifiers-list .identifier-row');
    const identifiers = [];
    rows.forEach(row => {
        const kind = row.querySelector('select').value;
        const value = row.querySelector('input').value.trim();
        if (value) {
            identifiers.push({ kind, value });
        }
    });

    if (identifiers.length === 0) {
        showToast('At least one identifier is required', 'error');
        return;
    }

    if (!invoke) return;

    try {
        if (id) {
            await invoke('update_contact', { id, name, trustLevel, identifiers });
            showToast('Contact updated', 'success');
        } else {
            await invoke('create_contact', { name, trustLevel, identifiers });
            showToast('Contact created', 'success');
        }
        hideContactModal();
        await loadContacts();
    } catch (err) {
        console.error('Failed to save contact:', err);
        showToast('Failed to save contact: ' + err, 'error');
    }
}

// Contacts event listeners
if (contactsBtn) {
    contactsBtn.addEventListener('click', showContacts);
}

if (contactsBack) {
    contactsBack.addEventListener('click', hideContacts);
}

// Close contact modal on overlay click
document.getElementById('contact-modal-overlay')?.addEventListener('click', function(e) {
    if (e.target === this) hideContactModal();
});

// ─── Memory ───

const memoryView = document.getElementById('memory-view');
const memoryBtn = document.getElementById('memory-btn');
const memoryBack = document.getElementById('memory-back');

function showMemory() {
    appView.style.display = 'none';
    settingsView.classList.add('hidden');
    timelineView?.classList.add('hidden');
    calendarView?.classList.add('hidden');
    notificationsView?.classList.add('hidden');
    contactsView?.classList.add('hidden');
    document.getElementById('sidebar').style.display = '';
    if (timelineRefreshInterval) { clearInterval(timelineRefreshInterval); timelineRefreshInterval = null; }
    memoryView.classList.remove('hidden');
    closeSidebar();
    loadMemories();
    loadEntities();
}

function hideMemory() {
    memoryView.classList.add('hidden');
    document.getElementById('sidebar').style.display = '';
    appView.style.display = 'flex';
    inputEl.focus();
}

async function loadMemories() {
    if (!invoke) return;
    try {
        const items = await invoke('list_memories');
        renderMemories(items);
    } catch (e) {
        console.error('Failed to load memories:', e);
    }
}

function renderMemories(items) {
    const container = document.getElementById('memories-list');
    if (!items || items.length === 0) {
        container.innerHTML = '<div class="empty-state">No memories stored yet. The agent will remember important information from your conversations.</div>';
        return;
    }

    container.innerHTML = items.map(item => {
        const timeAgo = formatTimelineTime(new Date(item.timestamp).getTime());
        return '<div class="memory-card" data-id="' + escapeHtml(item.id) + '">' +
            '<div class="memory-card-header">' +
                '<span class="memory-type-badge ' + escapeHtml(item.memory_type) + '">' + escapeHtml(item.memory_type) + '</span>' +
                '<span class="memory-source">' + escapeHtml(item.source) + '</span>' +
                '<span class="memory-time">' + timeAgo + '</span>' +
            '</div>' +
            '<div class="memory-content">' + escapeHtml(item.content) + '</div>' +
            '<div class="memory-actions">' +
                '<button class="memory-edit-btn" onclick="editMemory(\'' + escapeHtml(item.id) + '\', this)">Edit</button>' +
                '<button class="memory-delete-btn" onclick="deleteMemory(\'' + escapeHtml(item.id) + '\')">Delete</button>' +
            '</div>' +
        '</div>';
    }).join('');
}

async function editMemory(id, btn) {
    const card = btn.closest('.memory-card');
    const contentEl = card.querySelector('.memory-content');

    if (contentEl.contentEditable === 'true') {
        // Save mode
        const newContent = contentEl.textContent;
        try {
            await invoke('update_memory', { id: id, content: newContent });
            btn.textContent = 'Edit';
            contentEl.contentEditable = 'false';
            contentEl.classList.remove('editing');
        } catch (e) {
            console.error('Failed to update memory:', e);
            showToast('Failed to update memory: ' + e, 'error');
        }
    } else {
        // Edit mode
        contentEl.contentEditable = 'true';
        contentEl.classList.add('editing');
        contentEl.focus();
        btn.textContent = 'Save';
    }
}

async function deleteMemory(id) {
    if (!confirm('Delete this memory?')) return;
    try {
        await invoke('delete_memory', { id: id });
        loadMemories();
    } catch (e) {
        console.error('Failed to delete memory:', e);
        showToast('Failed to delete memory: ' + e, 'error');
    }
}

async function loadEntities() {
    if (!invoke) return;
    try {
        const entities = await invoke('list_entities');
        renderEntities(entities);
    } catch (e) {
        console.error('Failed to load entities:', e);
    }
}

function renderEntities(entities) {
    const container = document.getElementById('entities-list');
    if (!entities || entities.length === 0) {
        container.innerHTML = '<div class="empty-state">No entities discovered yet. The agent extracts people, organizations, and concepts from your conversations.</div>';
        return;
    }

    container.innerHTML = entities.map(e => {
        const typeClass = escapeHtml(e.entity_type.toLowerCase());
        let inner = '';

        // Entity ID
        inner += '<div class="entity-id-row"><span class="entity-section-label">ID</span> <span class="entity-id-value">' + escapeHtml(e.id) + '</span></div>';

        // Relations
        if (e.relations && e.relations.length > 0) {
            inner += '<div class="entity-relations">';
            inner += '<div class="entity-section-label">Relations</div>';
            e.relations.forEach(r => {
                const arrow = r.direction === 'out' ? '→' : '←';
                inner += '<div class="entity-relation-row">' +
                    '<span class="relation-arrow">' + arrow + '</span>' +
                    '<span class="relation-label">' + escapeHtml(r.relation) + '</span>' +
                    '<span class="relation-target">' + escapeHtml(r.target_name) + '</span>' +
                '</div>';
            });
            inner += '</div>';
        } else {
            inner += '<div class="entity-no-relations">No relations yet</div>';
        }

        // Metadata (skip empty objects)
        if (e.metadata && Object.keys(e.metadata).length > 0) {
            inner += '<div class="entity-metadata">';
            inner += '<div class="entity-section-label">Metadata</div>';
            inner += '<pre class="entity-meta-json">' + escapeHtml(JSON.stringify(e.metadata, null, 2)) + '</pre>';
            inner += '</div>';
        }

        const detailsHtml = '<div class="entity-details">' + inner + '</div>';

        return '<details class="entity-card" data-id="' + escapeHtml(e.id) + '">' +
            '<summary class="entity-summary">' +
                '<span class="entity-type-badge ' + typeClass + '">' + escapeHtml(e.entity_type) + '</span>' +
                '<span class="entity-name">' + escapeHtml(e.name) + '</span>' +
                (e.relations && e.relations.length > 0
                    ? '<span class="entity-rel-count">' + e.relations.length + ' rel</span>'
                    : '') +
                '<button class="entity-delete-btn" onclick="event.preventDefault(); deleteEntity(\'' + escapeHtml(e.id) + '\')">×</button>' +
            '</summary>' +
            detailsHtml +
        '</details>';
    }).join('');
}

async function deleteEntity(id) {
    if (!confirm('Delete this entity and its relations?')) return;
    try {
        await invoke('delete_entity', { id });
        loadEntities();
    } catch (e) {
        console.error('Failed to delete entity:', e);
        showToast({ urgency: 'high', title: 'Error', body: 'Failed to delete entity' });
    }
}

function filterMemories(query) {
    const cards = document.querySelectorAll('#memories-list .memory-card');
    const lowerQuery = query.toLowerCase();
    cards.forEach(card => {
        const content = card.querySelector('.memory-content').textContent.toLowerCase();
        const source = card.querySelector('.memory-source')?.textContent.toLowerCase() || '';
        const matches = !lowerQuery || content.includes(lowerQuery) || source.includes(lowerQuery);
        card.style.display = matches ? '' : 'none';
    });
}

// Memory tab switching
document.querySelectorAll('.memory-tab').forEach(tab => {
    tab.addEventListener('click', () => {
        document.querySelectorAll('.memory-tab').forEach(t => t.classList.remove('active'));
        tab.classList.add('active');
        const target = tab.dataset.tab;
        document.getElementById('memories-panel').style.display = target === 'memories' ? '' : 'none';
        document.getElementById('entities-panel').style.display = target === 'entities' ? '' : 'none';
    });
});

// Memory search
document.getElementById('memory-search-input')?.addEventListener('input', (e) => {
    filterMemories(e.target.value);
});

// Memory event listeners
if (memoryBtn) {
    memoryBtn.addEventListener('click', showMemory);
}

if (memoryBack) {
    memoryBack.addEventListener('click', hideMemory);
}

// ─── Path-Grant Modal & Permissions Settings ───

const SYSTEM_PATH_PREFIXES = ['/etc', '/usr', '/bin', '/sbin', '/boot', '/sys', '/proc', '/var/run', '/var/lib'];

const grantQueue = [];
let grantInFlight = null;

function isSystemPath(p) {
    if (!p) return false;
    return SYSTEM_PATH_PREFIXES.some((pref) => p === pref || p.startsWith(pref + '/'));
}

function ellipsizePath(p, maxLen = 60) {
    if (!p || p.length <= maxLen) return p;
    const head = Math.ceil((maxLen - 3) * 0.55);
    const tail = (maxLen - 3) - head;
    return p.slice(0, head) + '...' + p.slice(p.length - tail);
}

function escapeHtml(s) {
    return String(s ?? '')
        .replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;')
        .replace(/"/g, '&quot;').replace(/'/g, '&#39;');
}

function enqueueGrantRequest(payload) {
    if (!payload || !payload.id) return;
    // Deduplicate by id (same request might fire on init + via event).
    if (grantInFlight && grantInFlight.id === payload.id) return;
    if (grantQueue.some((q) => q.id === payload.id)) return;
    grantQueue.push(payload);
    if (!grantInFlight) showNextGrantRequest();
    else updateGrantQueueIndicator();
}

async function recoverPendingGrants() {
    if (!invoke) return;
    try {
        const list = await invoke('list_pending_grants');
        (list || []).forEach(enqueueGrantRequest);
    } catch (err) {
        console.error('Failed to recover pending grants:', err);
    }
}

function showNextGrantRequest() {
    const overlay = document.getElementById('grant-modal-overlay');
    if (!overlay) return;
    if (grantQueue.length === 0) {
        grantInFlight = null;
        overlay.classList.add('hidden');
        return;
    }
    grantInFlight = grantQueue.shift();
    renderGrantModal(grantInFlight);
    overlay.classList.remove('hidden');
}

function renderGrantModal(req) {
    const titleEl = document.getElementById('grant-modal-title');
    const badgeEl = document.getElementById('grant-modal-badge');
    const questionEl = document.getElementById('grant-modal-question');
    const pathsEl = document.getElementById('grant-modal-paths');
    const toolEl = document.getElementById('grant-modal-tool');
    const allowAlwaysBtn = document.getElementById('grant-allow-always-btn');

    const access = (req.access || 'read').toLowerCase();
    const accessLabel = access === 'write' ? 'Write' : 'Read';
    const verb = access === 'write' ? 'write to' : 'read';

    badgeEl.textContent = accessLabel;
    badgeEl.className = 'grant-access-badge ' + (access === 'write' ? 'badge-write' : 'badge-read');

    const paths = Array.isArray(req.paths) ? req.paths : [];
    const isMove = paths.length > 1;
    if (isMove) {
        titleEl.textContent = 'Allow file move?';
        questionEl.textContent = `A tool wants to move a file:`;
    } else {
        titleEl.textContent = `Allow ${access} access?`;
        questionEl.textContent = `A tool wants to ${verb}:`;
    }

    pathsEl.innerHTML = '';
    paths.forEach((p, idx) => {
        const row = document.createElement('div');
        row.className = 'grant-modal-path';
        const prefix = isMove ? (idx === 0 ? 'From: ' : 'To: ') : '';
        row.innerHTML = `<span class="grant-modal-path-prefix">${escapeHtml(prefix)}</span><code title="${escapeHtml(p)}">${escapeHtml(ellipsizePath(p))}</code>`;
        pathsEl.appendChild(row);
    });

    toolEl.textContent = req.tool || req.requesting_tool || 'unknown';

    // Defensive: grey out Allow Always if any path looks like a system path.
    const anySystem = paths.some(isSystemPath);
    if (anySystem) {
        allowAlwaysBtn.disabled = true;
        allowAlwaysBtn.title = 'System paths cannot be granted permanently';
    } else {
        allowAlwaysBtn.disabled = false;
        allowAlwaysBtn.title = '';
    }

    updateGrantQueueIndicator();
}

function updateGrantQueueIndicator() {
    const queueEl = document.getElementById('grant-modal-queue');
    if (!queueEl) return;
    if (grantQueue.length > 0) {
        queueEl.classList.remove('hidden');
        queueEl.textContent = `${grantQueue.length} more request${grantQueue.length === 1 ? '' : 's'} waiting`;
    } else {
        queueEl.classList.add('hidden');
    }
}

async function resolveCurrentGrant(decision) {
    if (!grantInFlight || !invoke) return;
    const req = grantInFlight;
    grantInFlight = null;
    try {
        await invoke('resolve_pending_grant', { id: req.id, decision });
        if (decision === 'AllowAlways') {
            // Refresh arc grants list if settings is open.
            if (document.getElementById('settings-view') &&
                !document.getElementById('settings-view').classList.contains('hidden')) {
                loadArcGrants();
            }
        }
    } catch (err) {
        console.error('Failed to resolve grant:', err);
        showToast('Failed to resolve grant: ' + err, 'error');
    }
    showNextGrantRequest();
}

document.getElementById('grant-allow-btn')?.addEventListener('click', () => resolveCurrentGrant('Allow'));
document.getElementById('grant-allow-always-btn')?.addEventListener('click', () => resolveCurrentGrant('AllowAlways'));
document.getElementById('grant-deny-btn')?.addEventListener('click', () => resolveCurrentGrant('Deny'));

// ESC closes the modal as Deny (only when modal is visible).
document.addEventListener('keydown', (e) => {
    if (e.key !== 'Escape') return;
    const overlay = document.getElementById('grant-modal-overlay');
    if (!overlay || overlay.classList.contains('hidden')) return;
    if (!grantInFlight) return;
    e.stopPropagation();
    resolveCurrentGrant('Deny');
});

// Click backdrop = no-op (don't accidentally Deny). Modal is dismissed only by buttons or ESC.

// ─── Permissions settings: grant lists ──────────────────────────────

async function loadGrants() {
    await Promise.all([loadGlobalGrants(), loadArcGrants()]);
}

async function loadGlobalGrants() {
    if (!invoke) return;
    try {
        const grants = await invoke('list_global_grants');
        renderGrantsList('global-grants-list', grants || [], 'global');
    } catch (err) {
        console.error('Failed to load global grants:', err);
    }
}

async function loadArcGrants() {
    if (!invoke) return;
    if (!activeArcId) {
        // No active arc -- leave the empty state visible.
        renderGrantsList('arc-grants-list', [], 'arc');
        return;
    }
    try {
        const grants = await invoke('list_arc_grants', { arcId: activeArcId });
        renderGrantsList('arc-grants-list', grants || [], 'arc');
    } catch (err) {
        console.error('Failed to load arc grants:', err);
    }
}

function renderGrantsList(containerId, grants, scope) {
    const el = document.getElementById(containerId);
    if (!el) return;
    if (!grants || grants.length === 0) {
        el.innerHTML = `<p class="grants-empty">${scope === 'global' ? 'No global grants yet.' : 'No grants for this arc.'}</p>`;
        return;
    }
    el.innerHTML = '';
    grants.forEach((g) => {
        const card = document.createElement('div');
        card.className = 'grant-card';
        const access = (g.access || 'read').toLowerCase();
        const badgeClass = access === 'write' ? 'badge-write' : 'badge-read';
        const accessLabel = access === 'write' ? 'Write' : 'Read';
        card.innerHTML = `
            <div class="grant-card-main">
                <span class="grant-access-badge ${badgeClass}">${accessLabel}</span>
                <code class="grant-card-path" title="${escapeHtml(g.path)}">${escapeHtml(ellipsizePath(g.path, 70))}</code>
            </div>
            <button class="btn-secondary grant-revoke-btn" data-grant-id="${g.id}" data-scope="${scope}">Revoke</button>
        `;
        el.appendChild(card);
    });
    el.querySelectorAll('.grant-revoke-btn').forEach((btn) => {
        btn.addEventListener('click', () => {
            const id = parseInt(btn.dataset.grantId, 10);
            const sc = btn.dataset.scope;
            revokeGrant(id, sc);
        });
    });
}

async function revokeGrant(id, scope) {
    if (!invoke) return;
    try {
        const cmd = scope === 'global' ? 'revoke_global_grant' : 'revoke_arc_grant';
        await invoke(cmd, { id });
        showToast('Grant revoked', 'success');
        if (scope === 'global') loadGlobalGrants();
        else loadArcGrants();
    } catch (err) {
        showToast('Failed to revoke: ' + err, 'error');
    }
}

document.getElementById('add-grant-btn')?.addEventListener('click', async () => {
    if (!invoke) return;
    const pathEl = document.getElementById('new-grant-path');
    const accessEl = document.getElementById('new-grant-access');
    const path = (pathEl?.value || '').trim();
    const access = accessEl?.value || 'read';
    if (!path) {
        showToast('Enter a directory path', 'error');
        return;
    }
    if (!path.startsWith('/')) {
        showToast('Path must be absolute (start with /)', 'error');
        return;
    }
    try {
        await invoke('add_global_grant', { path, access });
        showToast('Grant added', 'success');
        pathEl.value = '';
        loadGlobalGrants();
    } catch (err) {
        showToast('Failed to add grant: ' + err, 'error');
    }
});

// ─── Onboarding wizard ───────────────────────────────────────────────
//
// On first launch (decided by the Rust side via invoke('is_first_launch'))
// we surface a tiny modal that walks the user through picking an LLM
// provider. Three terminating paths: skip, local-test-and-save, or
// cloud-test-and-save. All three call `complete_onboarding` so this
// never re-fires for the same user. Bug-paranoid: any IPC error in
// onboarding falls through to the main UI rather than trapping the user.

const ONB_LOCAL_DEFAULTS = {
    ollama: 'http://localhost:11434',
    llamacpp: 'http://localhost:8080',
};
const ONB_CLOUD_HINTS = {
    anthropic: 'sk-ant-...',
    deepseek: 'sk-...',
    openai: 'sk-...',
};

function showOnboardingStep(name) {
    const overlay = document.getElementById('onboarding-overlay');
    if (!overlay) return;
    overlay.querySelectorAll('.onboarding-step').forEach((s) => {
        s.style.display = s.dataset.step === name ? '' : 'none';
    });
}

function setOnbStatus(elId, kind, text) {
    const el = document.getElementById(elId);
    if (!el) return;
    el.className = 'onb-status ' + kind;
    el.textContent = text;
}

async function finishOnboarding() {
    try {
        await invoke('complete_onboarding');
    } catch (e) {
        // Hide the overlay anyway — better to land in a usable app than
        // to trap the user behind a broken sentinel write.
        console.warn('[athen] complete_onboarding failed:', e);
    }
    const overlay = document.getElementById('onboarding-overlay');
    if (overlay) overlay.style.display = 'none';
}

async function onboardingTestAndSave({ statusElId, id, baseUrl, model, apiKey }) {
    setOnbStatus(statusElId, 'busy', 'Testing connection…');
    try {
        const result = await invoke('test_provider', {
            id,
            baseUrl,
            model,
            apiKey: apiKey || null,
        });
        if (!result || !result.success) {
            setOnbStatus(statusElId, 'err', (result && result.message) || 'Connection failed');
            return false;
        }
    } catch (e) {
        setOnbStatus(statusElId, 'err', String(e));
        return false;
    }

    setOnbStatus(statusElId, 'busy', 'Saving…');
    try {
        await invoke('save_provider', {
            id,
            baseUrl,
            model,
            apiKey: apiKey || null,
        });
        await invoke('set_active_provider', { id });
    } catch (e) {
        setOnbStatus(statusElId, 'err', 'Save failed: ' + e);
        return false;
    }

    setOnbStatus(statusElId, 'ok', 'Connected. Saved.');
    return true;
}

function wireOnboardingButtons() {
    document.getElementById('onb-start-btn')?.addEventListener('click', () => {
        showOnboardingStep('pick');
    });
    document.getElementById('onb-skip-1')?.addEventListener('click', finishOnboarding);
    document.getElementById('onb-skip-2')?.addEventListener('click', finishOnboarding);
    document.getElementById('onb-back-2')?.addEventListener('click', () => showOnboardingStep('welcome'));
    document.getElementById('onb-back-3')?.addEventListener('click', () => showOnboardingStep('pick'));
    document.getElementById('onb-back-4')?.addEventListener('click', () => showOnboardingStep('pick'));

    document.getElementById('onb-pick-local')?.addEventListener('click', () => {
        const sel = document.getElementById('onb-local-type');
        const url = document.getElementById('onb-local-url');
        if (url && sel && !url.value) url.value = ONB_LOCAL_DEFAULTS[sel.value] || '';
        showOnboardingStep('local');
    });
    document.getElementById('onb-pick-cloud')?.addEventListener('click', () => {
        showOnboardingStep('cloud');
    });

    document.getElementById('onb-local-type')?.addEventListener('change', (e) => {
        const url = document.getElementById('onb-local-url');
        if (!url) return;
        url.placeholder = ONB_LOCAL_DEFAULTS[e.target.value] || '';
        // Clear stale URL so the new default placeholder shows.
        if (Object.values(ONB_LOCAL_DEFAULTS).includes(url.value)) {
            url.value = '';
        }
    });
    document.getElementById('onb-cloud-type')?.addEventListener('change', (e) => {
        const k = document.getElementById('onb-cloud-key');
        if (k) k.placeholder = ONB_CLOUD_HINTS[e.target.value] || 'sk-...';
    });

    document.getElementById('onb-local-test')?.addEventListener('click', async () => {
        const id = document.getElementById('onb-local-type').value;
        const baseUrl = document.getElementById('onb-local-url').value
            || ONB_LOCAL_DEFAULTS[id]
            || '';
        const model = document.getElementById('onb-local-model').value;
        const ok = await onboardingTestAndSave({
            statusElId: 'onb-local-status',
            id,
            baseUrl,
            model,
            apiKey: null,
        });
        if (ok) showOnboardingStep('done');
    });

    document.getElementById('onb-cloud-test')?.addEventListener('click', async () => {
        const id = document.getElementById('onb-cloud-type').value;
        const apiKey = document.getElementById('onb-cloud-key').value.trim();
        const model = document.getElementById('onb-cloud-model').value;
        if (!apiKey) {
            setOnbStatus('onb-cloud-status', 'err', 'API key required.');
            return;
        }
        const ok = await onboardingTestAndSave({
            statusElId: 'onb-cloud-status',
            id,
            baseUrl: '', // empty → backend uses default for the chosen provider
            model,
            apiKey,
        });
        if (ok) showOnboardingStep('done');
    });

    document.getElementById('onb-finish')?.addEventListener('click', finishOnboarding);
}

async function maybeRunOnboarding() {
    if (!invoke) return;
    let isFirst = false;
    try {
        isFirst = await invoke('is_first_launch');
    } catch (e) {
        // If the predicate itself fails, do NOT show onboarding. The
        // backend is conservative (returns false on any I/O ambiguity);
        // a hard error here means something is genuinely broken and we
        // shouldn't risk asking a returning user to reconfigure.
        console.warn('[athen] is_first_launch failed, skipping onboarding:', e);
        return;
    }
    if (!isFirst) return;
    const overlay = document.getElementById('onboarding-overlay');
    if (!overlay) return;
    overlay.style.display = 'flex';
    showOnboardingStep('welcome');
}

// ─── Initialize ───

inputEl.focus();
wireOnboardingButtons();
initTauri();
