// Athen Admin panel — plain JS, same-origin fetch with session cookie.
'use strict';

const $ = (sel) => document.querySelector(sel);

let ME = null;          // { id, username, role }
let USERS = [];         // admin cache for grant pickers
let logsSource = null;  // live EventSource for the logs drawer
let refreshTimer = null;

// ------------------------------------------------------------- helpers --

async function api(path, opts = {}) {
  const init = { headers: {}, ...opts };
  if (init.body !== undefined && typeof init.body !== 'string') {
    init.body = JSON.stringify(init.body);
    init.headers['Content-Type'] = 'application/json';
  }
  const resp = await fetch(path, init);
  if (resp.status === 401 && ME) { showLogin(); throw new Error('session expired'); }
  let data = null;
  try { data = await resp.json(); } catch { /* empty body */ }
  if (!resp.ok) throw new Error((data && data.error) || `HTTP ${resp.status}`);
  return data;
}

function toast(msg, kind = 'ok') {
  const el = document.createElement('div');
  el.className = `toast ${kind}`;
  el.textContent = msg;
  $('#toast-host').appendChild(el);
  setTimeout(() => el.remove(), kind === 'error' ? 6000 : 3000);
}

function esc(s) {
  const d = document.createElement('span');
  d.textContent = String(s ?? '');
  return d.innerHTML;
}

function svg(name) {
  const icons = {
    play: '<path d="M7 4l13 8-13 8z"/>',
    stop: '<rect x="6" y="6" width="12" height="12" rx="1.5"/>',
    logs: '<path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z"/><path d="M14 2v6h6M9 13h6M9 17h6"/>',
    trash: '<path d="M3 6h18M8 6V4a1 1 0 0 1 1-1h6a1 1 0 0 1 1 1v2m3 0v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6"/>',
    users: '<path d="M17 21v-2a4 4 0 0 0-4-4H7a4 4 0 0 0-4 4v2"/><circle cx="10" cy="7" r="4"/><path d="M21 21v-2a4 4 0 0 0-3-3.87"/>',
    chat: '<path d="M21 11.5a8.4 8.4 0 0 1-9 8.4 8.6 8.6 0 0 1-3.7-.84L3 20l1-4.9A8.4 8.4 0 0 1 3 11.5a8.4 8.4 0 0 1 9-8.4 8.4 8.4 0 0 1 9 8.4z"/>',
  };
  return `<svg viewBox="0 0 24 24" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.9" stroke-linecap="round" stroke-linejoin="round">${icons[name] || ''}</svg>`;
}

// ---------------------------------------------------------------- boot --

async function boot() {
  try {
    ME = await api('/panel/me');
    showApp();
  } catch {
    showLogin();
  }
}

function showLogin() {
  ME = null;
  stopRefresh();
  $('#view-app').classList.add('hidden');
  $('#view-login').classList.remove('hidden');
  $('#login-user').focus();
}

function showApp() {
  $('#view-login').classList.add('hidden');
  $('#view-app').classList.remove('hidden');
  $('#who').textContent = `${ME.username} · ${ME.role}`;
  document.querySelectorAll('.admin-only')
    .forEach((el) => el.classList.toggle('hidden', ME.role !== 'admin'));
  switchTab('instances');
}

function switchTab(tab) {
  document.querySelectorAll('.tab').forEach((t) => t.classList.toggle('active', t.dataset.tab === tab));
  $('#tab-instances').classList.toggle('hidden', tab !== 'instances');
  $('#tab-users').classList.toggle('hidden', tab !== 'users');
  stopRefresh();
  if (tab === 'instances') {
    loadInstances();
    refreshTimer = setInterval(loadInstances, 5000);
  } else {
    loadUsers();
  }
}

function stopRefresh() {
  if (refreshTimer) { clearInterval(refreshTimer); refreshTimer = null; }
}

// ----------------------------------------------------------- instances --

async function loadInstances() {
  let list;
  try { list = await api('/panel/instances'); } catch (e) { toast(e.message, 'error'); return; }
  const grid = $('#instances-grid');
  $('#instances-empty').classList.toggle('hidden', list.length > 0);
  grid.innerHTML = list.map(instanceCard).join('');
  grid.querySelectorAll('[data-action]').forEach((btn) => {
    btn.addEventListener('click', () => instanceAction(btn.dataset.action, btn.dataset.id, btn.dataset.name));
  });
}

