import { useEffect, useLayoutEffect, useRef, useState } from 'react';
import type { AthenClient } from '../api/client';
import type {
  ApprovalChoice,
  ApprovalQuestion,
  GrantDecision,
  GrantRequest,
  PendingApproval,
} from '../api/types';
import type { ChatItem } from '../chat/reducer';
import { GrantCard, QuestionCard, TaskCard, ThinkingBlock } from './cards';
import { Markdown } from './Markdown';
import { PlanCard, type PlanState } from './PlanGoal';
import { ToolCard, ToolGroup } from './toolcards';

export interface OutgoingImage {
  mime_type: string;
  base64: string;
  name: string;
}
export interface OutgoingFile {
  name: string;
  mime_type: string;
  base64: string;
}

export interface ChatCallbacks {
  onSend: (text: string, images: OutgoingImage[], files: OutgoingFile[]) => void;
  onCancel: () => void;
  onAnswerQuestion: (q: ApprovalQuestion, choice: ApprovalChoice) => Promise<void>;
  onDecideTask: (t: PendingApproval, approved: boolean) => Promise<void>;
  onDecideGrant: (g: GrantRequest, decision: GrantDecision, label: string) => Promise<void>;
  onApprovePlan: () => Promise<void>;
  onDiscardPlan: () => Promise<void>;
}

type ToolItem = Extract<ChatItem, { kind: 'tool' }>;
type RenderUnit = { kind: 'group'; key: string; tools: ToolItem[] } | { kind: 'item'; item: ChatItem };

/** Consecutive tool items collapse into one group (desktop rule). */
function toRenderUnits(items: ChatItem[]): RenderUnit[] {
  const units: RenderUnit[] = [];
  let buf: ToolItem[] = [];
  const flush = () => {
    if (buf.length === 1) units.push({ kind: 'item', item: buf[0] });
    else if (buf.length > 1) units.push({ kind: 'group', key: buf[0].key, tools: buf });
    buf = [];
  };
  for (const it of items) {
    if (it.kind === 'tool') buf.push(it);
    else {
      flush();
      units.push({ kind: 'item', item: it });
    }
  }
  flush();
  return units;
}

function Item({ it, cb, client }: { it: ChatItem; cb: ChatCallbacks; client: AthenClient }) {
  switch (it.kind) {
    case 'msg':
      if (it.role === 'system') return <div className="msg system">{it.content}</div>;
      if (it.role === 'user') return <div className="msg user">{it.content}</div>;
      return (
        <div className={`msg agent${it.streaming ? ' streaming' : ''}`}>
          {it.streaming ? it.content : <Markdown text={it.content} />}
        </div>
      );
    case 'thinking':
      return <ThinkingBlock content={it.content} done={it.done} />;
    case 'tool':
      return <ToolCard it={it} client={client} />;
    case 'question':
      return <QuestionCard q={it.q} resolved={it.resolved} onAnswer={(c) => cb.onAnswerQuestion(it.q, c)} />;
    case 'task':
      return <TaskCard t={it.t} resolved={it.resolved} onDecide={(a) => cb.onDecideTask(it.t, a)} />;
    case 'grant':
      return <GrantCard g={it.g} resolved={it.resolved} onDecide={(d, l) => cb.onDecideGrant(it.g, d, l)} />;
  }
}

const IMAGE_TYPES = ['image/png', 'image/jpeg', 'image/webp', 'image/gif'];

async function fileToB64(f: File): Promise<string> {
  return new Promise((resolve, reject) => {
    const r = new FileReader();
    r.onload = () => resolve(String(r.result).split(',')[1] ?? '');
    r.onerror = () => reject(new Error('read failed'));
    r.readAsDataURL(f);
  });
}

