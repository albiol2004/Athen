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

// ─── Session State ───

// The currently active session ID.
let activeSessionId = null;
// Whether the first user message in this session has been sent
// (used to auto-name the session).
let sessionHasMessages = false;

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
            // Each event carries { delta: String, is_final: bool }.
            window.__TAURI__.event.listen('agent-stream', (event) => {
                const { delta, is_final } = event.payload;

                if (is_final) {
                    // Stream complete -- re-render the full text with markdown
                    // for proper formatting now that we have the complete content.
                    if (streamingBubble && streamingText) {
                        streamingBubble.innerHTML = renderMarkdown(streamingText);
                    }
                    // Do NOT reset streamingBubble here -- the form submission
                    // handler will finalize the message with meta info.
                    return;
                }

                if (!delta) return;

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

                // Append the delta as escaped text. Full markdown rendering
                // happens when the stream is finalized (is_final=true).
                // Using textContent here is safe against XSS and fast for
                // frequent small updates.
                streamingBubble.textContent = streamingText;

                // Keep the view scrolled to the bottom during streaming.
                requestAnimationFrame(() => {
                    messagesEl.parentElement.scrollTo({
                        top: messagesEl.parentElement.scrollHeight,
                        behavior: 'auto'
                    });
                });
            });
        }

        setStatus('idle', 'Ready');

        // Load the current session ID, then load sessions and history.
        invoke('get_current_session').then((sid) => {
            activeSessionId = sid;
            loadSessions();
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
const sessionListEl = document.getElementById('session-list');
const sidebarEl = document.getElementById('sidebar');
const sidebarOverlay = document.getElementById('sidebar-overlay');
const sidebarToggle = document.getElementById('sidebar-toggle');

// ─── Sidebar Logic ───

async function loadSessions() {
    if (!invoke) return;
    try {
        const sessions = await invoke('list_sessions');
        renderSessionList(sessions);
    } catch (err) {
        console.error('Failed to load sessions:', err);
    }
}

function renderSessionList(sessions) {
    sessionListEl.innerHTML = '';

    if (!sessions || sessions.length === 0) {
        sessionListEl.innerHTML = '<div class="session-list-empty">No conversations yet</div>';
        return;
    }

    for (const session of sessions) {
        const item = document.createElement('div');
        item.className = 'session-item';
        if (session.session_id === activeSessionId) {
            item.classList.add('active');
        }
        item.dataset.sessionId = session.session_id;

        const content = document.createElement('div');
        content.className = 'session-item-content';

        const nameEl = document.createElement('div');
        nameEl.className = 'session-item-name';
        nameEl.textContent = session.name;
        content.appendChild(nameEl);

        const metaEl = document.createElement('div');
        metaEl.className = 'session-item-meta';

        const dateEl = document.createElement('span');
        dateEl.className = 'session-item-date';
        dateEl.textContent = formatSessionDate(session.updated_at);
        metaEl.appendChild(dateEl);

        if (session.message_count > 0) {
            const countEl = document.createElement('span');
            countEl.className = 'session-item-count';
            countEl.textContent = session.message_count;
            metaEl.appendChild(countEl);
        }

        content.appendChild(metaEl);
        item.appendChild(content);

        // Action buttons (rename + delete)
        const actions = document.createElement('div');
        actions.className = 'session-item-actions';

        const renameBtn = document.createElement('button');
        renameBtn.className = 'session-action-btn';
        renameBtn.title = 'Rename';
        renameBtn.innerHTML = '&#9998;'; // pencil
        renameBtn.addEventListener('click', (e) => {
            e.stopPropagation();
            startRenameSession(item, session.session_id, session.name);
        });
        actions.appendChild(renameBtn);

        const deleteBtn = document.createElement('button');
        deleteBtn.className = 'session-action-btn delete';
        deleteBtn.title = 'Delete';
        deleteBtn.innerHTML = '&#10005;'; // x mark
        deleteBtn.addEventListener('click', (e) => {
            e.stopPropagation();
            handleDeleteSession(session.session_id);
        });
        actions.appendChild(deleteBtn);

        item.appendChild(actions);

        // Click to switch session
        item.addEventListener('click', () => {
            handleSwitchSession(session.session_id);
        });

        // Double-click to rename
        nameEl.addEventListener('dblclick', (e) => {
            e.stopPropagation();
            startRenameSession(item, session.session_id, session.name);
        });

        sessionListEl.appendChild(item);
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

function startRenameSession(itemEl, sessionId, currentName) {
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
                await invoke('rename_session', { sessionId, name: newName });
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

async function handleSwitchSession(sessionId) {
    if (!invoke || sessionId === activeSessionId) return;

    try {
        const messages = await invoke('switch_session', { sessionId });
        activeSessionId = sessionId;

        // Check if the session has messages already (for auto-naming).
        sessionHasMessages = messages && messages.length > 0;

        // Clear the chat UI and render the loaded messages.
        clearChatUI();
        if (messages && messages.length > 0) {
            for (const msg of messages) {
                addMessage(msg.role, msg.content);
            }
        }

        // Update active highlight in sidebar.
        document.querySelectorAll('.session-item').forEach((el) => {
            el.classList.toggle('active', el.dataset.sessionId === sessionId);
        });

        // Close sidebar on mobile.
        closeSidebar();

        inputEl.focus();
    } catch (err) {
        console.error('Switch session failed:', err);
    }
}

async function handleDeleteSession(sessionId) {
    if (!invoke) return;
    if (!confirm('Delete this conversation? This cannot be undone.')) return;

    try {
        const newActiveId = await invoke('delete_session', { sessionId });

        // If the deleted session was the active one, the backend switched us.
        if (sessionId === activeSessionId) {
            activeSessionId = newActiveId;
            // Reload messages for the new active session.
            try {
                const messages = await invoke('get_history');
                clearChatUI();
                if (messages && messages.length > 0) {
                    for (const msg of messages) {
                        addMessage(msg.role, msg.content);
                    }
                    sessionHasMessages = true;
                } else {
                    sessionHasMessages = false;
                }
            } catch (err2) {
                console.error('Failed to load history after delete:', err2);
                clearChatUI();
            }
        }

        // Refresh the sidebar list.
        await loadSessions();
    } catch (err) {
        console.error('Delete session failed:', err);
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

// ─── Auto-name session ───

async function autoNameSession(message) {
    if (!invoke || !activeSessionId || sessionHasMessages) return;
    sessionHasMessages = true;

    // Truncate the first message to ~30 characters for the session name.
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
        await invoke('rename_session', { sessionId: activeSessionId, name });
        // Update the sidebar item in place.
        const item = sessionListEl.querySelector(
            `.session-item[data-session-id="${activeSessionId}"] .session-item-name`
        );
        if (item) {
            item.textContent = name;
        }
    } catch (err) {
        console.error('Auto-name session failed:', err);
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
        bubble.textContent = content;
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
        // Render the accumulated text with full markdown. This handles
        // the case where finalize runs before or after the is_final event.
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
    sendBtn.disabled = !enabled;
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
    loadSessions();
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

    // Auto-name the session from the first message.
    autoNameSession(message);

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

        if (didReceiveStreamChunks && streamingBubble) {
            // The response was already rendered progressively via streaming.
            // Just finalize with meta info (risk badge, domain, time).
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
    loadSessions();
});

// ─── History Restoration ───

async function loadHistory() {
    if (!invoke) return;
    try {
        const messages = await invoke('get_history');
        if (messages && messages.length > 0) {
            // Remove the welcome message since we have history.
            const welcome = messagesEl.querySelector('.welcome-message');
            if (welcome) welcome.remove();

            for (const msg of messages) {
                addMessage(msg.role, msg.content);
            }
            sessionHasMessages = true;
        }
    } catch (err) {
        console.error('Failed to load history:', err);
    }
}

// ─── New Chat (both sidebar button and header button) ───

async function handleNewChat() {
    if (!invoke) return;
    try {
        const newId = await invoke('new_session');
        activeSessionId = newId;
        sessionHasMessages = false;
        clearChatUI();
        closeSidebar();
        await loadSessions();
        inputEl.focus();
    } catch (err) {
        console.error('Failed to start new session:', err);
    }
}

const newChatBtn = document.getElementById('new-chat-btn');
if (newChatBtn) {
    newChatBtn.addEventListener('click', handleNewChat);
}

const sidebarNewChatBtn = document.getElementById('sidebar-new-chat-btn');
if (sidebarNewChatBtn) {
    sidebarNewChatBtn.addEventListener('click', handleNewChat);
}

// ─── Settings ───

const settingsView = document.getElementById('settings-view');
const settingsBtn = document.getElementById('settings-btn');
const settingsBack = document.getElementById('settings-back');
const appView = document.getElementById('app');
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
    settingsView.classList.remove('hidden');
    closeSidebar();
    loadSettings();
}

function showChat() {
    settingsView.classList.add('hidden');
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

// ─── Initialize ───

inputEl.focus();
initTauri();