function instanceCard(i) {
  const admin = ME.role === 'admin';
  const running = i.state === 'running';
  const actions = [];
  actions.push(`<a class="btn small primary" href="/i/${i.id}/chat">${svg('chat')} Open chat</a>`);
  if (admin) {
    actions.push(running
      ? `<button class="btn small" data-action="stop" data-id="${i.id}" data-name="${esc(i.name)}">${svg('stop')} Stop</button>`
      : `<button class="btn small" data-action="start" data-id="${i.id}" data-name="${esc(i.name)}">${svg('play')} Start</button>`);
    actions.push(`<button class="btn small" data-action="logs" data-id="${i.id}" data-name="${esc(i.name)}">${svg('logs')} Logs</button>`);
    actions.push(`<button class="btn small" data-action="grants" data-id="${i.id}" data-name="${esc(i.name)}">${svg('users')} Access</button>`);
    actions.push(`<button class="btn small danger" data-action="delete" data-id="${i.id}" data-name="${esc(i.name)}">${svg('trash')}</button>`);
  }
  return `<div class="card">
    <div class="card-top">
      <span class="card-name">${esc(i.name)}</span>
      <span class="badge ${esc(i.state)}">${esc(i.state)}</span>
    </div>
    <div class="card-meta">
      <span><code>${esc(i.container_name)}</code></span>
      <span>${esc(i.status)}</span>
    </div>
    <div class="card-actions">${actions.join('')}</div>
  </div>`;
}

async function instanceAction(action, id, name) {
  if (action === 'start' || action === 'stop') {
    try {
      await api(`/panel/instances/${id}/${action}`, { method: 'POST' });
      toast(`${name}: ${action} ok`);
      loadInstances();
    } catch (e) { toast(e.message, 'error'); }
  } else if (action === 'logs') {
    openLogs(id, name);
  } else if (action === 'grants') {
    openGrantsModal(id, name);
  } else if (action === 'delete') {
    openDeleteModal(id, name);
  }
}

// ---- logs drawer ----

function openLogs(id, name) {
  closeLogs();
  $('#logs-title').textContent = `Logs — ${name}`;
  $('#logs-body').textContent = '';
  $('#logs-drawer').classList.remove('hidden');
  logsSource = new EventSource(`/panel/instances/${id}/logs?tail=200&follow=true`);
  logsSource.addEventListener('log', (ev) => {
    const body = $('#logs-body');
    const pinned = body.scrollTop + body.clientHeight >= body.scrollHeight - 40;
    body.textContent += ev.data + '\n';
    if (pinned) body.scrollTop = body.scrollHeight;
  });
  logsSource.onerror = () => { /* container stopped or stream closed */ };
}

function closeLogs() {
  if (logsSource) { logsSource.close(); logsSource = null; }
  $('#logs-drawer').classList.add('hidden');
}

// ---- modals ----

function openModal(html) {
  $('#modal').innerHTML = html;
  $('#modal-overlay').classList.remove('hidden');
}

function closeModal() {
  $('#modal-overlay').classList.add('hidden');
  $('#modal').innerHTML = '';
}

async function ensureUsers() {
  if (ME.role !== 'admin') return [];
  try { USERS = await api('/panel/users'); } catch { USERS = []; }
  return USERS;
}

function userChecklist(checkedIds = []) {
  const users = USERS.filter((u) => u.role !== 'admin');
  if (!users.length) return '<div class="hint">No non-admin users yet — create one in the Users tab.</div>';
  return `<div class="check-list">${users.map((u) =>
    `<label><input type="checkbox" name="grant" value="${u.id}" ${checkedIds.includes(u.id) ? 'checked' : ''}> ${esc(u.username)}</label>`
  ).join('')}</div>`;
}

function grantValues() {
  return [...document.querySelectorAll('input[name="grant"]:checked')].map((c) => c.value);
}

