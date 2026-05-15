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
// Task IDs whose approval has already been initiated (UI click or
// Telegram callback). Prevents the `approval-resolved` event — which
// `approve_task` itself emits — from re-entering and double-executing.
const approvalsInFlight = new Set();

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
// "Hand off to a specialist": two figures with an arrow between them.
const ICON_DELEGATE    = toolSvg('<circle cx="6" cy="8" r="3"/><path d="M2 21v-2a4 4 0 0 1 4-4h0"/><circle cx="18" cy="8" r="3"/><path d="M14 21v-2a4 4 0 0 1 4-4h0"/><line x1="9" y1="12" x2="15" y2="12"/><polyline points="13 10 15 12 13 14"/>');
const ICON_MAIL        = toolSvg('<path d="M4 4h16a2 2 0 0 1 2 2v12a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V6a2 2 0 0 1 2-2z"/><polyline points="22,6 12,13 2,6"/>');
const ICON_PAPER_PLANE = toolSvg('<line x1="22" y1="2" x2="11" y2="13"/><polygon points="22 2 15 22 11 13 2 9 22 2"/>');
// Alarm clock with two ear-bells: communicates "scheduled wake-up".
const ICON_ALARM       = toolSvg('<circle cx="12" cy="13" r="8"/><polyline points="12 9 12 13 14.5 15"/><line x1="5" y1="3" x2="2" y2="6"/><line x1="22" y1="6" x2="19" y2="3"/>');
// Person with id-card aura: communicates "identity entry".
const ICON_IDENTITY    = toolSvg('<circle cx="12" cy="8" r="4"/><path d="M4 21v-1a8 8 0 0 1 16 0v1"/><line x1="9" y1="13" x2="15" y2="13"/>');

const BUILTIN_TOOL_ICONS = {
    'read': ICON_FILE_TEXT, 'list_directory': ICON_FOLDER, 'grep': ICON_FILE_SEARCH,
    'write': ICON_PEN_DOC, 'edit': ICON_PEN_DOC,
    'shell_execute': ICON_TERMINAL, 'shell_spawn': ICON_TERMINAL,
    'shell_kill': ICON_STOP, 'shell_logs': ICON_LOGS,
    'web_search': ICON_SEARCH, 'web_fetch': ICON_GLOBE,
    'email_send': ICON_MAIL,
    'send_telegram': ICON_PAPER_PLANE,
    'memory_store': ICON_BOOKMARK, 'memory_recall': ICON_SPARKLES,
    'calendar_list': ICON_CALENDAR, 'calendar_create': ICON_CAL_PLUS,
    'calendar_update': ICON_CALENDAR, 'calendar_delete': ICON_TRASH,
    'contacts_list': ICON_USERS, 'contacts_search': ICON_USER_SEARCH,
    'contacts_create': ICON_USER_PLUS, 'contacts_update': ICON_USER,
    'contacts_delete': ICON_TRASH,
    // delegation
    'delegate_to_agent': ICON_DELEGATE,
    // toolbox (pip/npm package management)
    'install_package': ICON_FOLDER_PLUS,
    'uninstall_package': ICON_TRASH,
    'list_installed_packages': ICON_BOOKMARK,
    // wake-ups (agent-authored scheduled follow-ups)
    'create_wakeup': ICON_ALARM,
    // identity (agent-authored entries to the user-maintained identity store)
    'identity_add': ICON_IDENTITY,
    // generic cloud HTTP API call (registered endpoint via Settings → Cloud APIs)
    'http_request': ICON_GLOBE,
};

const BUILTIN_TOOL_LABELS = {
    'read': 'Read', 'list_directory': 'List', 'grep': 'Search files',
    'write': 'Write', 'edit': 'Edit',
    'shell_execute': 'Run', 'shell_spawn': 'Spawn',
    'shell_kill': 'Stop', 'shell_logs': 'Logs',
    'web_search': 'Search web', 'web_fetch': 'Fetch',
    'email_send': 'Send email',
    'send_telegram': 'Send Telegram',
    'memory_store': 'Save', 'memory_recall': 'Recall',
    'calendar_list': 'Events', 'calendar_create': 'Create event',
    'calendar_update': 'Update event', 'calendar_delete': 'Delete event',
    'contacts_list': 'Contacts', 'contacts_search': 'Find contact',
    'contacts_create': 'Add contact', 'contacts_update': 'Update contact',
    'contacts_delete': 'Delete contact',
    'delegate_to_agent': 'Consult specialist',
    'install_package': 'Install package',
    'uninstall_package': 'Uninstall package',
    'list_installed_packages': 'List packages',
    'create_wakeup': 'Schedule wake-up',
    'identity_add': 'Note about you',
    'http_request': 'Cloud API',
};

// MCP-prefixed tools (e.g. `slack__post_message`) — strip prefix and try common
// suffix aliases so third-party MCP file-like tools pick up the same icons
// as the built-ins when the names happen to match.
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
        const { step, tool_name, status, detail, arc_id, args, result, error } = event.payload;

        // Drop progress for arcs the user isn't currently viewing — otherwise
        // a Telegram-driven background arc renders its tool cards into
        // whichever arc is on screen, then they vanish on tab-switch
        // because they were never part of that arc's persisted history.
        // Permissive when arc_id is missing (older code paths or frontend-only events).
        if (arc_id && arc_id !== activeArcId) return;

        // Update status bar as before.
        setStatus('working', `Step ${step}: ${tool_name} (${status})`);

        // Skip non-tool steps (e.g. "Evaluating risk...", "Task completed").
        if (step === 0 || tool_name === 'Task completed') return;

        // Create tool container if it does not exist yet. In optimistic
        // completion mode the assistant message can stream in before any
        // tool starts; insert the container above the streaming row so
        // tool cards always sit above the conclusion they belong to.
        if (!currentToolContainer) {
            currentToolContainer = document.createElement('div');
            currentToolContainer.className = 'tool-steps-container';
            const streamingRow = messagesEl.querySelector('#streaming-message');
            if (streamingRow) {
                messagesEl.insertBefore(currentToolContainer, streamingRow);
            } else {
                messagesEl.appendChild(currentToolContainer);
            }
        }

        // Build the live card. Reuse the same DOM constructor the
        // rehydrated path uses, so click-to-expand bodies (Edit diff,
        // Read content, Fetch page, …) work for the active turn too.
        // The auditor enriches terminal events with full args+result;
        // InProgress events stay flat (nothing to expand yet).
        const meta = {
            tool: tool_name,
            status,
            summary: detail || '',
            args: args || null,
            result: result || null,
            error: error || null,
        };
        const card = buildToolCardBlock(meta);
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

    // The file gate races in-app vs Telegram. When Telegram wins, the
    // backend emits this event with the resolved request id; drop it
    // from the queue (or close the modal if it's the in-flight one).
    window.__TAURI__.event.listen('grant-resolved-elsewhere', (event) => {
        const id = event.payload;
        if (!id) return;
        if (grantInFlight && grantInFlight.id === id) {
            grantInFlight = null;
            const overlay = document.getElementById('grant-modal-overlay');
            if (overlay) overlay.classList.add('hidden');
            showNextGrantRequest();
            return;
        }
        const idx = grantQueue.findIndex((q) => q.id === id);
        if (idx >= 0) {
            grantQueue.splice(idx, 1);
            updateGrantQueueIndicator();
        }
    });

    // When the approval router resolves a question through Telegram,
    // the UI card stays stale because it was driven by the legacy
    // `approve_task` flow. Auto-invoke approve_task with the choice so
    // the Telegram tap actually triggers execution and the card clears.
    // The `approvalsInFlight` set + handleApproval's guard prevent
    // re-entry (approve_task itself emits this same event).
    window.__TAURI__.event.listen('approval-resolved', (event) => {
        const { task_id, approved } = event.payload || {};
        if (!task_id) return;
        if (approvalsInFlight.has(task_id)) return;
        const card = document.getElementById(`approval-${task_id}`);
        if (!card) return;
        handleApproval(task_id, !!approved);
    });

    // Approval router questions (e.g. install_package gate). Distinct
    // from the legacy task-approval flow above: this comes from
    // ApprovalRouter::ask -> InAppApprovalSink, with a question_id +
    // explicit choice list. Resolved via submit_approval.
    window.__TAURI__.event.listen('approval-question', (event) => {
        const q = event.payload || {};
        // Approval-question events fire from background flows like
        // wake-ups and sense-driven autonomous tasks. Their `arc_id`
        // may differ from the one the user is currently viewing —
        // appending the card straight into the chat list would put it
        // in the wrong arc and the user would never see it (the chat
        // view might even be hidden behind Settings / Scheduled). When
        // the question targets a different arc, surface a toast that
        // jumps the user to that arc; the card renders the moment they
        // arrive (loadHistory replays approval-question state via a
        // pending queue — see `pendingApprovalQuestionsByArc`).
        if (q.arc_id && q.arc_id !== activeArcId) {
            stashApprovalQuestionForArc(q);
            showSenseNotification(
                'approval',
                'Athen',
                q.prompt || 'Approval needed',
                q.description || '',
                'high',
                'Athen needs your approval to continue',
                'urgent',
                q.arc_id,
                false,
            );
            return;
        }
        addApprovalQuestionDialog(q);
    });
    window.__TAURI__.event.listen('approval-cancel', (event) => {
        const id = event.payload;
        if (!id) return;
        const card = document.getElementById(`approval-q-${id}`);
        if (card) card.remove();
    });

    // Wake-up fired: a scheduled trigger just produced a Task and (likely)
    // dispatched it. Refresh the arc list so the freshly-created wake-up
    // arc appears in the sidebar, and surface a sense-event-style toast
    // so the user knows their wake-up actually ran.
    window.__TAURI__.event.listen('wakeup-fired', (event) => {
        const p = event.payload || {};
        loadArcs();
        const decisionLabel = ({
            silent_approve: 'running',
            notify_and_proceed: 'running',
            human_confirm: 'awaiting approval',
            hard_block: 'blocked',
            no_decision: 'no decision',
        })[p.decision] || p.decision || '';
        showSenseNotification(
            'wake-up',
            'Scheduled',
            (p.instruction || '').slice(0, 80),
            decisionLabel,
            'medium',
            `Autonomy ${p.autonomy || 'safe_only'} — ${decisionLabel}`,
            'read',
            p.arc_id,
            p.decision === 'silent_approve' || p.decision === 'notify_and_proceed',
        );
    });

    // Listen for sense events (email, calendar, messaging, etc.)
    window.__TAURI__.event.listen('sense-event', (event) => {
        const { source, from, subject, body_preview,
                relevance, reason, suggested_action, arc_id, dispatched } = event.payload;
        showSenseNotification(source, from, subject, body_preview,
                              relevance, reason, suggested_action, arc_id, dispatched);
    });
}

