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

function initTauri() {
    if (window.__TAURI__ && window.__TAURI__.core) {
        invoke = window.__TAURI__.core.invoke;

        // Listen for real-time agent progress events.
        if (window.__TAURI__.event && window.__TAURI__.event.listen) {
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
                card.className = `tool-execution-card ${statusClass}`;

                const statusIcon = status === 'Completed' ? '&#10003;' :
                                   status === 'Failed' ? '&#10007;' : '&#9679;';

                let detailHtml = '';
                if (detail) {
                    const truncated = detail.length > 80 ? detail.substring(0, 80) + '...' : detail;
                    detailHtml = `<span class="tool-detail">${escapeHtml(truncated)}</span>`;
                }

                card.innerHTML =
                    `<span class="tool-status-icon">${statusIcon}</span>` +
                    `<span class="tool-name">${escapeHtml(tool_name)}</span>` +
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
                const { delta, is_final, arc_id } = event.payload;

                // Check if this stream belongs to the currently visible arc.
                const isActiveArc = !arc_id || arc_id === activeArcId;

                if (is_final) {
                    if (isActiveArc && streamingBubble && streamingText) {
                        streamingBubble.innerHTML = renderMarkdown(streamingText);
                        streamingBubble.classList.remove('streaming');
                    }
                    // If it was a background arc, show a notification dot
                    // and refresh the sidebar.
                    if (!isActiveArc && arc_id) {
                        markArcWithNotification(arc_id);
                        loadArcs();
                    }
                    streamingBubble = null;
                    streamingText = '';
                    return;
                }

                if (!delta) return;

                // For background arcs, silently accumulate but don't render.
                if (!isActiveArc) return;

                didReceiveStreamChunks = true;
                streamingText += delta;

                // Create the streaming bubble on the first chunk.
                if (!streamingBubble) {
                    // Remove welcome message if present.
                    const welcome = messagesEl.querySelector('.welcome-message');
                    if (welcome) welcome.remove();

                    const row = document.createElement('div');
                    row.className = 'message-row assistant';
                    row.id = 'streaming-message';

                    const avatar = document.createElement('div');
                    avatar.className = 'message-avatar';
                    avatar.textContent = 'A';

                    const wrap = document.createElement('div');
                    wrap.className = 'message-content-wrap';

                    streamingBubble = document.createElement('div');
                    streamingBubble.className = 'message-bubble streaming';

                    wrap.appendChild(streamingBubble);
                    row.appendChild(avatar);
                    row.appendChild(wrap);
                    messagesEl.appendChild(row);
                }

                streamingBubble.textContent = streamingText;

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
            });

            // Listen for sense events (email, calendar, messaging, etc.)
            window.__TAURI__.event.listen('sense-event', (event) => {
                const { source, from, subject, body_preview,
                        relevance, reason, suggested_action, arc_id } = event.payload;
                showSenseNotification(source, from, subject, body_preview,
                                      relevance, reason, suggested_action, arc_id);
            });
        }

        setStatus('idle', 'Ready');

        // Load the current arc ID, then load arcs and history.
        invoke('get_current_arc').then((sid) => {
            activeArcId = sid;
            loadArcs();
            loadHistory();
        }).catch(() => {
            loadHistory();
        });
    } else {
        setStatus('working', 'Waiting for Tauri...');
        setTimeout(initTauri, 100);
    }
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

