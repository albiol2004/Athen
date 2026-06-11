// Memory panel: episodic memories (inline edit), knowledge-graph
// entities and relations.
//
// Shapes: commands.rs MemoryInfo / EntityInfo / RelationInfo;
// routes /memory, /entities, /relations in http_api.rs.

import { useState } from 'react';
import type { AthenClient } from '../api/client';
import {
  ConfirmButton,
  ErrorText,
  Loading,
  Section,
  useAction,
  useLoad,
} from './shared';

interface MemoryInfo {
  id: string;
  content: string;
  source: string;
  timestamp: string;
  memory_type: string;
}

interface EntityRelation {
  relation: string;
  target_name: string;
  direction: string;
}

interface EntityInfo {
  id: string;
  name: string;
  entity_type: string;
  relations: EntityRelation[];
}

interface RelationInfo {
  from_id: string;
  from_name: string;
  relation: string;
  to_id: string;
  to_name: string;
}

export function PanelMemory({ client }: { client: AthenClient }) {
  return (
    <>
      <MemoriesSection client={client} />
      <EntitiesSection client={client} />
      <RelationsSection client={client} />
    </>
  );
}

// ---------------------------------------------------------------------------
// Memories
// ---------------------------------------------------------------------------

function MemoriesSection({ client }: { client: AthenClient }) {
  const memories = useLoad(() => client.get<MemoryInfo[]>('/memory'), [client]);
  const act = useAction();
  const [editing, setEditing] = useState<{ id: string; content: string } | null>(null);

  const save = async () => {
    if (!editing) return;
    const ok = await act.run(() =>
      client.post(`/memory/${encodeURIComponent(editing.id)}`, { content: editing.content }),
    );
    if (ok) {
      setEditing(null);
      await memories.reload();
    }
  };

  const remove = async (id: string) => {
    const ok = await act.run(() => client.post(`/memory/${encodeURIComponent(id)}/delete`));
    if (ok) await memories.reload();
  };

  return (
    <Section title="Memories" hint="Episodic facts Athen recalls automatically when relevant.">
      {memories.loading && <Loading />}
      <ErrorText error={memories.error} />
      {!memories.loading && (memories.data ?? []).length === 0 && (
        <div className="st-dim">Nothing remembered yet.</div>
      )}
      <div className="st-list">
        {(memories.data ?? []).map((m) => (
          <div key={m.id} className="st-item">
            {editing?.id === m.id ? (
              <>
                <div className="st-item-main">
                  <textarea
                    rows={3}
                    style={{
                      width: '100%',
                      font: 'inherit',
                      color: 'var(--text)',
                      background: 'rgba(0,0,0,.25)',
                      border: '1px solid var(--glass-border)',
                      borderRadius: 9,
                      padding: '7px 10px',
                    }}
                    value={editing.content}
                    onChange={(e) => setEditing({ id: m.id, content: e.target.value })}
                  />
                </div>
                <div className="st-item-actions">
                  <button type="button" className="st-btn small primary" disabled={act.pending} onClick={() => void save()}>
                    Save
                  </button>
                  <button type="button" className="st-btn small" onClick={() => setEditing(null)}>
                    Cancel
                  </button>
                </div>
              </>
            ) : (
              <>
                <div className="st-item-main">
                  <div style={{ fontSize: 13, whiteSpace: 'pre-wrap' }}>{m.content}</div>
                  <div className="st-item-sub">
                    {m.memory_type} · {m.source}
                    {m.timestamp ? ` · ${new Date(m.timestamp).toLocaleString()}` : ''}
                  </div>
                </div>
                <div className="st-item-actions">
                  <button
                    type="button"
                    className="st-btn small"
                    onClick={() => setEditing({ id: m.id, content: m.content })}
                  >
                    Edit
                  </button>
                  <ConfirmButton label="Delete" className="small" onConfirm={() => void remove(m.id)} />
                </div>
              </>
            )}
          </div>
        ))}
      </div>
      <ErrorText error={act.error} />
    </Section>
  );
}

// ---------------------------------------------------------------------------
// Entities
// ---------------------------------------------------------------------------

