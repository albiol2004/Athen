// Contacts panel: full contact list with trust, identifiers, notes,
// block/unblock and delete.
//
// Response shape: contacts.rs ContactInfo. Trust strings come from
// trust_level_str / parse_trust_level: Unknown | Neutral | Known |
// Trusted | AuthUser (parse is case-insensitive).

import { useState } from 'react';
import type { AthenClient } from '../api/client';
import {
  ConfirmButton,
  ErrorText,
  Field,
  Loading,
  Section,
  useAction,
  useLoad,
} from './shared';

interface IdentifierInfo {
  value: string;
  kind: string;
}

interface ContactInfo {
  id: string;
  name: string;
  trust_level: string;
  trust_manual_override: boolean;
  identifiers: IdentifierInfo[];
  interaction_count: number;
  last_interaction: string | null;
  blocked: boolean;
  notes: string | null;
}

const TRUST_LEVELS = ['Unknown', 'Neutral', 'Known', 'Trusted', 'AuthUser'];
const IDENT_KINDS = ['email', 'phone', 'telegram_user', 'whatsapp', 'other'];

interface ContactForm {
  id: string | null;
  name: string;
  trust_level: string;
  identifiers: IdentifierInfo[];
  notes: string;
}

export function PanelContacts({ client }: { client: AthenClient }) {
  const contacts = useLoad(() => client.get<ContactInfo[]>('/contacts'), [client]);
  const act = useAction();
  const [form, setForm] = useState<ContactForm | null>(null);

  const open = (c: ContactInfo | null) => {
    setForm(
      c
        ? {
            id: c.id,
            name: c.name,
            trust_level: c.trust_level,
            identifiers: c.identifiers.map((i) => ({ ...i })),
            notes: c.notes ?? '',
          }
        : { id: null, name: '', trust_level: 'Neutral', identifiers: [], notes: '' },
    );
  };

  const save = async () => {
    if (!form) return;
    const identifiers = form.identifiers
      .filter((i) => i.value.trim())
      .map((i) => ({ value: i.value.trim(), kind: i.kind }));
    const ok = await act.run(() =>
      form.id
        ? client.post(`/contacts/${encodeURIComponent(form.id)}`, {
            name: form.name.trim(),
            trust_level: form.trust_level,
            identifiers,
            notes: form.notes || null,
          })
        : client.post('/contacts', {
            name: form.name.trim(),
            trust_level: form.trust_level,
            identifiers,
            notes: form.notes || null,
          }),
    );
    if (ok) {
      setForm(null);
      await contacts.reload();
    }
  };

  const quick = async (path: string) => {
    const ok = await act.run(() => client.post(path));
    if (ok) await contacts.reload();
  };

  const setTrust = async (c: ContactInfo, trust: string) => {
    const ok = await act.run(() =>
      client.post(`/contacts/${encodeURIComponent(c.id)}/trust`, { trust_level: trust }),
    );
    if (ok) await contacts.reload();
  };

  return (
    <Section
      title="Contacts"
      hint="Trust levels scale the risk gate: actions involving trusted contacts need less approval."
      actions={
        <button type="button" className="st-btn" onClick={() => open(null)}>
          + New contact
        </button>
      }
    >
      {contacts.loading && <Loading />}
      <ErrorText error={contacts.error} />
      {!contacts.loading && (contacts.data ?? []).length === 0 && (
        <div className="st-dim">No contacts yet.</div>
      )}
      <div className="st-list">
        {(contacts.data ?? []).map((c) => (
          <div key={c.id} className={`st-item${form?.id === c.id ? ' selected' : ''}`}>
            <div className="st-item-main">
              <div className="st-item-title">
                {c.name}
                {c.blocked && <span className="st-badge red">blocked</span>}
                {c.trust_level === 'AuthUser' && <span className="st-badge coral">owner</span>}
              </div>
              <div className="st-item-sub">
                {c.identifiers.map((i) => `${i.kind}: ${i.value}`).join(' · ') || 'no identifiers'}
                {c.interaction_count > 0 ? ` · ${c.interaction_count} interactions` : ''}
              </div>
              {c.notes && <div className="st-item-sub">{c.notes}</div>}
            </div>
            <div className="st-item-actions">
              <select
                value={c.trust_level}
                disabled={act.pending}
                onChange={(e) => void setTrust(c, e.target.value)}
                style={{
                  font: 'inherit',
                  fontSize: 12,
                  color: 'var(--text)',
                  background: 'rgba(0,0,0,.25)',
                  border: '1px solid var(--glass-border)',
                  borderRadius: 8,
                  padding: '4px 7px',
                }}
              >
                {TRUST_LEVELS.map((t) => (
                  <option key={t} value={t}>
                    {t}
                  </option>
                ))}
              </select>
              <button type="button" className="st-btn small" onClick={() => open(c)}>
                Edit
              </button>
              <button
                type="button"
                className="st-btn small"
                disabled={act.pending}
                onClick={() =>
                  void quick(`/contacts/${encodeURIComponent(c.id)}/${c.blocked ? 'unblock' : 'block'}`)
                }
              >
                {c.blocked ? 'Unblock' : 'Block'}
              </button>
              <ConfirmButton
                label="Delete"
                className="small"
                onConfirm={() => void quick(`/contacts/${encodeURIComponent(c.id)}/delete`)}
              />
            </div>
          </div>
        ))}
      </div>
      {form && (
        <>
          <hr className="st-divider" />
          <div className="st-row">
            <Field label="Name" grow>
              <input type="text" value={form.name} onChange={(e) => setForm({ ...form, name: e.target.value })} />
            </Field>
            <Field label="Trust level">
              <select
                value={form.trust_level}
                onChange={(e) => setForm({ ...form, trust_level: e.target.value })}
              >
                {TRUST_LEVELS.map((t) => (
                  <option key={t} value={t}>
                    {t}
                  </option>
                ))}
              </select>
            </Field>
          </div>
          {form.identifiers.map((ident, i) => (
            <div key={i} className="st-row">
              <Field label="Kind">
                <select
                  value={ident.kind}
                  onChange={(e) =>
                    setForm({
                      ...form,
                      identifiers: form.identifiers.map((x, j) =>
                        j === i ? { ...x, kind: e.target.value } : x,
                      ),
                    })
                  }
                >
                  {IDENT_KINDS.map((k) => (
                    <option key={k} value={k}>
                      {k}
                    </option>
                  ))}
                </select>
              </Field>
              <Field label="Value" grow>
                <input
                  type="text"
                  value={ident.value}
                  onChange={(e) =>
                    setForm({
                      ...form,
                      identifiers: form.identifiers.map((x, j) =>
                        j === i ? { ...x, value: e.target.value } : x,
                      ),
                    })
                  }
                />
              </Field>
              <button
                type="button"
                className="st-btn small"
                onClick={() =>
                  setForm({ ...form, identifiers: form.identifiers.filter((_, j) => j !== i) })
                }
              >
                Remove
              </button>
            </div>
          ))}
          <Field label="Notes" grow>
            <textarea
              rows={2}
              value={form.notes}
              onChange={(e) => setForm({ ...form, notes: e.target.value })}
            />
          </Field>
          <div className="st-row">
            <button
              type="button"
              className="st-btn small"
              onClick={() =>
                setForm({ ...form, identifiers: [...form.identifiers, { kind: 'email', value: '' }] })
              }
            >
              + Identifier
            </button>
            <button
              type="button"
              className="st-btn primary"
              disabled={act.pending || !form.name.trim()}
              onClick={() => void save()}
            >
              {form.id ? 'Save contact' : 'Create contact'}
            </button>
            <button type="button" className="st-btn" onClick={() => setForm(null)}>
              Cancel
            </button>
          </div>
        </>
      )}
      <ErrorText error={act.error} />
    </Section>
  );
}