function renderArcList(arcs) {
    sessionListEl.innerHTML = '';

    if (!arcs || arcs.length === 0) {
        sessionListEl.innerHTML = '<div class="session-list-empty">No conversations yet</div>';
        return;
    }

    for (const arc of arcs) {
        // Skip merged arcs
        if (arc.status === 'Merged') continue;

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

        sessionListEl.appendChild(item);
    }
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
        if (entries && entries.length > 0) {
            for (const entry of entries) {
                if (entry.entry_type === 'message') {
                    addMessage(entry.source, entry.content);
                } else if (entry.entry_type === 'email_event') {
                    const meta = entry.metadata ? (typeof entry.metadata === 'string' ? JSON.parse(entry.metadata) : entry.metadata) : {};
                    addEmailEntry(entry.content, meta);
                } else if (entry.entry_type === 'tool_call') {
                    addSystemEntry(entry.content, 'tool');
                }
            }
        }

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
                if (entries && entries.length > 0) {
                    for (const entry of entries) {
                        if (entry.entry_type === 'message') {
                            addMessage(entry.source, entry.content);
                        } else if (entry.entry_type === 'email_event') {
                            const meta = entry.metadata ? (typeof entry.metadata === 'string' ? JSON.parse(entry.metadata) : entry.metadata) : {};
                            addEmailEntry(entry.content, meta);
                        } else if (entry.entry_type === 'tool_call') {
                            addSystemEntry(entry.content, 'tool');
                        }
                    }
                    arcHasMessages = true;
                } else {
                    arcHasMessages = false;
                }
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

function renderMarkdown(text) {
    // Collect code blocks first to protect them from other transformations
    const codeBlocks = [];
    let processed = text.replace(/```(\w*)\n([\s\S]*?)```/g, (_match, lang, code) => {
        const idx = codeBlocks.length;
        const escapedCode = escapeHtml(code.replace(/\n$/, ''));
        const langLabel = lang ? `<span class="code-lang">${escapeHtml(lang)}</span>` : '';
        codeBlocks.push(`<pre>${langLabel}<code>${escapedCode}</code></pre>`);
        return `\x00CODEBLOCK_${idx}\x00`;
    });

    // Inline code (protect from further processing)
    const inlineCodes = [];
    processed = processed.replace(/`([^`\n]+)`/g, (_match, code) => {
        const idx = inlineCodes.length;
        inlineCodes.push(`<code>${escapeHtml(code)}</code>`);
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

        // Empty line — paragraph break
        if (line.trim() === '') {
            result.push('');
            i++;
            continue;
        }

        // Regular text — collect consecutive lines into a paragraph
        const paraLines = [];
        while (i < lines.length && lines[i].trim() !== '' &&
               !/^#{1,3}\s+/.test(lines[i]) &&
               !/^[\s]*[-*]\s+/.test(lines[i]) &&
               !/^[\s]*\d+\.\s+/.test(lines[i]) &&
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

    // Restore code blocks
    codeBlocks.forEach((block, idx) => {
        html = html.replace(`\x00CODEBLOCK_${idx}\x00`, block);
        // Also handle if wrapped in <p>
        html = html.replace(`<p>${block}</p>`, block);
    });

    // Restore inline codes
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
                const name = escapeHtml(tc.name || '');
                const summary = escapeHtml(tc.summary || '');
                toolsHtml += `<div class="tool-call">
                    <span class="tool-call-icon">&#128295;</span>
                    <span class="tool-call-name">${name}</span>
                    <span class="tool-call-summary">${summary}</span>
                </div>`;
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

async function loadHistory() {
    if (!invoke) return;
    try {
        const entries = await invoke('get_arc_history');
        if (entries && entries.length > 0) {
            arcHasMessages = true;
            // Remove the welcome message since we have history.
            const welcome = messagesEl.querySelector('.welcome-message');
            if (welcome) welcome.remove();

            for (const entry of entries) {
                if (entry.entry_type === 'message') {
                    addMessage(entry.source, entry.content);
                } else if (entry.entry_type === 'email_event') {
                    const meta = entry.metadata ? (typeof entry.metadata === 'string' ? JSON.parse(entry.metadata) : entry.metadata) : {};
                    addEmailEntry(entry.content, meta);
                } else if (entry.entry_type === 'tool_call') {
                    addSystemEntry(entry.content, 'tool');
                }
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
    } catch (err) {
        console.error('Failed to load settings:', err);
        showToast('Failed to load settings: ' + err, 'error');
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
                    invoke('mark_notification_seen', { id: data.id }).catch(() => {});
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

// ─── Arc Timeline ───

let timelineRefreshInterval = null;

const timelineToggleBtn = document.getElementById('timeline-toggle-btn');
const timelineBackBtn = document.getElementById('timeline-back');

function showTimeline() {
    appView.style.display = 'none';
    settingsView.classList.add('hidden');
    calendarView?.classList.add('hidden');
    contactsView?.classList.add('hidden');
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

// ─── Initialize ───

inputEl.focus();
initTauri();
