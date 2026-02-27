import ReactMarkdown from 'react-markdown';
import remarkGfm from 'remark-gfm';
import { ComponentPropsWithoutRef, ReactNode, isValidElement, memo, useMemo } from 'react';
import { parseFileMentions } from '../utils/fileMentionParser';
import { FileMention } from './FileMention';
import { HighlightedCode } from './HighlightedCode';

interface MessageContentProps {
  content: string;
  type?: 'text' | 'code' | 'diff' | 'tool_output';
  language?: string;
  metadata?: {
    filename?: string;
    lineNumbers?: boolean;
  };
  /** When true the content is still being streamed — defers expensive highlighting. */
  isStreaming?: boolean;
}


function extractCodeText(value: ReactNode): string {
  if (typeof value === 'string') {
    return value;
  }
  if (typeof value === 'number') {
    return String(value);
  }
  if (Array.isArray(value)) {
    return value.map(extractCodeText).join('');
  }
  return '';
}

function extractLanguage(className: string | undefined): string | undefined {
  if (!className) {
    return undefined;
  }
  const match = className.match(/(?:^|\s)language-([^\s]+)/);
  return match?.[1];
}

/**
 * Detect whether the last fenced code block in the markdown is still unclosed.
 * Counts opening/closing fence delimiters (``` or ~~~); an odd count means the
 * last fence was never closed (content is still being streamed into it).
 * Exported for testing.
 */
