import { useEffect, useLayoutEffect, useRef, useState } from 'react';
import type {
  ApprovalChoice,
  GrantDecision,
  GrantRequest,
  PendingApproval,
} from '../api/types';
import type { ApprovalQuestion } from '../api/types';
import type { ChatItem } from '../chat/reducer';
import { GrantCard, QuestionCard, TaskCard, ThinkingBlock, ToolChip } from './cards';
import { Markdown } from './Markdown';

export interface ChatCallbacks {
  onSend: (text: string) => void;
  onCancel: () => void;
  onAnswerQuestion: (q: ApprovalQuestion, choice: ApprovalChoice) => Promise<void>;
  onDecideTask: (t: PendingApproval, approved: boolean) => Promise<void>;
  onDecideGrant: (g: GrantRequest, decision: GrantDecision, label: string) => Promise<void>;
}

function Item({ it, cb }: { it: ChatItem; cb: ChatCallbacks }) {
  switch (it.kind) {
    case 'msg':
      if (it.role === 'system') return <div className="msg system">{it.content}</div>;
      if (it.role === 'user') return <div className="msg user">{it.content}</div>;
      // Streaming text renders plain (cheap, no half-parsed markdown
      // flicker); the final render upgrades to markdown.
      return (
        <div className={`msg agent${it.streaming ? ' streaming' : ''}`}>
          {it.streaming ? it.content : <Markdown text={it.content} />}
        </div>
      );
    case 'thinking':
      return <ThinkingBlock content={it.content} done={it.done} />;
    case 'tool':
      return <ToolChip name={it.name} status={it.status} detail={it.detail} failed={it.failed} />;
    case 'question':
      return (
        <QuestionCard q={it.q} resolved={it.resolved} onAnswer={(c) => cb.onAnswerQuestion(it.q, c)} />
      );
    case 'task':
      return (
        <TaskCard t={it.t} resolved={it.resolved} onDecide={(a) => cb.onDecideTask(it.t, a)} />
      );
    case 'grant':
      return (
        <GrantCard
          g={it.g}
          resolved={it.resolved}
          onDecide={(d, label) => cb.onDecideGrant(it.g, d, label)}
        />
      );
  }
}

export function Chat({
  items,
  busy,
  arcKey,
  cb,
}: {
  items: ChatItem[];
  busy: boolean;
  /** Changes on arc switch — forces a scroll-to-bottom. */
  arcKey: string | null;
  cb: ChatCallbacks;
}) {
  const scrollRef = useRef<HTMLDivElement>(null);
  // Auto-scroll only when the user is already pinned near the bottom —
  // streaming deltas must never yank the viewport while reading back.
  const pinnedRef = useRef(true);
  const [text, setText] = useState('');
  const inputRef = useRef<HTMLTextAreaElement>(null);

  const onScroll = () => {
    const el = scrollRef.current;
    if (el) pinnedRef.current = el.scrollTop + el.clientHeight >= el.scrollHeight - 80;
  };

  useLayoutEffect(() => {
    const el = scrollRef.current;
    if (el && pinnedRef.current) el.scrollTop = el.scrollHeight;
  }, [items]);

  useEffect(() => {
    const el = scrollRef.current;
    if (el) {
      el.scrollTop = el.scrollHeight;
      pinnedRef.current = true;
    }
  }, [arcKey]);

  const submit = () => {
    const t = text.trim();
    if (!t) return;
    setText('');
    pinnedRef.current = true; // own message always follows to the bottom
    cb.onSend(t);
    inputRef.current?.focus();
  };

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
          {items.map((it) => (
            <Item key={it.id} it={it} cb={cb} />
          ))}
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
        <form
          className="composer"
          onSubmit={(e) => {
            e.preventDefault();
            submit();
          }}
        >
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
          />
          {busy && (
            <button type="button" className="stop" onClick={cb.onCancel} title="Stop the running turn">
              <svg width="12" height="12" viewBox="0 0 24 24" aria-hidden="true">
                <rect x="5" y="5" width="14" height="14" rx="2.5" fill="currentColor" />
              </svg>
            </button>
          )}
          <button type="submit" className="send" disabled={!text.trim()}>
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