async function openNewInstanceModal() {
  await ensureUsers();
  openModal(`<h3>New instance</h3>
    <form id="form-new-instance">
      <label>Name <input id="ni-name" required placeholder="alice"></label>
      <label>Environment variables <textarea id="ni-env" placeholder="ATHEN_PROVIDER_DEEPSEEK_API_KEY=sk-...&#10;ATHEN_TELEGRAM_BOT_TOKEN=..."></textarea></label>
      <div class="hint">One KEY=VALUE per line. Secrets go here (never into the config files below).</div>
      <details>
        <summary>Seed config files (optional)</summary>
        <label>config.toml <textarea id="ni-config" placeholder="[telegram]&#10;enabled = false"></textarea></label>
        <label>models.toml <textarea id="ni-models" placeholder="[providers.deepseek]&#10;auth = &quot;None&quot; ..."></textarea></label>
      </details>
      <label>Grant access</label>
      ${userChecklist()}
      <div class="modal-actions">
        <button type="button" class="btn" id="modal-cancel">Cancel</button>
        <button type="submit" class="btn primary" id="ni-submit">Create &amp; start</button>
      </div>
    </form>`);
  $('#modal-cancel').addEventListener('click', closeModal);
  $('#form-new-instance').addEventListener('submit', async (ev) => {
    ev.preventDefault();
    const env = {};
    for (const line of $('#ni-env').value.split('\n')) {
      const t = line.trim();
      if (!t || t.startsWith('#')) continue;
      const eq = t.indexOf('=');
      if (eq <= 0) { toast(`bad env line: ${t}`, 'error'); return; }
      env[t.slice(0, eq).trim()] = t.slice(eq + 1).trim();
    }
    const body = {
      name: $('#ni-name').value.trim(),
      env,
      config_toml: $('#ni-config').value.trim() || null,
      models_toml: $('#ni-models').value.trim() || null,
      user_ids: grantValues(),
    };
    const btn = $('#ni-submit');
    btn.disabled = true;
    btn.textContent = 'Provisioning…';
    try {
      await api('/panel/instances', { method: 'POST', body });
      toast(`Instance "${body.name}" created`);
      closeModal();
      loadInstances();
    } catch (e) {
      toast(e.message, 'error');
      btn.disabled = false;
      btn.textContent = 'Create & start';
    }
  });
}

async function openGrantsModal(id, name) {
  await ensureUsers();
  let current = [];
  try {
    const list = await api('/panel/instances');
    current = (list.find((i) => i.id === id) || {}).user_ids || [];
  } catch { /* default empty */ }
  openModal(`<h3>Access — ${esc(name)}</h3>
    ${userChecklist(current)}
    <div class="modal-actions">
      <button type="button" class="btn" id="modal-cancel">Cancel</button>
      <button type="button" class="btn primary" id="grants-save">Save</button>
    </div>`);
  $('#modal-cancel').addEventListener('click', closeModal);
  $('#grants-save').addEventListener('click', async () => {
    try {
      await api(`/panel/instances/${id}/grants`, { method: 'POST', body: { user_ids: grantValues() } });
      toast('Access updated');
      closeModal();
      loadInstances();
    } catch (e) { toast(e.message, 'error'); }
  });
}

function openDeleteModal(id, name) {
  openModal(`<h3>Delete — ${esc(name)}</h3>
    <p>This removes the container. The data volume is kept unless you check below.</p>
    <label style="flex-direction:row;align-items:center;gap:8px">
      <input type="checkbox" id="del-data" style="width:auto"> Also delete the data volume (irreversible)
    </label>
    <div class="modal-actions">
      <button type="button" class="btn" id="modal-cancel">Cancel</button>
      <button type="button" class="btn danger" id="del-confirm">Delete instance</button>
    </div>`);
  $('#modal-cancel').addEventListener('click', closeModal);
  $('#del-confirm').addEventListener('click', async () => {
    try {
      await api(`/panel/instances/${id}/delete`, { method: 'POST', body: { delete_data: $('#del-data').checked } });
      toast(`Instance "${name}" deleted`);
      closeModal();
      loadInstances();
    } catch (e) { toast(e.message, 'error'); }
  });
}

// --------------------------------------------------------------- users --

async function loadUsers() {
  if (ME.role !== 'admin') return;
  let users, instances;
  try {
    [users, instances] = await Promise.all([api('/panel/users'), api('/panel/instances')]);
  } catch (e) { toast(e.message, 'error'); return; }
  USERS = users;
  const byUser = {};
  for (const i of instances) for (const uid of i.user_ids || []) (byUser[uid] = byUser[uid] || []).push(i.name);
  const tbody = $('#users-table tbody');
  tbody.innerHTML = users.map((u) => `<tr>
      <td>${esc(u.username)}</td>
      <td><span class="role-chip ${u.role}">${esc(u.role)}</span></td>
      <td>${u.role === 'admin' ? 'all' : esc((byUser[u.id] || []).join(', ') || '—')}</td>
      <td>${esc(u.created_at.slice(0, 10))}</td>
      <td>${u.id === ME.id ? '' : `<button class="btn small danger" data-del-user="${u.id}" data-name="${esc(u.username)}">${svg('trash')}</button>`}</td>
    </tr>`).join('');
  tbody.querySelectorAll('[data-del-user]').forEach((btn) => {
    btn.addEventListener('click', async () => {
      if (!confirm(`Delete user ${btn.dataset.name}? Their sessions and grants are removed.`)) return;
      try {
        await api(`/panel/users/${btn.dataset.delUser}/delete`, { method: 'POST' });
        toast('User deleted');
        loadUsers();
      } catch (e) { toast(e.message, 'error'); }
    });
  });
}

