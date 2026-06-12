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
    disk: '<ellipse cx="12" cy="5" rx="9" ry="3"/><path d="M3 5v14c0 1.66 4 3 9 3s9-1.34 9-3V5"/><path d="M3 12c0 1.66 4 3 9 3s9-1.34 9-3"/>',
    chat: '<path d="M21 11.5a8.4 8.4 0 0 1-9 8.4 8.6 8.6 0 0 1-3.7-.84L3 20l1-4.9A8.4 8.4 0 0 1 3 11.5a8.4 8.4 0 0 1 9-8.4 8.4 8.4 0 0 1 9 8.4z"/>',
    app: '<rect x="3" y="3" width="18" height="18" rx="2"/><path d="M3 9h18M9 21V9"/>',
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

// Deep-link return-to: the gateway redirects unauthenticated browsers from
// /i/{id}/… to /?next=<path>, so a shared instance link survives the login
// round-trip. Only same-origin instance paths are honored (open-redirect
// guard); location.replace keeps the login page out of Back history.
function consumeNextParam() {
  const next = new URLSearchParams(location.search).get('next');
  return next && /^\/i\/[A-Za-z0-9-]+(\/.*)?$/.test(next) ? next : null;
}

function showApp() {
  const next = consumeNextParam();
  if (next) { location.replace(next); return; }
  $('#view-login').classList.add('hidden');
  $('#view-app').classList.remove('hidden');
  $('#who').textContent = `${ME.username} · ${ME.role}`;
  document.querySelectorAll('.admin-only')
    .forEach((el) => el.classList.toggle('hidden', ME.role !== 'admin'));
  // socket_rootless: true = rootless (fine), false = rootful (warn),
  // absent/null = probe failed or non-admin (stay quiet).
  $('#rootful-banner').classList.toggle(
    'hidden',
    !(ME.role === 'admin' && ME.socket_rootless === false),
  );
  switchTab('instances');
}

function switchTab(tab) {
  document.querySelectorAll('.tab').forEach((t) => t.classList.toggle('active', t.dataset.tab === tab));
  $('#tab-instances').classList.toggle('hidden', tab !== 'instances');
  $('#tab-users').classList.toggle('hidden', tab !== 'users');
  $('#tab-audit').classList.toggle('hidden', tab !== 'audit');
  stopRefresh();
  if (tab === 'instances') {
    loadInstances();
    refreshTimer = setInterval(loadInstances, 5000);
  } else if (tab === 'users') {
    loadUsers();
  } else {
    loadAudit();
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
    btn.addEventListener('click', () => instanceAction(btn.dataset.action, btn.dataset.id, btn.dataset.name, btn.dataset));
  });
}

function instanceCard(i) {
  const admin = ME.role === 'admin';
  const running = i.state === 'running';
  const actions = [];
  actions.push(`<a class="btn small primary" href="/i/${i.id}/">${svg('app')} Open UI</a>`);
  actions.push(`<a class="btn small" href="/i/${i.id}/chat">${svg('chat')} Chat</a>`);
  if (admin) {
    actions.push(running
      ? `<button class="btn small" data-action="stop" data-id="${i.id}" data-name="${esc(i.name)}">${svg('stop')} Stop</button>`
      : `<button class="btn small" data-action="start" data-id="${i.id}" data-name="${esc(i.name)}">${svg('play')} Start</button>`);
    actions.push(`<button class="btn small" data-action="logs" data-id="${i.id}" data-name="${esc(i.name)}">${svg('logs')} Logs</button>`);
    actions.push(`<button class="btn small" data-action="grants" data-id="${i.id}" data-name="${esc(i.name)}">${svg('users')} Access</button>`);
    actions.push(`<button class="btn small" data-action="disk" data-id="${i.id}" data-name="${esc(i.name)}" data-limit="${i.disk_limit_mb ?? ''}">${svg('disk')} Quota</button>`);
    actions.push(`<button class="btn small danger" data-action="delete" data-id="${i.id}" data-name="${esc(i.name)}">${svg('trash')}</button>`);
  }
  // Operator metadata (container name, raw Docker status, quota chips) is
  // admin-only: plain users get a clean "my instance" card that doesn't
  // leak infra naming or quota policy.
  const meta = admin
    ? `<div class="card-meta">
      <span><code>${esc(i.container_name)}</code></span>
      <span>${esc(i.status)}</span>
      ${i.memory_mb || i.cpus
        ? `<span>${[i.memory_mb && `${i.memory_mb} MB`, i.cpus && `${i.cpus} CPU`].filter(Boolean).join(' · ')}</span>`
        : ''}
      ${diskMeta(i)}
    </div>`
    : '';
  return `<div class="card">
    <div class="card-top">
      <span class="card-name">${esc(i.name)}</span>
      <span class="badge ${esc(i.state)}">${esc(i.state)}</span>
    </div>
    ${meta}
    <div class="card-actions">${actions.join('')}</div>
  </div>`;
}

