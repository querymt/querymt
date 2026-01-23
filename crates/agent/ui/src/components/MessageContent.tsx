import ReactMarkdown from 'react-markdown';
import remarkGfm from 'remark-gfm';
import { ComponentPropsWithoutRef } from 'react';
import { parseFileMentions } from '../utils/fileMentionParser';
import { FileMention } from './FileMention';

interface MessageContentProps {
  content: string;
  type?: 'text' | 'code' | 'diff' | 'tool_output';
  language?: string;
  metadata?: {
    filename?: string;
    lineNumbers?: boolean;
  };
}

export function MessageContent({ content, type = 'text' }: MessageContentProps) {
  // For now, we'll use markdown for text and pre-formatted code for others
  // In the future, we can integrate @pierre/diffs for better code/diff rendering
  
  if (type === 'text' || !type) {
    // Parse content for file mentions
    const segments = parseFileMentions(content);
    const hasMentions = segments.some(seg => seg.type === 'mention');

    // Custom markdown components with better styling (no prose bloat)
    const markdownComponents = {
      // Code blocks with syntax highlighting
      code(props: ComponentPropsWithoutRef<'code'> & { inline?: boolean }) {
        const { inline, className, children, ...rest } = props;
        return !inline ? (
          <pre className="my-3 bg-cyber-bg/70 border border-cyber-border rounded-md p-3 overflow-x-auto font-mono text-sm">
            <code className={className} {...rest}>
              {children}
            </code>
          </pre>
        ) : (
          <code className="bg-cyber-bg/50 px-1.5 py-0.5 rounded text-cyber-cyan font-mono text-sm" {...rest}>
            {children}
          </code>
        );
      },
      // Links
      a(props: ComponentPropsWithoutRef<'a'>) {
        const { children, ...rest } = props;
        return (
          <a className="text-cyber-cyan hover:text-cyber-magenta transition-colors underline" {...rest}>
            {children}
          </a>
        );
      },
      // Paragraphs - tighter spacing
      p(props: ComponentPropsWithoutRef<'p'>) {
        const { children, ...rest } = props;
        return (
          <p className="mb-2 text-sm leading-relaxed text-gray-200" {...rest}>
            {children}
          </p>
        );
      },
      // Lists - tighter spacing
      ul(props: ComponentPropsWithoutRef<'ul'>) {
        const { children, ...rest } = props;
        return (
          <ul className="my-2 ml-4 space-y-1 list-disc text-sm text-gray-200" {...rest}>
            {children}
          </ul>
        );
      },
      ol(props: ComponentPropsWithoutRef<'ol'>) {
        const { children, ...rest } = props;
        return (
          <ol className="my-2 ml-4 space-y-1 list-decimal text-sm text-gray-200" {...rest}>
            {children}
          </ol>
        );
      },
      li(props: ComponentPropsWithoutRef<'li'>) {
        const { children, ...rest } = props;
        return (
          <li className="text-gray-200" {...rest}>
            {children}
          </li>
        );
      },
      // Headings - smaller, tighter
      h1(props: ComponentPropsWithoutRef<'h1'>) {
        const { children, ...rest } = props;
        return (
          <h1 className="text-lg font-semibold mt-4 mb-2 text-cyber-cyan" {...rest}>
            {children}
          </h1>
        );
      },
      h2(props: ComponentPropsWithoutRef<'h2'>) {
        const { children, ...rest } = props;
        return (
          <h2 className="text-base font-semibold mt-3 mb-2 text-cyber-cyan" {...rest}>
            {children}
          </h2>
        );
      },
      h3(props: ComponentPropsWithoutRef<'h3'>) {
        const { children, ...rest } = props;
        return (
          <h3 className="text-sm font-semibold mt-2 mb-1 text-gray-300" {...rest}>
            {children}
          </h3>
        );
      },
      // Blockquotes
      blockquote(props: ComponentPropsWithoutRef<'blockquote'>) {
        const { children, ...rest } = props;
        return (
          <blockquote className="my-2 pl-4 border-l-2 border-cyber-cyan/50 text-gray-400 italic text-sm" {...rest}>
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
          {content}
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
}
