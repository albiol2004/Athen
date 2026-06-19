import { memo } from 'react';
import ReactMarkdown from 'react-markdown';
import remarkGfm from 'remark-gfm';

// react-markdown renders to React elements (no innerHTML), which is the
// whole point: agent output and web content snippets inside it never
// reach the DOM as raw HTML. Raw HTML in the markdown source is skipped
// by default — keep it that way.
//
// memo'd: a sealed message's `text` is a stable string, so finished
// bubbles parse exactly once instead of re-running the full remark /
// remark-gfm pipeline on every streaming token of a *later* message.
export const Markdown = memo(function Markdown({ text }: { text: string }) {
  return (
    <div className="md">
      <ReactMarkdown
        remarkPlugins={[remarkGfm]}
        components={{
          a: ({ node: _node, ...props }) => (
            <a {...props} target="_blank" rel="noreferrer noopener" />
          ),
        }}
      >
        {text}
      </ReactMarkdown>
    </div>
  );
});