async function openNewUserModal() {
  let instances = [];
  try { instances = await api('/panel/instances'); } catch { /* empty */ }
  const checks = instances.length
    ? `<div class="check-list">${instances.map((i) =>
        `<label><input type="checkbox" name="uinst" value="${i.id}"> ${esc(i.name)}</label>`).join('')}</div>`
    : '<div class="hint">No instances yet — grant access later.</div>';
  openModal(`<h3>New user</h3>
    <form id="form-new-user">
      <div class="row">
        <label>Username <input id="nu-name" required autocomplete="off"></label>
        <label>Role <select id="nu-role"><option value="user" selected>user</option><option value="admin">admin</option></select></label>
      </div>
      <label>Password <input id="nu-pass" type="text" required minlength="8" autocomplete="off"></label>
      <div class="hint">Share it with the user out-of-band; they can change it after signing in.</div>
      <label>Instance access</label>
      ${checks}
      <div class="modal-actions">
        <button type="button" class="btn" id="modal-cancel">Cancel</button>
        <button type="submit" class="btn primary">Create user</button>
      </div>
    </form>`);
  $('#modal-cancel').addEventListener('click', closeModal);
  $('#form-new-user').addEventListener('submit', async (ev) => {
    ev.preventDefault();
    const body = {
      username: $('#nu-name').value.trim(),
      password: $('#nu-pass').value,
      role: $('#nu-role').value,
      instance_ids: [...document.querySelectorAll('input[name="uinst"]:checked')].map((c) => c.value),
    };
    try {
      await api('/panel/users', { method: 'POST', body });
      toast(`User "${body.username}" created`);
      closeModal();
      loadUsers();
    } catch (e) { toast(e.message, 'error'); }
  });
}

function openPasswordModal() {
  openModal(`<h3>Change password</h3>
    <form id="form-password">
      <label>Current password <input id="pw-current" type="password" required autocomplete="current-password"></label>
      <label>New password <input id="pw-new" type="password" required minlength="8" autocomplete="new-password"></label>
      <div class="modal-actions">
        <button type="button" class="btn" id="modal-cancel">Cancel</button>
        <button type="submit" class="btn primary">Change</button>
      </div>
    </form>`);
  $('#modal-cancel').addEventListener('click', closeModal);
  $('#form-password').addEventListener('submit', async (ev) => {
    ev.preventDefault();
    try {
      await api('/panel/password', { method: 'POST', body: { current: $('#pw-current').value, new: $('#pw-new').value } });
      toast('Password changed');
      closeModal();
    } catch (e) { toast(e.message, 'error'); }
  });
}

// -------------------------------------------------------------- wiring --

$('#login-form').addEventListener('submit', async (ev) => {
  ev.preventDefault();
  $('#login-error').classList.add('hidden');
  try {
    await api('/panel/login', {
      method: 'POST',
      body: { username: $('#login-user').value.trim(), password: $('#login-pass').value },
    });
    ME = await api('/panel/me');
    $('#login-pass').value = '';
    showApp();
  } catch (e) {
    const el = $('#login-error');
    el.textContent = e.message;
    el.classList.remove('hidden');
  }
});

document.querySelectorAll('.tab').forEach((t) =>
  t.addEventListener('click', () => switchTab(t.dataset.tab)));

$('#btn-logout').addEventListener('click', async () => {
  try { await api('/panel/logout', { method: 'POST' }); } catch { /* ignore */ }
  showLogin();
});
$('#btn-password').addEventListener('click', openPasswordModal);
$('#btn-new-instance').addEventListener('click', openNewInstanceModal);
$('#btn-new-user').addEventListener('click', openNewUserModal);
$('#logs-close').addEventListener('click', closeLogs);
$('#modal-overlay').addEventListener('click', (ev) => {
  if (ev.target === ev.currentTarget) closeModal();
});
document.addEventListener('keydown', (ev) => {
  if (ev.key === 'Escape') { closeModal(); closeLogs(); }
});

boot();