export function isLastFenceUnclosed(markdown: string): boolean {
  const fences = markdown.match(/^(`{3,}|~{3,})/gm);
  return fences != null && fences.length % 2 === 1;
}

export const MessageContent = memo(function MessageContent({ content, type = 'text', isStreaming }: MessageContentProps) {
  // For now, we'll use markdown for text and pre-formatted code for others
  // In the future, we can integrate @pierre/diffs for better code/diff rendering
  
  if (type === 'text' || !type) {
    const mainContent = useMemo(() => content, [content]);

    // Parse content for file mentions
    const segments = parseFileMentions(mainContent);
    const hasMentions = segments.some(seg => seg.type === 'mention');

    // Per-block streaming detection: only the last code block can be
    // in-progress during streaming (tokens always append at the end).
    // All earlier blocks have their closing fence and can be highlighted.
    const lastBlockIncomplete = isStreaming && isLastFenceUnclosed(mainContent);
    const fences = mainContent.match(/^(`{3,}|~{3,})/gm);
    const totalCodeBlocks = Math.ceil((fences?.length ?? 0) / 2);
    let codeBlockIndex = 0;

    // Custom markdown components with better styling (no prose bloat)
    const markdownComponents = {
      // Code block container with syntax highlighting and long-line containment
      pre(props: ComponentPropsWithoutRef<'pre'>) {
        const { children } = props;

        if (isValidElement(children)) {
          const childProps = children.props as { className?: string; children?: ReactNode };
          const code = extractCodeText(childProps.children ?? '');

          if (code.length > 0) {
            const myIndex = codeBlockIndex++;
            const blockIsStreaming = lastBlockIncomplete && myIndex === totalCodeBlocks - 1;
            return (
              <div className="my-3 bg-surface-canvas/70 border border-surface-border rounded-md overflow-hidden max-w-full">
                <HighlightedCode
                  code={code}
                  language={extractLanguage(childProps.className)}
                  lineNumbers={false}
                  maxHeight="none"
                  isStreaming={blockIsStreaming}
                />
              </div>
            );
          }
        }

        return (
          <pre className="my-3 bg-surface-canvas/70 border border-surface-border rounded-md p-3 overflow-x-auto max-w-full font-mono text-sm">
            {children}
          </pre>
        );
      },
      // Inline code and fenced code (react-markdown wraps fenced code in <pre>)
      code(props: ComponentPropsWithoutRef<'code'>) {
        const { className, children } = props;
        const content = String(children ?? '');
        const isBlockCode = Boolean(className) || content.includes('\n');

        if (isBlockCode) {
          return <code className={className}>{children}</code>;
        }

        return (
          <code className="bg-surface-canvas/50 px-1.5 py-0.5 rounded text-accent-primary font-mono text-sm">
            {children}
          </code>
        );
      },
      // Links
      a(props: ComponentPropsWithoutRef<'a'>) {
        const { children, ...rest } = props;
        return (
          <a className="text-accent-primary hover:text-accent-secondary transition-colors underline" {...rest}>
            {children}
          </a>
        );
      },
      // Paragraphs - tighter spacing
      p(props: ComponentPropsWithoutRef<'p'>) {
        const { children, ...rest } = props;
        return (
          <p className="mb-2 text-sm leading-relaxed text-ui-primary" {...rest}>
            {children}
          </p>
        );
      },
      // Lists - tighter spacing
      ul(props: ComponentPropsWithoutRef<'ul'>) {
        const { children, ...rest } = props;
        return (
          <ul className="my-2 ml-4 space-y-1 list-disc text-sm text-ui-primary" {...rest}>
            {children}
          </ul>
        );
      },
      ol(props: ComponentPropsWithoutRef<'ol'>) {
        const { children, ...rest } = props;
        return (
          <ol className="my-2 ml-4 space-y-1 list-decimal text-sm text-ui-primary" {...rest}>
            {children}
          </ol>
        );
      },
      li(props: ComponentPropsWithoutRef<'li'>) {
        const { children, ...rest } = props;
        return (
          <li className="text-ui-primary" {...rest}>
            {children}
          </li>
        );
      },
      // Headings - smaller, tighter
      h1(props: ComponentPropsWithoutRef<'h1'>) {
        const { children, ...rest } = props;
        return (
          <h1 className="text-lg font-semibold mt-4 mb-2 text-accent-primary" {...rest}>
            {children}
          </h1>
        );
      },
      h2(props: ComponentPropsWithoutRef<'h2'>) {
        const { children, ...rest } = props;
        return (
          <h2 className="text-base font-semibold mt-3 mb-2 text-accent-primary" {...rest}>
            {children}
          </h2>
        );
      },
      h3(props: ComponentPropsWithoutRef<'h3'>) {
        const { children, ...rest } = props;
        return (
          <h3 className="text-sm font-semibold mt-2 mb-1 text-ui-secondary" {...rest}>
            {children}
          </h3>
        );
      },
      // Blockquotes
      blockquote(props: ComponentPropsWithoutRef<'blockquote'>) {
        const { children, ...rest } = props;
        return (
          <blockquote className="my-2 pl-4 border-l-2 border-accent-primary/50 text-ui-secondary italic text-sm" {...rest}>
            {children}
          </blockquote>
        );
      },
    };

    // If there are file mentions, render them specially
    if (hasMentions) {
      return (
        <div className="message-content">
          {segments.map((segment, index) => {
            if (segment.type === 'mention' && segment.mention) {
              return <FileMention key={index} mention={segment.mention} />;
            }
            // Render text segments as markdown
            return (
              <ReactMarkdown 
                key={index}
                remarkPlugins={[remarkGfm]}
                components={markdownComponents}
              >
                {segment.content}
              </ReactMarkdown>
            );
          })}
        </div>
      );
    }

    // No mentions, render normally
    return (
      <div className="message-content">
        <ReactMarkdown 
          remarkPlugins={[remarkGfm]}
          components={markdownComponents}
        >
          {mainContent}
        </ReactMarkdown>
      </div>
    );
  }
  
  if (type === 'code' || type === 'tool_output' || type === 'diff') {
    return (
      <div className="event-diff-container">
        <pre className="text-sm overflow-x-auto p-4">
          <code>{content}</code>
        </pre>
      </div>
    );
  }
  
  return <pre className="text-sm whitespace-pre-wrap break-words">{content}</pre>;
});