export function Chat({
  items,
  busy,
  arcKey,
  plan,
  client,
  cb,
}: {
  items: ChatItem[];
  busy: boolean;
  arcKey: string | null;
  plan: PlanState | null;
  client: AthenClient;
  cb: ChatCallbacks;
}) {
  const scrollRef = useRef<HTMLDivElement>(null);
  const pinnedRef = useRef(true);
  const [text, setText] = useState('');
  const [images, setImages] = useState<OutgoingImage[]>([]);
  const [files, setFiles] = useState<OutgoingFile[]>([]);
  const inputRef = useRef<HTMLTextAreaElement>(null);
  const pickerRef = useRef<HTMLInputElement>(null);
  const [dragging, setDragging] = useState(false);

  const onScroll = () => {
    const el = scrollRef.current;
    if (el) pinnedRef.current = el.scrollTop + el.clientHeight >= el.scrollHeight - 80;
  };
  useLayoutEffect(() => {
    const el = scrollRef.current;
    if (el && pinnedRef.current) el.scrollTop = el.scrollHeight;
  }, [items, plan]);
  useEffect(() => {
    const el = scrollRef.current;
    if (el) {
      el.scrollTop = el.scrollHeight;
      pinnedRef.current = true;
    }
  }, [arcKey]);

  const addFiles = async (list: Iterable<File>) => {
    for (const f of list) {
      try {
        if (IMAGE_TYPES.includes(f.type)) {
          if (images.length >= 10) continue;
          const b64 = await fileToB64(f);
          setImages((cur) => (cur.length >= 10 ? cur : [...cur, { mime_type: f.type, base64: b64, name: f.name }]));
        } else if (files.length < 5) {
          const b64 = await fileToB64(f);
          setFiles((cur) =>
            cur.length >= 5
              ? cur
              : [...cur, { name: f.name, mime_type: f.type || 'text/plain', base64: b64 }],
          );
        }
      } catch {
        /* unreadable file — skip */
      }
    }
  };

  const submit = () => {
    const t = text.trim();
    if (!t && images.length === 0 && files.length === 0) return;
    setText('');
    const imgs = images;
    const fls = files;
    setImages([]);
    setFiles([]);
    pinnedRef.current = true;
    cb.onSend(t, imgs, fls);
    inputRef.current?.focus();
  };

  const units = toRenderUnits(items);

  return (
    <div className="chat">
      <div className="chat-scroll" ref={scrollRef} onScroll={onScroll}>
        <div className="chat-col">
          {items.length === 0 && (
            <div className="chat-welcome">
              <h2>
                What can I do for <em>you</em>?
              </h2>
              <p>Athen is watching its senses and ready for direct tasks.</p>
            </div>
          )}
          {units.map((u) =>
            u.kind === 'group' ? (
              <ToolGroup key={`g-${u.key}`} items={u.tools} client={client} />
            ) : (
              <Item key={u.item.id} it={u.item} cb={cb} client={client} />
            ),
          )}
          {plan && (plan.status ?? 'Drafting') === 'Drafting' && (
            <PlanCard plan={plan} onApprove={cb.onApprovePlan} onDiscard={cb.onDiscardPlan} />
          )}
          {busy && (
            <div className="busy-row">
              <span className="busy-dot" />
              <span className="busy-dot" />
              <span className="busy-dot" />
            </div>
          )}
        </div>
      </div>
      <div className="composer-wrap">
        {(images.length > 0 || files.length > 0) && (
          <div className="attach-chips">
            {images.map((im, i) => (
              <span className="attach-chip" key={`i${i}`}>
                <img src={`data:${im.mime_type};base64,${im.base64}`} alt="" />
                {im.name}
                <button onClick={() => setImages((cur) => cur.filter((_, j) => j !== i))}>×</button>
              </span>
            ))}
            {files.map((f, i) => (
              <span className="attach-chip" key={`f${i}`}>
                <svg width="11" height="11" viewBox="0 0 24 24" fill="none" aria-hidden="true">
                  <path
                    d="M14 3H7a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2V8l-5-5Zm0 0v5h5"
                    stroke="currentColor"
                    strokeWidth="1.8"
                    strokeLinejoin="round"
                  />
                </svg>
                {f.name}
                <button onClick={() => setFiles((cur) => cur.filter((_, j) => j !== i))}>×</button>
              </span>
            ))}
          </div>
        )}
        <form
          className={`composer${dragging ? ' dragover' : ''}`}
          onSubmit={(e) => {
            e.preventDefault();
            submit();
          }}
          onDragOver={(e) => {
            e.preventDefault();
            setDragging(true);
          }}
          onDragLeave={() => setDragging(false)}
          onDrop={(e) => {
            e.preventDefault();
            setDragging(false);
            void addFiles(e.dataTransfer.files);
          }}
        >
          <button
            type="button"
            className="attach-btn"
            title="Attach images or files"
            onClick={() => pickerRef.current?.click()}
          >
            <svg width="15" height="15" viewBox="0 0 24 24" fill="none" aria-hidden="true">
              <path
                d="M21 12.5 12.9 20.6a5.4 5.4 0 0 1-7.6-7.6l8.4-8.4a3.6 3.6 0 0 1 5.1 5.1l-8.2 8.2a1.8 1.8 0 0 1-2.5-2.5l7.4-7.4"
                stroke="currentColor"
                strokeWidth="1.7"
                strokeLinecap="round"
              />
            </svg>
          </button>
          <input
            ref={pickerRef}
            type="file"
            multiple
            hidden
            accept="image/*,.txt,.md,.json,.csv,.pdf,.log,.toml,.yaml,.yml"
            onChange={(e) => {
              if (e.target.files) void addFiles(e.target.files);
              e.target.value = '';
            }}
          />
          <textarea
            ref={inputRef}
            rows={1}
            placeholder={busy ? 'Message (queued for the running turn)…' : 'Message Athen…'}
            value={text}
            autoFocus
            onChange={(e) => {
              setText(e.target.value);
              const el = e.target;
              el.style.height = 'auto';
              el.style.height = `${Math.min(el.scrollHeight, 160)}px`;
            }}
            onKeyDown={(e) => {
              if (e.key === 'Enter' && !e.shiftKey) {
                e.preventDefault();
                submit();
              }
            }}
            onPaste={(e) => {
              const fs = [...e.clipboardData.files];
              if (fs.length) {
                e.preventDefault();
                void addFiles(fs);
              }
            }}
          />
          {busy && (
            <button type="button" className="stop" onClick={cb.onCancel} title="Stop the running turn">
              <svg width="12" height="12" viewBox="0 0 24 24" aria-hidden="true">
                <rect x="5" y="5" width="14" height="14" rx="2.5" fill="currentColor" />
              </svg>
            </button>
          )}
          <button type="submit" className="send" disabled={!text.trim() && !images.length && !files.length}>
            <svg width="15" height="15" viewBox="0 0 24 24" fill="none" aria-hidden="true">
              <path
                d="M5 12h13M13 6l6 6-6 6"
                stroke="currentColor"
                strokeWidth="2.2"
                strokeLinecap="round"
                strokeLinejoin="round"
              />
            </svg>
          </button>
        </form>
      </div>
    </div>
  );
}