// Disk usage chip: "disk 312 MB" / "disk 312 / 1024 MB"; red when over
// quota. Usage comes from the panel's periodic docker-df sweep, so it's
// absent for the first few minutes after a panel restart.
function diskMeta(i) {
  if (i.disk_used_mb == null && !i.disk_limit_mb) return '';
  const used = i.disk_used_mb != null ? `${i.disk_used_mb}` : '?';
  const limit = i.disk_limit_mb ? ` / ${i.disk_limit_mb}` : '';
  const over = i.disk_limit_mb && i.disk_used_mb != null && i.disk_used_mb > i.disk_limit_mb;
  return `<span${over ? ' class="disk-over"' : ''}>disk ${used}${limit} MB</span>`;
}

async function instanceAction(action, id, name, data = {}) {
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
  } else if (action === 'disk') {
    openDiskModal(id, name, data.limit);
  } else if (action === 'delete') {
    openDeleteModal(id, name);
  }
}

// Quota edit — also the way out of an enforced quota stop: raise the
// limit (or clear it), then Start the instance again.
function openDiskModal(id, name, current) {
  openModal(`<h3>Disk quota — ${esc(name)}</h3>
    <form id="form-disk">
      <label>Limit (MB) <input id="dq-mb" type="number" min="64" step="64" value="${esc(current || '')}" placeholder="no quota"></label>
      <div class="hint">Crossing the limit warns; still over a sweep later, the
      instance is stopped. Leave empty to remove the quota.</div>
      <div class="modal-actions">
        <button type="button" class="btn" id="modal-cancel">Cancel</button>
        <button type="submit" class="btn primary">Save</button>
      </div>
    </form>`);
  $('#modal-cancel').addEventListener('click', closeModal);
  $('#form-disk').addEventListener('submit', async (ev) => {
    ev.preventDefault();
    const v = $('#dq-mb').value.trim();
    try {
      await api(`/panel/instances/${id}/disk_limit`, {
        method: 'POST',
        body: { disk_limit_mb: v ? Number(v) : null },
      });
      toast(v ? `Quota set: ${v} MB` : 'Quota removed');
      closeModal();
      loadInstances();
    } catch (e) { toast(e.message, 'error'); }
  });
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

// --------------------------------------------------------- presets cache --

let PRESETS = null; // loaded once per page

async function ensurePresets() {
  if (PRESETS) return PRESETS;
  try { PRESETS = await api('/panel/instance_presets'); } catch { PRESETS = []; }
  return PRESETS;
}

// -------------------------------------------------------- new-instance modal --

function buildPresetOptions(presets) {
  return presets.map((p) =>
    `<option value="${esc(p.id)}" data-slug="${esc(p.default_slug)}" data-family="${esc(p.family)}" data-ctx="${esc(p.context_window_tokens)}" data-key-page="${esc(p.key_page_url)}" data-custom="${p.custom}">${esc(p.label)}</option>`
  ).join('');
}

async function openNewInstanceModal() {
  await ensureUsers();
  const presets = await ensurePresets();

  openModal(`<h3>New instance</h3>
    <form id="form-new-instance">
      <label>Name <input id="ni-name" required placeholder="alice"></label>

      <fieldset id="ni-provider-section">
        <legend>Model provider</legend>
        <label>Provider
          <select id="ni-preset">
            <option value="" data-custom="false">— none / configure later —</option>
            ${buildPresetOptions(presets)}
          </select>
        </label>
        <div id="ni-provider-fields" class="hidden">
          <div id="ni-custom-fields" class="hidden">
            <label>Provider ID <input id="ni-provider-id" placeholder="e.g. deepseek"></label>
            <label>Family <input id="ni-family" placeholder="e.g. DeepSeekV4Chat"></label>
          </div>
          <label>Model slug <input id="ni-slug" placeholder="e.g. deepseek-v4-flash"></label>
          <label>API key <input id="ni-apikey" type="password" autocomplete="off" placeholder="sk-..."></label>
          <div id="ni-key-hint" class="hint hidden"></div>
          <label>Context window (tokens) <input id="ni-ctx" type="number" min="1024" step="1024" placeholder="128000"></label>
        </div>
        <div id="ni-provider-none-hint" class="hint">Select a provider to pre-configure the instance's LLM routing. You can also leave this blank and configure it later from the instance's Settings page.</div>
      </fieldset>

      <label>Extra environment variables <textarea id="ni-env" placeholder="ATHEN_TELEGRAM_BOT_TOKEN=...&#10;ATHEN_IMAP_PASSWORD=..."></textarea></label>
      <div class="hint">One KEY=VALUE per line. The API key above is injected automatically — don't duplicate it here.</div>
      <div class="row">
        <label>Memory limit (MB) <input id="ni-mem" type="number" min="64" step="64" placeholder="unlimited"></label>
        <label>CPU limit (cores) <input id="ni-cpus" type="number" min="0.1" step="0.1" placeholder="unlimited"></label>
        <label>Disk quota (MB) <input id="ni-disk" type="number" min="64" step="64" placeholder="no quota"></label>
      </div>
      <div class="hint">Memory/CPU are hard cgroup limits — a runaway instance gets OOM-killed and restarted instead of starving the host. Disk is sweep-enforced: crossing the quota warns (audit + push); still over a sweep later, the instance is stopped until cleaned up or the quota is raised.</div>
      <details id="ni-advanced">
        <summary>Advanced: raw config files</summary>
        <div class="hint">Hand-written TOML seeds — use only when the provider form above can't express what you need. If you fill in a raw <code>models.toml</code> here, leave the provider form above blank (they are mutually exclusive).</div>
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

  // ── Preset dropdown wiring ─────────────────────────────────────────────
  const presetSel = $('#ni-preset');
  const providerFields = $('#ni-provider-fields');
  const customFields = $('#ni-custom-fields');
  const noneHint = $('#ni-provider-none-hint');
  const keyHint = $('#ni-key-hint');

  function applyPreset() {
    const opt = presetSel.options[presetSel.selectedIndex];
    const isNone = !opt.value && opt.dataset.custom !== 'true';
    const isCustom = opt.dataset.custom === 'true';

    providerFields.classList.toggle('hidden', isNone);
    customFields.classList.toggle('hidden', !isCustom);
    noneHint.classList.toggle('hidden', !isNone);

    if (!isNone && !isCustom) {
      // Pre-fill from preset data attributes.
      $('#ni-slug').value = opt.dataset.slug || '';
      $('#ni-ctx').value = opt.dataset.ctx || '128000';
      const kp = opt.dataset.keyPage;
      if (kp) {
        keyHint.textContent = `Get your API key at ${kp}`;
        keyHint.classList.remove('hidden');
      } else {
        keyHint.classList.add('hidden');
      }
    } else if (isCustom) {
      $('#ni-slug').value = '';
      $('#ni-ctx').value = '128000';
      keyHint.classList.add('hidden');
    }
  }

  presetSel.addEventListener('change', applyPreset);
  applyPreset(); // run once on open

  // ── Submit ─────────────────────────────────────────────────────────────
  $('#form-new-instance').addEventListener('submit', async (ev) => {
    ev.preventDefault();

    const rawModels = $('#ni-models').value.trim();
    const presetOpt = presetSel.options[presetSel.selectedIndex];
    const isCustom = presetOpt.dataset.custom === 'true';
    const providerIdValue = isCustom
      ? $('#ni-provider-id').value.trim()
      : presetOpt.value;

    // Ambiguity guard: provider form + raw models_toml are mutually exclusive.
    if (providerIdValue && rawModels) {
      toast('Cannot use both the provider form and a raw models.toml — clear one of them.', 'error');
      return;
    }

    const env = {};
    for (const line of $('#ni-env').value.split('\n')) {
      const t = line.trim();
      if (!t || t.startsWith('#')) continue;
      const eq = t.indexOf('=');
      if (eq <= 0) { toast(`bad env line: ${t}`, 'error'); return; }
      env[t.slice(0, eq).trim()] = t.slice(eq + 1).trim();
    }

    // Build llm_seed only when the provider form was filled in.
    let llm_seed = null;
    if (providerIdValue) {
      const familyValue = isCustom
        ? $('#ni-family').value.trim()
        : presetOpt.dataset.family;
      llm_seed = {
        provider_id: providerIdValue,
        slug: $('#ni-slug').value.trim(),
        api_key: $('#ni-apikey').value,
        family: familyValue || 'Default',
        context_window_tokens: $('#ni-ctx').value ? Number($('#ni-ctx').value) : null,
      };
    }

    const body = {
      name: $('#ni-name').value.trim(),
      env,
      config_toml: $('#ni-config').value.trim() || null,
      models_toml: rawModels || null,
      llm_seed,
      user_ids: grantValues(),
      memory_mb: $('#ni-mem').value ? Number($('#ni-mem').value) : null,
      cpus: $('#ni-cpus').value ? Number($('#ni-cpus').value) : null,
      disk_limit_mb: $('#ni-disk').value ? Number($('#ni-disk').value) : null,
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
      <td>
        <button class="btn small" data-role-user="${u.id}" data-name="${esc(u.username)}" data-to="${u.role === 'admin' ? 'user' : 'admin'}">${u.role === 'admin' ? 'Demote' : 'Make admin'}</button>
        ${u.id === ME.id ? '' : `<button class="btn small danger" data-del-user="${u.id}" data-name="${esc(u.username)}">${svg('trash')}</button>`}
      </td>
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
  tbody.querySelectorAll('[data-role-user]').forEach((btn) => {
    btn.addEventListener('click', async () => {
      const { name, to, roleUser } = btn.dataset;
      const warning = roleUser === ME.id && to === 'user'
        ? `Demote YOURSELF (${name}) to user? You lose panel admin rights immediately.`
        : `Change ${name}'s role to ${to}?`;
      if (!confirm(warning)) return;
      try {
        await api(`/panel/users/${roleUser}/role`, { method: 'POST', body: { role: to } });
        toast(`${name} is now ${to}`);
        // Self-demotion: the next API call already sees the new role.
        if (roleUser === ME.id) location.reload();
        else loadUsers();
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

// --------------------------------------------------------------- audit --

async function loadAudit() {
  if (ME.role !== 'admin') return;
  let rows;
  try { rows = await api('/panel/audit?limit=300'); } catch (e) { toast(e.message, 'error'); return; }
  $('#audit-table tbody').innerHTML = rows.map((r) => `<tr>
      <td>${esc(r.at.slice(0, 19).replace('T', ' '))}</td>
      <td>${esc(r.username)}</td>
      <td><code>${esc(r.action)}</code></td>
      <td>${esc(r.target || '—')}</td>
      <td>${esc(r.detail || '')}</td>
    </tr>`).join('');
}

// ------------------------------------------------------- notifications --

function openNotifyModal() {
  openModal(`<h3>Push notifications</h3>
    <p>When an instance you have access to needs an approval (or raises an
    urgent notification), the panel POSTs it to this webhook — works out
    of the box with an <a href="https://ntfy.sh" target="_blank" rel="noreferrer">ntfy</a>
    topic on your phone.</p>
    <form id="form-notify">
      <label>Webhook URL <input id="nf-url" type="url" placeholder="https://ntfy.sh/my-secret-topic" value="${esc(ME.notify_url || '')}"></label>
      <div class="hint">Pick an unguessable topic name — anyone who knows it can read your pushes. Leave empty to disable.</div>
      <div class="modal-actions">
        <button type="button" class="btn" id="modal-cancel">Cancel</button>
        <button type="submit" class="btn primary">Save</button>
      </div>
    </form>`);
  $('#modal-cancel').addEventListener('click', closeModal);
  $('#form-notify').addEventListener('submit', async (ev) => {
    ev.preventDefault();
    const url = $('#nf-url').value.trim();
    try {
      await api('/panel/notify', { method: 'POST', body: { url } });
      ME.notify_url = url;
      toast(url ? 'Notifications enabled' : 'Notifications disabled');
      closeModal();
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
$('#btn-notify').addEventListener('click', openNotifyModal);
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
