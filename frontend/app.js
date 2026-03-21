// ─── Tauri Initialization ───

let invoke;

function initTauri() {
    if (window.__TAURI__ && window.__TAURI__.core) {
        invoke = window.__TAURI__.core.invoke;

        // Listen for real-time agent progress events.
        if (window.__TAURI__.event && window.__TAURI__.event.listen) {
            window.__TAURI__.event.listen('agent-progress', (event) => {
                const { step, tool_name, status } = event.payload;
                setStatus('working', `Step ${step}: ${tool_name} (${status})`);
            });
        }

        setStatus('idle', 'Ready');
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

            addMessage('assistant', response.content || '', meta);
        }

        setStatus('idle', 'Ready');
    } catch (err) {
        console.error('Approval error:', err);
        addMessage('assistant', `Error: ${err}`, { isError: true });
        setStatus('error', 'Error');
    }

    setInputEnabled(true);
    inputEl.focus();
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

    // Show user message
    addMessage('user', message);
    inputEl.value = '';
    inputEl.style.height = 'auto';

    // Disable input while processing
    setInputEnabled(false);
    setStatus('working', 'Thinking...');

    try {
        // Call Tauri backend
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

        addMessage('assistant', response.content || '', meta);
        setStatus('idle', 'Ready');
    } catch (err) {
        console.error('Tauri invoke error:', err);
        addMessage('assistant', `Error: ${err}`, { isError: true });
        setStatus('error', 'Error');
    }

    setInputEnabled(true);
    inputEl.focus();
});

// ─── Initialize ───

inputEl.focus();
initTauri();