function EntitiesSection({ client }: { client: AthenClient }) {
  const entities = useLoad(() => client.get<EntityInfo[]>('/entities'), [client]);
  const act = useAction();
  const [editing, setEditing] = useState<{ id: string; name: string; entity_type: string } | null>(null);

  const save = async () => {
    if (!editing) return;
    const ok = await act.run(() =>
      client.post(`/entities/${encodeURIComponent(editing.id)}`, {
        name: editing.name,
        entity_type: editing.entity_type,
      }),
    );
    if (ok) {
      setEditing(null);
      await entities.reload();
    }
  };

  const remove = async (id: string) => {
    const ok = await act.run(() => client.post(`/entities/${encodeURIComponent(id)}/delete`));
    if (ok) await entities.reload();
  };

  return (
    <Section title="Entities" hint="People, projects and things in the knowledge graph.">
      {entities.loading && <Loading />}
      <ErrorText error={entities.error} />
      {!entities.loading && (entities.data ?? []).length === 0 && (
        <div className="st-dim">No entities yet.</div>
      )}
      <div className="st-list">
        {(entities.data ?? []).map((e) => (
          <div key={e.id} className="st-item">
            {editing?.id === e.id ? (
              <>
                <div className="st-item-main st-row" style={{ alignItems: 'center' }}>
                  <input
                    type="text"
                    value={editing.name}
                    onChange={(ev) => setEditing({ ...editing, name: ev.target.value })}
                    style={{
                      font: 'inherit',
                      color: 'var(--text)',
                      background: 'rgba(0,0,0,.25)',
                      border: '1px solid var(--glass-border)',
                      borderRadius: 9,
                      padding: '6px 10px',
                      flex: 1,
                      minWidth: 120,
                    }}
                  />
                  <input
                    type="text"
                    value={editing.entity_type}
                    placeholder="type"
                    onChange={(ev) => setEditing({ ...editing, entity_type: ev.target.value })}
                    style={{
                      font: 'inherit',
                      color: 'var(--text)',
                      background: 'rgba(0,0,0,.25)',
                      border: '1px solid var(--glass-border)',
                      borderRadius: 9,
                      padding: '6px 10px',
                      width: 130,
                    }}
                  />
                </div>
                <div className="st-item-actions">
                  <button type="button" className="st-btn small primary" disabled={act.pending} onClick={() => void save()}>
                    Save
                  </button>
                  <button type="button" className="st-btn small" onClick={() => setEditing(null)}>
                    Cancel
                  </button>
                </div>
              </>
            ) : (
              <>
                <div className="st-item-main">
                  <div className="st-item-title">
                    {e.name}
                    <span className="st-badge">{e.entity_type}</span>
                  </div>
                  {e.relations.length > 0 && (
                    <div className="st-item-sub">
                      {e.relations
                        .slice(0, 4)
                        .map((r) =>
                          r.direction === 'out'
                            ? `${r.relation} → ${r.target_name}`
                            : `← ${r.relation} ${r.target_name}`,
                        )
                        .join(' · ')}
                      {e.relations.length > 4 ? ' …' : ''}
                    </div>
                  )}
                </div>
                <div className="st-item-actions">
                  <button
                    type="button"
                    className="st-btn small"
                    onClick={() => setEditing({ id: e.id, name: e.name, entity_type: e.entity_type })}
                  >
                    Edit
                  </button>
                  <ConfirmButton label="Delete" className="small" onConfirm={() => void remove(e.id)} />
                </div>
              </>
            )}
          </div>
        ))}
      </div>
      <ErrorText error={act.error} />
    </Section>
  );
}

// ---------------------------------------------------------------------------
// Relations
// ---------------------------------------------------------------------------

function RelationsSection({ client }: { client: AthenClient }) {
  const relations = useLoad(() => client.get<RelationInfo[]>('/relations'), [client]);
  const act = useAction();

  const remove = async (r: RelationInfo) => {
    const ok = await act.run(() =>
      client.post('/relations/delete', { from_id: r.from_id, to_id: r.to_id, relation: r.relation }),
    );
    if (ok) await relations.reload();
  };

  return (
    <Section title="Relations" hint="Edges between entities.">
      {relations.loading && <Loading />}
      <ErrorText error={relations.error} />
      {!relations.loading && (relations.data ?? []).length === 0 && (
        <div className="st-dim">No relations yet.</div>
      )}
      <div className="st-list">
        {(relations.data ?? []).map((r, i) => (
          <div key={`${r.from_id}-${r.relation}-${r.to_id}-${i}`} className="st-item">
            <div className="st-item-main">
              <div className="st-item-sub" style={{ color: 'var(--text)', fontSize: 13 }}>
                {r.from_name} <span style={{ color: 'var(--coral)' }}>—{r.relation}→</span> {r.to_name}
              </div>
            </div>
            <ConfirmButton label="Delete" className="small" onConfirm={() => void remove(r)} />
          </div>
        ))}
      </div>
      <ErrorText error={act.error} />
    </Section>
  );
}
