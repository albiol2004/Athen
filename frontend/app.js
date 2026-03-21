// Wait for Tauri to be ready
let invoke;

function initTauri() {
    if (window.__TAURI__ && window.__TAURI__.core) {
        invoke = window.__TAURI__.core.invoke;

        // Listen for real-time agent progress events.
        if (window.__TAURI__.event && window.__TAURI__.event.listen) {
            window.__TAURI__.event.listen('agent-progress', (event) => {
                const { step, tool_name, status } = event.payload;
                setStatus(`Step ${step}: ${tool_name} (${status})`);
            });
        }

        setStatus('Ready');
    } else {
        setStatus('Waiting for Tauri...');
        setTimeout(initTauri, 100);
    }
}

const messagesEl = document.getElementById('messages');
const inputEl = document.getElementById('message-input');
const formEl = document.getElementById('input-form');
const statusEl = document.getElementById('status');

function addMessage(role, content, meta) {
    const div = document.createElement('div');
    div.className = `message ${role}`;

    let html = '';
    if (meta) {
        html += `<div class="meta">${meta}</div>`;
    }
    html += content;
    div.innerHTML = html;

    messagesEl.appendChild(div);
    messagesEl.parentElement.scrollTop = messagesEl.parentElement.scrollHeight;
}

function setStatus(text) {
    statusEl.textContent = text;
}

function setInputEnabled(enabled) {
    inputEl.disabled = !enabled;
    document.querySelector('#input-form button').disabled = !enabled;
}

formEl.addEventListener('submit', async (e) => {
    e.preventDefault();

    const message = inputEl.value.trim();
    if (!message) return;

    if (!invoke) {
        addMessage('assistant', 'Error: Tauri backend not connected. Is the app running inside Tauri?');
        return;
    }

    // Show user message
    addMessage('user', message);
    inputEl.value = '';

    // Disable input while processing
    setInputEnabled(false);
    setStatus('Thinking...');

    try {
        // Call Tauri backend
        const response = await invoke('send_message', { message });

        // Show risk info if available
        let meta = '';
        if (response.risk_level) {
            const riskClass = response.risk_level === 'Safe' ? 'safe' :
                             response.risk_level === 'Caution' ? 'caution' : 'danger';
            meta = `<span class="risk-badge ${riskClass}">${response.risk_level}</span> `;
        }
        if (response.domain) {
            meta += `${response.domain}`;
        }

        // Show tool calls if any
        let content = '';
        if (response.tool_calls && response.tool_calls.length > 0) {
            for (const tc of response.tool_calls) {
                content += `<div class="tool-call">* ${tc.name}: ${tc.summary}</div>`;
            }
        }
        content += response.content || '';

        addMessage('assistant', content, meta || undefined);
        setStatus('Ready');
    } catch (err) {
        console.error('Tauri invoke error:', err);
        addMessage('assistant', `Error: ${err}`);
        setStatus('Error');
    }

    setInputEnabled(true);
    inputEl.focus();
});

// Initialize
inputEl.focus();
initTauri();