// Route `<a target="_blank">` and any external http(s) anchor through the
// Tauri opener plugin so clicks actually reach the system browser. The
// WebView doesn't honor `target="_blank"` for navigation, so without this
// every external-link click is a silent no-op.
function installExternalLinkOpener() {
    document.addEventListener('click', (e) => {
        const a = e.target.closest('a');
        if (!a) return;
        const href = a.getAttribute('href') || '';
        if (!/^https?:\/\//i.test(href)) return;
        e.preventDefault();
        const opener = window.__TAURI__ && window.__TAURI__.opener;
        if (opener && opener.openUrl) {
            opener.openUrl(href).catch((err) => console.warn('openUrl failed:', err));
        } else {
            console.warn('opener plugin unavailable; cannot open', href);
        }
    });
}

function initTauri() {
    performance.mark('athen-init-start');
    if (window.__TAURI__ && window.__TAURI__.core) {
        invoke = window.__TAURI__.core.invoke;

        // Synchronous, lightweight: just registers .listen() handlers.
        // Must run before any task could fire (e.g. agent-stream).
        registerTauriEventListeners();
        installExternalLinkOpener();

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
        // Profile list is non-critical — defer so the chat UI paints first.
        scheduleIdle(() => loadAgentProfiles());
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
    // First fetch for the active-agents pill — the backend pulse will
    // drive subsequent refreshes via the `agents-changed` event listener
    // wired in `wireActiveAgentsPanel`. Seed the history feed too so the
    // Agent Control tab is non-empty on first open.
    scheduleIdle(() => refreshActiveAgents());
    scheduleIdle(() => refreshAgentRuns());
    // Initial composer-attach gate sync — loadSettings updates it later
    // whenever the user opens Settings, but on cold start we need to
    // run once so the paperclip's tooltip/state matches reality.
    scheduleIdle(async () => {
        try {
            const settings = await invoke('get_settings');
            updateComposerVisionGate(settings.providers);
        } catch (_) { /* non-critical */ }
    });
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

// Cached arc list, indexed by id. Populated by loadArcs() so UI elements
// like the per-arc profile picker can read metadata (active_profile_id,
// status, …) without a second IPC call.
const arcMetaById = new Map();

async function loadArcs() {
    if (!invoke) return;
    try {
        const arcs = await invoke('list_arcs');
        arcMetaById.clear();
        for (const a of arcs || []) {
            arcMetaById.set(a.id, a);
        }
        renderArcList(arcs || []);
        renderProfilePicker();
        renderReasoningPicker();
        renderTierPicker();
    } catch (err) {
        console.error('Failed to load arcs:', err);
    }
}

// ─── Agent profile picker ───

// Cached profile list. Loaded once at init via list_agent_profiles. Built-ins
// always sort first so the user sees curated specialists ahead of their own.
let agentProfiles = [];

async function loadAgentProfiles() {
    if (!invoke) return;
    try {
        agentProfiles = await invoke('list_agent_profiles');
        renderProfilePicker();
        renderReasoningPicker();
        renderTierPicker();
    } catch (err) {
        console.error('Failed to load agent profiles:', err);
        agentProfiles = [];
    }
}

function renderProfilePicker() {
    const sel = document.getElementById('arc-profile-picker');
    if (!sel) return;
    if (!agentProfiles || agentProfiles.length === 0) {
        sel.innerHTML = '<option value="default">Default</option>';
        sel.disabled = true;
        return;
    }
    sel.disabled = !activeArcId;
    const meta = activeArcId ? arcMetaById.get(activeArcId) : null;
    const activeId = (meta && meta.active_profile_id) || 'default';
    const opts = agentProfiles.map((p) => {
        const selected = p.id === activeId ? ' selected' : '';
        return `<option value="${escapeHtml(p.id)}"${selected}>${escapeHtml(p.display_name)}</option>`;
    });
    sel.innerHTML = opts.join('');
}

async function onProfileChange(ev) {
    if (!invoke || !activeArcId) return;
    const chosen = ev.target.value;
    // 'default' is the seeded fallback — clear the override on the arc so
    // future tasks resolve via the default profile path.
    const profileId = chosen === 'default' ? null : chosen;
    try {
        await invoke('set_arc_profile', { arcId: activeArcId, profileId });
        const meta = arcMetaById.get(activeArcId);
        if (meta) meta.active_profile_id = profileId;
    } catch (err) {
        console.error('set_arc_profile failed:', err);
        // Roll the dropdown back to whatever the arc actually has.
        renderProfilePicker();
        renderReasoningPicker();
        renderTierPicker();
    }
}

function renderReasoningPicker() {
    const sel = document.getElementById('arc-reasoning-picker');
    if (!sel) return;
    sel.disabled = !activeArcId;
    const meta = activeArcId ? arcMetaById.get(activeArcId) : null;
    sel.value = (meta && meta.reasoning_effort_override) || 'default';
}

async function onReasoningChange(ev) {
    if (!invoke || !activeArcId) return;
    const chosen = ev.target.value;
    const effort = chosen === 'default' ? null : chosen;
    try {
        await invoke('set_arc_reasoning_effort', { arcId: activeArcId, effort });
        const meta = arcMetaById.get(activeArcId);
        if (meta) meta.reasoning_effort_override = effort;
    } catch (err) {
        console.error('set_arc_reasoning_effort failed:', err);
        renderReasoningPicker();
    }
}

function renderTierPicker() {
    const sel = document.getElementById('arc-tier-picker');
    if (!sel) return;
    sel.disabled = !activeArcId;
    const meta = activeArcId ? arcMetaById.get(activeArcId) : null;
    sel.value = (meta && meta.tier_override) || 'auto';
}

async function onTierChange(ev) {
    if (!invoke || !activeArcId) return;
    const chosen = ev.target.value;
    // 'auto' is the cleared-override sentinel — backend treats it as None.
    const tier = chosen === 'auto' ? null : chosen;
    try {
        await invoke('set_arc_tier', { arcId: activeArcId, tier });
        const meta = arcMetaById.get(activeArcId);
        if (meta) meta.tier_override = tier;
    } catch (err) {
        console.error('set_arc_tier failed:', err);
        renderTierPicker();
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

        // Overflow menu: single kebab button reveals Rename / Compact / Branch / Delete.
        const actions = document.createElement('div');
        actions.className = 'session-item-actions';

        const menuBtn = document.createElement('button');
        menuBtn.className = 'session-action-btn arc-menu-trigger';
        menuBtn.title = 'More actions';
        menuBtn.innerHTML = '&#x22EF;'; // horizontal ellipsis
        menuBtn.addEventListener('click', (e) => {
            e.stopPropagation();
            toggleArcMenu(menuBtn, arc, item);
        });
        actions.appendChild(menuBtn);

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

// ─── Arc overflow menu ───
let openArcMenuEl = null;
let openArcMenuCleanup = null;
let openArcMenuTrigger = null;

function closeArcMenu() {
    if (openArcMenuCleanup) {
        openArcMenuCleanup();
        openArcMenuCleanup = null;
    }
    if (openArcMenuEl) {
        openArcMenuEl.remove();
        openArcMenuEl = null;
    }
    if (openArcMenuTrigger) {
        openArcMenuTrigger.classList.remove('active');
        openArcMenuTrigger = null;
    }
}

function toggleArcMenu(anchorEl, arc, itemEl) {
    if (openArcMenuTrigger === anchorEl) {
        closeArcMenu();
        return;
    }
    closeArcMenu();

    const menu = document.createElement('div');
    menu.className = 'arc-menu';
    menu.setAttribute('role', 'menu');

    const mkItem = (label, icon, onclick, danger) => {
        const btn = document.createElement('button');
        btn.className = 'arc-menu-item' + (danger ? ' danger' : '');
        btn.setAttribute('role', 'menuitem');
        const iconSpan = document.createElement('span');
        iconSpan.className = 'arc-menu-icon';
        iconSpan.innerHTML = icon;
        const labelSpan = document.createElement('span');
        labelSpan.className = 'arc-menu-label';
        labelSpan.textContent = label;
        btn.appendChild(iconSpan);
        btn.appendChild(labelSpan);
        btn.addEventListener('click', (e) => {
            e.stopPropagation();
            closeArcMenu();
            onclick();
        });
        return btn;
    };

    menu.appendChild(mkItem('Rename', '&#9998;', () => startRenameArc(itemEl, arc.id, arc.name)));
    menu.appendChild(mkItem('Compact', '&#x21A1;', () => {
        showToast('Compacting arc…', '');
        handleCompactArc(arc.id, null);
    }));
    menu.appendChild(mkItem('Branch', '&#x21B3;', () => branchFromArc(arc.id, arc.name)));

    const sep = document.createElement('div');
    sep.className = 'arc-menu-sep';
    menu.appendChild(sep);

    menu.appendChild(mkItem('Delete', '&#10005;', () => handleDeleteArc(arc.id), true));

    document.body.appendChild(menu);

    // Position relative to the trigger; flip up if it would overflow.
    const r = anchorEl.getBoundingClientRect();
    const menuW = menu.offsetWidth;
    const menuH = menu.offsetHeight;
    let left = r.right - menuW;
    if (left < 8) left = 8;
    if (left + menuW > window.innerWidth - 8) left = window.innerWidth - menuW - 8;
    let top = r.bottom + 4;
    if (top + menuH > window.innerHeight - 8) top = r.top - menuH - 4;
    menu.style.left = `${Math.round(left)}px`;
    menu.style.top = `${Math.round(top)}px`;

    openArcMenuEl = menu;
    openArcMenuTrigger = anchorEl;
    anchorEl.classList.add('active');

    const onDocClick = (e) => {
        if (!menu.contains(e.target) && e.target !== anchorEl) closeArcMenu();
    };
    const onKey = (e) => { if (e.key === 'Escape') closeArcMenu(); };
    const onScrollOrResize = () => closeArcMenu();
    // Defer so the originating click doesn't immediately close the menu.
    setTimeout(() => document.addEventListener('click', onDocClick, true), 0);
    document.addEventListener('keydown', onKey);
    sessionListEl.addEventListener('scroll', onScrollOrResize, true);
    window.addEventListener('resize', onScrollOrResize);
    openArcMenuCleanup = () => {
        document.removeEventListener('click', onDocClick, true);
        document.removeEventListener('keydown', onKey);
        sessionListEl.removeEventListener('scroll', onScrollOrResize, true);
        window.removeEventListener('resize', onScrollOrResize);
    };
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
    if (!invoke) return;
    // If we're on Settings/Calendar/etc., bring chat back regardless of
    // whether the arc is actually changing.
    returnToChatIfOnSubView();
    if (arcId === activeArcId) return;

    try {
        const entries = await invoke('switch_arc', { arcId });
        activeArcId = arcId;
        renderProfilePicker();
        renderReasoningPicker();
        renderTierPicker();

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

async function handleCompactArc(arcId, btnEl) {
    if (!invoke) return;
    if (btnEl) {
        btnEl.disabled = true;
        btnEl.classList.add('busy');
    }
    try {
        const result = await invoke('compact_arc', { arcId });
        if (result && result.compacted) {
            const before = result.tokens_before || 0;
            const after = result.tokens_after || 0;
            showToast(`Compacted: ${before} → ${after} tokens`, 'success');
            // If the compacted arc is the active one, refresh the view
            // so the new summary entry shows up in the timeline.
            if (arcId === activeArcId) {
                try {
                    const entries = await invoke('get_arc_history');
                    clearChatUI();
                    renderEntries(entries);
                } catch (err) {
                    console.error('Refresh after compact failed:', err);
                }
            }
            // Refresh sidebar so the entry_count badge updates.
            await loadArcs();
        } else {
            showToast('Nothing to compact yet (arc too short).', '');
        }
    } catch (err) {
        console.error('Compact arc failed:', err);
        showToast('Compact failed: ' + (err && err.toString ? err.toString() : 'unknown error'), 'error');
    } finally {
        if (btnEl) {
            btnEl.disabled = false;
            btnEl.classList.remove('busy');
        }
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
    // Fast linear pre-check. The previous regex `[\s:|-]+\|[\s:|-]+`
    // had two greedy classes that both included `|`, so a separator
    // line of N pipe/dash chars meant N choices for "which `|` is the
    // divider" — exponential backtracking in JavaScriptCore. A
    // base64-image-in-a-markdown-table page reader output (e.g. PHP
    // info) hit this and pegged the WebProcess long enough for
    // WebKit's 10s IPC watchdog to abort it.
    let hasPipe = false, hasDash = false;
    for (let i = 0; i < line.length; i++) {
        const c = line.charCodeAt(i);
        // tab, space, '-', ':', '|'
        if (c === 9 || c === 32 || c === 45 || c === 58 || c === 124) {
            if (c === 124) hasPipe = true;
            else if (c === 45) hasDash = true;
        } else {
            return null;
        }
    }
    if (!hasPipe || !hasDash) return null;
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
                                relevance, reason, suggestedAction, arcId, dispatched) {
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
    // When `dispatched` is true the agent is already working on this event —
    // showing user-action prompts (Draft Reply / Summarize / Add to Calendar)
    // would be misleading. Show a status badge + Open Arc instead.
    let actionsHtml = '';

    if (dispatched) {
        actionsHtml += '<span class="email-action-status">Athen is on it…</span>';
    } else if (source === 'calendar') {
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

// Fetch persisted attachments for an arc-reload bubble and splice
// thumbnails in just above the meta line. The list_for_event call may
// fail (no DB, expired event_id, etc.) — we silently skip in that case
// so the bubble still reads cleanly without an error chip.
async function hydrateAttachmentsAsync(messageRow, eventId) {
    if (!invoke || !messageRow) return;
    try {
        const items = await invoke('list_attachments_for_event', { eventId });
        if (!Array.isArray(items) || items.length === 0) return;
        const wrap = messageRow.querySelector('.message-content-wrap');
        if (!wrap) return;
        // Insert above the meta row if present, otherwise at the end.
        const chips = renderAttachmentChips(items);
        const metaRow = wrap.querySelector('.message-meta');
        if (metaRow) {
            wrap.insertBefore(chips, metaRow);
        } else {
            wrap.appendChild(chips);
        }
    } catch (err) {
        console.debug('hydrateAttachmentsAsync skipped:', err);
    }
}

// Build a row of inline attachment chips for a message bubble.
// Images render as a thumbnail (clickable to open full-size in a new
// tab via the data URL); non-image MIMEs render as a name + icon chip.
// `purged` rows are grayed out — bytes are gone but the user still sees
// the file existed in this turn.
function renderAttachmentChips(attachments) {
    const row = document.createElement('div');
    row.className = 'message-attachments';
    for (const att of attachments) {
        const isImage = (att.mime_type || '').toLowerCase().startsWith('image/');
        if (isImage && att.data_url && !att.purged) {
            const img = document.createElement('img');
            img.className = 'message-attachment-thumb';
            img.src = att.data_url;
            img.alt = att.name || 'image';
            img.title = att.name || '';
            img.addEventListener('click', () => {
                window.open(att.data_url, '_blank');
            });
            row.appendChild(img);
        } else {
            const chip = document.createElement('span');
            chip.className = 'message-attachment-chip';
            if (att.purged) chip.classList.add('purged');
            const icon = isImage ? '\u{1F5BC}️' : '\u{1F4CE}';
            const sizeStr = formatAttachmentSize(att.size_bytes);
            chip.innerHTML =
                `<span class="att-icon">${icon}</span>` +
                `<span class="att-name">${escapeHtml(att.name || 'attachment')}</span>` +
                (sizeStr ? `<span class="att-size">${sizeStr}</span>` : '') +
                (att.purged ? '<span class="att-purged">expired</span>' : '');
            row.appendChild(chip);
        }
    }
    return row;
}

function formatAttachmentSize(bytes) {
    if (typeof bytes !== 'number' || bytes <= 0) return '';
    if (bytes < 1024) return `${bytes} B`;
    if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
    return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
}

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

    // Inline attachment thumbnails (composer uploads on live send;
    // hydrated from `list_attachments_for_event` on arc reload). Sits
    // *under* the bubble — same row as the meta line — so the message
    // text reads first, then the chips, then the timestamp.
    if (meta && Array.isArray(meta.attachments) && meta.attachments.length) {
        const chips = renderAttachmentChips(meta.attachments);
        wrap.appendChild(chips);
    }

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

// Renderer for ApprovalRouter questions (install_package gate, future
// router-based gates). Distinct from addApprovalDialog: no risk score,
// caller-supplied choice list, resolved via submit_approval(question_id,
// choice_key) instead of approve_task.
function addApprovalQuestionDialog(question) {
    if (!question || !question.id) return;
    if (document.getElementById(`approval-q-${question.id}`)) return;

    const welcome = messagesEl.querySelector('.welcome-message');
    if (welcome) welcome.remove();

    const row = document.createElement('div');
    row.className = 'message-row assistant';
    row.id = `approval-q-${question.id}`;

    const avatar = document.createElement('div');
    avatar.className = 'message-avatar';
    avatar.textContent = 'A';

    const wrap = document.createElement('div');
    wrap.className = 'message-content-wrap';

    const bubble = document.createElement('div');
    bubble.className = 'message-bubble approval-bubble';

    const description = question.description
        ? `<div class="approval-description">${escapeHtml(question.description)}</div>`
        : '';

    bubble.innerHTML = `
        <div class="approval-header">
            <span class="approval-icon">&#9888;</span>
            <span class="approval-title">${escapeHtml(question.prompt || 'Approval needed')}</span>
        </div>
        <div class="approval-details">
            ${description}
        </div>
        <div class="approval-actions"></div>
    `;

    const actions = bubble.querySelector('.approval-actions');
    const choices = Array.isArray(question.choices) && question.choices.length > 0
        ? question.choices
        : [{ key: 'approve', label: 'Approve', kind: 'approve' },
           { key: 'deny', label: 'Deny', kind: 'deny' }];
    for (const c of choices) {
        const btn = document.createElement('button');
        btn.textContent = c.label || c.key;
        btn.dataset.choiceKey = c.key;
        btn.className = (c.kind === 'approve' || c.kind === 'allow_once' || c.kind === 'allow_always')
            ? 'btn-approve'
            : 'btn-deny';
        btn.addEventListener('click', () => {
            handleApprovalQuestion(question.id, c.key, row);
        });
        actions.appendChild(btn);
    }

    wrap.appendChild(bubble);

    const metaRow = document.createElement('div');
    metaRow.className = 'message-meta';
    metaRow.innerHTML = `<span class="message-time">${formatTime(new Date())}</span>`;
    wrap.appendChild(metaRow);

    row.appendChild(avatar);
    row.appendChild(wrap);
    messagesEl.appendChild(row);

    requestAnimationFrame(() => {
        messagesEl.parentElement.scrollTo({
            top: messagesEl.parentElement.scrollHeight,
            behavior: 'smooth'
        });
    });
}

async function handleApprovalQuestion(questionId, choiceKey, cardEl) {
    if (!invoke) return;
    if (cardEl) {
        cardEl.querySelectorAll('button').forEach(b => { b.disabled = true; });
    }
    try {
        await invoke('submit_approval', {
            questionId: questionId,
            choiceKey: choiceKey,
        });
    } catch (e) {
        console.error('submit_approval failed:', e);
    } finally {
        if (cardEl) cardEl.remove();
    }
}

async function handleApproval(taskId, approved) {
    if (!invoke) return;
    if (approvalsInFlight.has(taskId)) return;
    approvalsInFlight.add(taskId);

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

// ─── Composer image attachments ───
//
// Phase 1 vision support. The user can attach images to the next user
// turn via:
//   • the paperclip button (file picker, multi-select)
//   • drag-and-drop onto the composer
//   • Ctrl/Cmd-V paste while the composer is focused
//
// Images are kept entirely in-memory as { mime_type, dataUrl } and only
// sent to Rust at submit time, base64-only, via the `images` parameter
// of the `send_message` command. We do not persist them — Phase 2 will
// add proper attachment storage so reopened arcs can show the picture.

const MAX_COMPOSER_IMAGES = 5;
const MAX_COMPOSER_IMAGE_BYTES = 10 * 1024 * 1024; // 10 MB per image
const MAX_COMPOSER_ATTACHMENTS = 5;
const MAX_COMPOSER_ATTACHMENT_BYTES = 25 * 1024 * 1024; // 25 MB per file
const composerImagesEl = document.getElementById('composer-attachments');
const composerImageInputEl = document.getElementById('composer-image-input');
const composerAttachBtn = document.getElementById('composer-attach-btn');
let composerImages = []; // [{ id, mime_type, base64, dataUrl, name }]
let composerAttachments = []; // [{ id, mime_type, base64, name, size }]

function fmtBytes(n) {
    if (n < 1024) return `${n}B`;
    if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)}KB`;
    return `${(n / 1024 / 1024).toFixed(1)}MB`;
}

function refreshComposerImagesUI() {
    if (!composerImagesEl) return;
    composerImagesEl.innerHTML = '';
    if (composerImages.length === 0 && composerAttachments.length === 0) {
        composerImagesEl.classList.add('hidden');
        return;
    }
    composerImagesEl.classList.remove('hidden');
    for (const img of composerImages) {
        const chip = document.createElement('div');
        chip.className = 'composer-image-chip';
        chip.title = img.name || img.mime_type;
        chip.innerHTML = `
            <img src="${img.dataUrl}" alt="">
            <button type="button" class="composer-image-remove" aria-label="Remove image" data-id="${img.id}">×</button>
        `;
        composerImagesEl.appendChild(chip);
    }
    for (const att of composerAttachments) {
        const chip = document.createElement('div');
        chip.className = 'composer-file-chip';
        chip.title = `${att.name} (${att.mime_type}, ${fmtBytes(att.size)})`;
        chip.innerHTML = `
            <span class="composer-file-icon" aria-hidden="true">📄</span>
            <span class="composer-file-name">${att.name}</span>
            <span class="composer-file-size">${fmtBytes(att.size)}</span>
            <button type="button" class="composer-attachment-remove" aria-label="Remove file" data-id="${att.id}">×</button>
        `;
        composerImagesEl.appendChild(chip);
    }
}

function addComposerImageFromFile(file) {
    if (!file || !file.type || !file.type.startsWith('image/')) return;
    if (file.size > MAX_COMPOSER_IMAGE_BYTES) {
        addMessage('assistant', `Image "${file.name}" is too large (max ${(MAX_COMPOSER_IMAGE_BYTES / 1024 / 1024) | 0} MB).`, { isError: true });
        return;
    }
    if (composerImages.length >= MAX_COMPOSER_IMAGES) {
        addMessage('assistant', `Up to ${MAX_COMPOSER_IMAGES} images per turn.`, { isError: true });
        return;
    }
    const reader = new FileReader();
    reader.onload = () => {
        const dataUrl = String(reader.result || '');
        const m = dataUrl.match(/^data:([^;]+);base64,(.+)$/);
        if (!m) return;
        composerImages.push({
            id: `img-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`,
            mime_type: m[1],
            base64: m[2],
            dataUrl,
            name: file.name || '',
        });
        refreshComposerImagesUI();
    };
    reader.readAsDataURL(file);
}

// Non-image attachment (PDF, text/*). Distinct from images because the
// backend pipeline treats them differently — images go in as a
// multimodal user turn, attachments persist to AttachmentStore and get
// surfaced via the same path that handles inbound email/Telegram.
function addComposerAttachmentFromFile(file) {
    if (!file || !file.type) return;
    if (file.size > MAX_COMPOSER_ATTACHMENT_BYTES) {
        addMessage('assistant', `File "${file.name}" is too large (max ${(MAX_COMPOSER_ATTACHMENT_BYTES / 1024 / 1024) | 0} MB).`, { isError: true });
        return;
    }
    if (composerAttachments.length >= MAX_COMPOSER_ATTACHMENTS) {
        addMessage('assistant', `Up to ${MAX_COMPOSER_ATTACHMENTS} files per turn.`, { isError: true });
        return;
    }
    const reader = new FileReader();
    reader.onload = () => {
        const dataUrl = String(reader.result || '');
        const m = dataUrl.match(/^data:([^;]+);base64,(.+)$/);
        if (!m) return;
        composerAttachments.push({
            id: `att-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`,
            mime_type: m[1],
            base64: m[2],
            name: file.name || 'file',
            size: file.size,
        });
        refreshComposerImagesUI();
    };
    reader.readAsDataURL(file);
}

// Route a dropped/picked/pasted file to the right bucket based on MIME.
// Vision-gated paths still validate against the active provider; PDFs
// and text/* always work because the surfacing pipeline is provider-
// agnostic (text fallback + agent tools).
function addComposerFileFromFile(file) {
    if (!file || !file.type) return;
    if (file.type.startsWith('image/')) {
        addComposerImageFromFile(file);
    } else {
        addComposerAttachmentFromFile(file);
    }
}

if (composerAttachBtn && composerImageInputEl) {
    composerAttachBtn.addEventListener('click', () => {
        // Always open the picker. The active provider may refuse images,
        // but PDFs / text files flow through a text-based pipeline that
        // works regardless of vision support — so the paperclip is no
        // longer hard-gated. The change handler enforces the per-bucket
        // rules below.
        composerImageInputEl.click();
    });
    composerImageInputEl.addEventListener('change', () => {
        for (const f of composerImageInputEl.files || []) {
            addComposerFileFromFile(f);
        }
        composerImageInputEl.value = '';
    });
}

if (composerImagesEl) {
    composerImagesEl.addEventListener('click', (e) => {
        const imgBtn = e.target.closest('.composer-image-remove');
        if (imgBtn) {
            const id = imgBtn.dataset.id;
            composerImages = composerImages.filter((i) => i.id !== id);
            refreshComposerImagesUI();
            return;
        }
        const attBtn = e.target.closest('.composer-attachment-remove');
        if (attBtn) {
            const id = attBtn.dataset.id;
            composerAttachments = composerAttachments.filter((a) => a.id !== id);
            refreshComposerImagesUI();
        }
    });
}

if (inputEl) {
    inputEl.addEventListener('paste', (e) => {
        const items = e.clipboardData?.items || [];
        for (const item of items) {
            if (item.kind === 'file') {
                const file = item.getAsFile();
                if (file) addComposerFileFromFile(file);
            }
        }
    });
}

if (formEl) {
    formEl.addEventListener('dragover', (e) => {
        if (e.dataTransfer && Array.from(e.dataTransfer.items || []).some((it) => it.kind === 'file')) {
            e.preventDefault();
            formEl.classList.add('dragover');
        }
    });
    formEl.addEventListener('dragleave', () => formEl.classList.remove('dragover'));
    formEl.addEventListener('drop', (e) => {
        formEl.classList.remove('dragover');
        const files = e.dataTransfer?.files || [];
        if (!files.length) return;
        e.preventDefault();
        for (const f of files) addComposerFileFromFile(f);
    });
}

function consumeComposerImagesForSend() {
    if (composerImages.length === 0) return null;
    const payload = composerImages.map((i) => ({
        mime_type: i.mime_type,
        data: { kind: 'base64', data: i.base64 },
    }));
    composerImages = [];
    refreshComposerImagesUI();
    return payload;
}

function consumeComposerAttachmentsForSend() {
    if (composerAttachments.length === 0) return null;
    const payload = composerAttachments.map((a) => ({
        name: a.name,
        mime_type: a.mime_type,
        base64: a.base64,
    }));
    composerAttachments = [];
    refreshComposerImagesUI();
    return payload;
}

// Provider IDs whose adapters never accept multimodal regardless of the
// `supports_vision` toggle (DeepSeek standard chat, plain Ollama and
// llama.cpp wrappers). Mirrors the backend gate in commands.rs::send_message.
// Google (Gemini) carries images natively through `inlineData` so it's
// excluded — the user still needs to tick "Vision-capable model" though.
const NON_VISION_ADAPTER_IDS = new Set(['deepseek', 'ollama', 'llamacpp']);

function updateComposerVisionGate(providers) {
    if (!composerAttachBtn) return;
    let hint = '';
    let visionOk = false;
    if (Array.isArray(providers)) {
        const active = providers.find((p) => p && p.is_active);
        if (!active) {
            hint = 'No active LLM provider — open Settings to add one.';
        } else if (NON_VISION_ADAPTER_IDS.has(active.id)) {
            hint = `Active provider (${active.name || active.id}) cannot accept images. Switch to Claude 3.5+, GPT-4o, or any other vision-capable provider in Settings.`;
        } else if (!active.supports_vision) {
            hint = `Tick "Vision-capable model" on the active provider (${active.name || active.id}) in Settings to enable image input.`;
        } else {
            visionOk = true;
            hint = 'Attach image';
        }
    }
    composerAttachBtn.title = hint;
    composerAttachBtn.classList.toggle('disabled', !visionOk);
    composerAttachBtn.dataset.visionOk = visionOk ? '1' : '0';
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

    // Snapshot any attached images and clear the composer chips before
    // we render the user bubble so the next paste/drop starts clean.
    const composerImagesPayload = consumeComposerImagesForSend();
    const composerAttachmentsPayload = consumeComposerAttachmentsForSend();

    // Show user message with inline thumbnails for any composer-attached
    // media. The wire payload is shaped for the backend (no dataUrl), so
    // we reconstruct a data URL from the base64 we already have. This
    // matches the shape that `list_attachments_for_event` returns on
    // arc reload, so renderHistoryEntry can use the same renderer.
    const liveAttachments = [
        ...(composerImagesPayload || []).map((img, idx) => ({
            name: `pasted-image-${idx + 1}`,
            mime_type: img.mime_type,
            data_url: `data:${img.mime_type};base64,${img.data.data}`,
            purged: false,
        })),
        ...(composerAttachmentsPayload || []).map((a) => ({
            name: a.name,
            mime_type: a.mime_type,
            // size_bytes is approximate (raw base64 length × 3/4) but
            // good enough for the chip's "1.2 KB" hint.
            size_bytes: Math.floor((a.base64.length * 3) / 4),
            purged: false,
        })),
    ];
    addMessage('user', message, liveAttachments.length ? { attachments: liveAttachments } : undefined);
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
        const response = await invoke('send_message', {
            message,
            images: composerImagesPayload,
            attachments: composerAttachmentsPayload,
        });

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
                    let labelRaw = builtinToolLabel(rawName);
                    if (rawName === 'http_request' && tc.args && tc.args.endpoint) {
                        labelRaw = `Cloud API: ${tc.args.endpoint}`;
                    }
                    const label = escapeHtml(labelRaw);
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
            // Rescue path for Qwen-class models on llama.cpp `--jinja`:
            // when the model's entire reply is wrapped in <think>...</think>,
            // the parser routes everything to reasoning_content and the
            // content stream stays empty — so no bubble was created above
            // the thinking block. The non-streaming fallback in the
            // executor still returns the promoted reasoning as
            // `response.content`, so render it now to avoid a silent
            // turn that visibly stops at "Thinking...".
            if (!streamingBubble && response.content) {
                const wrap = streamedRow.querySelector('.message-content-wrap');
                if (wrap) {
                    const bubble = document.createElement('div');
                    bubble.className = 'message-bubble';
                    bubble.innerHTML = renderMarkdown(response.content);
                    // Sit above the meta row finalize() just appended,
                    // so the order stays: thinking → reply → time.
                    const metaRow = wrap.querySelector('.message-meta');
                    if (metaRow) wrap.insertBefore(bubble, metaRow);
                    else wrap.appendChild(bubble);
                }
            }
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
        body.appendChild(buildToolCardBlock(meta));
    }
    details.appendChild(body);

    messagesEl.appendChild(details);
}

// Build a single tool-call card. For `delegate_to_agent`, wraps the card
// with a nested expandable view that lazily fetches the sub-arc's tool
// calls and renders them inline (Claude-Code-style sub-agent activity).
// For other built-in tools, wraps the card in a <details> whose body
// shows the actual content that was read/written/fetched/edited — using
// the args+result already persisted on the entry, so the expansion is
// instant and stays consistent with what the agent actually saw.
function buildToolCardBlock(meta) {
    const toolName = meta.tool || '';
    const card = buildToolCard(meta);

    if (toolName === 'delegate_to_agent') {
        const result = meta.result && typeof meta.result === 'object' ? meta.result : null;
        const subArcId = result ? result.sub_arc_id : null;
        if (!subArcId) return card;

        const wrapper = document.createElement('div');
        wrapper.className = 'tool-card-block delegate-block';

        const details = document.createElement('details');
        details.className = 'sub-agent-steps';

        const summary = document.createElement('summary');
        summary.className = 'sub-agent-steps-summary';
        summary.textContent = '⤷ specialist steps';
        details.appendChild(summary);

        const body = document.createElement('div');
        body.className = 'sub-agent-steps-body';
        body.textContent = 'Loading...';
        details.appendChild(body);

        let loaded = false;
        details.addEventListener('toggle', async () => {
            if (!details.open || loaded) return;
            loaded = true;
            try {
                const entries = await invoke('get_arc_entries', { arcId: subArcId });
                renderSubAgentSteps(body, entries || []);
            } catch (e) {
                body.textContent = `Could not load specialist steps: ${e}`;
            }
        });

        wrapper.appendChild(card);
        wrapper.appendChild(details);
        return wrapper;
    }

    // Per-tool expanded body. Returns a DOM node when the tool has a
    // recognised renderer; otherwise the card stays a flat strip.
    const body = renderToolBody(meta);
    if (!body) return card;

    const details = document.createElement('details');
    details.className = 'tool-card-expand';
    const summary = document.createElement('summary');
    summary.className = 'tool-card-expand-summary';
    summary.appendChild(card);
    details.appendChild(summary);
    const bodyWrap = document.createElement('div');
    bodyWrap.className = 'tool-card-expand-body';
    bodyWrap.appendChild(body);
    details.appendChild(bodyWrap);
    return details;
}

// Map a tool-call's persisted metadata to a DOM body that renders what
// happened. Returns null when no specialised renderer exists — the
// caller falls back to a flat card with no expansion.
function renderToolBody(meta) {
    const tool = meta.tool || '';
    const args = (meta.args && typeof meta.args === 'object') ? meta.args : {};
    const result = (meta.result && typeof meta.result === 'object') ? meta.result : {};
    const error = typeof meta.error === 'string' ? meta.error : null;

    // Surface tool errors above any body — the user wants to see the
    // failure mode before the (often empty) result blob.
    const errorNode = error ? renderToolError(error) : null;
    let main = null;

    switch (tool) {
        case 'edit':            main = renderEditDiff(args, result); break;
        case 'read':            main = renderReadContent(args, result); break;
        case 'write':           main = renderWriteContent(args, result); break;
        case 'list_directory':  main = renderListDirectory(args, result); break;
        case 'grep':            main = renderGrep(args, result); break;
        case 'web_fetch':       main = renderWebFetch(args, result); break;
        case 'web_search':      main = renderWebSearch(args, result); break;
        case 'shell_execute':
        case 'shell_spawn':     main = renderShell(args, result); break;
        case 'shell_kill':
        case 'shell_logs':      main = renderShellMeta(args, result); break;
        case 'email_send':      main = renderEmailSend(args, result); break;
        case 'send_telegram':   main = renderSendTelegram(args, result); break;
        case 'memory_store':    main = renderMemoryStore(args, result); break;
        case 'memory_recall':   main = renderMemoryRecall(args, result); break;
        case 'calendar_list':   main = renderCalendarList(args, result); break;
        case 'calendar_create': main = renderCalendarCreate(args, result); break;
        case 'calendar_update': main = renderCalendarUpdate(args, result); break;
        case 'calendar_delete': main = renderCalendarDelete(args, result); break;
        case 'contacts_list':   main = renderContactsList(args, result); break;
        case 'contacts_search': main = renderContactsSearch(args, result); break;
        case 'contacts_create': main = renderContactsCreate(args, result); break;
        case 'contacts_update': main = renderContactsUpdate(args, result); break;
        case 'contacts_delete': main = renderContactsDelete(args, result); break;
        case 'install_package':       main = renderInstallPackage(args, result); break;
        case 'uninstall_package':     main = renderUninstallPackage(args, result); break;
        case 'list_installed_packages': main = renderListInstalledPackages(args, result); break;
        case 'create_wakeup':   main = renderCreateWakeup(args, result); break;
        case 'identity_add':    main = renderIdentityAdd(args, result); break;
        case 'http_request':    main = renderHttpRequest(args, result); break;
        default:
            // No bespoke layout — fall back to a labelled-fields dump
            // so the user still gets a structured view instead of the
            // card showing nothing on click.
            main = renderGenericFields(tool, args, result);
            break;
    }

    if (!main && !errorNode) return null;
    if (errorNode && !main) return errorNode;
    if (main && !errorNode) return main;

    const frag = document.createElement('div');
    frag.appendChild(errorNode);
    frag.appendChild(main);
    return frag;
}

function renderToolError(msg) {
    const div = document.createElement('div');
    div.className = 'tool-body-error';
    div.textContent = msg;
    return div;
}

// Detect language from a file path, deferring to the small offline
// highlighter loaded in syntax.js. Always-safe wrapper: if the
// highlighter didn't load (e.g. broken bundle), we fall back to plain
// escaped text instead of crashing the card render.
function _hl(content, lang) {
    if (window.AthenSyntax && typeof window.AthenSyntax.highlightCode === 'function') {
        return window.AthenSyntax.highlightCode(content, lang);
    }
    return escapeHtml(content);
}
function _detectLang(path) {
    if (window.AthenSyntax && typeof window.AthenSyntax.detectLanguage === 'function') {
        return window.AthenSyntax.detectLanguage(path);
    }
    return null;
}

// Naive line diff: split old_string / new_string by '\n' and render
// each as a coloured row. For exact-string Edits this matches what the
// user actually swapped — no LCS needed because the change is already
// minimised by definition. Each row gets per-language token colouring
// on top of the red/green band so structural code highlights stay
// visible inside the diff.
function renderEditDiff(args, _result) {
    const oldStr = typeof args.old_string === 'string' ? args.old_string : '';
    const newStr = typeof args.new_string === 'string' ? args.new_string : '';
    if (!oldStr && !newStr) return null;

    const path = typeof args.path === 'string' ? args.path : '';
    const lang = _detectLang(path);
    const wrap = document.createElement('div');
    wrap.className = 'tool-body-diff';
    if (path) {
        const head = document.createElement('div');
        head.className = 'tool-body-path';
        head.textContent = path;
        wrap.appendChild(head);
    }

    const block = document.createElement('pre');
    block.className = 'tool-body-diff-block';
    const pushRows = (text, kindClass, prefix) => {
        for (const line of text.split('\n')) {
            const row = document.createElement('div');
            row.className = 'diff-row ' + kindClass;
            row.innerHTML = '<span class="diff-marker">' + prefix + '</span>' +
                            _hl(line, lang);
            block.appendChild(row);
        }
    };
    pushRows(oldStr, 'diff-old', '- ');
    pushRows(newStr, 'diff-new', '+ ');
    wrap.appendChild(block);
    return wrap;
}

function renderReadContent(args, result) {
    const content = typeof result.content === 'string' ? result.content : '';
    const path = typeof args.path === 'string' ? args.path : '';
    if (!content) return null;
    const wrap = document.createElement('div');
    if (path) {
        const head = document.createElement('div');
        head.className = 'tool-body-path';
        const totalLines = typeof result.total_lines === 'number' ? result.total_lines : null;
        const returned = typeof result.lines_returned === 'number' ? result.lines_returned : null;
        head.textContent = totalLines && returned && returned < totalLines
            ? `${path} — showing ${returned} of ${totalLines} lines`
            : path;
        wrap.appendChild(head);
    }
    const pre = document.createElement('pre');
    pre.className = 'tool-body-code';
    pre.innerHTML = _hl(content, _detectLang(path));
    wrap.appendChild(pre);
    return wrap;
}

function renderWriteContent(args, result) {
    const content = typeof args.content === 'string' ? args.content : '';
    const path = typeof args.path === 'string' ? args.path : '';
    if (!content) return null;
    const wrap = document.createElement('div');
    if (path) {
        const head = document.createElement('div');
        head.className = 'tool-body-path';
        const bytes = typeof result.bytes_written === 'number' ? result.bytes_written : null;
        head.textContent = bytes != null ? `${path} (${bytes} bytes)` : path;
        wrap.appendChild(head);
    }
    const pre = document.createElement('pre');
    pre.className = 'tool-body-code';
    pre.innerHTML = _hl(content, _detectLang(path));
    wrap.appendChild(pre);
    return wrap;
}

// Inline icons for list_directory entries. Folders get a brand-tinted
// folder glyph so the eye sorts them out fast; files get a neutral
// file-text icon; symlinks an arrow chip.
const _LIST_ICON_FOLDER  = '<svg viewBox="0 0 24 24" width="13" height="13" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z"/></svg>';
const _LIST_ICON_FILE    = '<svg viewBox="0 0 24 24" width="13" height="13" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z"/><path d="M14 2v6h6"/></svg>';
const _LIST_ICON_SYMLINK = '<svg viewBox="0 0 24 24" width="13" height="13" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M10 13a5 5 0 0 0 7 0l3-3a5 5 0 0 0-7-7l-1 1"/><path d="M14 11a5 5 0 0 0-7 0l-3 3a5 5 0 0 0 7 7l1-1"/></svg>';

function renderListDirectory(args, result) {
    const entries = Array.isArray(result.entries) ? result.entries : null;
    if (!entries) return null;
    const path = typeof args.path === 'string' ? args.path : '.';
    const wrap = document.createElement('div');
    const head = document.createElement('div');
    head.className = 'tool-body-path';
    head.textContent = `${path} (${entries.length})`;
    wrap.appendChild(head);

    // Sort: directories first, then files, then symlinks; alpha within
    // each group. Mirrors how every file manager presents the same
    // data — far more scannable than insertion order.
    const sorted = entries.slice().sort((a, b) => {
        const rank = (e) => e && e.type === 'directory' ? 0 : (e && e.type === 'symlink' ? 2 : 1);
        const dr = rank(a) - rank(b);
        if (dr !== 0) return dr;
        return String(a && a.name || '').localeCompare(String(b && b.name || ''));
    });

    const list = document.createElement('ul');
    list.className = 'tool-body-list';
    for (const e of sorted) {
        const li = document.createElement('li');
        const t = e && e.type;
        const isDir = t === 'directory';
        const isSym = t === 'symlink';
        li.className = 'list-entry ' + (isDir ? 'dir' : (isSym ? 'sym' : 'file'));
        const icon = isDir ? _LIST_ICON_FOLDER
                   : isSym ? _LIST_ICON_SYMLINK
                   : _LIST_ICON_FILE;
        const name = (e && e.name) || '';
        li.innerHTML = '<span class="list-icon">' + icon + '</span>' +
                       '<span class="list-name">' + escapeHtml(isDir ? name + '/' : name) + '</span>';
        list.appendChild(li);
    }
    wrap.appendChild(list);
    return wrap;
}

function renderGrep(args, result) {
    const matches = typeof result.matches === 'string' ? result.matches : '';
    if (!matches) return null;
    const pattern = typeof args.pattern === 'string' ? args.pattern : '';
    const path = typeof args.path === 'string' ? args.path : '.';
    const wrap = document.createElement('div');
    const head = document.createElement('div');
    head.className = 'tool-body-path';
    head.textContent = pattern ? `"${pattern}" in ${path}` : path;
    wrap.appendChild(head);

    const pre = document.createElement('pre');
    pre.className = 'tool-body-code';
    // Highlight the matched substring inside each line so the eye
    // jumps to it. We escape-then-mark to keep the output safe even
    // if the pattern contains HTML metacharacters in the file.
    if (pattern) {
        const escaped = escapeHtml(matches);
        // Build a literal-pattern regex that ignores ripgrep's own
        // colour codes (we don't ship --color anyway, but stay safe).
        try {
            const rx = new RegExp(pattern.replace(/[.*+?^${}()|[\]\\]/g, '\\$&'), 'g');
            pre.innerHTML = escaped.replace(rx, (m) => '<mark class="grep-match">' + m + '</mark>');
        } catch {
            pre.textContent = matches;
        }
    } else {
        pre.textContent = matches;
    }
    wrap.appendChild(pre);
    return wrap;
}

function renderShell(args, result) {
    const cmd = typeof args.command === 'string' ? args.command : '';
    const stdout = typeof result.stdout === 'string' ? result.stdout : '';
    const stderr = typeof result.stderr === 'string' ? result.stderr : '';
    const exit = (typeof result.exit_code === 'number') ? result.exit_code : null;
    if (!cmd && !stdout && !stderr) return null;

    const wrap = document.createElement('div');
    if (cmd) {
        const head = document.createElement('div');
        head.className = 'tool-body-path mono';
        head.textContent = '$ ' + cmd + (exit != null ? ` → ${exit}` : '');
        wrap.appendChild(head);
    }
    if (stdout) wrap.appendChild(_shellPre(stdout, 'tool-body-code ansi'));
    if (stderr) {
        const label = document.createElement('div');
        label.className = 'tool-body-sublabel';
        label.textContent = 'stderr';
        wrap.appendChild(label);
        wrap.appendChild(_shellPre(stderr, 'tool-body-code ansi stderr'));
    }
    return wrap;
}

// Render a chunk of shell output. Branches on whether the text
// actually contains ANSI escapes — plain output (most short commands)
// goes through `textContent` to avoid spinning up the regex parser
// for nothing; coloured output (cargo, ls, npm, git) gets the small
// AthenAnsi pass.
function _shellPre(text, className) {
    const pre = document.createElement('pre');
    pre.className = className;
    if (window.AthenAnsi && window.AthenAnsi.hasAnsi(text)) {
        pre.innerHTML = window.AthenAnsi.toHtml(text);
    } else {
        pre.textContent = text;
    }
    return pre;
}

function renderWebFetch(args, result) {
    const content = typeof result.content === 'string' ? result.content : '';
    const url = (typeof result.url === 'string' && result.url) ||
                (typeof args.url === 'string' ? args.url : '');
    const title = typeof result.title === 'string' ? result.title : '';
    if (!content) return null;

    const wrap = document.createElement('div');
    if (url || title) {
        const head = document.createElement('div');
        head.className = 'tool-body-path web-fetch-head';
        if (title) {
            const t = document.createElement('span');
            t.className = 'web-fetch-title';
            t.textContent = title;
            head.appendChild(t);
        }
        if (url) {
            // "Open original ↗" — explicit link out instead of an
            // iframe, avoiding the prompt-injection / tracker pile-up
            // that comes with rendering arbitrary HTML in-window.
            const a = document.createElement('a');
            a.href = url;
            a.target = '_blank';
            a.rel = 'noopener noreferrer';
            a.className = 'web-fetch-link';
            a.textContent = url + ' ↗';
            head.appendChild(a);
        }
        wrap.appendChild(head);
    }
    // The page reader returns cleaned readability-mode markdown — but
    // a misbehaving site can slip raw HTML through (e.g. a WordPress
    // page emitted four `<link rel="stylesheet">` tags that survived
    // html2md, got injected via innerHTML below, and overrode every
    // app style with the foreign theme). Defensive scrub: strip the
    // tags that can pull external resources or restyle the document.
    // The backend reader has its own strip pass; this is a belt-and-
    // braces line so already-poisoned arc entries stay safe.
    const body = document.createElement('div');
    body.className = 'tool-body-prose';
    body.innerHTML = renderMarkdown(sanitizeWebContent(content));
    wrap.appendChild(body);
    return wrap;
}

// Remove every HTML tag that can pull external CSS, scripts, or
// document-replacing content from page-reader output before it reaches
// `innerHTML`. The match is intentionally coarse and case-insensitive;
// false positives strip the textual representation of a tag (acceptable
// in extracted-prose context), false negatives are caught at the next
// layer (the renderer itself only honours markdown syntax for inline
// markup, so raw `<div>`s flatten harmlessly).
function sanitizeWebContent(text) {
    if (typeof text !== 'string' || text.length === 0) return text;
    // Paired tags: drop body too. Anchored to `</tag>` (case-insensitive).
    const paired = ['script', 'style', 'iframe', 'object', 'embed', 'svg',
                    'noscript', 'template', 'head'];
    let out = text;
    for (const tag of paired) {
        const re = new RegExp(`<${tag}\\b[\\s\\S]*?</${tag}\\s*>`, 'gi');
        out = out.replace(re, '');
    }
    // Void / head-only tags: drop the open tag alone.
    const voidTags = ['link', 'meta', 'base'];
    for (const tag of voidTags) {
        const re = new RegExp(`<${tag}\\b[^>]*>`, 'gi');
        out = out.replace(re, '');
    }
    return out;
}

// ─── Shared layout helpers for the labeled-fields renderer style ───
// The vast majority of tools have the shape "small set of named values".
// Emit each as a `Label: value` row so the user reads the content the
// same way they would in any settings dialog.

// Render an array of [label, value, opts?] tuples as a labelled-fields
// grid. `value` may be: string | string[] | DOM Node | null. Opts:
//   { block: true } — value spans the full row instead of sitting next
//                     to the label (use for body text, descriptions).
//   { mono:  true } — value rendered in monospace.
//   { html:  true } — value is treated as already-safe HTML.
function renderFields(rows) {
    const wrap = document.createElement('div');
    wrap.className = 'tool-body-fields';
    for (const r of rows) {
        if (!r) continue;
        const [label, value, opts] = r;
        const o = opts || {};
        const isEmpty = value == null || value === '' ||
                        (Array.isArray(value) && value.length === 0);
        if (isEmpty) continue;

        const labelEl = document.createElement('div');
        labelEl.className = 'tool-field-label';
        labelEl.textContent = label;
        const valueEl = document.createElement('div');
        valueEl.className = 'tool-field-value' +
            (o.mono ? ' mono' : '') +
            (o.block ? ' block' : '');

        if (value instanceof Node) {
            valueEl.appendChild(value);
        } else if (Array.isArray(value)) {
            // Render array values as small chips so the eye can count
            // recipients / tags without parsing commas.
            for (const item of value) {
                const chip = document.createElement('span');
                chip.className = 'tool-field-chip';
                chip.textContent = String(item);
                valueEl.appendChild(chip);
            }
        } else if (o.html) {
            valueEl.innerHTML = String(value);
        } else {
            valueEl.textContent = String(value);
        }

        if (o.block) {
            // Block rows take their own line under the label.
            const blockWrap = document.createElement('div');
            blockWrap.className = 'tool-field-row block';
            blockWrap.appendChild(labelEl);
            blockWrap.appendChild(valueEl);
            wrap.appendChild(blockWrap);
        } else {
            wrap.appendChild(labelEl);
            wrap.appendChild(valueEl);
        }
    }
    return wrap.children.length ? wrap : null;
}

// Render a coloured pill — used for trust levels, autonomy bands,
// statuses ("created", "deleted") so they stand out from neutral text.
function renderPill(text, kind) {
    const span = document.createElement('span');
    span.className = 'tool-pill' + (kind ? ' tool-pill-' + kind : '');
    span.textContent = text;
    return span;
}

// Convenience: arrange a vertical list of cards (events, contacts,
// search hits) inside a scrollable panel.
function renderRowsList(rows) {
    const wrap = document.createElement('div');
    wrap.className = 'tool-body-rows';
    for (const r of rows) wrap.appendChild(r);
    return wrap;
}

// Format an ISO/RFC3339 string in local time. Returns the input
// unchanged if parsing fails — better than swallowing the value.
function fmtDateTime(s) {
    if (!s) return '';
    const d = new Date(s);
    if (isNaN(d.getTime())) return s;
    const opts = { year: 'numeric', month: 'short', day: '2-digit',
                   hour: '2-digit', minute: '2-digit' };
    try { return d.toLocaleString(undefined, opts); }
    catch { return s; }
}

// ─── Per-tool renderers ───

function renderEmailSend(args, result) {
    const to      = Array.isArray(args.to)  ? args.to  : (args.to ? [args.to] : []);
    const cc      = Array.isArray(args.cc)  ? args.cc  : [];
    const bcc     = Array.isArray(args.bcc) ? args.bcc : [];
    const subject = typeof args.subject === 'string' ? args.subject : '';
    const bodyText = typeof args.body_text === 'string' ? args.body_text : '';
    const bodyHtml = typeof args.body_html === 'string' ? args.body_html : '';
    const inReplyTo = typeof args.in_reply_to === 'string' ? args.in_reply_to : '';
    const messageId = typeof result.message_id === 'string' ? result.message_id : '';

    const bodyEl = document.createElement('div');
    bodyEl.className = 'tool-body-prose email-body';
    if (bodyText) bodyEl.innerHTML = renderMarkdown(bodyText);
    else if (bodyHtml) bodyEl.textContent = bodyHtml;

    return renderFields([
        ['Sent To',  to],
        cc.length  ? ['Cc',  cc]  : null,
        bcc.length ? ['Bcc', bcc] : null,
        ['Subject',  subject],
        bodyEl.children.length || bodyEl.textContent ? ['Body', bodyEl, { block: true }] : null,
        inReplyTo  ? ['In Reply To', inReplyTo, { mono: true }] : null,
        messageId  ? ['Message ID',  messageId, { mono: true }] : null,
    ]);
}

function renderSendTelegram(args, result) {
    const chatIdArg = args.chat_id;
    const text = typeof args.text === 'string' ? args.text : '';
    const replyTo = args.reply_to_message_id;
    const atts = Array.isArray(args.attachments) ? args.attachments : [];
    const chatIdResult = result.chat_id;
    const messageIds = Array.isArray(result.message_ids) ? result.message_ids : [];
    const toOwner = result.to_owner === true;

    const bodyEl = document.createElement('div');
    bodyEl.className = 'tool-body-prose';
    if (text) bodyEl.innerHTML = renderMarkdown(text);

    let attsEl = null;
    if (atts.length) {
        attsEl = document.createElement('div');
        attsEl.className = 'tool-body-rows';
        for (const a of atts) {
            const path = typeof a.path === 'string' ? a.path : '';
            const kind = typeof a.kind === 'string' ? a.kind : 'auto';
            const caption = typeof a.caption === 'string' ? a.caption : '';
            const row = document.createElement('div');
            row.className = 'tool-row';
            const lhs = document.createElement('span');
            lhs.className = 'mono';
            lhs.textContent = path;
            const rhs = document.createElement('span');
            rhs.className = 'tool-tag';
            rhs.textContent = kind;
            row.appendChild(lhs);
            row.appendChild(rhs);
            if (caption) {
                const cap = document.createElement('div');
                cap.className = 'tool-row-sub';
                cap.textContent = caption;
                row.appendChild(cap);
            }
            attsEl.appendChild(row);
        }
    }

    const chatLabel = chatIdResult != null ? String(chatIdResult)
                    : (chatIdArg != null ? String(chatIdArg) : '(owner default)');

    return renderFields([
        ['Chat', chatLabel + (toOwner ? ' (owner)' : ''), { mono: true }],
        replyTo != null ? ['Reply to', String(replyTo), { mono: true }] : null,
        bodyEl.children.length || bodyEl.textContent ? ['Text', bodyEl, { block: true }] : null,
        atts.length ? ['Attachments', attsEl, { block: true }] : null,
        messageIds.length ? ['Message IDs', messageIds.join(', '), { mono: true }] : null,
    ]);
}

function renderWebSearch(args, result) {
    const query = typeof args.query === 'string' ? args.query : '';
    const provider = typeof result.answered_by === 'string' ? result.answered_by
                  : (typeof result.provider === 'string' ? result.provider : '');
    const results = Array.isArray(result.results) ? result.results : [];

    const head = renderFields([
        ['Query', query],
        provider ? ['Provider', provider, { mono: true }] : null,
        ['Results', String(results.length)],
    ]);
    const wrap = document.createElement('div');
    if (head) wrap.appendChild(head);
    if (results.length) {
        const list = document.createElement('div');
        list.className = 'tool-body-rows search-hits';
        for (const r of results) {
            const row = document.createElement('div');
            row.className = 'search-hit';
            const title = document.createElement('a');
            title.className = 'search-hit-title';
            title.href = r.url || '#';
            title.target = '_blank';
            title.rel = 'noopener noreferrer';
            title.textContent = r.title || r.url || '(untitled)';
            row.appendChild(title);
            if (r.url) {
                const u = document.createElement('div');
                u.className = 'search-hit-url';
                u.textContent = r.url;
                row.appendChild(u);
            }
            if (r.snippet) {
                const s = document.createElement('div');
                s.className = 'search-hit-snippet';
                s.textContent = r.snippet;
                row.appendChild(s);
            }
            list.appendChild(row);
        }
        wrap.appendChild(list);
    }
    return wrap.children.length ? wrap : null;
}

function renderMemoryStore(args, _result) {
    const key   = typeof args.key   === 'string' ? args.key   : '';
    const value = typeof args.value === 'string' ? args.value : '';
    return renderFields([
        ['Key',   key,   { mono: true }],
        ['Value', value, { block: true }],
    ]);
}

function renderMemoryRecall(args, result) {
    const key = typeof args.key === 'string' ? args.key : '';
    if (Array.isArray(result.keys)) {
        return renderFields([
            ['Stored keys', result.keys.length ? result.keys : ['(none)']],
        ]);
    }
    if (result.found === false) {
        return renderFields([
            ['Key',    key, { mono: true }],
            ['Result', '(not found)'],
        ]);
    }
    const value = typeof result.value === 'string' ? result.value : '';
    return renderFields([
        ['Key',   key,   { mono: true }],
        ['Value', value, { block: true }],
    ]);
}

function renderCalendarList(args, result) {
    const start = typeof args.start === 'string' ? args.start : '';
    const end   = typeof args.end   === 'string' ? args.end   : '';
    const events = Array.isArray(result.events) ? result.events : [];

    const head = renderFields([
        ['Range', `${fmtDateTime(start)} → ${fmtDateTime(end)}`],
        ['Found', String(events.length)],
    ]);
    const wrap = document.createElement('div');
    if (head) wrap.appendChild(head);
    if (events.length) {
        const list = document.createElement('div');
        list.className = 'tool-body-rows event-list';
        for (const e of events) {
            const row = document.createElement('div');
            row.className = 'event-row';
            const title = document.createElement('div');
            title.className = 'event-title';
            title.textContent = e.title || '(untitled)';
            row.appendChild(title);
            const meta = document.createElement('div');
            meta.className = 'event-meta';
            const when = e.all_day
                ? `${fmtDateTime(e.start_time)} (all day)`
                : `${fmtDateTime(e.start_time)} → ${fmtDateTime(e.end_time)}`;
            meta.textContent = when;
            row.appendChild(meta);
            if (e.location) {
                const loc = document.createElement('div');
                loc.className = 'event-location';
                loc.textContent = '📍 ' + e.location;
                row.appendChild(loc);
            }
            if (e.category) {
                row.appendChild(renderPill(e.category, 'neutral'));
            }
            list.appendChild(row);
        }
        wrap.appendChild(list);
    }
    return wrap.children.length ? wrap : null;
}

function renderCalendarCreate(args, result) {
    const reminders = Array.isArray(args.reminder_minutes) ? args.reminder_minutes : [];
    return renderFields([
        ['Title',       args.title || result.title],
        ['Start',       fmtDateTime(args.start_time || result.start_time)],
        ['End',         fmtDateTime(args.end_time   || result.end_time)],
        args.all_day ? ['All Day', 'yes'] : null,
        ['Location',    args.location],
        ['Category',    args.category],
        ['Recurrence',  args.recurrence],
        reminders.length ? ['Reminders', reminders.map(m => `${m} min`)] : null,
        args.description ? ['Description', args.description, { block: true }] : null,
        ['Event ID',    result.id, { mono: true }],
    ]);
}

function renderCalendarUpdate(args, _result) {
    // Show only the fields the agent actually changed, plus the id.
    const rows = [['Event ID', args.id, { mono: true }]];
    const optional = [
        ['Title',       args.title],
        ['Start',       args.start_time ? fmtDateTime(args.start_time) : null],
        ['End',         args.end_time   ? fmtDateTime(args.end_time)   : null],
        ['All Day',     typeof args.all_day === 'boolean' ? (args.all_day ? 'yes' : 'no') : null],
        ['Location',    args.location],
        ['Category',    args.category],
        ['Color',       args.color, { mono: true }],
        ['Recurrence',  args.recurrence],
        ['Reminders',   Array.isArray(args.reminder_minutes) ? args.reminder_minutes.map(m => `${m} min`) : null],
        ['Description', args.description, { block: true }],
    ];
    for (const r of optional) {
        const v = r[1];
        if (v != null && v !== '' && !(Array.isArray(v) && v.length === 0)) rows.push(r);
    }
    return renderFields(rows);
}

function renderCalendarDelete(args, _result) {
    return renderFields([
        ['Event ID', args.id, { mono: true }],
        ['Status', renderPill('deleted', 'danger')],
    ]);
}

// Helper used by all contact rows. Identifiers come back as
// [{ value, kind }] — render each as `kind: value`.
function _contactRow(c) {
    if (!c) return document.createTextNode('');
    const row = document.createElement('div');
    row.className = 'contact-row';
    const head = document.createElement('div');
    head.className = 'contact-head';
    const name = document.createElement('span');
    name.className = 'contact-name';
    name.textContent = c.name || '(unnamed)';
    head.appendChild(name);
    if (c.trust_level) {
        const trust = String(c.trust_level).toLowerCase();
        const kind = trust === 'trusted' ? 'good'
                  : trust === 'known'    ? 'info'
                  : trust === 'blocked'  ? 'danger'
                  : 'neutral';
        head.appendChild(renderPill(c.trust_level, kind));
    }
    if (c.blocked) head.appendChild(renderPill('blocked', 'danger'));
    row.appendChild(head);
    const ids = Array.isArray(c.identifiers) ? c.identifiers : [];
    if (ids.length) {
        const ul = document.createElement('ul');
        ul.className = 'contact-ids';
        for (const i of ids) {
            const li = document.createElement('li');
            li.innerHTML = '<span class="contact-id-kind">' + escapeHtml(String(i.kind || '')) +
                           '</span><span class="contact-id-value">' + escapeHtml(String(i.value || '')) + '</span>';
            ul.appendChild(li);
        }
        row.appendChild(ul);
    }
    return row;
}

function renderContactsList(_args, result) {
    const contacts = Array.isArray(result.contacts) ? result.contacts : [];
    const head = renderFields([['Total', String(contacts.length)]]);
    const wrap = document.createElement('div');
    if (head) wrap.appendChild(head);
    if (contacts.length) {
        const list = document.createElement('div');
        list.className = 'tool-body-rows';
        for (const c of contacts) list.appendChild(_contactRow(c));
        wrap.appendChild(list);
    }
    return wrap.children.length ? wrap : null;
}

function renderContactsSearch(args, result) {
    const contacts = Array.isArray(result.contacts) ? result.contacts : [];
    const head = renderFields([
        ['Query', args.query],
        ['Found', String(contacts.length)],
    ]);
    const wrap = document.createElement('div');
    if (head) wrap.appendChild(head);
    if (contacts.length) {
        const list = document.createElement('div');
        list.className = 'tool-body-rows';
        for (const c of contacts) list.appendChild(_contactRow(c));
        wrap.appendChild(list);
    }
    return wrap.children.length ? wrap : null;
}

function renderContactsCreate(args, result) {
    const ids = Array.isArray(args.identifiers) ? args.identifiers : [];
    const idChips = ids.map(i => `${i.kind || '?'}: ${i.value || ''}`);
    return renderFields([
        ['Name',        args.name],
        ['Trust Level', args.trust_level || 'Neutral'],
        ['Identifiers', idChips],
        ['Contact ID',  (result && result.id) || (result && result.contact && result.contact.id), { mono: true }],
    ]);
}

function renderContactsUpdate(args, _result) {
    const ids = Array.isArray(args.identifiers)
        ? args.identifiers.map(i => `${i.kind || '?'}: ${i.value || ''}`)
        : null;
    const rows = [['Contact ID', args.id, { mono: true }]];
    if (args.name) rows.push(['Name', args.name]);
    if (args.trust_level) rows.push(['Trust Level', args.trust_level]);
    if (ids) rows.push(['Identifiers', ids]);
    return renderFields(rows);
}

function renderContactsDelete(args, _result) {
    return renderFields([
        ['Contact ID', args.id, { mono: true }],
        ['Status', renderPill('deleted', 'danger')],
    ]);
}

function renderInstallPackage(args, result) {
    return renderFields([
        ['Runtime', args.runtime],
        ['Package', args.package, { mono: true }],
        ['Reason',  args.reason, { block: true }],
        result.installed_version ? ['Installed', result.installed_version, { mono: true }] : null,
    ]);
}

function renderUninstallPackage(args, _result) {
    return renderFields([
        ['Runtime', args.runtime],
        ['Package', args.package, { mono: true }],
    ]);
}

function renderListInstalledPackages(_args, result) {
    // Result shape varies by version of the toolbox manifest reader;
    // be defensive and surface whatever we find.
    const py   = Array.isArray(result.python) ? result.python
              : (result.runtimes && Array.isArray(result.runtimes.python) ? result.runtimes.python : []);
    const node = Array.isArray(result.node)   ? result.node
              : (result.runtimes && Array.isArray(result.runtimes.node)   ? result.runtimes.node   : []);
    const fmt = (arr) => arr.map(p => typeof p === 'string' ? p : (p.name + (p.version ? `@${p.version}` : '')));
    return renderFields([
        py.length   ? ['Python (pip)', fmt(py)]   : null,
        node.length ? ['Node (npm)',   fmt(node)] : null,
        (!py.length && !node.length) ? ['Installed', '(none)'] : null,
    ]);
}

function renderCreateWakeup(args, result) {
    const sched = args.schedule || {};
    let when = '';
    if (sched.kind === 'one_shot') {
        when = sched.in ? `in ${sched.in}` : `at ${fmtDateTime(sched.at)}`;
    } else if (sched.kind === 'interval') {
        when = `every ${sched.every_seconds}s`;
    } else if (sched.kind === 'cron') {
        when = `cron: ${sched.expr}`;
    }
    const tools = Array.isArray(args.tool_allowlist) ? args.tool_allowlist : [];
    const contacts = Array.isArray(args.contact_allowlist) ? args.contact_allowlist : [];
    const autonomy = args.autonomy || (result && result.autonomy) || 'safe_only';
    const autonomyKind = autonomy === 'auto' ? 'warning'
                       : autonomy === 'notify_only' ? 'info' : 'good';
    return renderFields([
        ['When',         when],
        ['Instruction',  args.instruction, { block: true }],
        ['Profile',      args.profile || 'assistant'],
        ['Autonomy',     renderPill(autonomy, autonomyKind)],
        tools.length    ? ['Tools',    tools]    : null,
        contacts.length ? ['Contacts', contacts] : null,
        args.preferred_channel ? ['Notify Via', args.preferred_channel] : null,
        result.wakeup_id    ? ['Wake-up ID',  result.wakeup_id, { mono: true }] : null,
        result.next_fire_at ? ['Next Fire',   fmtDateTime(result.next_fire_at)] : null,
        result.computed_impact ? ['Computed Risk', renderPill(result.computed_impact, 'neutral')] : null,
    ]);
}

function renderIdentityAdd(args, result) {
    const category = (args.category || result.category || '').toString();
    const body = (args.body || result.body || '').toString();
    const applies = Array.isArray(args.applies_to) && args.applies_to.length
        ? args.applies_to.map(String)
        : (Array.isArray(result.applies_to) && result.applies_to.length
            ? result.applies_to.map(String)
            : ['Always']);
    const isRule = category === 'rules';
    const categoryNode = isRule
        ? renderPill(category + ' (review)', 'warning')
        : renderPill(category, 'neutral');
    return renderFields([
        ['Category',   categoryNode],
        ['Body',       body, { block: true }],
        ['Applies to', applies],
        result.id ? ['Entry ID', result.id, { mono: true }] : null,
    ]);
}

function renderHttpRequest(args, result) {
    const endpoint = String(args.endpoint || result.endpoint || '');
    const method = String(args.method || 'GET').toUpperCase();
    const path = String(args.path || '');
    const status = result.status;
    const latency = result.latency_ms;
    const bytes = result.body_bytes;
    const contentType = result.content_type || '';

    const ok = typeof status === 'number' && status >= 200 && status < 300;
    const statusNode = (status !== undefined)
        ? renderPill(`${status}${ok ? ' OK' : ''}`, ok ? 'success' : 'warning')
        : null;

    // Body lands as either parsed JSON (object) or {raw_text: '...'} for
    // non-JSON responses. Render JSON pretty-printed; raw_text inline.
    let bodyDisplay = '';
    let bodyMono = true;
    let bodyBlock = true;
    if (result.body && typeof result.body === 'object') {
        if (typeof result.body.raw_text === 'string') {
            bodyDisplay = result.body.raw_text;
        } else if (typeof result.body.parse_error === 'string') {
            bodyDisplay = `Parse error: ${result.body.parse_error}\n\n${result.body.raw_text || ''}`;
        } else {
            try {
                bodyDisplay = JSON.stringify(result.body, null, 2);
            } catch (_) {
                bodyDisplay = String(result.body);
            }
        }
    }
    // Cap to 4k chars in the UI — full body is in the agent's context anyway.
    if (bodyDisplay.length > 4000) {
        bodyDisplay = bodyDisplay.slice(0, 4000) + '\n… (truncated)';
    }

    const fields = [
        ['Endpoint',     endpoint],
        ['Method',       renderPill(method, method === 'GET' ? 'neutral' : 'warning')],
        ['Path',         path, { mono: true }],
        statusNode ? ['Status', statusNode] : null,
        (typeof latency === 'number') ? ['Latency', `${latency} ms`] : null,
        (typeof bytes === 'number')  ? ['Bytes',   bytes.toLocaleString()] : null,
        contentType ? ['Content-Type', contentType, { mono: true }] : null,
        bodyDisplay ? ['Response',     bodyDisplay, { block: bodyBlock, mono: bodyMono }] : null,
    ];
    return renderFields(fields);
}

function renderShellMeta(args, result) {
    return renderFields([
        ['PID',     args.pid, { mono: true }],
        result.stdout ? ['Output', result.stdout, { block: true, mono: true }] : null,
        result.signal ? ['Signal', result.signal] : null,
    ]);
}

// Last-resort renderer for any tool we don't have a bespoke layout
// for. Strips noise (timestamps, internal ids, raw blobs) and shows
// remaining args + result entries as labelled fields. Better than the
// card showing nothing on click; trivial to upgrade later by adding a
// dedicated case to `renderToolBody`'s switch.
function renderGenericFields(_tool, args, result) {
    const NOISE = new Set([
        'execution_time_ms', 'message', 'success',
    ]);
    const rows = [];
    const pushRows = (obj, prefix) => {
        if (!obj || typeof obj !== 'object' || Array.isArray(obj)) return;
        for (const [k, v] of Object.entries(obj)) {
            if (NOISE.has(k)) continue;
            if (v == null || v === '') continue;
            const label = (prefix ? prefix + ' ' : '') + _humanizeKey(k);
            if (Array.isArray(v)) {
                if (v.length === 0) continue;
                if (v.every(x => typeof x === 'string' || typeof x === 'number')) {
                    rows.push([label, v.map(String)]);
                } else {
                    // Compact JSON for arrays of objects.
                    rows.push([label, JSON.stringify(v), { mono: true, block: true }]);
                }
            } else if (typeof v === 'object') {
                rows.push([label, JSON.stringify(v), { mono: true, block: true }]);
            } else if (typeof v === 'string' && v.length > 80) {
                rows.push([label, v, { block: true }]);
            } else {
                rows.push([label, String(v)]);
            }
        }
    };
    pushRows(args, '');
    pushRows(result, '→');
    return renderFields(rows);
}

function _humanizeKey(k) {
    return String(k)
        .replace(/[_-]+/g, ' ')
        .replace(/\b\w/g, c => c.toUpperCase());
}

// Build the inner card element only — no wrappers, no nested rendering.
function buildToolCard(meta) {
    const toolName = meta.tool || '';
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
    let labelText = icon ? builtinToolLabel(toolName) : toolName;
    // http_request reads as "Cloud API" by default; promote to
    // "Cloud API: <endpoint>" so the user can tell which API the
    // agent hit at a glance, without expanding the card.
    if (toolName === 'http_request') {
        const ep = (meta.args && meta.args.endpoint) || '';
        if (ep) labelText = `Cloud API: ${ep}`;
    }
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
    return card;
}

// Render the sub-agent's tool_call entries as a vertical list of cards,
// each with a "view result" button that toggles the full JSON metadata.
function renderSubAgentSteps(container, entries) {
    container.innerHTML = '';
    const toolCalls = (entries || []).filter(e => e.entry_type === 'tool_call');
    if (toolCalls.length === 0) {
        container.textContent = '(specialist used no tools)';
        return;
    }
    for (const tc of toolCalls) {
        const meta = parseEntryMetadata(tc.metadata) || {};
        const row = document.createElement('div');
        row.className = 'sub-agent-step-row';

        row.appendChild(buildToolCard(meta));

        // "View result" toggle: dumps the full meta.result/error JSON.
        const detailToggle = document.createElement('details');
        detailToggle.className = 'sub-agent-step-detail';
        const sum = document.createElement('summary');
        sum.textContent = 'view result';
        detailToggle.appendChild(sum);
        const pre = document.createElement('pre');
        pre.className = 'sub-agent-step-json';
        const payload = {
            args: meta.args ?? null,
            result: meta.result ?? null,
            error: meta.error ?? null,
        };
        pre.textContent = JSON.stringify(payload, null, 2);
        detailToggle.appendChild(pre);
        row.appendChild(detailToggle);

        container.appendChild(row);
    }
}

// Render a single non-tool-call history entry. tool_call entries should be
// routed through renderToolGroup via buildRenderUnits, not here.
function renderHistoryEntry(entry) {
    if (entry.entry_type === 'message') {
        const meta = parseEntryMetadata(entry.metadata) || {};
        const eventId = meta.attachment_event_id || (meta.source === 'user_upload' ? meta.event_id : null);
        if (eventId) {
            // Render synchronously without thumbnails first, then patch
            // them in once the bytes have been hydrated from disk.
            // Splitting it this way avoids stalling history render on the
            // file reads — long arcs with many attachments would
            // otherwise serialize behind every Tauri round-trip.
            addMessage(entry.source, entry.content);
            const lastRow = messagesEl.lastElementChild;
            if (lastRow && lastRow.classList.contains('message-row')) {
                hydrateAttachmentsAsync(lastRow, eventId);
            }
        } else {
            addMessage(entry.source, entry.content);
        }
    } else if (entry.entry_type === 'email_event') {
        const meta = parseEntryMetadata(entry.metadata) || {};
        addEmailEntry(entry.content, meta);
    } else if (entry.entry_type === 'summary') {
        addSummaryEntry(entry.content, entry.metadata);
    } else if (entry.entry_type === 'tool_call') {
        // Fallback for callers that didn't go through buildRenderUnits.
        renderToolGroup([entry]);
    }
}

// Render a compaction summary as a collapsed "Earlier in this arc..."
// block. The full summary is in the <details> body; the agent sees
// this content during dispatch but the user can ignore it most of
// the time. Older entries above the summary remain in the timeline
// (they are not deleted), so the user can still scroll to see what
// the summary covers.
function addSummaryEntry(content, metadataRaw) {
    const meta = parseEntryMetadata(metadataRaw) || {};
    const row = document.createElement('div');
    row.className = 'message-row system';
    const details = document.createElement('details');
    details.className = 'arc-summary-block';
    const sum = document.createElement('summary');
    let label = '\u{1F5C2}️  Earlier in this arc — compacted';
    if (typeof meta.summarized_entries === 'number') {
        label += ` (${meta.summarized_entries} entries)`;
    }
    if (typeof meta.tokens_before === 'number') {
        label += ` · ~${meta.tokens_before} tokens collapsed`;
    }
    sum.textContent = label;
    details.appendChild(sum);
    const body = document.createElement('pre');
    body.className = 'arc-summary-body';
    body.textContent = content;
    details.appendChild(body);
    row.appendChild(details);
    messagesEl.appendChild(row);
}

// Per-arc queue of approval-question payloads that arrived while the
// user was viewing a different arc. Drained when they switch to that
// arc — keeps the card from getting lost in the wrong chat view.
const pendingApprovalQuestionsByArc = new Map();

function stashApprovalQuestionForArc(q) {
    if (!q || !q.id || !q.arc_id) return;
    const list = pendingApprovalQuestionsByArc.get(q.arc_id) || [];
    if (!list.some(existing => existing.id === q.id)) {
        list.push(q);
        pendingApprovalQuestionsByArc.set(q.arc_id, list);
    }
}

function drainPendingApprovalQuestionsForActiveArc() {
    if (!activeArcId) return;
    const list = pendingApprovalQuestionsByArc.get(activeArcId);
    if (!list || list.length === 0) return;
    pendingApprovalQuestionsByArc.delete(activeArcId);
    for (const q of list) addApprovalQuestionDialog(q);
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
    drainPendingApprovalQuestionsForActiveArc();
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
        returnToChatIfOnSubView();
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

const arcProfilePicker = document.getElementById('arc-profile-picker');
if (arcProfilePicker) {
    arcProfilePicker.addEventListener('change', onProfileChange);
}

const arcReasoningPicker = document.getElementById('arc-reasoning-picker');
if (arcReasoningPicker) {
    arcReasoningPicker.addEventListener('change', onReasoningChange);
}

const arcTierPicker = document.getElementById('arc-tier-picker');
if (arcTierPicker) {
    arcTierPicker.addEventListener('change', onTierChange);
}

const newProfileBtn = document.getElementById('new-profile-btn');
if (newProfileBtn) {
    newProfileBtn.addEventListener('click', () => openProfileEditor('create', null));
}

const clearToolboxBtn = document.getElementById('clear-toolbox-btn');
if (clearToolboxBtn) {
    clearToolboxBtn.addEventListener('click', handleClearToolbox);
}
const profileModalClose = document.getElementById('profile-modal-close');
if (profileModalClose) {
    profileModalClose.addEventListener('click', closeProfileEditor);
}
const profileModalCancel = document.getElementById('profile-modal-cancel');
if (profileModalCancel) {
    profileModalCancel.addEventListener('click', closeProfileEditor);
}
const profileModalSave = document.getElementById('profile-modal-save');
if (profileModalSave) {
    profileModalSave.addEventListener('click', saveProfileFromEditor);
}
const profileModalOverlay = document.getElementById('profile-modal-overlay');
if (profileModalOverlay) {
    profileModalOverlay.addEventListener('click', (ev) => {
        if (ev.target === profileModalOverlay) closeProfileEditor();
    });
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

// Provider catalog — single source of truth. Populated at startup from
// the backend's `list_provider_catalog` command and used by both the
// onboarding wizard and the settings provider templates. The "custom"
// entry stays frontend-only since it's a UI affordance rather than a
// real provider id.
let PROVIDER_CATALOG = [];
const CUSTOM_PROVIDER_ENTRY = {
    id: 'custom',
    name: 'Custom Provider',
    provider_type: 'cloud',
    default_base_url: '',
    default_model: '',
    api_key_hint: 'sk-...',
};

function providerById(id) {
    return PROVIDER_CATALOG.find((p) => p.id === id) || (id === 'custom' ? CUSTOM_PROVIDER_ENTRY : null);
}

// Legacy alias — kept so callers reading `PROVIDER_DEFAULTS[id].base_url`
// keep working without churn. Lazily projects the catalog onto the old
// shape: { name, base_url, model, type }.
const PROVIDER_DEFAULTS = new Proxy({}, {
    get(_, id) {
        const entry = providerById(id);
        if (!entry) return undefined;
        return {
            name: entry.name,
            base_url: entry.default_base_url,
            model: entry.default_model,
            family: entry.default_family,
            type: entry.provider_type,
        };
    },
});

async function loadProviderCatalog() {
    if (!invoke || PROVIDER_CATALOG.length > 0) return;
    try {
        PROVIDER_CATALOG = await invoke('list_provider_catalog');
    } catch (e) {
        console.warn('[athen] list_provider_catalog failed:', e);
        PROVIDER_CATALOG = [];
    }
}

// Per-model quirks: family catalog. Loaded once and reused by every provider
// card's Model-family dropdown. Each entry: { id, label, default_slug }.
let MODEL_FAMILIES = [];

async function loadModelFamilies() {
    if (!invoke || MODEL_FAMILIES.length > 0) return;
    try {
        MODEL_FAMILIES = await invoke('list_model_families');
    } catch (e) {
        console.warn('[athen] list_model_families failed:', e);
        MODEL_FAMILIES = [];
    }
}

const SECURITY_HINTS = {
    assistant: 'Standard risk evaluation. The agent asks for approval on risky actions.',
    bunker:    'Maximum caution. Everything above read-only requires your approval.',
    yolo:      'Minimal friction. Only critical actions (financial, system config) need approval.',
};

function showSettings() {
    appView.style.display = 'none';
    timelineView?.classList.add('hidden');
    calendarView?.classList.add('hidden');
    document.getElementById('wakeups-view')?.classList.add('hidden');
    document.getElementById('agent-control-view')?.classList.add('hidden');
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
    document.getElementById('wakeups-view')?.classList.add('hidden');
    document.getElementById('agent-control-view')?.classList.add('hidden');
    contactsView?.classList.add('hidden');
    notificationsView?.classList.add('hidden');
    document.getElementById('memory-view')?.classList.add('hidden');
    document.getElementById('sidebar').style.display = '';
    if (timelineRefreshInterval) { clearInterval(timelineRefreshInterval); timelineRefreshInterval = null; }
    appView.style.display = 'flex';
    inputEl.focus();
}

// Returns true if any non-chat top-level view is currently visible.
function isOnSubView() {
    const ids = ['settings-view', 'timeline-view', 'calendar-view',
                 'contacts-view', 'notifications-view', 'memory-view',
                 'wakeups-view', 'agent-control-view'];
    return ids.some((id) => {
        const el = document.getElementById(id);
        return el && !el.classList.contains('hidden');
    });
}

// If the user is on Settings/Contacts/etc., return them to chat. No-op otherwise.
// Used by arc switching and new-arc so navigation feels seamless.
function returnToChatIfOnSubView() {
    if (isOnSubView()) showChat();
}

async function loadSettings() {
    if (!invoke) return;
    try {
        const settings = await invoke('get_settings');
        renderProviders(settings.providers);
        updateComposerVisionGate(settings.providers);
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
            // SMTP fields
            document.getElementById('email-smtp-server').value = settings.email.smtp_server || '';
            document.getElementById('email-smtp-port').value = settings.email.smtp_port || 587;
            document.getElementById('email-smtp-username').value = settings.email.smtp_username || '';
            document.getElementById('email-smtp-use-tls').checked = !!settings.email.smtp_use_tls;
            document.getElementById('email-from-address').value = settings.email.from_address || '';
            const smtpPwField = document.getElementById('email-smtp-password');
            if (settings.email.has_smtp_password) {
                smtpPwField.placeholder = '••••••••  (saved)';
            }
            toggleEmailFields(settings.email.enabled);
        }

        // Populate telegram settings
        if (settings.telegram) {
            document.getElementById('telegram-enabled').checked = settings.telegram.enabled;
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
        // Populate web search settings — keys are NOT echoed verbatim;
        // backend returns has-key + masked hint. Empty inputs mean
        // "leave unchanged" on save.
        if (settings.web_search) {
            const braveInput = document.getElementById('web-search-brave');
            const tavilyInput = document.getElementById('web-search-tavily');
            if (braveInput) {
                braveInput.value = '';
                braveInput.placeholder = settings.web_search.brave_configured
                    ? settings.web_search.brave_hint + '  (saved — leave blank to keep)'
                    : 'BSA…';
            }
            if (tavilyInput) {
                tavilyInput.value = '';
                tavilyInput.placeholder = settings.web_search.tavily_configured
                    ? settings.web_search.tavily_hint + '  (saved — leave blank to keep)'
                    : 'tvly-…';
            }
        }

        await loadMcpCatalog();
        await loadMcpServers();
        await loadToolboxPackages();
        await loadGrants();
        await loadProfileManager();
        await loadIdentityManager();
        await loadCloudApis();
        await loadAttachmentPolicySettings();
        await loadOwnerContact();

        // Let opt-in listeners (e.g. the email setup wizard's connected-pill
        // + advanced-auto-open logic) react after every settings field is in
        // the DOM. Keep this as the last step so listeners can rely on the
        // form being fully populated.
        window.dispatchEvent(new CustomEvent('athen:settings-loaded'));
    } catch (err) {
        console.error('Failed to load settings:', err);
        showToast('Failed to load settings: ' + err, 'error');
    }
}

// Map well-known MCP catalog icon names to inline SVG markup. The
// catalog stores short names like "folder" rather than emoji or paths so
// the same string can drive different icon sets per UI; the frontend is
// where those names get resolved. Returns `null` when the name is
// unknown so the caller can render a fallback.
function mcpIconSvg(name) {
    const size = 18;
    const wrap = (inner) =>
        `<svg viewBox="0 0 24 24" width="${size}" height="${size}" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${inner}</svg>`;
    switch ((name || '').toLowerCase()) {
        case 'folder':
        case 'files':
            return wrap('<path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z"/>');
        case 'globe':
        case 'web':
            return wrap('<circle cx="12" cy="12" r="10"/><line x1="2" y1="12" x2="22" y2="12"/><path d="M12 2a15.3 15.3 0 0 1 4 10 15.3 15.3 0 0 1-4 10 15.3 15.3 0 0 1-4-10 15.3 15.3 0 0 1 4-10z"/>');
        case 'terminal':
        case 'shell':
            return wrap('<polyline points="4 17 10 11 4 5"/><line x1="12" y1="19" x2="20" y2="19"/>');
        case 'database':
        case 'db':
            return wrap('<ellipse cx="12" cy="5" rx="9" ry="3"/><path d="M3 5v14a9 3 0 0 0 18 0V5"/><path d="M3 12a9 3 0 0 0 18 0"/>');
        case 'mail':
        case 'email':
            return wrap('<path d="M4 4h16c1.1 0 2 .9 2 2v12c0 1.1-.9 2-2 2H4c-1.1 0-2-.9-2-2V6c0-1.1.9-2 2-2z"/><polyline points="22,6 12,13 2,6"/>');
        case 'calendar':
            return wrap('<rect x="3" y="4" width="18" height="18" rx="2" ry="2"/><line x1="16" y1="2" x2="16" y2="6"/><line x1="8" y1="2" x2="8" y2="6"/><line x1="3" y1="10" x2="21" y2="10"/>');
        default:
            return null;
    }
}

// ─── Profile manager ──────────────────────────────────────────────────

const PROFILE_DOMAINS = [
    'Email', 'Calendar', 'Messaging', 'Coding', 'Research', 'Outreach',
    'Marketing', 'Finance', 'Scheduling', 'DataAnalysis', 'Writing',
    'Translation', 'Health', 'Legal', 'Infrastructure', 'Architecture',
    'Support', 'SocialMedia', 'Other',
];
const PROFILE_TASK_KINDS = [
    'Drafting', 'Editing', 'Summarizing', 'Researching', 'Scheduling',
    'CodeReview', 'Coding', 'Debugging', 'DataAnalysis', 'Outreach',
    'Triage', 'Other',
];

async function loadProfileManager() {
    const listEl = document.getElementById('profile-list');
    if (!listEl) return;
    try {
        // Reuse the cached list when possible — the per-arc picker keeps it
        // fresh, and the manager re-fetches after each save anyway.
        agentProfiles = await invoke('list_agent_profiles');
        renderProfileList();
        // Push the freshly loaded list back to the per-arc dropdown so the
        // two views never disagree.
        renderProfilePicker();
        renderReasoningPicker();
        renderTierPicker();
    } catch (err) {
        console.error('Failed to load profiles:', err);
        listEl.innerHTML = '<p class="setting-hint">Failed to load profiles.</p>';
    }
}

// Per-profile static-prefix token estimates, fetched lazily from
// `estimate_profile_tokens`. Cached for 60 seconds because the underlying
// inputs (identity, endpoints, tool registry) only change on user edits;
// invalidate explicitly via `invalidateProfileTokenCache()` whenever one of
// those panels saves.
const profileTokenEstimates = new Map(); // profile_id -> { estimate, fetchedAt }
const PROFILE_TOKEN_CACHE_TTL_MS = 60_000;

function invalidateProfileTokenCache() {
    profileTokenEstimates.clear();
}

function formatTokenCount(tokens) {
    if (!tokens || tokens < 0) return '0';
    if (tokens < 1000) return String(tokens);
    return (tokens / 1000).toFixed(1).replace(/\.0$/, '') + 'k';
}

async function fetchProfileTokenEstimate(profileId) {
    if (!invoke) return null;
    const cached = profileTokenEstimates.get(profileId);
    if (cached && Date.now() - cached.fetchedAt < PROFILE_TOKEN_CACHE_TTL_MS) {
        return cached.estimate;
    }
    try {
        const estimate = await invoke('estimate_profile_tokens', { profileId });
        profileTokenEstimates.set(profileId, { estimate, fetchedAt: Date.now() });
        return estimate;
    } catch (err) {
        console.warn('estimate_profile_tokens failed for', profileId, err);
        return null;
    }
}

function buildEstimateTooltip(est) {
    if (!est) return 'Token estimate unavailable.';
    const t = (chars) => Math.round(chars / 3.7);
    return [
        `System ~${t(est.system_prompt_chars).toLocaleString()} tok`,
        `+ Tools ~${t(est.tools_array_chars).toLocaleString()} tok`,
        `+ Identity ~${t(est.identity_chars).toLocaleString()} tok`,
        `+ Endpoints ~${t(est.endpoints_chars).toLocaleString()} tok`,
        `≈ ${est.approx_tokens.toLocaleString()} tokens`,
        '(heuristic — actual varies by tokenizer).',
    ].join(' ');
}

async function paintProfileChip(chipEl, profileId) {
    if (!chipEl) return;
    chipEl.classList.add('loading');
    chipEl.textContent = '…';
    chipEl.title = 'Calculating fresh-start token cost…';
    const est = await fetchProfileTokenEstimate(profileId);
    chipEl.classList.remove('loading');
    if (!est || !est.approx_tokens) {
        chipEl.textContent = '— tok';
        chipEl.title = 'Token estimate unavailable.';
        return;
    }
    chipEl.textContent = `≈ ${formatTokenCount(est.approx_tokens)} tok`;
    chipEl.title = buildEstimateTooltip(est);
}

async function paintAllProfileChips() {
    const chips = document.querySelectorAll('.profile-token-chip[data-profile-id]');
    if (!chips.length) return;
    await Promise.all(
        Array.from(chips).map((c) => paintProfileChip(c, c.dataset.profileId)),
    );
}

function renderProfileList() {
    const listEl = document.getElementById('profile-list');
    if (!listEl) return;
    listEl.innerHTML = '';
    if (!agentProfiles || agentProfiles.length === 0) {
        listEl.innerHTML = '<p class="setting-hint">No profiles yet.</p>';
        return;
    }
    for (const p of agentProfiles) {
        listEl.appendChild(buildProfileCard(p));
    }
    // Fire all estimate calls in parallel so the chips fill in within ~1s
    // of paint instead of one-at-a-time. Errors degrade per-chip; never
    // throw out of here or the whole list disappears.
    paintAllProfileChips().catch((err) =>
        console.warn('paintAllProfileChips:', err),
    );
}

function buildProfileCard(p) {
    const card = document.createElement('div');
    card.className = 'profile-card';
    const isBuiltin = !!p.builtin;
    const badge = isBuiltin
        ? '<span class="profile-card-badge builtin">Built-in</span>'
        : '<span class="profile-card-badge">Custom</span>';
    const desc = p.description
        ? `<div class="profile-card-desc">${escapeHtml(p.description)}</div>`
        : '';
    // Built-ins get a "Restore default" affordance. User-authored profiles
    // get a "Delete" instead — there's no canonical version to revert to.
    const tailButton = isBuiltin
        ? '<button data-action="restore" title="Revert this built-in to its shipped values">Restore default</button>'
        : '<button data-action="delete" class="btn-danger">Delete</button>';
    card.innerHTML = `
        <div class="profile-card-main">
            <div class="profile-card-name">
                ${escapeHtml(p.display_name)} ${badge}
                <span class="profile-token-chip loading" data-profile-id="${escapeHtml(p.id)}" title="Calculating fresh-start token cost…">…</span>
            </div>
            ${desc}
        </div>
        <div class="profile-card-actions">
            <button data-action="edit">Edit</button>
            <button data-action="clone">Clone</button>
            ${tailButton}
        </div>
    `;
    card.querySelector('[data-action="edit"]')?.addEventListener('click', () => openProfileEditor('edit', p));
    card.querySelector('[data-action="clone"]')?.addEventListener('click', () => openProfileEditor('clone', p));
    card.querySelector('[data-action="delete"]')?.addEventListener('click', () => deleteProfile(p));
    card.querySelector('[data-action="restore"]')?.addEventListener('click', () => restoreProfile(p));
    return card;
}

async function restoreProfile(p) {
    if (!invoke) return;
    if (!confirm(`Restore "${p.display_name}" to its built-in defaults?\n\nYour edits to this profile will be lost.`)) {
        return;
    }
    try {
        await invoke('restore_agent_profile', { profileId: p.id });
        await loadProfileManager();
    } catch (err) {
        showToast('Restore failed: ' + err, 'error');
    }
}

async function deleteProfile(p) {
    if (!invoke) return;
    if (p.builtin) return; // Should be disabled in UI; defense in depth.
    if (!confirm(`Delete profile "${p.display_name}"?\n\nArcs using this profile will fall back to the default.`)) {
        return;
    }
    try {
        await invoke('delete_agent_profile', { profileId: p.id });
        await loadProfileManager();
    } catch (err) {
        showToast('Delete failed: ' + err, 'error');
    }
}

// Mode is 'create' | 'edit' | 'clone'. For 'clone', we copy the source
// profile's fields but suggest a new id/display name and force builtin=false.
function openProfileEditor(mode, source) {
    const overlay = document.getElementById('profile-modal-overlay');
    const titleEl = document.getElementById('profile-modal-title');
    const idEl = document.getElementById('profile-id');
    const displayEl = document.getElementById('profile-display-name');
    const descEl = document.getElementById('profile-description');
    const personaEl = document.getElementById('profile-persona');
    const strengthsEl = document.getElementById('profile-strengths');
    const modelEl = document.getElementById('profile-model-hint');
    const errEl = document.getElementById('profile-modal-error');
    const editIdEl = document.getElementById('profile-edit-id');

    errEl.classList.add('hidden');
    errEl.textContent = '';

    if (mode === 'edit' && source) {
        titleEl.textContent = 'Edit profile';
        idEl.value = source.id;
        idEl.disabled = true;
        editIdEl.value = source.id;
        displayEl.value = source.display_name || '';
        descEl.value = source.description || '';
        personaEl.value = source.custom_persona_addendum || '';
        strengthsEl.value = (source.expertise?.strengths || []).join(', ');
        modelEl.value = source.model_profile_hint || '';
        renderProfileChips(source.expertise || {});
    } else if (mode === 'clone' && source) {
        titleEl.textContent = 'Clone profile';
        idEl.value = `${source.id}_copy`;
        idEl.disabled = false;
        editIdEl.value = '';
        displayEl.value = `${source.display_name} (copy)`;
        descEl.value = source.description || '';
        personaEl.value = source.custom_persona_addendum || '';
        strengthsEl.value = (source.expertise?.strengths || []).join(', ');
        modelEl.value = source.model_profile_hint || '';
        renderProfileChips(source.expertise || {});
    } else {
        titleEl.textContent = 'New profile';
        idEl.value = '';
        idEl.disabled = false;
        editIdEl.value = '';
        displayEl.value = '';
        descEl.value = '';
        personaEl.value = '';
        strengthsEl.value = '';
        modelEl.value = '';
        renderProfileChips({});
    }

    overlay.classList.remove('hidden');
    displayEl.focus();

    // Show the token-budget panel only for existing profiles — there's
    // nothing to estimate for an unsaved create-flow draft. The chip on
    // the card already reflects the post-save number.
    const editingExisting = mode === 'edit' && source;
    renderProfileTokenBudget(editingExisting ? source.id : null);
}

function renderProfileTokenBudget(profileId) {
    const wrap = document.getElementById('profile-token-budget');
    const body = document.getElementById('profile-token-budget-body');
    if (!wrap || !body) return;
    if (!profileId) {
        wrap.classList.add('hidden');
        body.innerHTML = '';
        return;
    }
    wrap.classList.remove('hidden');
    body.innerHTML = '<div class="profile-token-budget-loading">Calculating…</div>';
    fetchProfileTokenEstimate(profileId).then((est) => {
        if (!est) {
            body.innerHTML = '<div class="profile-token-budget-loading">Estimate unavailable.</div>';
            return;
        }
        const t = (chars) => Math.round(chars / 3.7);
        const max = Math.max(
            est.system_prompt_chars,
            est.tools_array_chars,
            est.identity_chars,
            est.endpoints_chars,
            1,
        );
        const row = (label, chars) => {
            const tokens = t(chars);
            const pct = Math.round((chars / max) * 100);
            return `
                <div class="profile-token-row">
                    <div class="profile-token-row-label">${escapeHtml(label)}</div>
                    <div class="profile-token-row-value">~${tokens.toLocaleString()} tok</div>
                    <div class="profile-token-row-bar"><span style="width: ${pct}%"></span></div>
                </div>
            `;
        };
        body.innerHTML = `
            ${row('System prompt', est.system_prompt_chars)}
            ${row('Tool schemas', est.tools_array_chars)}
            ${row('Identity', est.identity_chars)}
            ${row('Endpoints', est.endpoints_chars)}
            <div class="profile-token-row total">
                <div class="profile-token-row-label">Total</div>
                <div class="profile-token-row-value">~${est.approx_tokens.toLocaleString()} tok</div>
                <div class="profile-token-row-bar"></div>
            </div>
            <div class="profile-token-budget-meta">
                ${est.tool_count_revealed} tool${est.tool_count_revealed === 1 ? '' : 's'} with inline schemas
                · ${est.tool_count_available} available
                · ${est.identity_entry_count} identity entr${est.identity_entry_count === 1 ? 'y' : 'ies'}
                · ${est.endpoint_count} endpoint${est.endpoint_count === 1 ? '' : 's'}
            </div>
        `;
    });
}

function renderProfileChips(expertise) {
    const domains = new Set(expertise.domains || []);
    const taskKinds = new Set(expertise.task_kinds || []);
    const avoid = new Set(expertise.avoid || []);

    fillChipGrid('profile-domains', PROFILE_DOMAINS, domains);
    fillChipGrid('profile-task-kinds', PROFILE_TASK_KINDS, taskKinds);
    fillChipGrid('profile-avoid', PROFILE_TASK_KINDS, avoid);
}

function fillChipGrid(elementId, values, selectedSet) {
    const grid = document.getElementById(elementId);
    if (!grid) return;
    grid.innerHTML = '';
    for (const v of values) {
        const chip = document.createElement('button');
        chip.type = 'button';
        chip.className = 'profile-chip' + (selectedSet.has(v) ? ' selected' : '');
        chip.dataset.value = v;
        chip.textContent = v;
        chip.addEventListener('click', () => {
            chip.classList.toggle('selected');
        });
        grid.appendChild(chip);
    }
}

function readChipSelection(elementId) {
    const grid = document.getElementById(elementId);
    if (!grid) return [];
    return Array.from(grid.querySelectorAll('.profile-chip.selected')).map(
        (c) => c.dataset.value
    );
}

function closeProfileEditor() {
    document.getElementById('profile-modal-overlay').classList.add('hidden');
}

async function saveProfileFromEditor() {
    if (!invoke) return;
    const editId = document.getElementById('profile-edit-id').value;
    const id = document.getElementById('profile-id').value.trim();
    const displayName = document.getElementById('profile-display-name').value.trim();
    const description = document.getElementById('profile-description').value.trim();
    const persona = document.getElementById('profile-persona').value.trim();
    const strengthsRaw = document.getElementById('profile-strengths').value;
    const modelHint = document.getElementById('profile-model-hint').value.trim();
    const errEl = document.getElementById('profile-modal-error');

    const showError = (msg) => {
        errEl.textContent = msg;
        errEl.classList.remove('hidden');
    };

    if (!displayName) return showError('Display name is required.');
    if (!id) return showError('ID is required.');
    if (!/^[a-z0-9_]+$/.test(id)) {
        return showError('ID must contain only lowercase letters, numbers, and underscores.');
    }

    const strengths = strengthsRaw
        .split(',')
        .map((s) => s.trim())
        .filter(Boolean);

    const input = {
        id,
        displayName,
        description,
        customPersonaAddendum: persona || null,
        toolSelection: { All: null },
        expertise: {
            domains: readChipSelection('profile-domains'),
            taskKinds: readChipSelection('profile-task-kinds'),
            languages: [],
            strengths,
            avoid: readChipSelection('profile-avoid'),
        },
        modelProfileHint: modelHint || null,
    };

    // Tauri's IPC encoder rewrites camelCase command args to snake_case for
    // serde, but nested objects we own must already be snake_case-keyed.
    const serdeInput = {
        id: input.id,
        display_name: input.displayName,
        description: input.description,
        custom_persona_addendum: input.customPersonaAddendum,
        // ToolSelection is an enum — `"All"` (a unit variant) is the safest
        // default. The manager UI doesn't expose group/explicit/deny yet.
        tool_selection: 'All',
        expertise: {
            domains: input.expertise.domains,
            task_kinds: input.expertise.taskKinds,
            languages: input.expertise.languages,
            strengths: input.expertise.strengths,
            avoid: input.expertise.avoid,
        },
        model_profile_hint: input.modelProfileHint,
    };

    try {
        if (editId) {
            await invoke('update_agent_profile', { input: serdeInput });
        } else {
            await invoke('create_agent_profile', { input: serdeInput });
        }
        // Cost may have shifted (tool_selection, persona length) — drop
        // the cache so the chip refetches when the list re-renders.
        invalidateProfileTokenCache();
        closeProfileEditor();
        await loadProfileManager();
    } catch (err) {
        showError(String(err));
    }
}

// ─── Identity ─────────────────────────────────────────────────────────
//
// User-editable identity store. Each entry is markdown body + applies_to
// scope tags + pinned flag. Categories are user-editable too — the four
// seeds (personality, rules, knowledge, team) ship pre-configured but can
// be renamed, deleted, or extended.

let identityCategories = [];
let identityEntries = [];
let identitySelectedCategory = null;

// Rough char-to-token heuristic. Matches the chars/4 estimate the
// compaction subsystem uses (athen-app::compaction). Identity is plain
// English markdown; this estimate is conservative but consistent with
// what the user sees in compaction warnings.
function estimateTokens(text) {
    if (!text) return 0;
    return Math.ceil(text.length / 4);
}

async function loadIdentityManager() {
    if (!invoke) return;
    try {
        const [cats, entries] = await Promise.all([
            invoke('list_identity_categories'),
            invoke('list_identity_entries', { category: null }),
        ]);
        identityCategories = cats || [];
        identityEntries = entries || [];
        // Identity content feeds every profile's static prefix; drop
        // cached profile estimates so the chips refetch on next render.
        invalidateProfileTokenCache();
        // Preserve selection across reloads when possible; otherwise pick
        // the first category so users always see something.
        if (
            !identitySelectedCategory ||
            !identityCategories.find((c) => c.name === identitySelectedCategory)
        ) {
            identitySelectedCategory = identityCategories.length
                ? identityCategories[0].name
                : null;
        }
        renderIdentitySidebar();
        renderIdentityDetail();
        updateIdentityTokenFooter();
    } catch (err) {
        console.error('Failed to load identity store:', err);
    }
}

function renderIdentitySidebar() {
    const listEl = document.getElementById('identity-category-list');
    if (!listEl) return;
    listEl.innerHTML = '';
    if (identityCategories.length === 0) {
        const empty = document.createElement('div');
        empty.className = 'identity-detail-empty';
        empty.textContent = 'No categories yet.';
        listEl.appendChild(empty);
        return;
    }
    for (const cat of identityCategories) {
        const count = identityEntries.filter((e) => e.category === cat.name).length;
        const item = document.createElement('div');
        item.className = 'identity-category-item';
        if (cat.is_seed) item.classList.add('seed');
        if (cat.name === identitySelectedCategory) item.classList.add('selected');
        item.innerHTML = `
            <span class="identity-cat-name">${escapeHtml(cat.name)}</span>
            <span class="identity-cat-count">(${count})</span>
        `;
        item.addEventListener('click', () => {
            identitySelectedCategory = cat.name;
            renderIdentitySidebar();
            renderIdentityDetail();
        });
        listEl.appendChild(item);
    }
}

function renderIdentityDetail() {
    const detail = document.getElementById('identity-detail');
    if (!detail) return;
    if (!identitySelectedCategory) {
        detail.innerHTML =
            '<p class="setting-hint">Add a category to start defining identity.</p>';
        return;
    }
    const cat = identityCategories.find((c) => c.name === identitySelectedCategory);
    if (!cat) {
        detail.innerHTML = '<p class="setting-hint">Category not found.</p>';
        return;
    }
    detail.innerHTML = '';

    const header = document.createElement('div');
    header.className = 'identity-detail-header';
    header.innerHTML = `
        <h3>${escapeHtml(cat.name)}</h3>
        <div class="identity-detail-header-actions">
            <button data-action="edit-cat">Edit</button>
            <button data-action="delete-cat" class="btn-danger">Delete</button>
        </div>
    `;
    header.querySelector('[data-action="edit-cat"]').addEventListener('click', () =>
        openIdentityCategoryModal('edit', cat),
    );
    header.querySelector('[data-action="delete-cat"]').addEventListener('click', () =>
        deleteIdentityCategory(cat),
    );
    detail.appendChild(header);

    if (cat.description) {
        const desc = document.createElement('div');
        desc.className = 'identity-detail-description';
        desc.textContent = cat.description;
        detail.appendChild(desc);
    }

    const entriesWrap = document.createElement('div');
    entriesWrap.className = 'identity-entries';
    const entries = identityEntries.filter((e) => e.category === cat.name);
    if (entries.length === 0) {
        const empty = document.createElement('div');
        empty.className = 'identity-detail-empty';
        empty.textContent = 'No entries yet — add one below.';
        entriesWrap.appendChild(empty);
    } else {
        for (const entry of entries) {
            entriesWrap.appendChild(buildIdentityEntryCard(entry, cat));
        }
    }
    detail.appendChild(entriesWrap);

    const addBtn = document.createElement('button');
    addBtn.className = 'btn-secondary identity-add-entry-btn';
    addBtn.type = 'button';
    addBtn.textContent = '+ Add entry';
    addBtn.addEventListener('click', () => addIdentityEntry(cat));
    detail.appendChild(addBtn);
}

function buildIdentityEntryCard(entry, cat) {
    const card = document.createElement('div');
    card.className = 'identity-entry-card';

    if (entry.proposed_by_agent) {
        const isRule = cat.name === 'rules';
        const chip = document.createElement('span');
        chip.className = 'identity-proposed-chip' + (isRule ? ' rules' : '');
        const label = document.createElement('span');
        label.className = 'identity-proposed-chip-label';
        label.textContent = isRule ? 'New rule — review' : 'added by agent';
        chip.appendChild(label);
        const dismiss = document.createElement('button');
        dismiss.type = 'button';
        dismiss.className = 'identity-proposed-chip-dismiss';
        dismiss.title = 'Dismiss this suggestion';
        dismiss.setAttribute('aria-label', 'Dismiss this suggestion');
        dismiss.textContent = '×';
        dismiss.addEventListener('click', async () => {
            try {
                await invoke('dismiss_identity_entry', { id: entry.id });
                await loadIdentityManager();
            } catch (err) {
                showToast('Dismiss failed: ' + err, 'error');
            }
        });
        chip.appendChild(dismiss);
        card.appendChild(chip);
    }

    const body = document.createElement('textarea');
    body.className = 'identity-entry-body';
    body.value = entry.body;
    body.placeholder = 'Free-form markdown — describe the trait, rule, or fact.';
    card.appendChild(body);

    const controls = document.createElement('div');
    controls.className = 'identity-entry-controls';

    const scopeRow = document.createElement('div');
    scopeRow.className = 'identity-scope-chip-row';
    // Local mutable copy — the card holds onto it until Save commits.
    const localScope = JSON.parse(JSON.stringify(entry.applies_to || []));
    renderScopeChips(scopeRow, localScope);
    controls.appendChild(scopeRow);

    const pinToggle = document.createElement('span');
    pinToggle.className = 'identity-pin-toggle' + (entry.pinned ? ' active' : '');
    pinToggle.innerHTML = (entry.pinned ? '★' : '☆') + ' pinned';
    let localPinned = !!entry.pinned;
    pinToggle.addEventListener('click', () => {
        localPinned = !localPinned;
        pinToggle.classList.toggle('active', localPinned);
        pinToggle.innerHTML = (localPinned ? '★' : '☆') + ' pinned';
    });
    controls.appendChild(pinToggle);

    const actions = document.createElement('div');
    actions.className = 'identity-entry-actions';
    const saveBtn = document.createElement('button');
    saveBtn.textContent = 'Save';
    saveBtn.addEventListener('click', async () => {
        try {
            await invoke('upsert_identity_entry', {
                input: {
                    id: entry.id,
                    category: cat.name,
                    body: body.value,
                    applies_to: localScope,
                    pinned: localPinned,
                },
            });
            await loadIdentityManager();
            showToast('Saved.', 'success');
        } catch (err) {
            showToast('Save failed: ' + err, 'error');
        }
    });
    const delBtn = document.createElement('button');
    delBtn.className = 'btn-danger';
    delBtn.textContent = 'Delete';
    delBtn.addEventListener('click', async () => {
        if (!confirm('Delete this entry?')) return;
        try {
            await invoke('delete_identity_entry', { id: entry.id });
            await loadIdentityManager();
        } catch (err) {
            showToast('Delete failed: ' + err, 'error');
        }
    });
    actions.appendChild(saveBtn);
    actions.appendChild(delBtn);
    controls.appendChild(actions);

    card.appendChild(controls);
    return card;
}

// Renders the applies_to chip row in-place. `tags` is mutated by user
// clicks; the caller is responsible for re-rendering siblings if needed.
function renderScopeChips(container, tags) {
    container.innerHTML = '';
    const profiles = agentProfiles || [];
    const choices = [{ id: '__always__', label: 'Always' }];
    for (const p of profiles) {
        choices.push({ id: p.id, label: p.id });
    }
    for (const c of choices) {
        const chip = document.createElement('span');
        chip.className = 'identity-scope-chip';
        chip.textContent = c.label;
        const isSelected =
            (c.id === '__always__' && tags.some((t) => t === 'Always')) ||
            (c.id !== '__always__' &&
                tags.some((t) => t && typeof t === 'object' && t.Profile === c.id));
        if (isSelected) chip.classList.add('selected');
        chip.addEventListener('click', () => {
            if (c.id === '__always__') {
                const has = tags.some((t) => t === 'Always');
                if (has) {
                    const idx = tags.findIndex((t) => t === 'Always');
                    if (idx >= 0) tags.splice(idx, 1);
                } else {
                    // Always supersedes per-profile chips; clear them for
                    // clarity. The store still accepts mixed sets, but the
                    // UI keeps the model crisp.
                    tags.length = 0;
                    tags.push('Always');
                }
            } else {
                // Selecting a specific profile turns off Always.
                const alwaysIdx = tags.findIndex((t) => t === 'Always');
                if (alwaysIdx >= 0) tags.splice(alwaysIdx, 1);
                const idx = tags.findIndex(
                    (t) => t && typeof t === 'object' && t.Profile === c.id,
                );
                if (idx >= 0) {
                    tags.splice(idx, 1);
                } else {
                    tags.push({ Profile: c.id });
                }
            }
            renderScopeChips(container, tags);
        });
        container.appendChild(chip);
    }
}

async function addIdentityEntry(cat) {
    if (!invoke) return;
    // Use the category's default_applies_to as the seed for new entries.
    const scope = cat.default_applies_to && cat.default_applies_to.length
        ? cat.default_applies_to
        : ['Always'];
    try {
        await invoke('upsert_identity_entry', {
            input: {
                id: null,
                category: cat.name,
                body: '',
                applies_to: scope,
                pinned: false,
            },
        });
        await loadIdentityManager();
    } catch (err) {
        showToast('Add failed: ' + err, 'error');
    }
}

async function deleteIdentityCategory(cat) {
    if (!invoke) return;
    const count = identityEntries.filter((e) => e.category === cat.name).length;
    const msg =
        count > 0
            ? `Delete category "${cat.name}" and its ${count} entrie${count === 1 ? 'y' : 's'}?`
            : `Delete category "${cat.name}"?`;
    if (!confirm(msg)) return;
    try {
        await invoke('delete_identity_category', { name: cat.name });
        if (identitySelectedCategory === cat.name) identitySelectedCategory = null;
        await loadIdentityManager();
    } catch (err) {
        showToast('Delete failed: ' + err, 'error');
    }
}

async function updateIdentityTokenFooter() {
    const countEl = document.getElementById('identity-token-count');
    const pctEl = document.getElementById('identity-token-pct');
    const warnEl = document.getElementById('identity-token-warning');
    if (!countEl || !pctEl || !warnEl) return;
    // Prefer the backend estimator (matches the executor's renderer
    // exactly); fall back to a quick FE-only sum if the call fails so
    // the footer still shows something useful.
    let tokens;
    try {
        const total = await invoke('estimate_identity_total');
        tokens = total?.approx_tokens ?? 0;
    } catch (err) {
        let chars = 0;
        for (const e of identityEntries) {
            chars += (e.body || '').length;
            chars += (e.category || '').length + 4; // approximate "## name\n"
        }
        tokens = Math.ceil(chars / 4);
    }
    // Reference window: assume 8K. We don't know the user's smallest model
    // here; this is a rough indicator. Settings could later populate the
    // real value once we wire model context windows into the frontend.
    const referenceWindow = 8000;
    const pct = referenceWindow > 0 ? Math.round((tokens / referenceWindow) * 100) : 0;
    countEl.textContent = tokens.toLocaleString();
    pctEl.textContent = pct;
    warnEl.classList.remove('warn-yellow', 'warn-red');
    if (pct >= 15) {
        warnEl.classList.remove('hidden');
        warnEl.classList.add('warn-red');
        warnEl.textContent =
            'Long identity blocks crowd out task context on smaller models. Consider trimming or scoping entries to fewer profiles.';
    } else if (pct >= 5) {
        warnEl.classList.remove('hidden');
        warnEl.classList.add('warn-yellow');
        warnEl.textContent =
            'Identity is getting sizeable. Smaller models (8K context) will feel this.';
    } else {
        warnEl.classList.add('hidden');
        warnEl.textContent = '';
    }
}

// ─── Identity category modal ───

function openIdentityCategoryModal(mode, source) {
    const overlay = document.getElementById('identity-category-modal-overlay');
    const titleEl = document.getElementById('identity-category-modal-title');
    const nameEl = document.getElementById('identity-category-name');
    const descEl = document.getElementById('identity-category-description');
    const errEl = document.getElementById('identity-category-modal-error');
    const origNameEl = document.getElementById('identity-category-original-name');
    const scopeEl = document.getElementById('identity-category-default-scope');
    if (!overlay || !nameEl) return;

    errEl.classList.add('hidden');
    errEl.textContent = '';

    let scopeTags;
    if (mode === 'edit' && source) {
        titleEl.textContent = 'Edit category';
        nameEl.value = source.name;
        nameEl.disabled = true; // renaming is destructive — drop+create instead.
        descEl.value = source.description || '';
        origNameEl.value = source.name;
        scopeTags = JSON.parse(JSON.stringify(source.default_applies_to || ['Always']));
    } else {
        titleEl.textContent = 'New category';
        nameEl.value = '';
        nameEl.disabled = false;
        descEl.value = '';
        origNameEl.value = '';
        scopeTags = ['Always'];
    }
    renderScopeChips(scopeEl, scopeTags);
    // Stash the live array on the overlay so the save handler reads the
    // user's clicks. renderScopeChips mutates `scopeTags` in place when
    // chips are toggled, so no manual sync is needed.
    overlay.__identityScopeTags = scopeTags;

    overlay.classList.remove('hidden');
    nameEl.focus();
}

function closeIdentityCategoryModal() {
    const overlay = document.getElementById('identity-category-modal-overlay');
    if (!overlay) return;
    delete overlay.__identityScopeTags;
    overlay.classList.add('hidden');
}

async function saveIdentityCategoryFromModal() {
    const overlay = document.getElementById('identity-category-modal-overlay');
    const nameEl = document.getElementById('identity-category-name');
    const descEl = document.getElementById('identity-category-description');
    const errEl = document.getElementById('identity-category-modal-error');
    const origNameEl = document.getElementById('identity-category-original-name');
    if (!overlay || !nameEl) return;

    const name = (nameEl.value || '').trim();
    if (!name) {
        errEl.textContent = 'Name is required.';
        errEl.classList.remove('hidden');
        return;
    }
    const scopeTags = overlay.__identityScopeTags || ['Always'];
    const isEdit = !!origNameEl.value;
    // For new categories pick a sort_order that puts them at the end.
    const maxSort = identityCategories.reduce(
        (m, c) => Math.max(m, c.sort_order || 0),
        0,
    );
    const existing = identityCategories.find((c) => c.name === name);
    const sortOrder = existing ? existing.sort_order : maxSort + 10;
    try {
        await invoke('upsert_identity_category', {
            input: {
                name,
                description: descEl.value || '',
                default_applies_to: scopeTags,
                sort_order: sortOrder,
            },
        });
        identitySelectedCategory = name;
        closeIdentityCategoryModal();
        await loadIdentityManager();
    } catch (err) {
        errEl.textContent = String(err);
        errEl.classList.remove('hidden');
    }
}

// Wire identity-modal buttons once on first load.
(function wireIdentityModalButtons() {
    const closeBtn = document.getElementById('identity-category-modal-close');
    const cancelBtn = document.getElementById('identity-category-modal-cancel');
    const saveBtn = document.getElementById('identity-category-modal-save');
    const overlay = document.getElementById('identity-category-modal-overlay');
    if (closeBtn) closeBtn.addEventListener('click', closeIdentityCategoryModal);
    if (cancelBtn) cancelBtn.addEventListener('click', closeIdentityCategoryModal);
    if (saveBtn) saveBtn.addEventListener('click', saveIdentityCategoryFromModal);
    if (overlay) {
        overlay.addEventListener('click', (ev) => {
            if (ev.target === overlay) closeIdentityCategoryModal();
        });
    }
    const newBtn = document.getElementById('identity-new-category-btn');
    if (newBtn) newBtn.addEventListener('click', () => openIdentityCategoryModal('create'));
})();

// ─── Shell toolbox ────────────────────────────────────────────────────

async function loadToolboxPackages() {
    const listEl = document.getElementById('toolbox-list');
    if (!listEl) return;
    try {
        const pkgs = await invoke('list_toolbox_packages');
        renderToolboxPackages(pkgs);
    } catch (err) {
        console.error('Failed to load toolbox packages:', err);
        listEl.innerHTML = '<p class="setting-hint">Failed to load installed packages.</p>';
    }
}

function renderToolboxPackages(pkgs) {
    const listEl = document.getElementById('toolbox-list');
    listEl.innerHTML = '';
    if (!pkgs || pkgs.length === 0) {
        listEl.innerHTML =
            '<p class="setting-hint">No packages installed yet. The agent will install packages here when needed.</p>';
        return;
    }

    const groups = { python: [], node: [] };
    for (const p of pkgs) {
        if (groups[p.runtime]) {
            groups[p.runtime].push(p);
        } else {
            groups[p.runtime] = [p];
        }
    }

    const titles = { python: 'Python', node: 'Node' };
    for (const runtime of Object.keys(groups)) {
        const items = groups[runtime];
        if (!items || items.length === 0) continue;
        items.sort((a, b) => a.package.localeCompare(b.package));
        const groupEl = document.createElement('div');
        groupEl.className = 'toolbox-group';

        const heading = document.createElement('h3');
        heading.className = 'toolbox-group-title';
        heading.textContent = `${titles[runtime] || runtime} (${items.length})`;
        groupEl.appendChild(heading);

        for (const p of items) {
            groupEl.appendChild(buildToolboxRow(p));
        }
        listEl.appendChild(groupEl);
    }
}

function buildToolboxRow(p) {
    const row = document.createElement('div');
    row.className = 'toolbox-row';

    const head = document.createElement('div');
    head.className = 'toolbox-row-head';

    const name = document.createElement('span');
    name.className = 'toolbox-row-name';
    name.textContent = p.package;
    head.appendChild(name);

    if (p.installed_version) {
        const ver = document.createElement('span');
        ver.className = 'toolbox-row-version';
        ver.textContent = p.installed_version;
        head.appendChild(ver);
    }

    const date = document.createElement('span');
    date.className = 'toolbox-row-date';
    date.textContent = formatRelativeTime(p.installed_at);
    head.appendChild(date);

    row.appendChild(head);

    if (p.reason) {
        const reason = document.createElement('div');
        reason.className = 'toolbox-row-reason';
        reason.textContent = p.reason;
        row.appendChild(reason);
    }
    return row;
}

function formatRelativeTime(iso) {
    if (!iso) return '';
    const t = new Date(iso).getTime();
    if (Number.isNaN(t)) return iso;
    const diffSec = Math.floor((Date.now() - t) / 1000);
    if (diffSec < 60) return 'just now';
    const diffMin = Math.floor(diffSec / 60);
    if (diffMin < 60) return `${diffMin}m ago`;
    const diffHr = Math.floor(diffMin / 60);
    if (diffHr < 24) return `${diffHr}h ago`;
    const diffDay = Math.floor(diffHr / 24);
    if (diffDay < 30) return `${diffDay}d ago`;
    const diffMo = Math.floor(diffDay / 30);
    if (diffMo < 12) return `${diffMo}mo ago`;
    return `${Math.floor(diffMo / 12)}y ago`;
}

async function handleClearToolbox() {
    const ok = window.confirm(
        'Remove every package the agent has installed in ~/.athen/toolbox? \nThis cannot be undone.'
    );
    if (!ok) return;
    try {
        await invoke('clear_toolbox');
        showToast('Toolbox cleared', 'success');
        await loadToolboxPackages();
    } catch (err) {
        console.error('clear_toolbox failed:', err);
        showToast('Failed to clear toolbox: ' + err, 'error');
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
        const svg = mcpIconSvg(entry.icon);
        if (svg) {
            icon.innerHTML = svg;
        } else {
            // Unknown icon name → fall back to the literal string so we at
            // least see something rather than a blank slot. Lets us notice
            // and add the missing mapping.
            icon.textContent = entry.icon;
        }
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

    const familyOptions = (MODEL_FAMILIES.length > 0 ? MODEL_FAMILIES : [{ id: 'Default', label: 'Default (unknown / generic)', default_slug: '' }])
        .map((f) => {
            const sel = (f.id === (provider.family || 'Default')) ? ' selected' : '';
            return `<option value="${escapeHtml(f.id)}" data-default-slug="${escapeHtml(f.default_slug)}"${sel}>${escapeHtml(f.label)}</option>`;
        })
        .join('');

    body.innerHTML = `
        <div class="provider-field">
            <label>Base URL</label>
            <input type="text" class="provider-url" value="${escapeHtml(provider.base_url)}" placeholder="https://api.example.com">
        </div>
        <div class="provider-field">
            <label>Model family</label>
            <select class="provider-family">${familyOptions}</select>
            <div class="field-hint">Picks the per-model quirks profile (tool-call format, reasoning surface, template strictness). Leave on Default if unsure — it reproduces the OpenAI-compat baseline. Selecting a family pre-fills the model slug below; you can edit the slug for dated or fine-tuned variants of the same family.</div>
        </div>
        <div class="provider-field">
            <label>Model slug</label>
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
        <div class="provider-field provider-field-checkbox">
            <label class="checkbox-row">
                <input type="checkbox" class="provider-supports-vision" ${provider.supports_vision ? 'checked' : ''}>
                <span>Vision-capable model (accepts image input)</span>
            </label>
            <div class="field-hint">Tick this when the model above is one of: Claude Sonnet/Opus 3.5+, GPT-4o / GPT-4o-mini, Gemini 1.5+, or any other multimodal model. Athen will only forward attached images when this is on.</div>
        </div>
        <div class="provider-field provider-field-checkbox">
            <label class="checkbox-row">
                <input type="checkbox" class="provider-supports-documents" ${provider.supports_documents ? 'checked' : ''}>
                <span>Document-capable model (accepts native PDF input)</span>
            </label>
            <div class="field-hint">Tick this when the model can render PDFs natively (Claude Sonnet/Opus 3.5+, Gemini 1.5+). Independent of vision. When off, Athen falls back to extracting PDF text locally and inlining it — your model still sees the contents either way.</div>
        </div>
        <div class="provider-field">
            <label class="advanced-toggle provider-advanced-toggle">
                <span class="advanced-arrow">&#9654;</span>
                <span>Advanced</span>
            </label>
        </div>
        <div class="provider-advanced" style="display: none;">
            <div class="provider-field">
                <label>Context window (tokens)</label>
                <input type="number" class="provider-context-window" min="1024" step="1024" value="${provider.context_window_tokens}" placeholder="e.g. 32000">
                <div class="field-hint">Authoritative ceiling used by arc compaction. Set this to your model's real context length (32k for Qwen3.5 9B local, 200k for Claude, 128k for GPT-4o). Compaction fires when arc tokens exceed Trigger %, summarises down to Target %.</div>
            </div>
            <div class="provider-field provider-field-row">
                <div class="provider-subfield">
                    <label>Compaction trigger %</label>
                    <input type="number" class="provider-compaction-trigger" min="1" max="100" value="${provider.compaction_trigger_pct}">
                </div>
                <div class="provider-subfield">
                    <label>Compaction target %</label>
                    <input type="number" class="provider-compaction-target" min="1" max="100" value="${provider.compaction_target_pct}">
                </div>
            </div>
            <div class="provider-field">
                <label>Sampling temperature</label>
                <input type="number" class="provider-temperature" min="0" max="2" step="0.05" value="${provider.temperature ?? ''}" placeholder="Adapter default (~0.7)">
                <div class="field-hint">Lower = more deterministic. Leave blank for the provider's default (0.7 across most APIs). Try 0.0–0.3 for benchmarking, code, or strict tool-calling; 0.7+ for creative tasks.</div>
            </div>
            <div class="provider-field provider-tier-models">
                <label>Per-tier model slugs</label>
                <div class="field-hint">Athen's internal tasks are tagged with a complexity tier — "Cheap" for memory judgments and error translations, "Fast" for the main agent loop, "Code" for code-heavy work, "Powerful" for high-stakes reasoning. Pick a different slug per tier to route lightweight tasks to a cheaper model while keeping the powerful one reserved for the main loop. Leave any slot empty to use the Model slug above.</div>
                <div class="provider-tier-row">
                    <span class="provider-tier-label">Cheap</span>
                    <input type="text" class="provider-tier-cheap" value="${escapeHtml(provider.tier_models?.Cheap || '')}" placeholder="${escapeHtml((providerById(provider.id) || {}).default_tier_cheap || provider.model)}">
                </div>
                <div class="provider-tier-row">
                    <span class="provider-tier-label">Fast</span>
                    <input type="text" class="provider-tier-fast" value="${escapeHtml(provider.tier_models?.Fast || '')}" placeholder="${escapeHtml((providerById(provider.id) || {}).default_tier_fast || provider.model)}">
                </div>
                <div class="provider-tier-row">
                    <span class="provider-tier-label">Code</span>
                    <input type="text" class="provider-tier-code" value="${escapeHtml(provider.tier_models?.Code || '')}" placeholder="${escapeHtml((providerById(provider.id) || {}).default_tier_code || provider.model)}">
                </div>
                <div class="provider-tier-row">
                    <span class="provider-tier-label">Powerful</span>
                    <input type="text" class="provider-tier-powerful" value="${escapeHtml(provider.tier_models?.Powerful || '')}" placeholder="${escapeHtml((providerById(provider.id) || {}).default_tier_powerful || provider.model)}">
                </div>
                <button class="btn-tertiary provider-tier-reset" type="button">Use defaults</button>
            </div>
        </div>
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

    // Family dropdown: pre-fill the model slug with the family's default
    // when the user picks a new family. Empty default ("Default" family)
    // leaves the slug alone — that's a useful no-op for the safety-net case.
    const familySelect = body.querySelector('.provider-family');
    const modelInput = body.querySelector('.provider-model');
    if (familySelect && modelInput) {
        familySelect.addEventListener('change', () => {
            const opt = familySelect.options[familySelect.selectedIndex];
            const slug = opt ? opt.getAttribute('data-default-slug') : '';
            if (slug) {
                modelInput.value = slug;
            }
        });
    }

    const advToggle = body.querySelector('.provider-advanced-toggle');
    const advPane = body.querySelector('.provider-advanced');
    if (advToggle && advPane) {
        advToggle.addEventListener('click', () => {
            const arrow = advToggle.querySelector('.advanced-arrow');
            if (advPane.style.display === 'none') {
                advPane.style.display = 'block';
                if (arrow) arrow.innerHTML = '&#9660;';
            } else {
                advPane.style.display = 'none';
                if (arrow) arrow.innerHTML = '&#9654;';
            }
        });
    }

    // "Use defaults" button: fill empty per-tier inputs from the catalog
    // presets so the user gets a head start. Doesn't overwrite values the
    // user typed — only blanks. If the user wants to wipe their edits and
    // start over, they can clear inputs first and then click.
    const tierResetBtn = body.querySelector('.provider-tier-reset');
    if (tierResetBtn) {
        tierResetBtn.addEventListener('click', () => {
            const catalogEntry = providerById(provider.id) || {};
            const presets = {
                '.provider-tier-cheap': catalogEntry.default_tier_cheap,
                '.provider-tier-fast': catalogEntry.default_tier_fast,
                '.provider-tier-code': catalogEntry.default_tier_code,
                '.provider-tier-powerful': catalogEntry.default_tier_powerful,
            };
            for (const [selector, preset] of Object.entries(presets)) {
                const input = body.querySelector(selector);
                if (input && preset) input.value = preset;
            }
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
    const visionInput = card.querySelector('.provider-supports-vision');
    const supportsVision = visionInput ? !!visionInput.checked : null;
    const documentsInput = card.querySelector('.provider-supports-documents');
    const supportsDocuments = documentsInput ? !!documentsInput.checked : null;
    const familySelect = card.querySelector('.provider-family');
    const family = familySelect ? familySelect.value : null;

    // Advanced fields. Empty inputs map to null so the backend preserves
    // existing values for window/triggers and treats null-temperature as
    // "use the adapter's baked-in default" (currently 0.7 across the
    // OpenAI-compat / DeepSeek paths).
    const ctxWindowInput = card.querySelector('.provider-context-window');
    const ctxWindowVal = ctxWindowInput ? ctxWindowInput.value.trim() : '';
    const contextWindowTokens = ctxWindowVal === '' ? null : parseInt(ctxWindowVal, 10);

    const trigInput = card.querySelector('.provider-compaction-trigger');
    const trigVal = trigInput ? trigInput.value.trim() : '';
    const compactionTriggerPct = trigVal === '' ? null : parseInt(trigVal, 10);

    const tgtInput = card.querySelector('.provider-compaction-target');
    const tgtVal = tgtInput ? tgtInput.value.trim() : '';
    const compactionTargetPct = tgtVal === '' ? null : parseInt(tgtVal, 10);

    const tempInput = card.querySelector('.provider-temperature');
    const tempVal = tempInput ? tempInput.value.trim() : '';
    const temperature = tempVal === '' ? null : parseFloat(tempVal);

    // Collect the four per-tier slug inputs. Empty strings are passed
    // through verbatim — the backend treats them as "fall back to the
    // default Model slug" rather than "preserve the existing tier_models
    // map" (which is what passing `null` does).
    const tierModels = {
        Cheap: (card.querySelector('.provider-tier-cheap')?.value || '').trim(),
        Fast: (card.querySelector('.provider-tier-fast')?.value || '').trim(),
        Code: (card.querySelector('.provider-tier-code')?.value || '').trim(),
        Powerful: (card.querySelector('.provider-tier-powerful')?.value || '').trim(),
    };

    if (compactionTriggerPct !== null && compactionTargetPct !== null
        && compactionTriggerPct <= compactionTargetPct) {
        showToast('Compaction trigger must be greater than target — bumping trigger.', 'info');
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
            supportsVision: supportsVision,
            supportsDocuments: supportsDocuments,
            family: family,
            contextWindowTokens: contextWindowTokens,
            compactionTriggerPct: compactionTriggerPct,
            compactionTargetPct: compactionTargetPct,
            temperature: temperature,
            tierModels: tierModels,
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

    // Optimistic removal: drop the card immediately so the user gets
    // instant feedback. If the backend call fails, loadSettings() in
    // the catch arm puts it back.
    const card = providerListEl.querySelector(
        `.provider-card[data-provider-id="${id}"]`
    );
    if (card) card.remove();

    try {
        const msg = await invoke('delete_provider', { id: id });
        showToast(msg, 'success');
        await loadSettings();
    } catch (err) {
        showToast('Failed to delete: ' + err, 'error');
        await loadSettings();
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

// Add provider template buttons — rendered dynamically from the catalog
// so onboarding and settings always agree on the supported providers.

function renderProviderTemplates() {
    if (!providerTemplates) return;
    providerTemplates.innerHTML = '';
    const all = [...PROVIDER_CATALOG, CUSTOM_PROVIDER_ENTRY];
    for (const entry of all) {
        const btn = document.createElement('button');
        btn.className = 'template-btn';
        btn.dataset.provider = entry.id;
        const suffix = entry.provider_type === 'local' ? ' (local)' : '';
        btn.textContent = entry.name + suffix;
        providerTemplates.appendChild(btn);
    }
}

if (addProviderBtn) {
    addProviderBtn.addEventListener('click', () => {
        providerTemplates.classList.toggle('hidden');
    });
}

// Event delegation — buttons exist only after the catalog loads, and we
// don't want to re-bind every render.
if (providerTemplates) {
    providerTemplates.addEventListener('click', (e) => {
        const btn = e.target.closest('.template-btn');
        if (!btn) return;
        const providerId = btn.dataset.provider;
        const defaults = PROVIDER_DEFAULTS[providerId];
        if (!defaults) return;
        providerTemplates.classList.add('hidden');

        const existingCard = providerListEl.querySelector(
            `.provider-card[data-provider-id="${providerId}"]`
        );
        if (existingCard) {
            existingCard.classList.add('expanded');
            existingCard.scrollIntoView({ behavior: 'smooth', block: 'center' });
            return;
        }

        const newProvider = {
            id: providerId,
            name: defaults.name,
            provider_type: defaults.type,
            base_url: defaults.base_url,
            model: defaults.model,
            // Pre-select the matching family in the dropdown so the user
            // doesn't have to manually flip it after adding the chip. The
            // family-change listener fires on createProviderCard only for
            // user interactions; the initial value comes straight from
            // `provider.family` via the `selected` attribute.
            family: defaults.family || 'Default',
            has_api_key: false,
            api_key_hint: '',
            is_active: false,
        };

        const card = createProviderCard(newProvider);
        card.classList.add('expanded');
        providerListEl.appendChild(card);
        card.scrollIntoView({ behavior: 'smooth', block: 'center' });
    });
}

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

// ─── Custom titlebar window controls ───
window.addEventListener('blur',  () => document.body.classList.add('window-blurred'));
window.addEventListener('focus', () => document.body.classList.remove('window-blurred'));

(function wireTitlebar() {
    function currentWindow() {
        const w = window.__TAURI__ && window.__TAURI__.window;
        if (!w) return null;
        if (typeof w.getCurrentWindow === 'function') return w.getCurrentWindow();
        if (typeof w.getCurrent === 'function') return w.getCurrent();
        return null;
    }
    const closeBtn = document.getElementById('win-close');
    const minBtn   = document.getElementById('win-minimize');
    const maxBtn   = document.getElementById('win-maximize');
    if (closeBtn) closeBtn.addEventListener('click', async () => {
        // Close-to-tray is handled in Rust (CloseRequested intercepted).
        const w = currentWindow();
        if (w) await w.close();
    });
    if (minBtn) minBtn.addEventListener('click', async () => {
        const w = currentWindow();
        if (w) await w.minimize();
    });
    if (maxBtn) maxBtn.addEventListener('click', async () => {
        const w = currentWindow();
        if (!w) return;
        if (typeof w.toggleMaximize === 'function') await w.toggleMaximize();
        else if (await w.isMaximized()) await w.unmaximize();
        else await w.maximize();
    });
})();

// ─── Settings tabs ───
const SETTINGS_TAB_STORAGE_KEY = 'athen.settings.activeTab';

function setSettingsTab(tabId) {
    const tabs = document.querySelectorAll('.settings-tab');
    const panes = document.querySelectorAll('.settings-tab-pane');
    let matched = false;
    tabs.forEach((btn) => {
        const isActive = btn.dataset.settingsTab === tabId;
        btn.classList.toggle('active', isActive);
        btn.setAttribute('aria-selected', isActive ? 'true' : 'false');
        if (isActive) matched = true;
    });
    panes.forEach((pane) => {
        pane.classList.toggle('active', pane.dataset.settingsPane === tabId);
    });
    if (matched) {
        try { localStorage.setItem(SETTINGS_TAB_STORAGE_KEY, tabId); } catch (_) {}
        const content = document.querySelector('.settings-content');
        if (content) content.scrollTop = 0;
    }
}

document.querySelectorAll('.settings-tab').forEach((btn) => {
    btn.addEventListener('click', () => setSettingsTab(btn.dataset.settingsTab));
});

(function restoreSettingsTab() {
    let stored = null;
    try { stored = localStorage.getItem(SETTINGS_TAB_STORAGE_KEY); } catch (_) {}
    if (stored && document.querySelector(`.settings-tab[data-settings-tab="${stored}"]`)) {
        setSettingsTab(stored);
    }
})();

// ─── Auto-updater ───

const UPDATE_DISMISS_KEY = 'athen.update.dismissedVersion';

(function wireUpdater() {
    const banner = document.getElementById('update-banner');
    const text = banner && banner.querySelector('.update-banner-text');
    const installBtn = document.getElementById('update-install-btn');
    const dismissBtn = document.getElementById('update-dismiss-btn');
    if (!banner || !text || !installBtn || !dismissBtn) return;

    let pendingVersion = null;
    let pendingReleaseUrl = null;
    let isSystemInstall = false;

    async function checkForUpdate() {
        try {
            if (!window.__TAURI__ || !window.__TAURI__.core) return;
            const info = await window.__TAURI__.core.invoke('check_for_update');
            if (!info || !info.available || !info.version) return;
            // Skip if user already dismissed this exact version this install.
            let dismissed = null;
            try { dismissed = localStorage.getItem(UPDATE_DISMISS_KEY); } catch (_) {}
            if (dismissed === info.version) return;
            pendingVersion = info.version;
            pendingReleaseUrl = info.release_url || null;
            isSystemInstall = info.installer_kind === 'system';
            text.textContent = `Athen ${info.version} is available (you have ${info.current_version}).`;
            installBtn.textContent = isSystemInstall ? 'Open download page' : 'Install & restart';
            banner.hidden = false;
        } catch (err) {
            // Silent: no network / no signed manifest yet — don't bother the user.
            console.debug('update check failed:', err);
        }
    }

    installBtn.addEventListener('click', async () => {
        // System-managed installs (rpm/deb/aur): can't self-update — open the release page.
        if (isSystemInstall) {
            if (pendingReleaseUrl) {
                try {
                    await window.__TAURI__.core.invoke('open_external_url', { url: pendingReleaseUrl });
                } catch (err) {
                    showToast('Failed to open release page: ' + err, 'error');
                }
            }
            return;
        }
        installBtn.disabled = true;
        installBtn.textContent = 'Installing…';
        try {
            await window.__TAURI__.core.invoke('install_update');
            // install_update calls app.restart() — execution stops here.
        } catch (err) {
            installBtn.disabled = false;
            installBtn.textContent = 'Install & restart';
            showToast('Update failed: ' + (err && err.message ? err.message : err), 'error');
        }
    });

    dismissBtn.addEventListener('click', () => {
        if (pendingVersion) {
            try { localStorage.setItem(UPDATE_DISMISS_KEY, pendingVersion); } catch (_) {}
        }
        banner.hidden = true;
    });

    // Defer the first check so it doesn't compete with app startup work.
    setTimeout(checkForUpdate, 5000);
    // Re-check every 6 hours for long-running sessions.
    setInterval(checkForUpdate, 6 * 60 * 60 * 1000);
})();

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

// ─── SMTP Settings (outbound) ───

document.getElementById('email-smtp-password-toggle')?.addEventListener('click', function() {
    const input = document.getElementById('email-smtp-password');
    if (input) {
        input.type = input.type === 'password' ? 'text' : 'password';
    }
});

document.getElementById('save-smtp-btn')?.addEventListener('click', async function() {
    const password = document.getElementById('email-smtp-password').value;
    try {
        const result = await window.__TAURI__.core.invoke('save_smtp_settings', {
            smtpServer: document.getElementById('email-smtp-server').value,
            smtpPort: parseInt(document.getElementById('email-smtp-port').value) || 587,
            smtpUsername: document.getElementById('email-smtp-username').value,
            smtpPassword: password || null,
            smtpUseTls: document.getElementById('email-smtp-use-tls').checked,
            fromAddress: document.getElementById('email-from-address').value,
        });
        showSmtpTestResult(true, result);
    } catch (e) {
        showSmtpTestResult(false, e.toString());
    }
});

document.getElementById('test-smtp-btn')?.addEventListener('click', async function() {
    const btn = this;
    btn.disabled = true;
    btn.textContent = 'Testing...';
    try {
        const result = await window.__TAURI__.core.invoke('test_smtp_connection', {
            smtpServer: document.getElementById('email-smtp-server').value,
            smtpPort: parseInt(document.getElementById('email-smtp-port').value) || 587,
            smtpUsername: document.getElementById('email-smtp-username').value,
            smtpPassword: document.getElementById('email-smtp-password').value,
            smtpUseTls: document.getElementById('email-smtp-use-tls').checked,
            fromAddress: document.getElementById('email-from-address').value,
        });
        showSmtpTestResult(result.success, result.message);
    } catch (e) {
        showSmtpTestResult(false, e.toString());
    } finally {
        btn.disabled = false;
        btn.textContent = 'Test SMTP';
    }
});

function showSmtpTestResult(success, message) {
    const el = document.getElementById('smtp-test-result');
    if (!el) return;
    el.className = 'test-result ' + (success ? 'test-success' : 'test-error');
    el.textContent = message;
    el.classList.remove('hidden');
    setTimeout(() => el.classList.add('hidden'), 5000);
}

// ─── Email setup wizard (Phase 2) ───
//
// Provider autodetect, combined Test & Save, translated error banners,
// connected-state pill. Wraps the existing split-button flow with a
// guided one-click path; the four split buttons remain for power users
// who want to test or save only one half.
//
// Tauri JSON casing (verified 2026-05-13):
//  - command argument names: camelCase (Tauri auto-converts from Rust snake_case args)
//  - struct fields in returned values: snake_case (serde uses field names as-is,
//    no `rename_all` on ProviderHint / TestResult / TranslatedError).
// So we invoke with { smtpPassword: "..." } but read result.imap.ok and
// hint.app_password_url.

const EMAIL_DETECT_DEBOUNCE_MS = 600;
let _emailDetectTimer = null;
let _lastDetectedEmail = null;
let _lastProviderHint = null;
let _emailDetectAbortToken = 0;

function emailDomain(addr) {
    if (!addr) return null;
    const at = addr.indexOf('@');
    if (at < 0 || at === addr.length - 1) return null;
    return addr.slice(at + 1).trim().toLowerCase();
}

function looksLikeFullEmail(addr) {
    if (!addr) return false;
    const m = addr.trim().match(/^[^\s@]+@([^\s@]+\.[^\s@]+)$/);
    return !!m;
}

function securityToTls(security) {
    // For incoming IMAP: 993 SSL or 143 STARTTLS both want TLS on; "none" => off.
    return security === 'ssl' || security === 'start_tls';
}

function smtpSecurityToImplicitTls(security) {
    // Our `email-smtp-use-tls` checkbox means "implicit SSL/TLS on 465".
    // STARTTLS on 587 -> unchecked. None -> unchecked.
    return security === 'ssl';
}

function setIfEmpty(id, value) {
    const el = document.getElementById(id);
    if (!el) return;
    if (el.type === 'checkbox') {
        // For checkboxes we don't have a clean "empty" notion; only set if
        // the user hasn't interacted (dataset.userTouched not set).
        if (!el.dataset.userTouched) {
            el.checked = !!value;
        }
        return;
    }
    if (!el.value || el.value.trim() === '') {
        el.value = value == null ? '' : String(value);
    }
}

// Mark a checkbox as user-touched so autodetect won't clobber it.
document.querySelectorAll('#email-use-tls, #email-smtp-use-tls').forEach((cb) => {
    cb.addEventListener('change', () => { cb.dataset.userTouched = '1'; });
});

function applyProviderHint(hint) {
    _lastProviderHint = hint;
    const detailsEl = document.getElementById('email-advanced-details');

    if (!hint) {
        renderProviderHintEmpty();
        // Open advanced if we have a full email but no match — user needs
        // to fill server settings manually.
        const username = document.getElementById('email-username')?.value.trim() || '';
        if (detailsEl && looksLikeFullEmail(username)) {
            detailsEl.open = true;
        }
        return;
    }

    // Pre-fill IMAP fields.
    setIfEmpty('email-imap-server', hint.incoming?.host);
    setIfEmpty('email-imap-port', hint.incoming?.port);
    const imapTlsEl = document.getElementById('email-use-tls');
    if (imapTlsEl && !imapTlsEl.dataset.userTouched && hint.incoming) {
        imapTlsEl.checked = securityToTls(hint.incoming.security);
    }

    // Pre-fill SMTP fields.
    setIfEmpty('email-smtp-server', hint.outgoing?.host);
    setIfEmpty('email-smtp-port', hint.outgoing?.port);
    const smtpTlsEl = document.getElementById('email-smtp-use-tls');
    if (smtpTlsEl && !smtpTlsEl.dataset.userTouched && hint.outgoing) {
        smtpTlsEl.checked = smtpSecurityToImplicitTls(hint.outgoing.security);
    }

    // Mirror email address into SMTP username + From if those are empty.
    const username = document.getElementById('email-username')?.value.trim() || '';
    if (username) {
        setIfEmpty('email-smtp-username', username);
        setIfEmpty('email-from-address', username);
    }

    renderProviderHint(hint);
}

function renderProviderHintEmpty() {
    const box = document.getElementById('email-provider-hint');
    if (!box) return;
    const username = document.getElementById('email-username')?.value.trim() || '';
    if (looksLikeFullEmail(username)) {
        const domain = emailDomain(username);
        box.className = 'email-provider-hint';
        box.style.display = '';
        box.innerHTML = `
            <p class="email-provider-hint-title">No match for ${escapeHtml(domain || '')}</p>
            <p class="email-provider-hint-body">We don't recognise this provider — fill in the server settings under Advanced below, or hit Test &amp; Save to try common defaults.</p>
        `;
    } else {
        box.style.display = 'none';
        box.innerHTML = '';
    }
}

function renderProviderHint(hint) {
    const box = document.getElementById('email-provider-hint');
    if (!box) return;
    const isBridge = hint.auth_kind === 'bridge_required';
    box.className = 'email-provider-hint' + (isBridge ? ' email-provider-hint-warning' : '');
    box.style.display = '';

    const parts = [];
    parts.push(`<p class="email-provider-hint-title">Detected: <span class="email-provider-hint-name">${escapeHtml(hint.display_name)}</span></p>`);

    if (hint.notes) {
        parts.push(`<p class="email-provider-hint-notes">${escapeHtml(hint.notes)}</p>`);
    }

    const actions = [];
    if (hint.auth_kind === 'app_password' && hint.app_password_url) {
        actions.push(`<a class="email-provider-hint-link" href="${escapeHtml(hint.app_password_url)}" target="_blank" rel="noopener">Open ${escapeHtml(hint.display_name)} app passwords &rarr;</a>`);
    }
    if (isBridge) {
        actions.push(`<a class="email-provider-hint-link" href="https://proton.me/mail/bridge" target="_blank" rel="noopener">Open Proton Bridge docs &rarr;</a>`);
    }
    if (hint.auth_kind === 'o_auth2') {
        // OAuth2 will land in Move #3 of the integrations push.
        parts.push(`<p class="email-provider-hint-notes">${escapeHtml(hint.display_name)} prefers OAuth login. App password support is still available on most accounts — open the link to generate one.</p>`);
    }
    if (actions.length) {
        parts.push(`<div class="email-provider-hint-actions">${actions.join('')}</div>`);
    }

    box.innerHTML = parts.join('');
}

async function runEmailDetect(email) {
    if (!invoke) return;
    if (!looksLikeFullEmail(email)) {
        _lastDetectedEmail = null;
        applyProviderHint(null);
        return;
    }
    if (_lastDetectedEmail === email) return;
    _lastDetectedEmail = email;

    const token = ++_emailDetectAbortToken;
    try {
        const hint = await invoke('email_detect', { email });
        if (token !== _emailDetectAbortToken) return; // superseded
        applyProviderHint(hint || null);
    } catch (e) {
        if (token !== _emailDetectAbortToken) return;
        console.warn('email_detect failed:', e);
        applyProviderHint(null);
    }
}

document.getElementById('email-username')?.addEventListener('input', function() {
    const email = this.value.trim();
    if (_emailDetectTimer) clearTimeout(_emailDetectTimer);
    _emailDetectTimer = setTimeout(() => runEmailDetect(email), EMAIL_DETECT_DEBOUNCE_MS);
});
document.getElementById('email-username')?.addEventListener('blur', function() {
    if (_emailDetectTimer) { clearTimeout(_emailDetectTimer); _emailDetectTimer = null; }
    runEmailDetect(this.value.trim());
});

// Well-known ports override the checkbox, custom ports fall back to it.
// This prevents the most common misconfiguration (SSL checkbox + STARTTLS
// port 587 -> rustls "InvalidContentType" against a plaintext banner)
// from reaching the backend. The checkbox is intentionally kept as a hint
// for non-standard ports so the legacy save_email_settings /
// save_smtp_settings payload shapes (which take the boolean directly) are
// unaffected.
function inferImapSecurity(port, checkboxChecked) {
    if (port === 993) return 'ssl';
    if (port === 143) return checkboxChecked ? 'start_tls' : 'none';
    return checkboxChecked ? 'ssl' : 'none';
}

function inferSmtpSecurity(port, checkboxChecked) {
    if (port === 465) return 'ssl';
    if (port === 587 || port === 25) return 'start_tls';
    return checkboxChecked ? 'ssl' : 'start_tls';
}

function readEmailTestConfig() {
    const imapPort = parseInt(document.getElementById('email-imap-port').value, 10) || 993;
    const smtpPort = parseInt(document.getElementById('email-smtp-port').value, 10) || 587;
    return {
        imap_host: document.getElementById('email-imap-server').value.trim(),
        imap_port: imapPort,
        imap_security: inferImapSecurity(imapPort, document.getElementById('email-use-tls').checked),
        imap_username: document.getElementById('email-username').value.trim(),

        smtp_host: document.getElementById('email-smtp-server').value.trim(),
        smtp_port: smtpPort,
        smtp_security: inferSmtpSecurity(smtpPort, document.getElementById('email-smtp-use-tls').checked),
        smtp_username: (document.getElementById('email-smtp-username').value.trim()
            || document.getElementById('email-username').value.trim()),
    };
}

function setEmailButtonsDisabled(disabled) {
    [
        'test-and-save-btn',
        'test-email-btn',
        'save-email-btn',
        'test-smtp-btn',
        'save-smtp-btn',
    ].forEach((id) => {
        const b = document.getElementById(id);
        if (b) b.disabled = disabled;
    });
}

function showCombinedResultSuccess(message, note) {
    const el = document.getElementById('email-combined-result');
    if (!el) return;
    el.className = 'test-result success test-result-rich';
    const noteHtml = note
        ? `<p class="test-result-body" style="margin-top:0.5em;opacity:0.85;">${escapeHtml(note)}</p>`
        : '';
    el.innerHTML = `
        <p class="test-result-title">Connected</p>
        <p class="test-result-body">${escapeHtml(message)}</p>
        ${noteHtml}
    `;
    el.classList.remove('hidden');
}

function showCombinedResultError(translated, rawError, stageLabel) {
    const el = document.getElementById('email-combined-result');
    if (!el) return;
    el.className = 'test-result error test-result-rich';

    if (translated) {
        const actionHtml = (translated.action_label && translated.action_url)
            ? `<a class="test-result-action" href="${escapeHtml(translated.action_url)}" target="_blank" rel="noopener">${escapeHtml(translated.action_label)} &rarr;</a>`
            : '';
        const detailsHtml = rawError
            ? `<details class="test-result-details"><summary>Technical details</summary><pre>${escapeHtml(rawError)}</pre></details>`
            : '';
        el.innerHTML = `
            <p class="test-result-title">${escapeHtml(translated.title)}</p>
            <p class="test-result-body">${escapeHtml(translated.body)}</p>
            ${actionHtml}
            ${detailsHtml}
        `;
    } else {
        const prefix = stageLabel ? `${stageLabel}: ` : '';
        el.innerHTML = `
            <p class="test-result-title">Connection failed</p>
            <p class="test-result-body">${escapeHtml(prefix + (rawError || 'Unknown error'))}</p>
        `;
    }
    el.classList.remove('hidden');
}

function hideCombinedResult() {
    const el = document.getElementById('email-combined-result');
    if (el) {
        el.classList.add('hidden');
        el.innerHTML = '';
    }
}

async function saveImapHalf() {
    const password = document.getElementById('email-password').value;
    await invoke('save_email_settings', {
        enabled: document.getElementById('email-enabled').checked,
        imapServer: document.getElementById('email-imap-server').value,
        imapPort: parseInt(document.getElementById('email-imap-port').value, 10) || 993,
        username: document.getElementById('email-username').value,
        password: password || null,
        useTls: document.getElementById('email-use-tls').checked,
        folders: document.getElementById('email-folders').value,
        pollIntervalSecs: parseInt(document.getElementById('email-poll-interval').value, 10) || 60,
        lookbackHours: parseInt(document.getElementById('email-lookback').value, 10) || 24,
    });
}

async function saveSmtpHalf(fallbackPassword) {
    // If the user only filled the IMAP password and left SMTP blank (most
    // providers accept the same credential for both), persist the IMAP
    // password under SMTP too so the backend doesn't silently re-use a
    // stale saved value or refuse to send.
    const own = document.getElementById('email-smtp-password').value;
    const password = own || fallbackPassword || null;
    await invoke('save_smtp_settings', {
        smtpServer: document.getElementById('email-smtp-server').value,
        smtpPort: parseInt(document.getElementById('email-smtp-port').value, 10) || 587,
        smtpUsername: document.getElementById('email-smtp-username').value,
        smtpPassword: password,
        smtpUseTls: document.getElementById('email-smtp-use-tls').checked,
        fromAddress: document.getElementById('email-from-address').value,
    });
}

document.getElementById('test-and-save-btn')?.addEventListener('click', async function() {
    if (!invoke) return;
    hideCombinedResult();

    const username = document.getElementById('email-username').value.trim();
    const password = document.getElementById('email-password').value;
    const smtpPassword = document.getElementById('email-smtp-password').value || password;

    if (!username || !password) {
        showCombinedResultError(
            { title: 'Missing details', body: 'Enter your email address and password before testing.', action_label: null, action_url: null },
            null, null,
        );
        return;
    }

    const originalLabel = this.textContent;
    this.textContent = 'Testing…';
    setEmailButtonsDisabled(true);

    let result;
    try {
        const config = readEmailTestConfig();
        result = await invoke('email_test_connection', {
            config,
            password,
            smtpPassword,
        });
    } catch (e) {
        const raw = (e && e.toString) ? e.toString() : String(e);
        await renderCombinedFailure(raw, null, username);
        this.textContent = originalLabel;
        setEmailButtonsDisabled(false);
        return;
    }

    const imapOk = result?.imap?.ok;
    const smtpOk = result?.smtp?.ok;

    if (imapOk && smtpOk) {
        try {
            // If the backend auto-corrected SSL/STARTTLS to match the
            // port (synthetic stage `auto_corrected_security`), flip the
            // checkbox before persisting so the saved config reflects
            // what actually works on the wire.
            let autoCorrectNote = null;
            if (result?.smtp?.stage === 'auto_corrected_security') {
                const smtpPort = parseInt(document.getElementById('email-smtp-port').value, 10) || 587;
                const corrected = inferSmtpSecurity(smtpPort, false);
                const useTlsCheckbox = document.getElementById('email-smtp-use-tls');
                if (useTlsCheckbox) useTlsCheckbox.checked = (corrected === 'ssl');
                autoCorrectNote = `We adjusted the SSL/STARTTLS setting to match port ${smtpPort}. Click Save to keep the corrected setting.`;
            }
            await saveImapHalf();
            await saveSmtpHalf(password);
            const providerName = _lastProviderHint?.display_name || 'your email';
            showCombinedResultSuccess(`Connected to ${providerName} as ${username}.`, autoCorrectNote);
            refreshConnectedPill(true, username);
        } catch (e) {
            const raw = (e && e.toString) ? e.toString() : String(e);
            showCombinedResultError(
                { title: 'Saved settings failed', body: raw, action_label: null, action_url: null },
                raw, null,
            );
        }
    } else {
        // Pick the failed half (IMAP first if both failed).
        const failedHalf = !imapOk ? result.imap : result.smtp;
        const stageLabel = !imapOk ? `IMAP / ${failedHalf?.stage || 'connect'}` : `SMTP / ${failedHalf?.stage || 'connect'}`;
        const raw = failedHalf?.error || 'Connection failed.';
        await renderCombinedFailure(raw, stageLabel, username);
    }

    this.textContent = originalLabel;
    setEmailButtonsDisabled(false);
});

async function renderCombinedFailure(rawError, stageLabel, username) {
    let translated = null;
    try {
        translated = await invoke('email_translate_error', {
            rawError,
            domain: emailDomain(username),
        });
    } catch (e) {
        console.warn('email_translate_error failed:', e);
    }
    showCombinedResultError(translated, rawError, stageLabel);
}

// ─── Connected-state pill ───

function refreshConnectedPill(enabled, username) {
    const pill = document.getElementById('email-connected-pill');
    if (!pill) return;
    if (enabled && username && username.trim() !== '') {
        pill.textContent = `Connected as ${username.trim()}`;
        pill.style.display = '';
    } else {
        pill.style.display = 'none';
        pill.textContent = '';
    }
}

document.getElementById('email-enabled')?.addEventListener('change', function() {
    refreshConnectedPill(this.checked, document.getElementById('email-username')?.value || '');
});

// On settings panel load, evaluate the pill + auto-open Advanced for
// returning users who have any IMAP/SMTP server already set. Hooks into
// the existing loadSettings flow by waiting one tick after the DOM is
// populated — loadSettings runs synchronously inside the fetch await,
// so we listen for a 'settings-loaded' event if one exists, otherwise
// piggy-back on the next animation frame.
window.addEventListener('athen:settings-loaded', () => {
    const enabled = document.getElementById('email-enabled')?.checked || false;
    const username = document.getElementById('email-username')?.value || '';
    refreshConnectedPill(enabled, username);

    const detailsEl = document.getElementById('email-advanced-details');
    const imapServer = document.getElementById('email-imap-server')?.value || '';
    const smtpServer = document.getElementById('email-smtp-server')?.value || '';
    if (detailsEl && (imapServer || smtpServer)) {
        detailsEl.open = true;
    }
    // Trigger a passive detect for the saved address — refreshes the
    // hint card without clobbering anything (setIfEmpty guards values).
    if (looksLikeFullEmail(username)) {
        runEmailDetect(username);
    }
});

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
    const chatIdsStr = document.getElementById('telegram-chat-ids').value;
    const pollInterval = parseInt(document.getElementById('telegram-poll-interval').value);

    const allowedChatIds = chatIdsStr
        ? chatIdsStr.split(',').map(s => parseInt(s.trim())).filter(n => !isNaN(n))
        : [];

    try {
        const result = await window.__TAURI__.core.invoke('save_telegram_settings', {
            enabled: document.getElementById('telegram-enabled').checked,
            botToken: token || null,
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

// ─── Owner Contact ("My Contact Info") ───

// IdentifierKind variants the backend understands. Values match the
// snake_case scheme form (`identifier_kind_scheme` on the Rust side) so
// the round-trip through `save_owner_contact` is stable.
const OWNER_IDENTIFIER_KINDS = [
    { value: 'email', label: 'Email' },
    { value: 'telegram_user', label: 'Telegram user ID' },
    { value: 'phone', label: 'Phone' },
    { value: 'whatsapp', label: 'WhatsApp' },
    { value: 'signal', label: 'Signal' },
    { value: 'username', label: 'Username' },
    { value: 'other', label: 'Other' },
];

function renderOwnerIdentifierRow(kind, value) {
    const row = document.createElement('div');
    row.className = 'setting-row owner-identifier-row';
    const select = document.createElement('select');
    select.className = 'settings-select owner-identifier-kind';
    for (const k of OWNER_IDENTIFIER_KINDS) {
        const opt = document.createElement('option');
        opt.value = k.value;
        opt.textContent = k.label;
        if (k.value === kind) opt.selected = true;
        select.appendChild(opt);
    }
    const input = document.createElement('input');
    input.type = 'text';
    input.className = 'settings-input owner-identifier-value';
    input.placeholder = 'e.g. you@example.com or 123456789';
    if (value) input.value = value;
    const remove = document.createElement('button');
    remove.className = 'btn-secondary owner-identifier-remove';
    remove.title = 'Remove';
    remove.type = 'button';
    remove.textContent = '×';
    remove.addEventListener('click', () => row.remove());
    row.appendChild(select);
    row.appendChild(input);
    row.appendChild(remove);
    return row;
}

function clearOwnerContactError() {
    const errEl = document.getElementById('owner-contact-error');
    if (!errEl) return;
    errEl.style.display = 'none';
    errEl.textContent = '';
}

function showOwnerContactError(msg) {
    const errEl = document.getElementById('owner-contact-error');
    if (!errEl) return;
    errEl.textContent = msg;
    errEl.style.display = 'block';
}

function renderOwnerContact(view) {
    const nameEl = document.getElementById('owner-name');
    const listEl = document.getElementById('owner-identifiers-list');
    if (!nameEl || !listEl) return;
    nameEl.value = view ? (view.name || '') : '';
    listEl.innerHTML = '';
    if (view && Array.isArray(view.identifiers) && view.identifiers.length > 0) {
        for (const id of view.identifiers) {
            listEl.appendChild(renderOwnerIdentifierRow(id.kind, id.value));
        }
    }
}

async function loadOwnerContact() {
    if (!invoke) return;
    clearOwnerContactError();
    try {
        const view = await invoke('get_owner_contact');
        renderOwnerContact(view);
    } catch (err) {
        console.error('Failed to load owner contact:', err);
        showOwnerContactError('Failed to load: ' + (err && err.toString ? err.toString() : err));
    }
}

document.getElementById('owner-add-identifier-btn')?.addEventListener('click', function () {
    const listEl = document.getElementById('owner-identifiers-list');
    if (!listEl) return;
    listEl.appendChild(renderOwnerIdentifierRow('email', ''));
});

document.getElementById('save-owner-contact-btn')?.addEventListener('click', async function () {
    clearOwnerContactError();
    const nameEl = document.getElementById('owner-name');
    const listEl = document.getElementById('owner-identifiers-list');
    if (!nameEl || !listEl) return;
    const name = (nameEl.value || '').trim();
    const rows = listEl.querySelectorAll('.owner-identifier-row');
    const identifiers = [];
    for (const row of rows) {
        const kindEl = row.querySelector('.owner-identifier-kind');
        const valueEl = row.querySelector('.owner-identifier-value');
        if (!kindEl || !valueEl) continue;
        const value = (valueEl.value || '').trim();
        if (!value) continue;
        identifiers.push({ kind: kindEl.value, value });
    }
    const btn = this;
    btn.disabled = true;
    const origText = btn.textContent;
    btn.textContent = 'Saving…';
    try {
        const view = await invoke('save_owner_contact', { name, identifiers });
        renderOwnerContact(view);
        showToast('Contact info saved.', 'success');
    } catch (err) {
        showOwnerContactError(err && err.toString ? err.toString() : String(err));
    } finally {
        btn.disabled = false;
        btn.textContent = origText;
    }
});

document.getElementById('clear-owner-contact-btn')?.addEventListener('click', async function () {
    clearOwnerContactError();
    const ok = window.confirm(
        'Remove your contact info? Athen will no longer recognize messages as coming from you.'
    );
    if (!ok) return;
    const btn = this;
    btn.disabled = true;
    const origText = btn.textContent;
    btn.textContent = 'Removing…';
    try {
        await invoke('clear_owner_contact');
        renderOwnerContact(null);
        showToast('Contact info removed.', 'success');
    } catch (err) {
        showOwnerContactError(err && err.toString ? err.toString() : String(err));
    } finally {
        btn.disabled = false;
        btn.textContent = origText;
    }
});

// ─── Web Search Settings ───

function showWebSearchTestResult(success, message) {
    const el = document.getElementById('web-search-test-result');
    if (!el) return;
    el.className = 'test-result ' + (success ? 'test-success' : 'test-error');
    el.textContent = message;
    el.classList.remove('hidden');
    setTimeout(() => el.classList.add('hidden'), 5000);
}

document.getElementById('web-search-brave-toggle')?.addEventListener('click', function (ev) {
    ev.preventDefault();
    const input = document.getElementById('web-search-brave');
    if (!input) return;
    input.type = input.type === 'password' ? 'text' : 'password';
});

document.getElementById('web-search-tavily-toggle')?.addEventListener('click', function (ev) {
    ev.preventDefault();
    const input = document.getElementById('web-search-tavily');
    if (!input) return;
    input.type = input.type === 'password' ? 'text' : 'password';
});

document.getElementById('test-web-search-brave-btn')?.addEventListener('click', async function () {
    const btn = this;
    const key = (document.getElementById('web-search-brave').value || '').trim();
    if (!key) {
        showWebSearchTestResult(false, 'Enter a Brave key first.');
        return;
    }
    btn.disabled = true;
    const orig = btn.textContent;
    btn.textContent = 'Testing…';
    try {
        const result = await window.__TAURI__.core.invoke('test_web_search_provider', {
            provider: 'brave',
            apiKey: key,
        });
        showWebSearchTestResult(result.success, result.message);
    } catch (e) {
        showWebSearchTestResult(false, e.toString());
    } finally {
        btn.disabled = false;
        btn.textContent = orig;
    }
});

document.getElementById('test-web-search-tavily-btn')?.addEventListener('click', async function () {
    const btn = this;
    const key = (document.getElementById('web-search-tavily').value || '').trim();
    if (!key) {
        showWebSearchTestResult(false, 'Enter a Tavily key first.');
        return;
    }
    btn.disabled = true;
    const orig = btn.textContent;
    btn.textContent = 'Testing…';
    try {
        const result = await window.__TAURI__.core.invoke('test_web_search_provider', {
            provider: 'tavily',
            apiKey: key,
        });
        showWebSearchTestResult(result.success, result.message);
    } catch (e) {
        showWebSearchTestResult(false, e.toString());
    } finally {
        btn.disabled = false;
        btn.textContent = orig;
    }
});

document.getElementById('save-web-search-btn')?.addEventListener('click', async function () {
    // Empty input → null (preserve existing). Anything else → that
    // value. Matches the convention save_provider uses for LLM keys.
    const brave = document.getElementById('web-search-brave').value;
    const tavily = document.getElementById('web-search-tavily').value;
    try {
        const result = await window.__TAURI__.core.invoke('save_web_search_settings', {
            braveApiKey: brave === '' ? null : brave,
            tavilyApiKey: tavily === '' ? null : tavily,
        });
        showWebSearchTestResult(true, result);
        await loadSettings();
    } catch (e) {
        showWebSearchTestResult(false, e.toString());
    }
});

// ─── Attachment Policy ───

function showAttachmentPolicyResult(success, message) {
    const el = document.getElementById('attachment-policy-result');
    if (!el) return;
    el.classList.remove('hidden');
    el.classList.toggle('success', !!success);
    el.classList.toggle('error', !success);
    el.textContent = message;
}

async function loadAttachmentPolicySettings() {
    try {
        const s = await window.__TAURI__.core.invoke('get_attachment_policy_settings');
        const setVal = (id, v) => {
            const el = document.getElementById(id);
            if (el != null && v != null) el.value = v;
        };
        const checked = new Set(s.mime_bundles || []);
        for (const cb of document.querySelectorAll('.att-mime-bundle-checkbox')) {
            cb.checked = checked.has(cb.dataset.bundle);
        }
        setVal('att-max-attachment-mb', s.max_attachment_mb);
        setVal('att-max-event-mb', s.max_event_mb);
        setVal('att-min-inline-trust', s.min_inline_trust);
        setVal('att-min-download-trust', s.min_download_trust);
        setVal('att-byte-ttl-days', s.byte_ttl_days);
    } catch (e) {
        console.warn('Failed to load attachment policy settings:', e);
    }
}

document
    .getElementById('save-attachment-policy-btn')
    ?.addEventListener('click', async function () {
        const bundles = Array.from(
            document.querySelectorAll('.att-mime-bundle-checkbox')
        )
            .filter((cb) => cb.checked)
            .map((cb) => cb.dataset.bundle);
        const maxAtt = parseInt(document.getElementById('att-max-attachment-mb').value, 10);
        const maxEvent = parseInt(document.getElementById('att-max-event-mb').value, 10);
        const inline = document.getElementById('att-min-inline-trust').value;
        const download = document.getElementById('att-min-download-trust').value;
        const ttl = parseInt(document.getElementById('att-byte-ttl-days').value, 10);
        try {
            const result = await window.__TAURI__.core.invoke('save_attachment_policy_settings', {
                mimeBundles: bundles,
                maxAttachmentMb: maxAtt,
                maxEventMb: maxEvent,
                minInlineTrust: inline,
                minDownloadTrust: download,
                byteTtlDays: ttl,
            });
            showAttachmentPolicyResult(true, result);
        } catch (e) {
            showAttachmentPolicyResult(false, e.toString());
        }
    });

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
    document.getElementById('wakeups-view')?.classList.add('hidden');
    document.getElementById('agent-control-view')?.classList.add('hidden');
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
    document.getElementById('wakeups-view')?.classList.add('hidden');
    document.getElementById('agent-control-view')?.classList.add('hidden');
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
    document.getElementById('wakeups-view')?.classList.add('hidden');
    document.getElementById('agent-control-view')?.classList.add('hidden');
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
    document.getElementById('wakeups-view')?.classList.add('hidden');
    document.getElementById('agent-control-view')?.classList.add('hidden');
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
    document.getElementById('wakeups-view')?.classList.add('hidden');
    document.getElementById('agent-control-view')?.classList.add('hidden');
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

// Onboarding maps derived from PROVIDER_CATALOG so the wizard never gets
// out of sync with what the backend actually supports. Populated by
// `populateOnboardingProviderPickers` once the catalog is loaded.
const ONB_LOCAL_DEFAULTS = {};
const ONB_CLOUD_HINTS = {};

function populateOnboardingProviderPickers() {
    const cloudSel = document.getElementById('onb-cloud-type');
    const localSel = document.getElementById('onb-local-type');
    if (cloudSel) cloudSel.innerHTML = '';
    if (localSel) localSel.innerHTML = '';

    for (const p of PROVIDER_CATALOG) {
        if (p.provider_type === 'cloud') {
            ONB_CLOUD_HINTS[p.id] = p.api_key_hint || 'sk-...';
            if (cloudSel) {
                const opt = document.createElement('option');
                opt.value = p.id;
                opt.textContent = p.name;
                cloudSel.appendChild(opt);
            }
        } else if (p.provider_type === 'local') {
            ONB_LOCAL_DEFAULTS[p.id] = p.default_base_url;
            if (localSel) {
                const opt = document.createElement('option');
                opt.value = p.id;
                const portHint = p.default_base_url.match(/:(\d+)$/);
                opt.textContent = p.name + (portHint ? ` (default port ${portHint[1]})` : '');
                localSel.appendChild(opt);
            }
        }
    }
}

// Memory step state — captured cloud key from the LLM step is offered as
// a default for the OpenAI embedding key so users don't paste twice.
let onbCloudKeyCache = '';
let onbCloudIdCache = '';
let onbMemSelected = null; // 'cloud' | 'ollama' | 'skip'

// Maps each step to which progress pill should light up. Welcome has no
// pill (the indicator stays hidden until the user actually starts).
const ONB_PROGRESS_FOR_STEP = {
    pick: 'pick',
    local: 'pick',
    cloud: 'pick',
    memory: 'memory',
    search: 'search',
    runtimes: 'runtimes',
    identity: 'identity',
    done: 'done',
};

function showOnboardingStep(name) {
    const overlay = document.getElementById('onboarding-overlay');
    if (!overlay) return;
    overlay.querySelectorAll('.onboarding-step').forEach((s) => {
        s.style.display = s.dataset.step === name ? '' : 'none';
    });

    const progress = document.getElementById('onb-progress');
    if (progress) {
        const target = ONB_PROGRESS_FOR_STEP[name];
        if (target) {
            progress.style.display = '';
            progress.querySelectorAll('.onb-progress-step').forEach((p) => {
                p.classList.toggle('active', p.dataset.step === target);
            });
        } else {
            progress.style.display = 'none';
        }
    }
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
        if (ok) enterMemoryStep();
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
            baseUrl: '',
            model,
            apiKey,
        });
        if (ok) {
            onbCloudKeyCache = apiKey;
            onbCloudIdCache = id;
            enterMemoryStep();
        }
    });

    wireMemoryStep();
    wireSearchStep();
    wireRuntimesStep();
    wireIdentityStep();

    document.getElementById('onb-finish')?.addEventListener('click', finishOnboarding);
}

// ─── Memory (embeddings) step ───────────────────────────────────────
//
// Reached after the user successfully configures any LLM provider. The
// device-tier hint comes from `detect_device_capabilities`; the three
// pick buttons each unfold a small config panel before the test+save.

async function enterMemoryStep() {
    showOnboardingStep('memory');
    onbMemSelected = null;
    document.getElementById('onb-mem-config').style.display = 'none';
    document.getElementById('onb-mem-status').textContent = '';

    const tierEl = document.getElementById('onb-device-tier');
    if (tierEl && !tierEl.dataset.loaded) {
        try {
            const caps = await invoke('detect_device_capabilities');
            const reason = caps.tier_reason || '';
            tierEl.textContent = reason;
            tierEl.dataset.loaded = '1';
        } catch (e) {
            console.warn('[athen] detect_device_capabilities failed:', e);
        }
    }
}

function wireMemoryStep() {
    document.getElementById('onb-back-mem')?.addEventListener('click', () => {
        showOnboardingStep('pick');
    });

    document.getElementById('onb-mem-cloud')?.addEventListener('click', () => {
        onbMemSelected = 'cloud';
        document.getElementById('onb-mem-config').style.display = '';
        document.getElementById('onb-mem-key-row').style.display = '';
        document.getElementById('onb-mem-url-row').style.display = 'none';
        const keyEl = document.getElementById('onb-mem-key');
        if (keyEl && !keyEl.value && onbCloudIdCache === 'openai') {
            keyEl.value = onbCloudKeyCache;
        }
        document.getElementById('onb-mem-model').placeholder = 'text-embedding-3-small';
    });

    document.getElementById('onb-mem-ollama')?.addEventListener('click', () => {
        onbMemSelected = 'ollama';
        document.getElementById('onb-mem-config').style.display = '';
        document.getElementById('onb-mem-key-row').style.display = 'none';
        document.getElementById('onb-mem-url-row').style.display = '';
        const urlEl = document.getElementById('onb-mem-url');
        if (urlEl && !urlEl.value) urlEl.value = 'http://localhost:11434';
        document.getElementById('onb-mem-model').placeholder = 'nomic-embed-text';
    });

    document.getElementById('onb-mem-skip')?.addEventListener('click', async () => {
        try {
            await invoke('save_embedding_settings', {
                mode: 'Off',
                provider: 'keyword',
                model: null,
                baseUrl: null,
                apiKey: null,
            });
        } catch (e) {
            console.warn('[athen] save embedding (skip) failed:', e);
        }
        enterSearchStep();
    });

    document.getElementById('onb-mem-test')?.addEventListener('click', async () => {
        if (!onbMemSelected) return;
        const provider = onbMemSelected === 'cloud' ? 'openai' : 'ollama';
        const baseUrl = onbMemSelected === 'ollama'
            ? (document.getElementById('onb-mem-url').value || 'http://localhost:11434')
            : null;
        const apiKey = onbMemSelected === 'cloud'
            ? document.getElementById('onb-mem-key').value.trim()
            : null;
        const model = document.getElementById('onb-mem-model').value || null;

        if (onbMemSelected === 'cloud' && !apiKey) {
            setOnbStatus('onb-mem-status', 'err', 'API key required.');
            return;
        }

        setOnbStatus('onb-mem-status', 'busy', 'Testing connection…');
        try {
            const result = await invoke('test_embedding_provider', {
                provider,
                model,
                baseUrl,
                apiKey,
            });
            if (!result || !result.success) {
                setOnbStatus('onb-mem-status', 'err', (result && result.message) || 'Connection failed');
                return;
            }
        } catch (e) {
            setOnbStatus('onb-mem-status', 'err', String(e));
            return;
        }

        setOnbStatus('onb-mem-status', 'busy', 'Saving…');
        try {
            await invoke('save_embedding_settings', {
                mode: 'Specific',
                provider,
                model,
                baseUrl,
                apiKey,
            });
        } catch (e) {
            setOnbStatus('onb-mem-status', 'err', 'Save failed: ' + e);
            return;
        }

        setOnbStatus('onb-mem-status', 'ok', 'Saved.');
        enterSearchStep();
    });
}

// ─── Search step ────────────────────────────────────────────────────
//
// Optional. Brave / Tavily keys upgrade the keyless DDG fallback. Both
// fields can be left blank — the runtime always has a working chain
// because DuckDuckGo doesn't require a key.

async function enterSearchStep() {
    showOnboardingStep('search');
}

function wireSearchStep() {
    document.getElementById('onb-search-skip')?.addEventListener('click', () => {
        enterRuntimesStep();
    });

    document.getElementById('onb-search-save')?.addEventListener('click', async () => {
        const brave = (document.getElementById('onb-search-brave').value || '').trim();
        const tavily = (document.getElementById('onb-search-tavily').value || '').trim();

        // Test whatever was provided before saving so the user gets a
        // meaningful error if the key is wrong, instead of a silent
        // restart.
        if (brave) {
            setOnbStatus('onb-search-status', 'busy', 'Testing Brave…');
            try {
                const result = await invoke('test_web_search_provider', {
                    provider: 'brave',
                    apiKey: brave,
                });
                if (!result || !result.success) {
                    setOnbStatus(
                        'onb-search-status',
                        'err',
                        'Brave: ' + ((result && result.message) || 'failed'),
                    );
                    return;
                }
            } catch (e) {
                setOnbStatus('onb-search-status', 'err', 'Brave: ' + String(e));
                return;
            }
        }
        if (tavily) {
            setOnbStatus('onb-search-status', 'busy', 'Testing Tavily…');
            try {
                const result = await invoke('test_web_search_provider', {
                    provider: 'tavily',
                    apiKey: tavily,
                });
                if (!result || !result.success) {
                    setOnbStatus(
                        'onb-search-status',
                        'err',
                        'Tavily: ' + ((result && result.message) || 'failed'),
                    );
                    return;
                }
            } catch (e) {
                setOnbStatus('onb-search-status', 'err', 'Tavily: ' + String(e));
                return;
            }
        }

        setOnbStatus('onb-search-status', 'busy', 'Saving…');
        try {
            await invoke('save_web_search_settings', {
                braveApiKey: brave,
                tavilyApiKey: tavily,
            });
        } catch (e) {
            setOnbStatus('onb-search-status', 'err', 'Save failed: ' + e);
            return;
        }
        setOnbStatus('onb-search-status', 'ok', 'Saved.');
        enterRuntimesStep();
    });
}

// ─── Runtimes (Python / Node) step ──────────────────────────────────
//
// Last hop before "done". Probes for a system Python and Node and, if
// either is missing, offers a one-click portable install. The install
// runs entirely in the backend; we listen on `runtime-install-progress`
// for live byte counters and re-render the row's status text. Skipping
// is fully supported — Athen falls back to whatever the next probe
// finds at runtime, same as before this step existed.

let onbRuntimesUnlisten = null;
let onbRuntimesInstalling = new Set();

async function enterRuntimesStep() {
    showOnboardingStep('runtimes');
    if (!onbRuntimesUnlisten && window.__TAURI__?.event?.listen) {
        try {
            onbRuntimesUnlisten = await window.__TAURI__.event.listen(
                'runtime-install-progress',
                (e) => updateRuntimeProgress(e.payload),
            );
        } catch (err) {
            console.warn('[athen] runtime-install-progress listen failed:', err);
        }
    }
    await refreshRuntimesStatus();
}

function updateRuntimeProgress(payload) {
    if (!payload) return;
    const row = document.querySelector(
        `.onb-runtime-status[data-kind="${payload.kind}"]`,
    );
    if (!row) return;
    const phase = payload.progress?.phase;
    if (phase === 'downloading') {
        const dl = payload.progress.downloaded || 0;
        const total = payload.progress.total;
        if (total) {
            const pct = Math.min(100, Math.floor((dl / total) * 100));
            row.textContent = `Downloading… ${pct}% (${formatMB(dl)} / ${formatMB(total)})`;
        } else {
            row.textContent = `Downloading… ${formatMB(dl)}`;
        }
    } else if (phase === 'verifying') {
        row.textContent = 'Verifying checksum…';
    } else if (phase === 'extracting') {
        row.textContent = 'Extracting…';
    } else if (phase === 'resolving') {
        row.textContent = 'Resolving download…';
    }
}

function formatMB(bytes) {
    return (bytes / (1024 * 1024)).toFixed(1) + ' MB';
}

async function refreshRuntimesStatus() {
    let status = null;
    try {
        status = await invoke('get_runtime_status');
    } catch (e) {
        console.warn('[athen] get_runtime_status failed:', e);
        return;
    }
    renderRuntimeRow('python', {
        system: status.system_python,
        portable: status.portable_python,
        pinned: status.python_pinned_version,
        supported: status.python_supported,
    });
    renderRuntimeRow('node', {
        system: status.system_node,
        portable: status.portable_node,
        pinned: status.node_pinned_version,
        supported: status.node_supported,
    });
}

function renderRuntimeRow(kind, info) {
    const statusEl = document.querySelector(`.onb-runtime-status[data-kind="${kind}"]`);
    const btn = document.querySelector(`.onb-runtime-install[data-kind="${kind}"]`);
    if (!statusEl || !btn) return;
    if (onbRuntimesInstalling.has(kind)) return; // mid-install — leave the streamed text alone
    if (info.system) {
        statusEl.textContent = `Found on system: ${info.system}`;
        btn.textContent = 'Reinstall portable';
        btn.disabled = !info.supported;
    } else if (info.portable) {
        statusEl.textContent = `Portable installed: ${info.portable.version}`;
        btn.textContent = 'Reinstall';
        btn.disabled = !info.supported;
    } else {
        statusEl.textContent = info.supported
            ? `Not detected — install portable v${info.pinned}`
            : 'Not supported on this OS / architecture';
        btn.textContent = 'Install';
        btn.disabled = !info.supported;
    }
}

function wireRuntimesStep() {
    document.querySelectorAll('.onb-runtime-install').forEach((btn) => {
        btn.addEventListener('click', async (ev) => {
            const kind = ev.currentTarget.dataset.kind;
            if (!kind) return;
            ev.currentTarget.disabled = true;
            onbRuntimesInstalling.add(kind);
            const statusEl = document.querySelector(`.onb-runtime-status[data-kind="${kind}"]`);
            if (statusEl) statusEl.textContent = 'Starting…';
            try {
                await invoke('install_runtime', { kind });
                if (statusEl) statusEl.textContent = 'Installed.';
            } catch (e) {
                if (statusEl) statusEl.textContent = 'Install failed: ' + e;
                console.warn('[athen] install_runtime failed:', e);
            } finally {
                onbRuntimesInstalling.delete(kind);
                ev.currentTarget.disabled = false;
                await refreshRuntimesStatus();
            }
        });
    });

    document.getElementById('onb-runtimes-skip')?.addEventListener('click', () => {
        enterIdentityStep();
    });
    document.getElementById('onb-runtimes-continue')?.addEventListener('click', () => {
        enterIdentityStep();
    });
}

// ─── Identity ("Who are you?") step ─────────────────────────────────
//
// Final substantive step before `done`. Reuses the owner-contact module
// (`OWNER_IDENTIFIER_KINDS`, `renderOwnerIdentifierRow`) so the wizard
// shares the validation surface with Settings → Connections → My
// Contact Info. The step is optional: skipping or submitting empty just
// advances to `done` without calling `save_owner_contact`.

function appendOnbOwnerIdentifierRow(kind, value) {
    const listEl = document.getElementById('onb-owner-identifiers-list');
    if (!listEl) return;
    listEl.appendChild(renderOwnerIdentifierRow(kind || 'email', value || ''));
}

function clearOnbOwnerError() {
    const errEl = document.getElementById('onb-owner-error');
    if (!errEl) return;
    errEl.style.display = 'none';
    errEl.textContent = '';
}

function showOnbOwnerError(msg) {
    const errEl = document.getElementById('onb-owner-error');
    if (!errEl) return;
    errEl.textContent = msg;
    errEl.style.display = 'block';
}

async function enterIdentityStep() {
    showOnboardingStep('identity');
    clearOnbOwnerError();
    // Seed an empty identifier row so the user sees the shape of the
    // input without having to click + first. Only do this once per
    // visit — re-entering after a back/forward should keep what they
    // typed.
    const listEl = document.getElementById('onb-owner-identifiers-list');
    if (listEl && !listEl.dataset.seeded) {
        appendOnbOwnerIdentifierRow('email', '');
        listEl.dataset.seeded = '1';
    }
}

function wireIdentityStep() {
    document.getElementById('onb-owner-add-identifier-btn')?.addEventListener('click', () => {
        appendOnbOwnerIdentifierRow('email', '');
    });

    document.getElementById('onb-back-identity')?.addEventListener('click', () => {
        showOnboardingStep('runtimes');
    });

    document.getElementById('onb-skip-identity')?.addEventListener('click', () => {
        showOnboardingStep('done');
    });

    document.getElementById('onb-next-identity')?.addEventListener('click', async () => {
        clearOnbOwnerError();
        const nameEl = document.getElementById('onb-owner-name');
        const listEl = document.getElementById('onb-owner-identifiers-list');
        if (!nameEl || !listEl) {
            showOnboardingStep('done');
            return;
        }
        const name = (nameEl.value || '').trim();
        const rows = listEl.querySelectorAll('.owner-identifier-row');
        const identifiers = [];
        for (const row of rows) {
            const kindEl = row.querySelector('.owner-identifier-kind');
            const valueEl = row.querySelector('.owner-identifier-value');
            if (!kindEl || !valueEl) continue;
            const value = (valueEl.value || '').trim();
            if (!value) continue;
            identifiers.push({ kind: kindEl.value, value });
        }
        // Empty → treat as Skip. Don't bother the backend; identity is
        // optional and Athen still works without owner trust.
        if (!name && identifiers.length === 0) {
            showOnboardingStep('done');
            return;
        }
        const btn = document.getElementById('onb-next-identity');
        if (btn) {
            btn.disabled = true;
            btn.textContent = 'Saving…';
        }
        try {
            await invoke('save_owner_contact', { name, identifiers });
            showOnboardingStep('done');
        } catch (err) {
            showOnbOwnerError(err && err.toString ? err.toString() : String(err));
        } finally {
            if (btn) {
                btn.disabled = false;
                btn.textContent = 'Continue';
            }
        }
    });
}

async function maybeRunOnboarding() {
    if (!invoke) return;

    // Catalog has to be loaded before we render the wizard or the
    // settings template buttons, because both pull from it.
    await loadProviderCatalog();
    await loadModelFamilies();
    populateOnboardingProviderPickers();
    renderProviderTemplates();

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

// ─── Wake-ups (scheduled / recurring / one-shot triggers) ───

const wakeupsView = document.getElementById('wakeups-view');
const wakeupsBtn = document.getElementById('wakeups-btn');
const wakeupsBack = document.getElementById('wakeups-back');
const wakeupsListEl = document.getElementById('wakeups-list');
const wakeupsEmptyEl = document.getElementById('wakeups-empty');
const wakeupsForm = document.getElementById('wakeups-form');
const wakeupsNewBtn = document.getElementById('wakeups-new-btn');
const wakeupFormCancel = document.getElementById('wakeup-form-cancel');
const wakeupFormError = document.getElementById('wakeup-form-error');
const wakeupScheduleKindEl = document.getElementById('wakeup-schedule-kind');
const wakeupOneshotFields = document.getElementById('wakeup-oneshot-fields');
const wakeupCronFields = document.getElementById('wakeup-cron-fields');
const wakeupIntervalFields = document.getElementById('wakeup-interval-fields');
const wakeupCalGridEl = document.getElementById('wakeup-cal-grid');
const wakeupCalMonthLabelEl = document.getElementById('wakeup-cal-month-label');
const wakeupCalPrevBtn = document.getElementById('wakeup-cal-prev');
const wakeupCalNextBtn = document.getElementById('wakeup-cal-next');
const wakeupQuickDatesEl = document.getElementById('wakeup-quick-dates');
const wakeupHourEl = document.getElementById('wakeup-hour');
const wakeupMinuteEl = document.getElementById('wakeup-minute');
const wakeupDatetimePreviewEl = document.getElementById('wakeup-datetime-preview');
const wakeupCronExprEl = document.getElementById('wakeup-cron-expr');
const wakeupCronTzEl = document.getElementById('wakeup-cron-tz');
const wakeupIntervalSecsEl = document.getElementById('wakeup-interval-secs');
const wakeupInstructionEl = document.getElementById('wakeup-instruction');
const wakeupArcSelectEl = document.getElementById('wakeup-arc');
const wakeupAutonomyEl = document.getElementById('wakeup-autonomy');
const wakeupToolListEl = document.getElementById('wakeup-tool-list');
const wakeupContactListEl = document.getElementById('wakeup-contact-list');

function showWakeups() {
    if (!wakeupsView) return;
    if (typeof appView !== 'undefined' && appView) appView.style.display = 'none';
    settingsView?.classList.add('hidden');
    timelineView?.classList.add('hidden');
    calendarView?.classList.add('hidden');
    document.getElementById('wakeups-view')?.classList.add('hidden');
    document.getElementById('agent-control-view')?.classList.add('hidden');
    notificationsView?.classList.add('hidden');
    contactsView?.classList.add('hidden');
    memoryView?.classList.add('hidden');
    document.getElementById('sidebar').style.display = '';
    if (typeof timelineRefreshInterval !== 'undefined' && timelineRefreshInterval) {
        clearInterval(timelineRefreshInterval);
        timelineRefreshInterval = null;
    }
    wakeupsView.classList.remove('hidden');
    closeSidebar();
    loadWakeups();
}

function hideWakeups() {
    wakeupsView?.classList.add('hidden');
    document.getElementById('sidebar').style.display = '';
    if (typeof appView !== 'undefined' && appView) appView.style.display = 'flex';
    inputEl?.focus();
}

function setWakeupScheduleKind(kind) {
    wakeupOneshotFields?.classList.toggle('hidden', kind !== 'one_shot');
    wakeupCronFields?.classList.toggle('hidden', kind !== 'cron');
    wakeupIntervalFields?.classList.toggle('hidden', kind !== 'interval');
}

// Inline custom datetime picker for one-shot wake-ups. Native
// <input type="datetime-local"> freezes the WebKitGTK window — see
// memory feedback_native_datepicker_webkitgtk.md. Everything below is
// rendered inline in the form so there's no popup focus to fight.
let wakeupSelectedDate = null;   // Date at midnight local — the chosen day
let wakeupViewedMonth = null;    // Date at the 1st of the month being shown

const WAKEUP_MONTH_NAMES = [
    'January', 'February', 'March', 'April', 'May', 'June',
    'July', 'August', 'September', 'October', 'November', 'December',
];
const WAKEUP_DOW_NAMES = ['Mon', 'Tue', 'Wed', 'Thu', 'Fri', 'Sat', 'Sun'];

function midnight(date) {
    const d = new Date(date);
    d.setHours(0, 0, 0, 0);
    return d;
}

function sameDay(a, b) {
    return a && b && a.getFullYear() === b.getFullYear()
        && a.getMonth() === b.getMonth()
        && a.getDate() === b.getDate();
}

// Tracks which existing wake-up the form is currently editing.
// `null` means "create a new one" (the default).
let wakeupEditingId = null;

function openWakeupForm(existing) {
    if (!wakeupsForm) return;
    wakeupEditingId = existing ? existing.id : null;

    // Seed the picker. For edits we pull from the existing schedule;
    // for new ones we default to "in 5 minutes" so smoke tests fire fast.
    let seedDate;
    if (existing && existing.next_fire_at) {
        seedDate = new Date(existing.next_fire_at);
    } else if (existing && existing.schedule_kind === 'one_shot' && existing.schedule_summary) {
        // Fallback: schedule_summary is "Once at <RFC3339>" — try to parse it.
        const match = existing.schedule_summary.match(/Once at (.+)$/);
        const parsed = match ? new Date(match[1]) : null;
        seedDate = parsed && !Number.isNaN(parsed.getTime())
            ? parsed
            : new Date(Date.now() + 5 * 60_000);
    } else {
        seedDate = new Date(Date.now() + 5 * 60_000);
    }
    wakeupSelectedDate = midnight(seedDate);
    wakeupViewedMonth = new Date(wakeupSelectedDate.getFullYear(), wakeupSelectedDate.getMonth(), 1);
    if (wakeupHourEl) wakeupHourEl.value = String(seedDate.getHours());
    if (wakeupMinuteEl) wakeupMinuteEl.value = String(seedDate.getMinutes()).padStart(2, '0');

    renderWakeupCalendar();
    refreshWakeupDatetimePreview();

    if (wakeupAutonomyEl) {
        wakeupAutonomyEl.value = (existing && existing.autonomy) || 'safe_only';
    }

    if (existing) {
        wakeupInstructionEl.value = existing.instruction || '';
        const kind = existing.schedule_kind || 'one_shot';
        wakeupScheduleKindEl.value = kind;
        setWakeupScheduleKind(kind);
        // Cron / interval fields are best-effort: parse the summary text.
        if (kind === 'cron') {
            const m = (existing.schedule_summary || '').match(/Cron `([^`]+)` \(([^)]+)\)/);
            if (m) {
                if (wakeupCronExprEl) wakeupCronExprEl.value = m[1];
                if (wakeupCronTzEl) wakeupCronTzEl.value = m[2];
            }
        } else if (kind === 'interval') {
            const m = (existing.schedule_summary || '').match(/Every (\d+)s/);
            if (m && wakeupIntervalSecsEl) wakeupIntervalSecsEl.value = m[1];
        }
    } else {
        wakeupInstructionEl.value = '';
        wakeupScheduleKindEl.value = 'one_shot';
        setWakeupScheduleKind('one_shot');
    }

    populateWakeupArcOptions(existing ? existing.arc_id : null);
    populateWakeupToolAllowlist(existing ? existing.tool_allowlist : null);
    populateWakeupContactAllowlist(existing ? existing.contact_allowlist : null);
    const inheritEl = document.getElementById('wakeup-inherit-restrictions');
    if (inheritEl) {
        // Default true on create; respect saved value on edit.
        inheritEl.checked = existing
            ? (existing.inherit_restrictions !== false)
            : true;
    }

    // Reflect mode in the submit button so the user knows what they're about to do.
    const saveBtn = document.getElementById('wakeup-form-save');
    if (saveBtn) saveBtn.textContent = existing ? 'Save changes' : 'Create wake-up';

    wakeupFormError?.classList.add('hidden');
    wakeupsForm.classList.remove('hidden');
    wakeupInstructionEl.focus();
}

async function populateWakeupArcOptions(preferredArcId) {
    if (!wakeupArcSelectEl || !invoke) return;
    // Reset to just the "New arc" option PLUS — synchronously — a
    // placeholder entry for the preferred arc when given, so a fast-
    // clicking user who hits Save before list_arcs returns still
    // submits the right arc_id. The placeholder gets replaced by the
    // real label below if the arc shows up in `list_arcs`.
    wakeupArcSelectEl.innerHTML = '<option value="">New arc (created on fire)</option>';
    if (preferredArcId) {
        const placeholder = document.createElement('option');
        placeholder.value = preferredArcId;
        const cached = arcMetaById.get(preferredArcId);
        placeholder.textContent = cached?.name || preferredArcId;
        wakeupArcSelectEl.appendChild(placeholder);
        wakeupArcSelectEl.value = preferredArcId;
    }
    try {
        const arcs = await invoke('list_arcs');
        if (!Array.isArray(arcs)) return;
        // Drop completed arcs to keep the dropdown short and avoid
        // pointing at a stale arc.
        const live = arcs.filter(a => a && a.id && a.status !== 'completed');
        // Capture the user's current selection (the placeholder we
        // synchronously inserted above, OR a deliberate change made
        // while list_arcs was in flight) so the live-list rebuild
        // doesn't clobber it.
        const currentSelection = wakeupArcSelectEl.value;
        wakeupArcSelectEl.innerHTML = '<option value="">New arc (created on fire)</option>';
        // Active arc first if it's in the list, then the rest by recency.
        live.sort((a, b) => {
            if (a.id === activeArcId) return -1;
            if (b.id === activeArcId) return 1;
            const at = a.updated_at || a.created_at || '';
            const bt = b.updated_at || b.created_at || '';
            return bt.localeCompare(at);
        });
        for (const arc of live) {
            const opt = document.createElement('option');
            opt.value = arc.id;
            const label = arc.name || arc.id;
            opt.textContent = arc.id === activeArcId ? `${label} (current)` : label;
            wakeupArcSelectEl.appendChild(opt);
        }
        // Resolution order: user already moved the dropdown > preferred
        // (existing wake-up's arc_id) > current arc > none.
        if (currentSelection && live.some(a => a.id === currentSelection)) {
            wakeupArcSelectEl.value = currentSelection;
        } else if (preferredArcId && live.some(a => a.id === preferredArcId)) {
            wakeupArcSelectEl.value = preferredArcId;
        } else if (!preferredArcId && activeArcId && live.some(a => a.id === activeArcId)) {
            wakeupArcSelectEl.value = activeArcId;
        } else if (preferredArcId && !live.some(a => a.id === preferredArcId)) {
            // Preferred arc isn't in the live list (deleted? completed?).
            // Keep the placeholder we synchronously inserted at the top
            // of this function so the wake-up still points at it on
            // submit — resolve_target_arc will create a fresh arc on
            // fire if the arc really is gone.
            const placeholder = document.createElement('option');
            placeholder.value = preferredArcId;
            placeholder.textContent = `${preferredArcId} (not in active list)`;
            wakeupArcSelectEl.appendChild(placeholder);
            wakeupArcSelectEl.value = preferredArcId;
        }
    } catch (e) {
        console.warn('Failed to load arcs for wake-up picker:', e);
    }
}

// Cached so opening / re-opening the form doesn't re-hit the backend.
// Cleared once the user actually leaves the wake-ups view.
let wakeupToolInventoryCache = null;
let wakeupContactInventoryCache = null;

async function populateWakeupToolAllowlist(selectedNames) {
    if (!wakeupToolListEl || !invoke) return;
    const selected = new Set(Array.isArray(selectedNames) ? selectedNames : []);
    wakeupToolListEl.innerHTML = '<div class="wakeup-allowlist-loading">Loading tools…</div>';
    try {
        if (!wakeupToolInventoryCache) {
            wakeupToolInventoryCache = await invoke('list_available_tools');
        }
        const tools = Array.isArray(wakeupToolInventoryCache) ? wakeupToolInventoryCache : [];
        wakeupToolListEl.innerHTML = '';
        if (tools.length === 0) {
            wakeupToolListEl.innerHTML = '<div class="wakeup-allowlist-empty">No tools available.</div>';
            return;
        }
        // Group by category (backend already sorted by category then label).
        const groups = new Map();
        for (const t of tools) {
            const cat = t.category || 'Other';
            if (!groups.has(cat)) groups.set(cat, []);
            groups.get(cat).push(t);
        }
        // Render each group as a collapsible <details> so the picker
        // doesn't dump 30+ rows on the user. Pre-expand groups that
        // contain anything pre-selected so edits land already-open.
        for (const [cat, items] of groups) {
            const details = document.createElement('details');
            details.className = 'wakeup-allowlist-group';
            const anySelected = items.some(t => selected.has(t.name));
            if (anySelected) details.open = true;
            const summary = document.createElement('summary');
            summary.className = 'wakeup-allowlist-group-summary';
            const selCount = items.filter(t => selected.has(t.name)).length;
            summary.textContent = selCount > 0
                ? `${cat}  (${selCount}/${items.length})`
                : `${cat}  (${items.length})`;
            details.appendChild(summary);
            for (const t of items) {
                const row = document.createElement('label');
                row.className = 'wakeup-allowlist-row';
                row.title = t.name; // raw id available on hover
                const cb = document.createElement('input');
                cb.type = 'checkbox';
                cb.value = t.name;
                cb.dataset.kind = 'tool';
                cb.checked = selected.has(t.name);
                const head = document.createElement('span');
                head.className = 'wakeup-allowlist-head';
                head.textContent = t.display_name || t.name;
                if (t.outbound) {
                    const badge = document.createElement('span');
                    badge.className = 'wakeup-allowlist-badge';
                    badge.textContent = 'sends';
                    head.appendChild(badge);
                }
                if (t.escape_hatch) {
                    const badge = document.createElement('span');
                    badge.className = 'wakeup-allowlist-badge wakeup-allowlist-badge-warn';
                    badge.textContent = 'bypasses limits';
                    badge.title = 'Spawns a sub-agent that does NOT inherit this wake-up\'s tool/contact restrictions yet (tracked under task #175). Only enable if you intend that.';
                    head.appendChild(badge);
                }
                const desc = document.createElement('span');
                desc.className = 'wakeup-allowlist-desc';
                desc.textContent = t.description || '';
                row.appendChild(cb);
                row.appendChild(head);
                row.appendChild(desc);
                details.appendChild(row);
            }
            wakeupToolListEl.appendChild(details);
        }
    } catch (e) {
        console.warn('Failed to load tool inventory:', e);
        wakeupToolListEl.innerHTML = `<div class="wakeup-allowlist-error">Failed to load tools: ${e}</div>`;
    }
}

async function populateWakeupContactAllowlist(selectedIds) {
    if (!wakeupContactListEl || !invoke) return;
    const selected = new Set(Array.isArray(selectedIds) ? selectedIds : []);
    wakeupContactListEl.innerHTML = '<div class="wakeup-allowlist-loading">Loading contacts…</div>';
    try {
        if (!wakeupContactInventoryCache) {
            wakeupContactInventoryCache = await invoke('list_contacts');
        }
        const contacts = Array.isArray(wakeupContactInventoryCache) ? wakeupContactInventoryCache : [];
        wakeupContactListEl.innerHTML = '';
        if (contacts.length === 0) {
            wakeupContactListEl.innerHTML = '<div class="wakeup-allowlist-empty">No contacts yet — add some in the Contacts view to allowlist them here.</div>';
            return;
        }
        for (const c of contacts) {
            const row = document.createElement('label');
            row.className = 'wakeup-allowlist-row';
            const cb = document.createElement('input');
            cb.type = 'checkbox';
            cb.value = c.id;
            cb.dataset.kind = 'contact';
            cb.checked = selected.has(c.id);
            const head = document.createElement('span');
            head.className = 'wakeup-allowlist-head';
            head.textContent = c.name || c.id;
            const ids = Array.isArray(c.identifiers) ? c.identifiers : [];
            const idText = ids.map(i => i.value || '').filter(Boolean).join(', ');
            const desc = document.createElement('span');
            desc.className = 'wakeup-allowlist-desc';
            desc.textContent = idText || '(no identifiers)';
            row.appendChild(cb);
            row.appendChild(head);
            row.appendChild(desc);
            wakeupContactListEl.appendChild(row);
        }
    } catch (e) {
        console.warn('Failed to load contacts for wake-up picker:', e);
        wakeupContactListEl.innerHTML = `<div class="wakeup-allowlist-error">Failed to load contacts: ${e}</div>`;
    }
}

function readWakeupAllowlist(container, kind) {
    if (!container) return null;
    const checked = container.querySelectorAll(`input[type="checkbox"][data-kind="${kind}"]:checked`);
    const out = Array.from(checked).map(c => c.value).filter(Boolean);
    return out.length === 0 ? null : out;
}

function renderWakeupCalendar() {
    if (!wakeupCalGridEl || !wakeupViewedMonth) return;
    const monthStart = new Date(wakeupViewedMonth);
    if (wakeupCalMonthLabelEl) {
        wakeupCalMonthLabelEl.textContent =
            `${WAKEUP_MONTH_NAMES[monthStart.getMonth()]} ${monthStart.getFullYear()}`;
    }
    wakeupCalGridEl.innerHTML = '';

    // DOW header row (Mon–Sun).
    for (const name of WAKEUP_DOW_NAMES) {
        const h = document.createElement('div');
        h.className = 'wakeup-cal-dow';
        h.textContent = name;
        wakeupCalGridEl.appendChild(h);
    }

    // First cell: the Monday on or before the 1st of the viewed month.
    const firstOfMonth = new Date(monthStart);
    const dow = firstOfMonth.getDay(); // 0=Sun..6=Sat
    const offset = (dow === 0 ? 6 : dow - 1); // distance back to Monday
    const cursor = new Date(firstOfMonth);
    cursor.setDate(cursor.getDate() - offset);

    const today = midnight(new Date());
    // 6 weeks always — keeps grid height stable across short months.
    for (let i = 0; i < 42; i++) {
        const cell = document.createElement('button');
        cell.type = 'button';
        cell.className = 'wakeup-cal-cell';
        cell.textContent = String(cursor.getDate());
        const cellDate = midnight(cursor);
        if (cellDate.getMonth() !== monthStart.getMonth()) {
            cell.classList.add('wakeup-cal-cell-other-month');
        }
        if (sameDay(cellDate, today)) {
            cell.classList.add('wakeup-cal-cell-today');
        }
        if (sameDay(cellDate, wakeupSelectedDate)) {
            cell.classList.add('wakeup-cal-cell-selected');
        }
        const captured = new Date(cellDate);
        cell.addEventListener('click', (ev) => {
            ev.preventDefault();
            wakeupSelectedDate = captured;
            // If user clicked into prev/next-month overflow, follow them.
            wakeupViewedMonth = new Date(captured.getFullYear(), captured.getMonth(), 1);
            renderWakeupCalendar();
            refreshWakeupDatetimePreview();
        });
        wakeupCalGridEl.appendChild(cell);
        cursor.setDate(cursor.getDate() + 1);
    }
}

function buildOneShotDate() {
    if (!wakeupSelectedDate) return null;
    const hh = parseInt(wakeupHourEl?.value, 10);
    const mm = parseInt(wakeupMinuteEl?.value, 10);
    if (!Number.isFinite(hh) || hh < 0 || hh > 23) return null;
    if (!Number.isFinite(mm) || mm < 0 || mm > 59) return null;
    const d = new Date(wakeupSelectedDate);
    d.setHours(hh, mm, 0, 0);
    return d;
}

function refreshWakeupDatetimePreview() {
    if (!wakeupDatetimePreviewEl) return;
    const d = buildOneShotDate();
    if (!d) {
        wakeupDatetimePreviewEl.textContent = '';
        wakeupDatetimePreviewEl.classList.remove('wakeup-preview-error');
        return;
    }
    const ms = d.getTime() - Date.now();
    if (ms <= 0) {
        wakeupDatetimePreviewEl.textContent = `${d.toLocaleString()}  (in the past)`;
        wakeupDatetimePreviewEl.classList.add('wakeup-preview-error');
        return;
    }
    wakeupDatetimePreviewEl.classList.remove('wakeup-preview-error');
    const totalMin = Math.round(ms / 60_000);
    const days = Math.floor(totalMin / 1440);
    const hrs = Math.floor((totalMin % 1440) / 60);
    const mins = totalMin % 60;
    const parts = [];
    if (days) parts.push(`${days}d`);
    if (hrs) parts.push(`${hrs}h`);
    if (mins || (!days && !hrs)) parts.push(`${mins}m`);
    wakeupDatetimePreviewEl.textContent = `${d.toLocaleString()}  (in ${parts.join(' ')})`;
}

function closeWakeupForm() {
    wakeupsForm?.classList.add('hidden');
    wakeupFormError?.classList.add('hidden');
    wakeupEditingId = null;
}

function buildSchedulePayload() {
    const kind = wakeupScheduleKindEl.value;
    if (kind === 'one_shot') {
        const at = buildOneShotDate();
        if (!at) throw new Error('Pick a date and a valid time');
        if (at.getTime() <= Date.now()) throw new Error('Time must be in the future');
        return { kind: 'one_shot', at: at.toISOString() };
    }
    if (kind === 'cron') {
        const expr = (wakeupCronExprEl.value || '').trim();
        const tz = (wakeupCronTzEl.value || '').trim() || 'UTC';
        if (!expr) throw new Error('Cron expression is required');
        return { kind: 'cron', expr, tz };
    }
    if (kind === 'interval') {
        const secs = parseInt(wakeupIntervalSecsEl.value, 10);
        if (!Number.isFinite(secs) || secs <= 0) throw new Error('Interval must be a positive number of seconds');
        return { kind: 'interval', every_seconds: secs, anchor: null };
    }
    throw new Error(`Unknown schedule kind: ${kind}`);
}

async function submitWakeup(ev) {
    ev.preventDefault();
    if (!invoke) return;
    wakeupFormError?.classList.add('hidden');
    let schedule;
    try {
        schedule = buildSchedulePayload();
    } catch (e) {
        wakeupFormError.textContent = e.message;
        wakeupFormError.classList.remove('hidden');
        return;
    }
    const instruction = wakeupInstructionEl.value.trim();
    if (!instruction) {
        wakeupFormError.textContent = 'Instruction is required';
        wakeupFormError.classList.remove('hidden');
        return;
    }
    const arcId = (wakeupArcSelectEl?.value || '').trim();
    const autonomy = (wakeupAutonomyEl?.value || 'safe_only').trim();
    const toolAllowlist = readWakeupAllowlist(wakeupToolListEl, 'tool');
    const contactAllowlist = readWakeupAllowlist(wakeupContactListEl, 'contact');
    const inheritEl = document.getElementById('wakeup-inherit-restrictions');
    const inheritRestrictions = inheritEl ? !!inheritEl.checked : true;
    const reqPayload = {
        instruction,
        schedule,
        autonomy,
        inherit_restrictions: inheritRestrictions,
        ...(arcId ? { arc_id: arcId } : {}),
        ...(toolAllowlist ? { tool_allowlist: toolAllowlist } : {}),
        ...(contactAllowlist ? { contact_allowlist: contactAllowlist } : {}),
    };
    try {
        console.log('[wakeup] submit', { editing: wakeupEditingId, payload: reqPayload });
        if (wakeupEditingId) {
            await invoke('update_wakeup', { id: wakeupEditingId, req: reqPayload });
        } else {
            await invoke('create_wakeup', { req: reqPayload });
        }
        console.log('[wakeup] saved, closing form and reloading list');
        closeWakeupForm();
    } catch (e) {
        console.error('[wakeup] save failed:', e);
        wakeupFormError.textContent = String(e);
        wakeupFormError.classList.remove('hidden');
        return;
    }
    // Reload outside the try so a render exception in loadWakeups doesn't
    // get swallowed as a "save failed" — they're separate concerns.
    try {
        await loadWakeups();
        console.log('[wakeup] list reloaded');
    } catch (e) {
        console.error('[wakeup] list reload failed:', e);
    }
}

function fmtWakeupTime(iso) {
    if (!iso) return '—';
    const d = new Date(iso);
    if (Number.isNaN(d.getTime())) return iso;
    const now = Date.now();
    const diffMs = d.getTime() - now;
    const absSec = Math.round(Math.abs(diffMs) / 1000);
    const local = d.toLocaleString();
    if (Math.abs(diffMs) < 90_000) return `${local}  (${diffMs >= 0 ? 'in ' : ''}${absSec}s${diffMs >= 0 ? '' : ' ago'})`;
    return local;
}

function renderWakeupRow(w) {
    const row = document.createElement('div');
    row.className = 'wakeup-row';
    row.dataset.id = w.id;

    const main = document.createElement('div');
    main.className = 'wakeup-row-main';
    const instr = document.createElement('div');
    instr.className = 'wakeup-row-instruction';
    instr.textContent = w.instruction;
    const meta = document.createElement('div');
    meta.className = 'wakeup-row-meta';
    const scheduleSpan = document.createElement('span');
    scheduleSpan.textContent = w.schedule_summary;
    const nextSpan = document.createElement('span');
    nextSpan.textContent = w.next_fire_at ? `Next: ${fmtWakeupTime(w.next_fire_at)}` : 'Done';
    const lastSpan = document.createElement('span');
    lastSpan.textContent = w.last_fired_at ? `Last fired: ${fmtWakeupTime(w.last_fired_at)}` : 'Never fired';
    const autonomySpan = document.createElement('span');
    const autonomyLabel = ({
        auto: 'Auto',
        safe_only: 'Safe-only',
        notify_only: 'Notify-only',
    })[w.autonomy] || w.autonomy;
    autonomySpan.textContent = `Autonomy: ${autonomyLabel}`;
    meta.appendChild(scheduleSpan);
    meta.appendChild(nextSpan);
    meta.appendChild(lastSpan);
    meta.appendChild(autonomySpan);
    main.appendChild(instr);
    main.appendChild(meta);

    const actions = document.createElement('div');
    actions.className = 'wakeup-row-actions';

    const editBtn = document.createElement('button');
    editBtn.className = 'btn-secondary wakeup-edit-btn';
    editBtn.type = 'button';
    editBtn.textContent = 'Edit';
    editBtn.addEventListener('click', () => openWakeupForm(w));

    const enableBtn = document.createElement('button');
    enableBtn.className = 'btn-secondary wakeup-toggle-btn';
    enableBtn.type = 'button';
    enableBtn.textContent = w.enabled ? 'Disable' : 'Enable';
    enableBtn.addEventListener('click', async () => {
        try {
            await invoke('set_wakeup_enabled', { id: w.id, enabled: !w.enabled });
            await loadWakeups();
        } catch (e) {
            alert(`Toggle failed: ${e}`);
        }
    });

    const deleteBtn = document.createElement('button');
    deleteBtn.className = 'btn-secondary wakeup-delete-btn';
    deleteBtn.type = 'button';
    deleteBtn.textContent = 'Delete';
    deleteBtn.addEventListener('click', async () => {
        if (!confirm(`Delete this wake-up?\n\n"${w.instruction}"`)) return;
        try {
            await invoke('delete_wakeup', { id: w.id });
            await loadWakeups();
        } catch (e) {
            alert(`Delete failed: ${e}`);
        }
    });

    actions.appendChild(editBtn);
    actions.appendChild(enableBtn);
    actions.appendChild(deleteBtn);

    if (!w.enabled) row.classList.add('wakeup-row-disabled');
    row.appendChild(main);
    row.appendChild(actions);
    return row;
}

async function loadWakeups() {
    if (!invoke || !wakeupsListEl) return;
    try {
        const rows = await invoke('list_wakeups');
        wakeupsListEl.innerHTML = '';
        if (!Array.isArray(rows) || rows.length === 0) {
            wakeupsEmptyEl?.classList.remove('hidden');
            return;
        }
        wakeupsEmptyEl?.classList.add('hidden');
        // Sort: enabled+pending first by next_fire_at asc, then disabled, then completed.
        rows.sort((a, b) => {
            const aActive = a.enabled && a.next_fire_at;
            const bActive = b.enabled && b.next_fire_at;
            if (aActive !== bActive) return aActive ? -1 : 1;
            if (a.next_fire_at && b.next_fire_at) return new Date(a.next_fire_at) - new Date(b.next_fire_at);
            return new Date(b.created_at) - new Date(a.created_at);
        });
        for (const w of rows) {
            wakeupsListEl.appendChild(renderWakeupRow(w));
        }
    } catch (e) {
        console.error('Failed to load wakeups:', e);
        wakeupsListEl.innerHTML = `<div class="wakeups-error">Failed to load wake-ups: ${e}</div>`;
    }
}

if (wakeupsBtn) wakeupsBtn.addEventListener('click', showWakeups);
if (wakeupsBack) wakeupsBack.addEventListener('click', hideWakeups);
if (wakeupsNewBtn) wakeupsNewBtn.addEventListener('click', () => openWakeupForm(null));
if (wakeupFormCancel) wakeupFormCancel.addEventListener('click', closeWakeupForm);
if (wakeupsForm) wakeupsForm.addEventListener('submit', submitWakeup);
if (wakeupScheduleKindEl) {
    wakeupScheduleKindEl.addEventListener('change', () => {
        setWakeupScheduleKind(wakeupScheduleKindEl.value);
    });
}
if (wakeupHourEl) wakeupHourEl.addEventListener('input', refreshWakeupDatetimePreview);
if (wakeupMinuteEl) wakeupMinuteEl.addEventListener('input', refreshWakeupDatetimePreview);
if (wakeupCalPrevBtn) {
    wakeupCalPrevBtn.addEventListener('click', () => {
        if (!wakeupViewedMonth) return;
        wakeupViewedMonth = new Date(wakeupViewedMonth.getFullYear(), wakeupViewedMonth.getMonth() - 1, 1);
        renderWakeupCalendar();
    });
}
if (wakeupCalNextBtn) {
    wakeupCalNextBtn.addEventListener('click', () => {
        if (!wakeupViewedMonth) return;
        wakeupViewedMonth = new Date(wakeupViewedMonth.getFullYear(), wakeupViewedMonth.getMonth() + 1, 1);
        renderWakeupCalendar();
    });
}
if (wakeupQuickDatesEl) {
    wakeupQuickDatesEl.addEventListener('click', (ev) => {
        const btn = ev.target.closest('.wakeup-preset');
        if (!btn) return;
        ev.preventDefault();
        const offset = parseInt(btn.dataset.dayOffset, 10);
        if (!Number.isFinite(offset)) return;
        const d = midnight(new Date());
        d.setDate(d.getDate() + offset);
        wakeupSelectedDate = d;
        wakeupViewedMonth = new Date(d.getFullYear(), d.getMonth(), 1);
        renderWakeupCalendar();
        refreshWakeupDatetimePreview();
    });
}

// ─── Cloud APIs (registered HTTP endpoints) ─────────────────────────
//
// Each row is a `RegisteredEndpoint` projected through `EndpointWire` —
// the secret stays in the vault, the UI only sees `has_credential` for
// the badge. Adding an endpoint pops a modal that prefills from a
// preset so onboarding is "pick provider, paste key, click save".

let cloudApiEndpoints = [];
let cloudApiPresets = [];

async function loadCloudApis() {
    const list = document.getElementById('cloud-apis-list');
    if (!list) return;
    try {
        const [endpoints, presets] = await Promise.all([
            invoke('list_http_endpoints'),
            invoke('list_http_endpoint_presets'),
        ]);
        cloudApiEndpoints = endpoints || [];
        cloudApiPresets = presets || [];
        renderCloudApisList();
        // Endpoint changes show up in every profile's `http_request`
        // section — drop the cached estimates so chips refetch.
        invalidateProfileTokenCache();
    } catch (err) {
        console.error('Failed to load cloud APIs:', err);
        list.innerHTML = `<p class="setting-hint">Failed to load endpoints: ${escapeHtml(String(err))}</p>`;
    }
}

function renderCloudApisList() {
    const list = document.getElementById('cloud-apis-list');
    if (!list) return;
    if (!cloudApiEndpoints.length) {
        list.innerHTML = '<p class="setting-hint">No endpoints registered yet. Click <strong>+ Add Endpoint</strong> below — pick a preset (Brave, Hunter, Open-Meteo, …), paste your key, save.</p>';
        return;
    }
    list.innerHTML = cloudApiEndpoints.map((e) => {
        const credBadge = e.has_credential
            ? '<span class="cloud-api-badge cloud-api-badge-ok">Key set</span>'
            : '<span class="cloud-api-badge cloud-api-badge-warn">No key</span>';
        const enabledBadge = e.enabled
            ? ''
            : '<span class="cloud-api-badge cloud-api-badge-muted">Disabled</span>';
        const lastUsed = e.last_used
            ? new Date(e.last_used).toLocaleDateString()
            : 'never';
        const authLabel = describeAuthMethod(e.auth_method);
        return `
            <div class="cloud-api-row" data-endpoint-id="${escapeHtml(e.id)}">
                <div class="cloud-api-row-main">
                    <div class="cloud-api-row-name">
                        <strong>${escapeHtml(e.name)}</strong>
                        <span class="cloud-api-row-provider">${escapeHtml(e.provider || '')}</span>
                        ${credBadge}
                        ${enabledBadge}
                    </div>
                    <div class="cloud-api-row-meta">
                        <span title="Auth method">${escapeHtml(authLabel)}</span>
                        <span title="30-day call count">${e.call_count_30d} calls / 30d</span>
                        <span title="Last call">last: ${escapeHtml(lastUsed)}</span>
                    </div>
                    <div class="cloud-api-row-url">${escapeHtml(e.base_url)}</div>
                </div>
                <div class="cloud-api-row-actions">
                    <label class="toggle-label">
                        <input type="checkbox" class="cloud-api-enabled" ${e.enabled ? 'checked' : ''} data-endpoint-id="${escapeHtml(e.id)}">
                        <span>Enabled</span>
                    </label>
                    <button class="btn-secondary cloud-api-test-btn" data-endpoint-id="${escapeHtml(e.id)}" type="button">Test</button>
                    <button class="btn-secondary cloud-api-edit-btn" data-endpoint-id="${escapeHtml(e.id)}" type="button">Edit</button>
                    <button class="btn-secondary cloud-api-delete-btn" data-endpoint-id="${escapeHtml(e.id)}" type="button">Delete</button>
                </div>
                <div class="cloud-api-row-test-result hidden" data-endpoint-id="${escapeHtml(e.id)}"></div>
            </div>
        `;
    }).join('');

    list.querySelectorAll('.cloud-api-enabled').forEach((cb) => {
        cb.addEventListener('change', async (ev) => {
            const id = ev.target.dataset.endpointId;
            try {
                await invoke('set_http_endpoint_enabled', { id, enabled: ev.target.checked });
            } catch (err) {
                showToast('Failed to toggle: ' + err, 'error');
                ev.target.checked = !ev.target.checked;
            }
        });
    });
    list.querySelectorAll('.cloud-api-test-btn').forEach((b) => {
        b.addEventListener('click', () => testCloudApi(b.dataset.endpointId));
    });
    list.querySelectorAll('.cloud-api-edit-btn').forEach((b) => {
        b.addEventListener('click', () => openCloudApiModal(b.dataset.endpointId));
    });
    list.querySelectorAll('.cloud-api-delete-btn').forEach((b) => {
        b.addEventListener('click', () => deleteCloudApi(b.dataset.endpointId));
    });
}

function describeAuthMethod(am) {
    if (!am || am === 'None') return 'no auth';
    if (am === 'BearerToken') return 'Bearer token';
    if (am.Header) return `Header: ${am.Header.name}`;
    if (am.QueryParam) return `?${am.QueryParam.name}=…`;
    if (am.BasicAuth) return `Basic (${am.BasicAuth.user})`;
    return 'unknown';
}

async function testCloudApi(id) {
    const ep = cloudApiEndpoints.find((e) => e.id === id);
    if (!ep) return;
    // Pull the preset's test_path when the endpoint matches a preset by
    // base_url. Many APIs (Open-Meteo, NewsAPI, …) 404 on the base URL
    // alone; the preset knows a known-safe sample path to hit instead.
    const matchingPreset = cloudApiPresets.find(
        (p) => p.base_url === ep.base_url || p.label === ep.name || p.provider === ep.provider,
    );
    const defaultPath = matchingPreset?.test_path || '';
    const path = window.prompt(
        `Test path for "${ep.name}" (joined to ${ep.base_url}). Leave blank to hit the base URL.`,
        defaultPath,
    );
    // null = user cancelled.
    if (path === null) return;

    const resultEl = document.querySelector(`.cloud-api-row-test-result[data-endpoint-id="${id}"]`);
    if (resultEl) {
        resultEl.classList.remove('hidden');
        resultEl.textContent = 'Testing…';
        resultEl.className = 'cloud-api-row-test-result';
    }
    try {
        const res = await invoke('test_http_endpoint', { id, path });
        if (resultEl) {
            resultEl.classList.toggle('cloud-api-row-test-ok', !!res.ok);
            resultEl.classList.toggle('cloud-api-row-test-fail', !res.ok);
            resultEl.textContent = `HTTP ${res.status} · ${res.latency_ms}ms — ${(res.body_snippet || '').slice(0, 200)}`;
        }
    } catch (err) {
        if (resultEl) {
            resultEl.classList.add('cloud-api-row-test-fail');
            resultEl.textContent = `Failed: ${err}`;
        }
    }
}

async function deleteCloudApi(id) {
    const ep = cloudApiEndpoints.find((e) => e.id === id);
    if (!ep) return;
    if (!confirm(`Delete endpoint "${ep.name}"? Its credential will also be removed from the vault.`)) return;
    try {
        await invoke('delete_http_endpoint', { id });
        await loadCloudApis();
    } catch (err) {
        showToast('Delete failed: ' + err, 'error');
    }
}

// Modal state. `cloudApiEditingId === null` means "create"; otherwise
// the form is editing an existing row and the credential field's blank
// value preserves the vault entry.
let cloudApiEditingId = null;

function openCloudApiModal(id = null) {
    cloudApiEditingId = id;
    const overlay = document.getElementById('cloud-api-modal-overlay');
    if (!overlay) return;
    const presetSelect = document.getElementById('cloud-api-preset');
    presetSelect.innerHTML = '<option value="">— Custom (no preset) —</option>'
        + cloudApiPresets.map((p) =>
            `<option value="${escapeHtml(p.slug)}">${escapeHtml(p.label)} — ${escapeHtml(p.free_tier_blurb)}</option>`).join('');
    presetSelect.value = '';

    const form = {
        name: document.getElementById('cloud-api-name'),
        provider: document.getElementById('cloud-api-provider'),
        baseUrl: document.getElementById('cloud-api-base-url'),
        authKind: document.getElementById('cloud-api-auth-kind'),
        authParam: document.getElementById('cloud-api-auth-param'),
        authUser: document.getElementById('cloud-api-auth-user'),
        credential: document.getElementById('cloud-api-credential'),
        rateLimit: document.getElementById('cloud-api-rate-limit'),
        risk: document.getElementById('cloud-api-risk'),
        notes: document.getElementById('cloud-api-notes'),
        enabled: document.getElementById('cloud-api-enabled'),
        title: document.getElementById('cloud-api-modal-title'),
        signupHint: document.getElementById('cloud-api-signup-hint'),
        error: document.getElementById('cloud-api-modal-error'),
    };

    if (id) {
        const ep = cloudApiEndpoints.find((e) => e.id === id);
        if (!ep) return;
        form.title.textContent = `Edit "${ep.name}"`;
        form.name.value = ep.name;
        form.provider.value = ep.provider || '';
        form.baseUrl.value = ep.base_url;
        applyAuthMethodToForm(form, ep.auth_method);
        form.credential.value = '';
        form.credential.placeholder = ep.has_credential
            ? 'Leave blank to keep existing key'
            : 'Paste API key';
        form.rateLimit.value = ep.rate_limit_per_minute || '';
        form.risk.value = ep.risk_override || '';
        form.notes.value = ep.notes || '';
        form.enabled.checked = ep.enabled;
        form.signupHint.innerHTML = '';
    } else {
        form.title.textContent = 'New endpoint';
        form.name.value = '';
        form.provider.value = '';
        form.baseUrl.value = '';
        applyAuthMethodToForm(form, 'None');
        form.credential.value = '';
        form.credential.placeholder = 'Paste API key';
        form.rateLimit.value = '';
        form.risk.value = '';
        form.notes.value = '';
        form.enabled.checked = true;
        form.signupHint.innerHTML = '';
    }
    form.error.classList.add('hidden');
    overlay.classList.remove('hidden');
    setTimeout(() => form.name.focus(), 50);
}

function closeCloudApiModal() {
    const overlay = document.getElementById('cloud-api-modal-overlay');
    if (overlay) overlay.classList.add('hidden');
    cloudApiEditingId = null;
}

function applyAuthMethodToForm(form, am) {
    let kind = 'None';
    let paramName = '';
    let user = '';
    if (am === 'BearerToken') kind = 'BearerToken';
    else if (am && am.Header) { kind = 'Header'; paramName = am.Header.name; }
    else if (am && am.QueryParam) { kind = 'QueryParam'; paramName = am.QueryParam.name; }
    else if (am && am.BasicAuth) { kind = 'BasicAuth'; user = am.BasicAuth.user; }
    form.authKind.value = kind;
    form.authParam.value = paramName;
    form.authUser.value = user;
    refreshCloudApiAuthFields();
}

function readAuthMethodFromForm() {
    const kind = document.getElementById('cloud-api-auth-kind').value;
    const paramName = document.getElementById('cloud-api-auth-param').value.trim();
    const user = document.getElementById('cloud-api-auth-user').value.trim();
    switch (kind) {
        case 'None': return 'None';
        case 'BearerToken': return 'BearerToken';
        case 'Header': return { Header: { name: paramName || 'X-Api-Key' } };
        case 'QueryParam': return { QueryParam: { name: paramName || 'api_key' } };
        case 'BasicAuth': return { BasicAuth: { user: user } };
        default: return 'None';
    }
}

function refreshCloudApiAuthFields() {
    const kind = document.getElementById('cloud-api-auth-kind').value;
    document.getElementById('cloud-api-auth-param-row').classList.toggle('hidden',
        kind !== 'Header' && kind !== 'QueryParam');
    document.getElementById('cloud-api-auth-user-row').classList.toggle('hidden',
        kind !== 'BasicAuth');
    document.getElementById('cloud-api-credential-row').classList.toggle('hidden',
        kind === 'None');
    const param = document.getElementById('cloud-api-auth-param');
    if (kind === 'Header') param.placeholder = 'X-Api-Key';
    else if (kind === 'QueryParam') param.placeholder = 'api_key';
}

function applyPresetToModal(slug) {
    const p = cloudApiPresets.find((x) => x.slug === slug);
    if (!p) return;
    const form = {
        name: document.getElementById('cloud-api-name'),
        provider: document.getElementById('cloud-api-provider'),
        baseUrl: document.getElementById('cloud-api-base-url'),
        authKind: document.getElementById('cloud-api-auth-kind'),
        authParam: document.getElementById('cloud-api-auth-param'),
        authUser: document.getElementById('cloud-api-auth-user'),
        rateLimit: document.getElementById('cloud-api-rate-limit'),
        risk: document.getElementById('cloud-api-risk'),
        signupHint: document.getElementById('cloud-api-signup-hint'),
    };
    if (!form.name.value) form.name.value = p.label;
    form.provider.value = p.provider;
    form.baseUrl.value = p.base_url;
    applyAuthMethodToForm({
        authKind: form.authKind, authParam: form.authParam, authUser: form.authUser,
    }, p.auth_method);
    if (p.default_rate_limit_per_minute) form.rateLimit.value = p.default_rate_limit_per_minute;
    if (p.suggested_risk) form.risk.value = p.suggested_risk;
    form.signupHint.innerHTML = `Free tier: ${escapeHtml(p.free_tier_blurb)}. Get a key at <a href="${escapeHtml(p.signup_url)}" target="_blank" rel="noopener">${escapeHtml(p.signup_url)}</a>.`;
}

async function saveCloudApiModal() {
    const errorEl = document.getElementById('cloud-api-modal-error');
    errorEl.classList.add('hidden');
    const name = document.getElementById('cloud-api-name').value.trim();
    const baseUrl = document.getElementById('cloud-api-base-url').value.trim();
    if (!name) { errorEl.textContent = 'Name is required'; errorEl.classList.remove('hidden'); return; }
    if (!baseUrl) { errorEl.textContent = 'Base URL is required'; errorEl.classList.remove('hidden'); return; }

    const input = {
        id: cloudApiEditingId || null,
        name,
        provider: document.getElementById('cloud-api-provider').value.trim(),
        base_url: baseUrl,
        enabled: document.getElementById('cloud-api-enabled').checked,
        auth_method: readAuthMethodFromForm(),
        default_headers: [],
        default_query_params: [],
        rate_limit_per_minute: parseInt(document.getElementById('cloud-api-rate-limit').value, 10) || 0,
        risk_override: document.getElementById('cloud-api-risk').value || null,
        notes: document.getElementById('cloud-api-notes').value.trim() || null,
        credential: document.getElementById('cloud-api-credential').value || null,
    };
    try {
        await invoke('upsert_http_endpoint', { input });
        closeCloudApiModal();
        await loadCloudApis();
        showToast(cloudApiEditingId ? 'Endpoint updated' : 'Endpoint added', 'success');
    } catch (err) {
        errorEl.textContent = String(err);
        errorEl.classList.remove('hidden');
    }
}

function wireCloudApisModal() {
    const addBtn = document.getElementById('cloud-apis-add-btn');
    if (addBtn) addBtn.addEventListener('click', () => openCloudApiModal(null));
    const closeBtn = document.getElementById('cloud-api-modal-close');
    if (closeBtn) closeBtn.addEventListener('click', closeCloudApiModal);
    const cancelBtn = document.getElementById('cloud-api-modal-cancel');
    if (cancelBtn) cancelBtn.addEventListener('click', closeCloudApiModal);
    const saveBtn = document.getElementById('cloud-api-modal-save');
    if (saveBtn) saveBtn.addEventListener('click', saveCloudApiModal);
    const presetSelect = document.getElementById('cloud-api-preset');
    if (presetSelect) presetSelect.addEventListener('change', (ev) => applyPresetToModal(ev.target.value));
    const authKind = document.getElementById('cloud-api-auth-kind');
    if (authKind) authKind.addEventListener('change', refreshCloudApiAuthFields);
    const overlay = document.getElementById('cloud-api-modal-overlay');
    if (overlay) overlay.addEventListener('click', (ev) => {
        if (ev.target === overlay) closeCloudApiModal();
    });
}

// ─── Active agents pill + Agent Control view ─────────────────────────
//
// Live "watch the agents work" indicator wired against the
// `list_active_agents` and `list_recent_agent_runs` Tauri commands and
// the `agents-changed` push event. The topbar pill (count + pulse) is
// a from-anywhere indicator; clicking it navigates to the dedicated
// Agent Control view, which surfaces both the live cards and the
// recent-history table.

let activeAgentsCache = [];
let agentRunsCache = [];
let activeAgentsRefreshing = false;
let agentRunsRefreshing = false;
let agentControlTickHandle = null;
let agentHistoryFilter = 'all';
// Tracks which task_ids are currently expanded in the active list, so a
// full re-render (driven by the `agents-changed` push event) can restore
// the expansion state instead of collapsing everything.
const expandedActiveTaskIds = new Set();
// Same for the history pane, keyed by task_id.
const expandedHistoryTaskIds = new Set();
// Per-task in-memory cache of step-card DOM nodes — the second click on
// an already-loaded card is instant and survives re-renders. History
// only: active cards re-fetch on every `agents-changed` to surface
// freshly-streamed steps without an explicit collapse-and-expand cycle.
const agentStepsLoaded = new Set();
// Tracks the most recently rendered step_count per active task so we can
// detect increments and trigger a one-off bump animation on the count
// badge. Wiped when a task drops out of the active list (registry
// finalize). Cheap; only holds Number entries keyed by task_id.
const lastActiveStepCount = new Map();

const agentControlView = document.getElementById('agent-control-view');
const agentControlBtn = document.getElementById('agent-control-btn');
const agentControlBack = document.getElementById('agent-control-back');

const AGENT_SOURCE_ICONS = {
    user_chat: '\u{1F464}',
    telegram: '\u{1F4AC}',
    email: '\u{2709}\u{FE0F}',
    calendar: '\u{1F4C5}',
    wakeup: '\u{23F0}',
    subagent: '\u{1FAA8}',
    other: '\u{2022}',
};

function activeAgentsPillEl() {
    return document.getElementById('active-agents-pill');
}
function agentSourceKey(source) {
    if (!source) return 'other';
    const s = String(source).toLowerCase();
    return AGENT_SOURCE_ICONS[s] ? s : 'other';
}
function agentSourceIcon(source) {
    return AGENT_SOURCE_ICONS[agentSourceKey(source)];
}

function formatAgentElapsed(startedAt) {
    if (!startedAt) return '';
    const start = new Date(startedAt).getTime();
    if (!Number.isFinite(start)) return '';
    let diffSec = Math.max(0, Math.floor((Date.now() - start) / 1000));
    if (diffSec < 60) return `${diffSec}s`;
    const m = Math.floor(diffSec / 60);
    const s = diffSec % 60;
    if (m < 60) return s ? `${m}m ${s}s` : `${m}m`;
    const h = Math.floor(m / 60);
    const mm = m % 60;
    return mm ? `${h}h ${mm}m` : `${h}h`;
}

function formatAgentDuration(startedAt, finishedAt) {
    if (!startedAt || !finishedAt) return '—';
    const start = new Date(startedAt).getTime();
    const end = new Date(finishedAt).getTime();
    if (!Number.isFinite(start) || !Number.isFinite(end) || end < start) return '—';
    const diffSec = Math.max(0, Math.floor((end - start) / 1000));
    if (diffSec < 60) return `${diffSec}s`;
    const m = Math.floor(diffSec / 60);
    const s = diffSec % 60;
    if (m < 60) return s ? `${m}m ${s}s` : `${m}m`;
    const h = Math.floor(m / 60);
    const mm = m % 60;
    return mm ? `${h}h ${mm}m` : `${h}h`;
}

function formatAgentRelative(ts) {
    if (!ts) return '';
    const t = new Date(ts).getTime();
    if (!Number.isFinite(t)) return '';
    const diffSec = Math.max(0, Math.floor((Date.now() - t) / 1000));
    if (diffSec < 60) return `${diffSec}s ago`;
    const m = Math.floor(diffSec / 60);
    if (m < 60) return `${m}m ago`;
    const h = Math.floor(m / 60);
    if (h < 24) return `${h}h ago`;
    const d = Math.floor(h / 24);
    if (d < 30) return `${d}d ago`;
    try { return new Date(ts).toLocaleDateString(); } catch (_) { return ''; }
}

async function refreshActiveAgents() {
    if (!invoke || activeAgentsRefreshing) return;
    activeAgentsRefreshing = true;
    try {
        activeAgentsCache = await invoke('list_active_agents');
        if (!Array.isArray(activeAgentsCache)) activeAgentsCache = [];
    } catch (err) {
        // Registry not initialized yet (early startup race), or backend
        // is rebuilding; treat as "no agents" rather than spamming logs.
        activeAgentsCache = [];
    } finally {
        activeAgentsRefreshing = false;
    }
    renderActiveAgentsPill();
    renderAgentControlActive();
    renderAgentControlRunningCount();
}

async function refreshAgentRuns() {
    if (!invoke || agentRunsRefreshing) return;
    agentRunsRefreshing = true;
    try {
        agentRunsCache = await invoke('list_recent_agent_runs', { limit: 100 });
        if (!Array.isArray(agentRunsCache)) agentRunsCache = [];
    } catch (err) {
        // Store not initialized yet — treat as empty.
        agentRunsCache = [];
    } finally {
        agentRunsRefreshing = false;
    }
    renderAgentControlHistory();
}

function renderActiveAgentsPill() {
    const pill = activeAgentsPillEl();
    if (!pill) return;
    const count = activeAgentsCache.length;
    const countEl = pill.querySelector('.agents-count');
    const labelEl = pill.querySelector('.agents-label');
    if (countEl) countEl.textContent = String(count);
    if (labelEl) labelEl.textContent = count === 1 ? 'working' : 'working';
    pill.classList.toggle('hidden', count === 0);
}

function renderAgentControlRunningCount() {
    const el = document.getElementById('agent-control-running-count');
    const count = activeAgentsCache.length;
    if (el) el.textContent = `${count} running`;
    // Mirror into the Active sub-tab badge so the count is visible even
    // when the user is on the History tab.
    const badge = document.getElementById('agent-tab-active-count');
    if (badge) {
        badge.textContent = String(count);
        badge.classList.toggle('hidden', count === 0);
    }
}

// Inline chevron used by both expandable cards and history rows.
function agentChevronSvg() {
    return '<svg viewBox="0 0 12 12" aria-hidden="true" focusable="false">'
        + '<path d="M4 2.5L8 6L4 9.5" fill="none" stroke="currentColor" '
        + 'stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"/></svg>';
}

// Lazily load the per-turn tool_call rows for a run and render them via
// the existing buildToolCardBlock so the expanded view matches the chat
// transcript visually. Falls back gracefully when arc_id or turn_id are
// missing (older runs from before the migration).
//
// `silent`: when true, fetch first and only swap DOM in one pass (no
// "Loading…" placeholder). Used by active-card live refreshes so the
// already-rendered steps don't blink during background re-fetch.
// Per-container request-seq guard. Pulse N+1 may complete before pulse
// N (DeepSeek bursts 3–5 tool calls/s, get_arc_entries latency varies).
// Without this guard, an older fetch can overwrite the latest snapshot
// with stale data. We bump a seq on each call and drop late results.
const stepsRequestSeq = new WeakMap();

async function loadAgentSteps(container, arcId, turnId, silent = false) {
    if (!container) return;
    if (!arcId || !turnId) {
        container.innerHTML = '<div class="agent-card-steps-empty">'
            + 'No step data — open the arc to see the transcript.</div>';
        return;
    }
    const seq = (stepsRequestSeq.get(container) || 0) + 1;
    stepsRequestSeq.set(container, seq);
    if (!silent) {
        container.innerHTML = '<div class="agent-card-steps-empty muted">Loading steps…</div>';
    }
    try {
        const entries = await invoke('get_arc_entries', { arcId });
        // Drop stale: another fetch superseded us before this one
        // returned. Rendering would revert the DOM to an older snapshot.
        if (stepsRequestSeq.get(container) !== seq) return;
        const toolCalls = (entries || []).filter(
            (e) => e && e.entry_type === 'tool_call' && e.turn_id === turnId,
        );
        if (toolCalls.length === 0) {
            container.innerHTML = '<div class="agent-card-steps-empty">'
                + 'This run finished without any tool calls.</div>';
            // Reset the previous-count tracker so a later first card
            // doesn't get flagged as "fresh" on the very next render.
            delete container.dataset.lastStepCount;
            return;
        }
        // Build into a fragment then swap in one pass — keeps the
        // existing cards visible until the new ones arrive (no flicker).
        const frag = document.createDocumentFragment();
        for (const tc of toolCalls) {
            const meta = parseEntryMetadata(tc.metadata) || {};
            frag.appendChild(buildToolCardBlock(meta));
        }
        // Detect newly-arrived rows: if the count grew since the last
        // render of this container, mark every card past the previous
        // index as `.fresh` for a brief left-edge highlight. Skip on
        // the first render (when there's no prior count to compare).
        const prevCountStr = container.dataset.lastStepCount;
        const prevCount = prevCountStr === undefined ? -1 : parseInt(prevCountStr, 10);
        container.dataset.lastStepCount = String(toolCalls.length);
        if (prevCount >= 0 && toolCalls.length > prevCount) {
            const newCards = Array.from(frag.children).slice(prevCount);
            for (const node of newCards) {
                node.classList.add('fresh');
                setTimeout(() => node.classList.remove('fresh'), 700);
            }
        }
        container.replaceChildren(frag);
    } catch (err) {
        if (stepsRequestSeq.get(container) !== seq) return;
        if (!silent) {
            container.innerHTML = '<div class="agent-card-steps-empty">'
                + `Could not load steps: ${escapeHtml(String(err))}</div>`;
        }
        // On silent refresh failure, leave the previous content in place
        // — the next `agents-changed` will retry.
    }
}

// Toggle expansion on an active agent card. Always loads (no cache):
// active runs stream new tool_call rows, so the user expects to see
// fresh content on every expand. Skips clicks inside the "Jump to arc"
// or per-card Stop buttons (those run their own handlers).
async function toggleAgentCardExpand(card) {
    const taskId = card.getAttribute('data-task-id');
    if (!taskId) return;
    // Direct-child div only — the footer also has `.agent-card-steps`
    // on its count badge (a span), and a plain querySelector would
    // match that first in document order.
    const stepsEl = card.querySelector(':scope > :scope > div.agent-card-steps');
    if (!stepsEl) return;
    const wasExpanded = card.classList.contains('expanded');
    if (wasExpanded) {
        card.classList.remove('expanded');
        stepsEl.hidden = true;
        expandedActiveTaskIds.delete(taskId);
        return;
    }
    card.classList.add('expanded');
    stepsEl.hidden = false;
    expandedActiveTaskIds.add(taskId);
    const arcId = card.getAttribute('data-arc-id') || '';
    const turnId = card.getAttribute('data-turn-id') || '';
    // Non-silent (show "Loading…") on user-driven expand; the auto
    // refresh path uses silent=true to avoid flashing the placeholder
    // every time the registry pulses.
    await loadAgentSteps(stepsEl, arcId, turnId);
}

// Same as above for a history row item (the wrapping <div.agent-history-item>).
async function toggleAgentHistoryExpand(item) {
    const taskId = item.getAttribute('data-task-id');
    if (!taskId) return;
    const row = item.querySelector('.agent-history-row');
    const stepsEl = item.querySelector('.agent-history-steps');
    if (!row || !stepsEl) return;
    const wasExpanded = row.classList.contains('expanded');
    if (wasExpanded) {
        row.classList.remove('expanded');
        stepsEl.hidden = true;
        expandedHistoryTaskIds.delete(taskId);
        return;
    }
    row.classList.add('expanded');
    stepsEl.hidden = false;
    expandedHistoryTaskIds.add(taskId);
    if (!agentStepsLoaded.has(taskId)) {
        agentStepsLoaded.add(taskId);
        const arcId = item.getAttribute('data-arc-id') || '';
        const turnId = item.getAttribute('data-turn-id') || '';
        await loadAgentSteps(stepsEl, arcId, turnId);
    }
}

// Build a single active-agent card from scratch. Returns a real DOM node
// (not an HTML string) so the diff path in renderAgentControlActive can
// append/insert it directly. Wires its own click + button handlers — the
// outer render function no longer re-binds on every pulse.
function buildActiveAgentCard(agent) {
    const sourceKey = agentSourceKey(agent.source);
    const icon = agentSourceIcon(agent.source);
    const title = escapeHtml(agent.title || '(untitled)');
    const elapsed = escapeHtml(formatAgentElapsed(agent.started_at));
    const stepCount = Number.isFinite(agent.step_count) ? agent.step_count : 0;
    const tool = agent.current_tool ? escapeHtml(agent.current_tool) : '';
    const action = agent.current_action ? escapeHtml(agent.current_action) : '';
    const arcId = agent.arc_id ? escapeHtml(agent.arc_id) : '';
    const taskId = escapeHtml(agent.task_id);
    const turnId = agent.turn_id ? escapeHtml(agent.turn_id) : '';
    // Hide the chip entirely when there's no current_tool — "—" dashes
    // feel like missing data; absence is cleaner.
    const toolChip = tool ? `<span class="agent-card-tool-chip">${tool}</span>` : '';
    const actionInner = action || '<span class="muted">starting…</span>';
    const jumpBtn = arcId
        ? `<button class="agent-card-jump" type="button" data-jump-arc="${arcId}">Jump to arc</button>`
        : '';
    const stopBtn = `<button class="agent-card-stop" type="button" title="Stop agent" aria-label="Stop agent" data-stop-task="${taskId}">
        <svg viewBox="0 0 24 24" width="13" height="13" fill="currentColor" aria-hidden="true"><rect x="6" y="6" width="12" height="12" rx="2"/></svg>
    </button>`;
    const card = document.createElement('article');
    card.className = 'agent-card';
    card.setAttribute('data-source', sourceKey);
    card.setAttribute('data-task-id', taskId);
    card.setAttribute('data-arc-id', arcId);
    card.setAttribute('data-turn-id', turnId);
    card.innerHTML = `
    <header class="agent-card-head">
        <span class="agent-card-icon" aria-hidden="true">${icon}</span>
        <span class="agent-card-title" title="${title}">${title}</span>
        <span class="agent-card-elapsed" data-elapsed-for="${taskId}">${elapsed}</span>
        <span class="agent-card-chevron" aria-hidden="true">${agentChevronSvg()}</span>
    </header>
    <div class="agent-card-body">
        ${toolChip}<span class="agent-card-action">${actionInner}</span>
    </div>
    <footer class="agent-card-foot">
        <span class="agent-card-steps" data-steps-for="${taskId}">${stepCount} step${stepCount === 1 ? '' : 's'}</span>
        <span class="agent-card-foot-actions">${jumpBtn}${stopBtn}</span>
    </footer>
    <div class="agent-card-steps" hidden></div>`;

    // Wire interactions once at construction. The diff path keeps this
    // node alive across pulses so handlers attach exactly one time per
    // task — no leak, no double-fire.
    const jumpEl = card.querySelector('[data-jump-arc]');
    if (jumpEl) {
        jumpEl.addEventListener('click', (ev) => {
            ev.stopPropagation();
            const aid = jumpEl.getAttribute('data-jump-arc');
            if (aid && typeof handleSwitchArc === 'function') {
                handleSwitchArc(aid);
            }
        });
    }
    const stopEl = card.querySelector('[data-stop-task]');
    if (stopEl) {
        stopEl.addEventListener('click', async (ev) => {
            ev.stopPropagation();
            const tid = stopEl.getAttribute('data-stop-task');
            if (!tid) return;
            card.classList.add('cancelling');
            const actionEl = card.querySelector('.agent-card-action');
            if (actionEl) actionEl.innerHTML = '<span class="muted">Stopping…</span>';
            stopEl.disabled = true;
            try {
                await invoke('cancel_agent', { taskId: tid });
            } catch (err) {
                console.warn('[athen] cancel_agent failed:', err);
                card.classList.remove('cancelling');
                stopEl.disabled = false;
            }
        });
    }
    card.addEventListener('click', (ev) => {
        if (ev.target.closest('.agent-card-jump')) return;
        if (ev.target.closest('.agent-card-stop')) return;
        if (ev.target.closest('button')) return;
        toggleAgentCardExpand(card);
    });
    return card;
}

// Patch a live card's mutable fields in place (elapsed, step count,
// current_tool chip, current_action). Title rarely changes but we
// refresh it for free since the cost is a single textContent write.
// Skips DOM if the value already matches — keeps style recalcs minimal.
function patchActiveCardInPlace(card, agent) {
    const taskId = agent.task_id;
    // Elapsed.
    const elapsedEl = card.querySelector(`[data-elapsed-for="${CSS.escape(taskId)}"]`);
    if (elapsedEl) {
        const next = formatAgentElapsed(agent.started_at);
        if (elapsedEl.textContent !== next) elapsedEl.textContent = next;
    }
    // Step count badge in the footer.
    const stepCount = Number.isFinite(agent.step_count) ? agent.step_count : 0;
    const stepsBadge = card.querySelector(`[data-steps-for="${CSS.escape(taskId)}"]`);
    if (stepsBadge) {
        const nextText = `${stepCount} step${stepCount === 1 ? '' : 's'}`;
        if (stepsBadge.textContent !== nextText) {
            stepsBadge.textContent = nextText;
            // Bump animation on growth — applies on the next frame so
            // the browser registers the class flip as an animation
            // start rather than a same-frame style flush.
            const prev = parseInt(card.dataset.lastFooterCount || '-1', 10);
            if (prev >= 0 && stepCount > prev) {
                requestAnimationFrame(() => {
                    stepsBadge.classList.add('bumped');
                    setTimeout(() => stepsBadge.classList.remove('bumped'), 420);
                });
            }
        }
        card.dataset.lastFooterCount = String(stepCount);
    }
    // Source + arc/turn ids — almost never change, but keep the
    // attributes truthful so jump/stop buttons keep targeting the
    // right thing if the backend ever rotates them.
    const nextSource = agentSourceKey(agent.source);
    if (card.getAttribute('data-source') !== nextSource) {
        card.setAttribute('data-source', nextSource);
    }
    const nextArc = agent.arc_id ? agent.arc_id : '';
    if (card.getAttribute('data-arc-id') !== nextArc) {
        card.setAttribute('data-arc-id', nextArc);
    }
    const nextTurn = agent.turn_id ? agent.turn_id : '';
    if (card.getAttribute('data-turn-id') !== nextTurn) {
        card.setAttribute('data-turn-id', nextTurn);
    }
    // Current tool + action row. Cancelling state owns the action row
    // until the registry actually drops the card, so don't overwrite it.
    if (!card.classList.contains('cancelling')) {
        const bodyEl = card.querySelector('.agent-card-body');
        if (bodyEl) {
            const tool = agent.current_tool ? escapeHtml(agent.current_tool) : '';
            const action = agent.current_action ? escapeHtml(agent.current_action) : '';
            const toolChip = tool ? `<span class="agent-card-tool-chip">${tool}</span>` : '';
            const actionInner = action || '<span class="muted">starting…</span>';
            const nextHtml = `${toolChip}<span class="agent-card-action">${actionInner}</span>`;
            if (bodyEl.innerHTML !== nextHtml) bodyEl.innerHTML = nextHtml;
        }
    }
}

function renderAgentControlActive() {
    const container = document.getElementById('agent-control-active');
    if (!container) return;
    if (activeAgentsCache.length === 0) {
        container.innerHTML = '<div class="agent-cards-empty">'
            + 'No agents are running right now.'
            + '<span class="empty-sub">Tasks you start show up here in real time.</span>'
            + '</div>';
        lastActiveStepCount.clear();
        return;
    }

    // If the previous render was the empty-state placeholder (or first
    // render of the panel), wipe it before diffing — there are no
    // existing card nodes to reuse.
    const placeholder = container.querySelector('.agent-cards-empty');
    if (placeholder) container.innerHTML = '';

    // Build a lookup of existing card nodes keyed by task_id.
    const existing = new Map();
    for (const node of Array.from(container.children)) {
        if (node.classList && node.classList.contains('agent-card')) {
            const tid = node.getAttribute('data-task-id') || '';
            if (tid) existing.set(tid, node);
        }
    }

    const liveTaskIds = new Set();
    const orderedNodes = [];

    // For each agent in the snapshot: reuse the existing card (in-place
    // patch) or build a fresh one. Preserves CSS transitions, focus, and
    // the expanded steps DOM across pulses.
    for (const agent of activeAgentsCache) {
        const tid = agent.task_id;
        liveTaskIds.add(tid);
        let card = existing.get(tid);
        if (card) {
            patchActiveCardInPlace(card, agent);
        } else {
            card = buildActiveAgentCard(agent);
            // Seed the in-place footer-count tracker so the very first
            // patch after construction doesn't fire the bump animation.
            const initial = Number.isFinite(agent.step_count) ? agent.step_count : 0;
            card.dataset.lastFooterCount = String(initial);
            // Restore expansion if the user had this card open before.
            if (expandedActiveTaskIds.has(tid)) {
                card.classList.add('expanded');
                const stepsEl = card.querySelector(':scope > div.agent-card-steps');
                if (stepsEl) {
                    stepsEl.hidden = false;
                    const arcId = card.getAttribute('data-arc-id') || '';
                    const turnId = card.getAttribute('data-turn-id') || '';
                    loadAgentSteps(stepsEl, arcId, turnId, true);
                }
            }
        }
        orderedNodes.push(card);
    }

    // Fade out + remove cards whose tasks have left the snapshot.
    for (const [tid, node] of existing) {
        if (liveTaskIds.has(tid)) continue;
        if (node.classList.contains('removing')) continue;
        node.classList.add('removing');
        const drop = () => { if (node.parentNode === container) node.remove(); };
        let removed = false;
        const onEnd = () => { if (!removed) { removed = true; drop(); } };
        node.addEventListener('transitionend', onEnd, { once: true });
        // Fallback: if no transition fires (display:none, reduced motion,
        // class collision), force the removal after the same window.
        setTimeout(onEnd, 240);
    }

    // Reorder to match snapshot order. appendChild on an already-attached
    // node moves it without rebuilding — cheaper than insertBefore in a
    // loop and preserves all DOM state.
    for (const node of orderedNodes) {
        container.appendChild(node);
    }

    // Refresh the live expanded-steps fetch for any card the user has
    // open — `agents-changed` is the heartbeat that streams new tool
    // rows into the visible body. Silent mode swaps in one pass.
    for (const node of orderedNodes) {
        const tid = node.getAttribute('data-task-id') || '';
        if (!tid || !expandedActiveTaskIds.has(tid)) continue;
        const stepsEl = node.querySelector(':scope > div.agent-card-steps');
        if (!stepsEl) continue;
        const arcId = node.getAttribute('data-arc-id') || '';
        const turnId = node.getAttribute('data-turn-id') || '';
        loadAgentSteps(stepsEl, arcId, turnId, true);
    }

    // Drop entries for tasks that have left the active list — keeps the
    // map bounded and prevents stale "first render" hits.
    for (const id of Array.from(lastActiveStepCount.keys())) {
        if (!liveTaskIds.has(id)) lastActiveStepCount.delete(id);
    }
}

// Lightweight in-place tick used by the 1s interval. Only updates
// elapsed times, step counts, and the current-tool chip — does NOT
// rebuild the card list, so expanded panes stay open and the step
// timeline below them isn't disturbed.
function tickAgentControlElapsed() {
    if (activeAgentsCache.length === 0) return;
    for (const agent of activeAgentsCache) {
        const elapsedEl = document.querySelector(
            `[data-elapsed-for="${CSS.escape(agent.task_id)}"]`,
        );
        if (elapsedEl) elapsedEl.textContent = formatAgentElapsed(agent.started_at);
        const stepsEl = document.querySelector(
            `[data-steps-for="${CSS.escape(agent.task_id)}"]`,
        );
        if (stepsEl) {
            const n = Number.isFinite(agent.step_count) ? agent.step_count : 0;
            stepsEl.textContent = `${n} step${n === 1 ? '' : 's'}`;
        }
    }
}

function renderAgentControlHistory() {
    const container = document.getElementById('agent-control-history');
    if (!container) return;
    // Skip rows that are still running — those are surfaced in the
    // Active sub-tab.
    const finalized = (agentRunsCache || []).filter(
        (r) => r.status && r.status !== 'running',
    );
    const filtered = agentHistoryFilter === 'all'
        ? finalized
        : finalized.filter((r) => agentSourceKey(r.source) === agentHistoryFilter);
    if (filtered.length === 0) {
        const msg = finalized.length === 0
            ? 'No agent runs yet.'
            : 'No runs match this filter.';
        const sub = finalized.length === 0
            ? 'Past runs will live here for 30 days.'
            : 'Try a different source.';
        container.innerHTML = `<div class="agent-history-empty">${msg}`
            + `<span class="empty-sub">${sub}</span></div>`;
        container.classList.remove('agent-history-list-wrap');
        return;
    }
    container.classList.add('agent-history-list-wrap');
    const items = filtered.map((run) => {
        const sourceKey = agentSourceKey(run.source);
        const icon = agentSourceIcon(run.source);
        const title = escapeHtml(run.title || '(untitled)');
        const started = escapeHtml(formatAgentRelative(run.started_at));
        const duration = escapeHtml(formatAgentDuration(run.started_at, run.finished_at));
        const status = String(run.status || 'completed').toLowerCase();
        const arcId = run.arc_id ? escapeHtml(run.arc_id) : '';
        const taskId = escapeHtml(run.task_id || '');
        const turnId = run.turn_id ? escapeHtml(run.turn_id) : '';
        const jumpBtn = arcId
            ? `<button class="agent-history-jump" type="button" title="Open arc" aria-label="Open arc">
                   <svg viewBox="0 0 24 24" width="13" height="13" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M7 17 17 7"/><path d="M8 7h9v9"/></svg>
               </button>`
            : '';
        return `
<div class="agent-history-item" data-source="${sourceKey}" data-task-id="${taskId}" data-arc-id="${arcId}" data-turn-id="${turnId}">
    <div class="agent-history-row" data-source="${sourceKey}" data-arc-id="${arcId}">
        <span class="agent-history-icon" aria-hidden="true">${icon}</span>
        <span class="agent-history-title" title="${title}">${title}</span>
        <span class="agent-history-meta">${started}</span>
        <span class="agent-history-duration">${duration}</span>
        <span class="agent-history-status" data-status="${escapeHtml(status)}">${escapeHtml(status)}</span>
        ${jumpBtn}
        <span class="agent-history-chevron" aria-hidden="true">${agentChevronSvg()}</span>
    </div>
    <div class="agent-history-steps" hidden></div>
</div>`;
    }).join('');
    container.innerHTML = items;
    container.querySelectorAll('.agent-history-item').forEach((item) => {
        const row = item.querySelector('.agent-history-row');
        if (!row) return;
        const jump = row.querySelector('.agent-history-jump');
        if (jump) {
            jump.addEventListener('click', (ev) => {
                ev.stopPropagation();
                const arcId = item.getAttribute('data-arc-id');
                if (arcId && typeof handleSwitchArc === 'function') {
                    handleSwitchArc(arcId);
                }
            });
        }
        row.addEventListener('click', (ev) => {
            if (ev.target.closest('.agent-history-jump')) return;
            toggleAgentHistoryExpand(item);
        });
        const taskId = item.getAttribute('data-task-id');
        if (taskId && expandedHistoryTaskIds.has(taskId)) {
            const stepsEl = item.querySelector('.agent-history-steps');
            row.classList.add('expanded');
            if (stepsEl) {
                stepsEl.hidden = false;
                if (agentStepsLoaded.has(taskId)) {
                    const arcId = item.getAttribute('data-arc-id') || '';
                    const turnId = item.getAttribute('data-turn-id') || '';
                    loadAgentSteps(stepsEl, arcId, turnId);
                }
            }
        }
    });
}

// Wires the sub-tab buttons + filter chips. Idempotent — runs once at
// app init time. The active-tab state is restored from localStorage and
// applied via a synthetic .click() so the same code path that handles
// user clicks also handles initial state.
function setupAgentControlTabs() {
    const tabs = document.querySelectorAll('.agent-control-tab');
    const panes = document.querySelectorAll('.agent-control-tab-pane');
    if (tabs.length) {
        tabs.forEach((btn) => {
            btn.addEventListener('click', () => {
                const which = btn.dataset.agentTab;
                tabs.forEach((t) => {
                    const on = t === btn;
                    t.classList.toggle('active', on);
                    t.setAttribute('aria-selected', on ? 'true' : 'false');
                });
                panes.forEach((p) => p.classList.toggle('active', p.dataset.agentPane === which));
                try { localStorage.setItem('agentControlTab', which); } catch (_) { /* ignore */ }
            });
        });
        let stored = null;
        try { stored = localStorage.getItem('agentControlTab'); } catch (_) { /* ignore */ }
        if (stored) {
            const target = document.querySelector(`.agent-control-tab[data-agent-tab="${stored}"]`);
            if (target) target.click();
        }
    }

    const chips = document.querySelectorAll('.agent-filter-chip');
    if (chips.length) {
        let storedFilter = null;
        try { storedFilter = localStorage.getItem('agentHistoryFilter'); } catch (_) { /* ignore */ }
        if (storedFilter) agentHistoryFilter = storedFilter;
        chips.forEach((chip) => {
            const which = chip.dataset.sourceFilter || 'all';
            chip.classList.toggle('active', which === agentHistoryFilter);
            chip.addEventListener('click', () => {
                agentHistoryFilter = which;
                chips.forEach((c) => c.classList.toggle('active', c === chip));
                try { localStorage.setItem('agentHistoryFilter', agentHistoryFilter); } catch (_) { /* ignore */ }
                renderAgentControlHistory();
            });
        });
    }
}

function showAgentControl() {
    if (!agentControlView) return;
    if (typeof appView !== 'undefined' && appView) appView.style.display = 'none';
    settingsView?.classList.add('hidden');
    timelineView?.classList.add('hidden');
    calendarView?.classList.add('hidden');
    document.getElementById('wakeups-view')?.classList.add('hidden');
    notificationsView?.classList.add('hidden');
    contactsView?.classList.add('hidden');
    memoryView?.classList.add('hidden');
    document.getElementById('sidebar').style.display = '';
    if (typeof timelineRefreshInterval !== 'undefined' && timelineRefreshInterval) {
        clearInterval(timelineRefreshInterval);
        timelineRefreshInterval = null;
    }
    agentControlView.classList.remove('hidden');
    closeSidebar();
    refreshActiveAgents();
    refreshAgentRuns();
    if (agentControlTickHandle) clearInterval(agentControlTickHandle);
    // 1s tick keeps the elapsed badges honest while the view is open.
    // Only patches in-place so expanded step panes don't snap shut.
    agentControlTickHandle = setInterval(() => {
        if (agentControlView.classList.contains('hidden')) return;
        tickAgentControlElapsed();
    }, 1000);
}

function hideAgentControl() {
    agentControlView?.classList.add('hidden');
    if (agentControlTickHandle) {
        clearInterval(agentControlTickHandle);
        agentControlTickHandle = null;
    }
    showChat();
}

function wireActiveAgentsPanel() {
    const pill = activeAgentsPillEl();
    if (pill) pill.addEventListener('click', showAgentControl);
    if (agentControlBtn) agentControlBtn.addEventListener('click', showAgentControl);
    if (agentControlBack) agentControlBack.addEventListener('click', hideAgentControl);

    // Sub-tabs + filter chips. Idempotent — wired once at init time;
    // showAgentControl just refreshes data afterwards.
    setupAgentControlTabs();

    // Backend pulse — registry mutations + finalized runs. Coalesce
    // bursts (DeepSeek streams 3–5 tool calls/s; without debounce every
    // pulse triggers a rebuild that destroys in-flight transitions and
    // forces redundant get_arc_entries fetches). 80ms feels live but
    // absorbs the burst.
    let agentsChangedTimer = null;
    if (window.__TAURI__?.event?.listen) {
        window.__TAURI__.event.listen('agents-changed', () => {
            if (agentsChangedTimer) return;
            agentsChangedTimer = setTimeout(() => {
                agentsChangedTimer = null;
                refreshActiveAgents();
                // Newly-finalized runs land in the history feed.
                refreshAgentRuns();
            }, 80);
        }).catch((err) => {
            console.warn('[athen] agents-changed listen failed:', err);
        });
    }
}

// ─── MCP Servers (BYO custom MCP) ──────────────────────────────────
//
// Two SQLite tables back this panel:
//   - mcp_custom_entries: the user-supplied McpCatalogEntry definitions
//   - mcp_enabled       : which ids are currently spawned
// The Tauri commands (mcp_list_custom / mcp_list_enabled / mcp_*) hide
// the join — the UI just renders rows and forwards toggles.
//
// Secrets never reach the FE. The modal collects them in
// `env_secrets[KEY] = value` and ships them to mcp_add_custom /
// mcp_test_spawn, which writes them straight into the vault under
// `mcp:<id>` (or a scratch scope for the dry-run path).

let mcpServersCustom = [];   // McpCatalogEntry definitions
let mcpServersEnabled = [];  // EnabledMcpView rows (status + tool_count)
let mcpServersExpanded = new Set();  // ids the user clicked open

async function loadMcpServers() {
    const list = document.getElementById('mcp-servers-list');
    if (!list) return;
    try {
        const [custom, enabled] = await Promise.all([
            invoke('mcp_list_custom'),
            invoke('mcp_list_enabled'),
        ]);
        mcpServersCustom = custom || [];
        mcpServersEnabled = enabled || [];
        renderMcpServersList();
    } catch (err) {
        console.error('Failed to load MCP servers:', err);
        list.innerHTML = `<p class="setting-hint">Failed to load: ${escapeHtml(String(err))}</p>`;
    }
}

function renderMcpServersList() {
    const list = document.getElementById('mcp-servers-list');
    if (!list) return;
    if (!mcpServersCustom.length) {
        list.innerHTML = '<p class="setting-hint">No MCP servers yet. Connect tools from Slack, Notion, GitHub, and more by adding an MCP server below. Each one runs as a sandboxed subprocess on your machine.</p>';
        return;
    }
    const enabledById = new Map(mcpServersEnabled.map((e) => [e.id, e]));
    list.innerHTML = mcpServersCustom.map((entry) => {
        const live = enabledById.get(entry.id);
        const isEnabled = !!live;
        const status = live ? live.status : 'disabled';
        const isError = status.startsWith('error:');
        let badge;
        if (!isEnabled) {
            badge = '<span class="mcp-server-badge mcp-server-badge-muted">Disabled</span>';
        } else if (isError) {
            badge = '<span class="mcp-server-badge mcp-server-badge-error">Error</span>';
        } else {
            badge = '<span class="mcp-server-badge mcp-server-badge-ok">Running</span>';
        }
        const toolBadge = live && live.tool_count !== null && live.tool_count !== undefined
            ? `<span class="mcp-server-badge mcp-server-badge-info">${live.tool_count} tool${live.tool_count === 1 ? '' : 's'}</span>`
            : '';
        const cmdLine = mcpServerCommandLine(entry);
        const expanded = mcpServersExpanded.has(entry.id);
        const errorBlock = isEnabled && isError
            ? `<div class="mcp-server-row-error">${escapeHtml(status.replace(/^error:\s*/, ''))}</div>`
            : '';
        return `
            <div class="mcp-server-row" data-mcp-id="${escapeHtml(entry.id)}">
                <div class="mcp-server-row-main">
                    <div class="mcp-server-row-info">
                        <div class="mcp-server-row-name">
                            <strong>${escapeHtml(entry.display_name || entry.id)}</strong>
                            ${badge}
                            ${toolBadge}
                        </div>
                        <div class="mcp-server-row-command">${escapeHtml(cmdLine)}</div>
                    </div>
                    <div class="mcp-server-row-actions">
                        <label class="toggle-label">
                            <input type="checkbox" class="mcp-server-enabled" ${isEnabled ? 'checked' : ''} data-mcp-id="${escapeHtml(entry.id)}">
                            <span>Enabled</span>
                        </label>
                        <button class="btn-secondary mcp-server-expand-btn" data-mcp-id="${escapeHtml(entry.id)}" type="button">${expanded ? 'Hide' : 'Tools'}</button>
                        <button class="btn-secondary mcp-server-delete-btn" data-mcp-id="${escapeHtml(entry.id)}" type="button">Delete</button>
                    </div>
                </div>
                ${errorBlock}
                ${expanded ? `<div class="mcp-server-row-expanded" data-mcp-expand="${escapeHtml(entry.id)}"><p class="setting-hint">Loading tools…</p></div>` : ''}
            </div>
        `;
    }).join('');

    list.querySelectorAll('.mcp-server-enabled').forEach((cb) => {
        cb.addEventListener('change', async (ev) => {
            const id = ev.target.dataset.mcpId;
            const enable = ev.target.checked;
            try {
                await invoke('mcp_set_enabled', { id, enable });
                await loadMcpServers();
            } catch (err) {
                showToast(`Failed to ${enable ? 'enable' : 'disable'}: ${err}`, 'error');
                ev.target.checked = !enable;
            }
        });
    });
    list.querySelectorAll('.mcp-server-expand-btn').forEach((b) => {
        b.addEventListener('click', () => toggleMcpServerExpand(b.dataset.mcpId));
    });
    list.querySelectorAll('.mcp-server-delete-btn').forEach((b) => {
        b.addEventListener('click', () => deleteMcpServer(b.dataset.mcpId));
    });

    // Lazy-fetch the tool list for any pane that's already expanded.
    for (const id of mcpServersExpanded) {
        loadMcpServerTools(id);
    }
}

function mcpServerCommandLine(entry) {
    const src = entry.source;
    if (!src || src.kind !== 'process') {
        return `[${src && src.kind ? src.kind : 'unknown'}]`;
    }
    const args = (src.args || []).join(' ');
    return args ? `${src.command} ${args}` : src.command;
}

function toggleMcpServerExpand(id) {
    if (mcpServersExpanded.has(id)) {
        mcpServersExpanded.delete(id);
        // Drop any in-flight edit state so a re-expand re-reads the
        // canonical persisted view (no stale dirty flag carrying over).
        mcpServersRiskState.delete(id);
    } else {
        mcpServersExpanded.add(id);
    }
    renderMcpServersList();
}

// Risk vocabulary used in the per-server / per-tool picker. The keys
// MUST match the `BaseImpact` Rust enum variants exactly — the Tauri
// command deserializes them straight into `BaseImpact`. The labels are
// the user-facing human strings (matched against existing risk-level
// rendering conventions in the rest of the UI).
const MCP_RISK_LEVELS = [
    { value: 'Read',         label: 'Read (silent)' },
    { value: 'WriteTemp',    label: 'Notify (fire and notify)' },
    { value: 'WritePersist', label: 'Write (ask if untrusted)' },
    { value: 'System',       label: 'System (always ask)' },
];

function mcpRiskOptionsHtml(selected) {
    return MCP_RISK_LEVELS
        .map((r) => `<option value="${r.value}"${r.value === selected ? ' selected' : ''}>${escapeHtml(r.label)}</option>`)
        .join('');
}

// Per-expanded-pane edit state, keyed by mcp id. Cleared when the user
// hits Save (or the pane is collapsed). Holds:
//   defaultRisk : current value of the per-server default dropdown
//   toolRisks   : Map<toolName, currentRiskValue> (every tool, not just dirty ones)
//   savedDefault: the value persisted on the backend (for dirty detection)
//   savedTools  : Map<toolName, persistedRiskValue> (ditto)
const mcpServersRiskState = new Map();

async function loadMcpServerTools(id) {
    const pane = document.querySelector(`[data-mcp-expand="${cssEscape(id)}"]`);
    if (!pane) return;

    // Find the persisted server default. `mcpServersCustom` is the
    // hydrated McpCatalogEntry list, which now carries `base_risk` +
    // `tool_risks` (added in this change). Old persisted entries (pre
    // serde defaults) might be missing the field — fall back to the
    // conservative `WritePersist`.
    const entry = mcpServersCustom.find((e) => e.id === id);
    const persistedDefault = (entry && entry.base_risk) ? entry.base_risk : 'WritePersist';
    const persistedTools = (entry && entry.tool_risks) ? entry.tool_risks : {};

    try {
        const tools = await invoke('mcp_list_tools_for', { id });
        if (!tools || tools.length === 0) {
            pane.innerHTML = '<p class="setting-hint">This server advertised no tools.</p>';
            return;
        }

        // Seed local edit state from the registry-stamped risk on each
        // tool. The stamped value already reflects per-tool overrides
        // when present, so this is the right starting point.
        const toolRisks = new Map();
        const savedTools = new Map();
        for (const t of tools) {
            const stamped = t.base_risk || persistedDefault;
            toolRisks.set(t.name, stamped);
            // For dirty detection we compare against the stored per-tool
            // override (if any) — falling back to the persisted server
            // default. This matches the "only send overrides that differ
            // from the default" contract.
            const persistedOverride = Object.prototype.hasOwnProperty.call(persistedTools, t.name)
                ? persistedTools[t.name]
                : persistedDefault;
            savedTools.set(t.name, persistedOverride);
        }
        mcpServersRiskState.set(id, {
            defaultRisk: persistedDefault,
            toolRisks,
            savedDefault: persistedDefault,
            savedTools,
        });

        const escapedId = escapeHtml(id);
        pane.innerHTML = `
            <div class="mcp-server-risk-default">
                <label for="mcp-server-default-risk-${escapedId}">Default risk for tools from this server</label>
                <select id="mcp-server-default-risk-${escapedId}" class="settings-input mcp-server-default-risk-select" data-mcp-id="${escapedId}">
                    ${mcpRiskOptionsHtml(persistedDefault)}
                </select>
                <p class="setting-hint">Sets the risk level applied to every tool from this server unless you override it per tool below.</p>
            </div>
            <ul class="mcp-server-tool-list mcp-server-tool-list-risk">
                ${tools.map((t) => `
                    <li class="mcp-server-tool-row" data-tool-name="${escapeHtml(t.name)}">
                        <div class="mcp-server-tool-info">
                            <code>${escapeHtml(t.name)}</code>
                            ${t.description ? `<span class="mcp-server-tool-desc">${escapeHtml(t.description)}</span>` : ''}
                        </div>
                        <select class="settings-input mcp-server-tool-risk-select" data-mcp-id="${escapedId}" data-tool-name="${escapeHtml(t.name)}">
                            ${mcpRiskOptionsHtml(toolRisks.get(t.name))}
                        </select>
                    </li>
                `).join('')}
            </ul>
            <div class="mcp-server-risk-actions">
                <button class="btn-primary mcp-server-risk-save-btn" data-mcp-id="${escapedId}" type="button" disabled>Save risk levels</button>
                <span class="mcp-server-risk-status" data-mcp-id="${escapedId}"></span>
            </div>
        `;

        // Wire the dropdowns to update local state + recompute dirty.
        const defaultSel = pane.querySelector('.mcp-server-default-risk-select');
        if (defaultSel) {
            defaultSel.addEventListener('change', (ev) => {
                const s = mcpServersRiskState.get(id);
                if (!s) return;
                s.defaultRisk = ev.target.value;
                refreshMcpRiskDirty(id);
            });
        }
        pane.querySelectorAll('.mcp-server-tool-risk-select').forEach((sel) => {
            sel.addEventListener('change', (ev) => {
                const s = mcpServersRiskState.get(id);
                if (!s) return;
                s.toolRisks.set(ev.target.dataset.toolName, ev.target.value);
                refreshMcpRiskDirty(id);
            });
        });
        const saveBtn = pane.querySelector('.mcp-server-risk-save-btn');
        if (saveBtn) {
            saveBtn.addEventListener('click', () => saveMcpServerRisks(id));
        }
    } catch (err) {
        pane.innerHTML = `<p class="setting-hint" style="color:#fca5a5;">Failed to list tools: ${escapeHtml(String(err))}</p>`;
    }
}

function refreshMcpRiskDirty(id) {
    const s = mcpServersRiskState.get(id);
    if (!s) return;
    let dirty = s.defaultRisk !== s.savedDefault;
    if (!dirty) {
        for (const [tool, risk] of s.toolRisks.entries()) {
            // The "saved" view collapses pass-throughs (where the
            // stored override equals the server default) into the
            // default. So an effective change is anything where the
            // current dropdown value differs from the recorded saved
            // override (which is also resolved against the default).
            const saved = s.savedTools.get(tool);
            if (saved !== risk) { dirty = true; break; }
        }
    }
    const escapedId = cssEscape(id);
    const btn = document.querySelector(`.mcp-server-risk-save-btn[data-mcp-id="${escapedId}"]`);
    if (btn) btn.disabled = !dirty;
}

async function saveMcpServerRisks(id) {
    const s = mcpServersRiskState.get(id);
    if (!s) return;
    const escapedId = cssEscape(id);
    const btn = document.querySelector(`.mcp-server-risk-save-btn[data-mcp-id="${escapedId}"]`);
    const statusEl = document.querySelector(`.mcp-server-risk-status[data-mcp-id="${escapedId}"]`);
    if (btn) btn.disabled = true;
    if (statusEl) {
        statusEl.className = 'mcp-server-risk-status mcp-server-risk-status-pending';
        statusEl.textContent = 'Saving…';
    }

    // Only send overrides that DIFFER from the chosen default — the
    // backend default applies everywhere else and keeps the persisted
    // overrides map small.
    const tool_overrides = {};
    for (const [tool, risk] of s.toolRisks.entries()) {
        if (risk !== s.defaultRisk) {
            tool_overrides[tool] = risk;
        }
    }

    try {
        await invoke('mcp_set_risks', {
            id,
            defaultRisk: s.defaultRisk,
            toolOverrides: tool_overrides,
        });
        // Recompute saved baseline from the just-saved values.
        s.savedDefault = s.defaultRisk;
        s.savedTools = new Map(s.toolRisks);
        // Also refresh `mcpServersCustom` so a later collapse/expand
        // cycle uses the new persisted shape.
        const entry = mcpServersCustom.find((e) => e.id === id);
        if (entry) {
            entry.base_risk = s.defaultRisk;
            entry.tool_risks = { ...tool_overrides };
        }
        if (statusEl) {
            statusEl.className = 'mcp-server-risk-status mcp-server-risk-status-ok';
            statusEl.textContent = 'Saved';
            setTimeout(() => {
                if (statusEl.textContent === 'Saved') {
                    statusEl.textContent = '';
                    statusEl.className = 'mcp-server-risk-status';
                }
            }, 2000);
        }
        refreshMcpRiskDirty(id);
    } catch (err) {
        if (statusEl) {
            statusEl.className = 'mcp-server-risk-status mcp-server-risk-status-fail';
            statusEl.textContent = `Failed: ${String(err)}`;
        }
        if (btn) btn.disabled = false;
    }
}

// CSS.escape isn't on every WebKitGTK version; fall back to a tame
// allowlist for the id slugs we generate (kebab-case ASCII).
function cssEscape(s) {
    if (typeof CSS !== 'undefined' && typeof CSS.escape === 'function') return CSS.escape(s);
    return String(s).replace(/[^a-zA-Z0-9_-]/g, (c) => `\\${c}`);
}

async function deleteMcpServer(id) {
    const entry = mcpServersCustom.find((e) => e.id === id);
    if (!entry) return;
    if (!confirm(`Delete MCP server "${entry.display_name || id}"? Any secrets it uses will be removed from the vault.`)) return;
    try {
        await invoke('mcp_remove_custom', { id });
        mcpServersExpanded.delete(id);
        await loadMcpServers();
        showToast('MCP server deleted', 'success');
    } catch (err) {
        showToast(`Delete failed: ${err}`, 'error');
    }
}

// ─── MCP Server modal ──

function slugifyMcpId(name) {
    return String(name || '')
        .toLowerCase()
        .replace(/[^a-z0-9]+/g, '-')
        .replace(/^-+|-+$/g, '')
        .slice(0, 48) || 'custom-mcp';
}

// Quote-aware split: splits on whitespace but preserves "double quoted"
// tokens so users can pass args with spaces.
function parseMcpArgs(raw) {
    const out = [];
    const re = /"([^"]*)"|(\S+)/g;
    let m;
    while ((m = re.exec(raw)) !== null) {
        out.push(m[1] !== undefined ? m[1] : m[2]);
    }
    return out;
}

function addMcpEnvRow(initial = { key: '', value: '', secret: false }) {
    const wrap = document.getElementById('mcp-server-env-rows');
    if (!wrap) return;
    const row = document.createElement('div');
    row.className = 'mcp-server-env-row';
    row.innerHTML = `
        <input type="text" class="settings-input mcp-server-env-key" placeholder="KEY" autocomplete="off" spellcheck="false">
        <input type="${initial.secret ? 'password' : 'text'}" class="settings-input mcp-server-env-value" placeholder="${initial.secret ? 'Secret value' : 'value'}" autocomplete="off" spellcheck="false">
        <label class="mcp-server-env-secret">
            <input type="checkbox" class="mcp-server-env-secret-cb">
            <span>Secret</span>
        </label>
        <button class="btn-secondary mcp-server-env-remove" type="button">×</button>
    `;
    row.querySelector('.mcp-server-env-key').value = initial.key || '';
    row.querySelector('.mcp-server-env-value').value = initial.value || '';
    const secretCb = row.querySelector('.mcp-server-env-secret-cb');
    secretCb.checked = !!initial.secret;
    secretCb.addEventListener('change', () => {
        const val = row.querySelector('.mcp-server-env-value');
        val.type = secretCb.checked ? 'password' : 'text';
        val.placeholder = secretCb.checked ? 'Secret value' : 'value';
        invalidateMcpServerTest();
    });
    row.querySelector('.mcp-server-env-remove').addEventListener('click', () => {
        row.remove();
        invalidateMcpServerTest();
    });
    [row.querySelector('.mcp-server-env-key'), row.querySelector('.mcp-server-env-value')]
        .forEach((el) => el.addEventListener('input', invalidateMcpServerTest));
    wrap.appendChild(row);
}

function readMcpEnvRows() {
    const rows = document.querySelectorAll('#mcp-server-env-rows .mcp-server-env-row');
    const bindings = [];      // entries to write into McpCatalogEntry.source.env
    const env_secrets = {};   // KEY → raw value, for the vault
    for (const row of rows) {
        const key = row.querySelector('.mcp-server-env-key').value.trim();
        const value = row.querySelector('.mcp-server-env-value').value;
        const secret = row.querySelector('.mcp-server-env-secret-cb').checked;
        if (!key) continue;
        if (secret) {
            // McpRegistry expects an EnvValue::Vault pointing at the scope
            // we use for this MCP. The backend overwrites scope for the
            // dry-run path; we still send a sensible "mcp:<id>" so the
            // persisted shape matches what the spec requires.
            bindings.push({ key, value: { kind: 'vault', scope: '', key: key } });
            env_secrets[key] = value;
        } else {
            bindings.push({ key, value: { kind: 'plain', value } });
        }
    }
    return { bindings, env_secrets };
}

// Save is gated on a successful test connection (or an existing saved
// entry being re-saved without changes). Any edit clears the test result
// and disables Save again.
function invalidateMcpServerTest() {
    const resultEl = document.getElementById('mcp-server-test-result');
    if (resultEl) {
        resultEl.classList.add('hidden');
        resultEl.className = 'mcp-server-test-result hidden';
        resultEl.textContent = '';
    }
    const saveBtn = document.getElementById('mcp-server-modal-save');
    if (saveBtn) saveBtn.disabled = true;
}

function openMcpServerModal() {
    const overlay = document.getElementById('mcp-server-modal-overlay');
    if (!overlay) return;
    document.getElementById('mcp-server-name').value = '';
    document.getElementById('mcp-server-command').value = '';
    document.getElementById('mcp-server-args').value = '';
    document.getElementById('mcp-server-working-dir').value = '';
    const envWrap = document.getElementById('mcp-server-env-rows');
    if (envWrap) envWrap.innerHTML = '';
    const errorEl = document.getElementById('mcp-server-modal-error');
    if (errorEl) errorEl.classList.add('hidden');
    invalidateMcpServerTest();
    overlay.classList.remove('hidden');
    setTimeout(() => document.getElementById('mcp-server-name').focus(), 50);
}

function closeMcpServerModal() {
    const overlay = document.getElementById('mcp-server-modal-overlay');
    if (overlay) overlay.classList.add('hidden');
}

function buildMcpEntryFromForm() {
    const displayName = document.getElementById('mcp-server-name').value.trim();
    const command = document.getElementById('mcp-server-command').value.trim();
    const argsRaw = document.getElementById('mcp-server-args').value;
    const workingDir = document.getElementById('mcp-server-working-dir').value.trim() || null;
    if (!displayName) throw new Error('Name is required');
    if (!command) throw new Error('Command is required');
    const id = slugifyMcpId(displayName);
    const { bindings, env_secrets } = readMcpEnvRows();
    // Now that we have the id, rewrite the per-binding scope so the
    // persisted EnvValue::Vault points at the right vault location.
    for (const b of bindings) {
        if (b.value && b.value.kind === 'vault') {
            b.value.scope = `mcp:${id}`;
        }
    }
    const entry = {
        id,
        display_name: displayName,
        description: '',
        icon: null,
        config_schema: {},
        source: {
            kind: 'process',
            command,
            args: parseMcpArgs(argsRaw),
            env: bindings,
            working_dir: workingDir,
        },
        // BYO MCPs default to WritePersist — same baseline as
        // mcp_custom_entries roundtrip test. The per-call risk gate is
        // still the real authority.
        base_risk: 'WritePersist',
    };
    return { entry, env_secrets };
}

async function testMcpServerConnection() {
    const resultEl = document.getElementById('mcp-server-test-result');
    const saveBtn = document.getElementById('mcp-server-modal-save');
    const errorEl = document.getElementById('mcp-server-modal-error');
    if (errorEl) errorEl.classList.add('hidden');
    if (saveBtn) saveBtn.disabled = true;
    if (!resultEl) return;
    resultEl.classList.remove('hidden');
    resultEl.className = 'mcp-server-test-result mcp-server-test-pending';
    resultEl.textContent = 'Spawning server and running MCP handshake…';

    let payload;
    try {
        payload = buildMcpEntryFromForm();
    } catch (err) {
        resultEl.className = 'mcp-server-test-result mcp-server-test-fail';
        resultEl.textContent = String(err.message || err);
        return;
    }

    try {
        const res = await invoke('mcp_test_spawn', {
            entry: payload.entry,
            envSecrets: payload.env_secrets,
        });
        resultEl.className = 'mcp-server-test-result mcp-server-test-ok';
        const preview = (res.tool_names || []).slice(0, 8).join(', ');
        const more = res.tool_count > 8 ? `, +${res.tool_count - 8} more` : '';
        resultEl.textContent = `Connected — found ${res.tool_count} tool${res.tool_count === 1 ? '' : 's'}${preview ? `: ${preview}${more}` : ''}`;
        if (saveBtn) saveBtn.disabled = false;
    } catch (err) {
        resultEl.className = 'mcp-server-test-result mcp-server-test-fail';
        resultEl.textContent = `Error: ${String(err)}`;
    }
}

async function saveMcpServerFromModal() {
    const errorEl = document.getElementById('mcp-server-modal-error');
    if (errorEl) errorEl.classList.add('hidden');
    let payload;
    try {
        payload = buildMcpEntryFromForm();
    } catch (err) {
        if (errorEl) {
            errorEl.textContent = String(err.message || err);
            errorEl.classList.remove('hidden');
        }
        return;
    }
    // Reject collisions against the bundled catalog (the user can still
    // toggle bundled entries through the "Tools (MCP)" panel above).
    if (mcpServersCustom.some((e) => e.id === payload.entry.id)) {
        if (errorEl) {
            errorEl.textContent = `An MCP server with id "${payload.entry.id}" already exists. Pick a different name.`;
            errorEl.classList.remove('hidden');
        }
        return;
    }
    try {
        await invoke('mcp_add_custom', {
            entry: payload.entry,
            envSecrets: payload.env_secrets,
            enableNow: true,
        });
        closeMcpServerModal();
        await loadMcpServers();
        showToast('MCP server added', 'success');
    } catch (err) {
        if (errorEl) {
            errorEl.textContent = String(err);
            errorEl.classList.remove('hidden');
        }
    }
}

function wireMcpServersPanel() {
    const addBtn = document.getElementById('mcp-servers-add-btn');
    if (addBtn) addBtn.addEventListener('click', openMcpServerModal);
    const closeBtn = document.getElementById('mcp-server-modal-close');
    if (closeBtn) closeBtn.addEventListener('click', closeMcpServerModal);
    const cancelBtn = document.getElementById('mcp-server-modal-cancel');
    if (cancelBtn) cancelBtn.addEventListener('click', closeMcpServerModal);
    const saveBtn = document.getElementById('mcp-server-modal-save');
    if (saveBtn) saveBtn.addEventListener('click', saveMcpServerFromModal);
    const testBtn = document.getElementById('mcp-server-test-btn');
    if (testBtn) testBtn.addEventListener('click', testMcpServerConnection);
    const envAddBtn = document.getElementById('mcp-server-env-add-btn');
    if (envAddBtn) envAddBtn.addEventListener('click', () => {
        addMcpEnvRow();
        invalidateMcpServerTest();
    });
    // Re-run "test required" gating when basic fields change.
    ['mcp-server-name', 'mcp-server-command', 'mcp-server-args', 'mcp-server-working-dir']
        .forEach((id) => {
            const el = document.getElementById(id);
            if (el) el.addEventListener('input', invalidateMcpServerTest);
        });
    const overlay = document.getElementById('mcp-server-modal-overlay');
    if (overlay) overlay.addEventListener('click', (ev) => {
        if (ev.target === overlay) closeMcpServerModal();
    });
}

// ─── Initialize ───

inputEl.focus();
wireOnboardingButtons();
wireCloudApisModal();
wireMcpServersPanel();
wireActiveAgentsPanel();
initTauri();
