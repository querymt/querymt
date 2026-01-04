import ReactMarkdown from 'react-markdown';
import remarkGfm from 'remark-gfm';
import { ComponentPropsWithoutRef } from 'react';

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
    return (
      <div className="prose prose-invert prose-sm max-w-none">
        <ReactMarkdown 
          remarkPlugins={[remarkGfm]}
          components={{
            // Custom styling for code blocks
            code(props: ComponentPropsWithoutRef<'code'> & { inline?: boolean }) {
              const { inline, className, children, ...rest } = props;
              return !inline ? (
                <pre className="bg-cyber-bg/50 border border-cyber-border rounded-lg p-4 overflow-x-auto">
                  <code className={className} {...rest}>
                    {children}
                  </code>
                </pre>
              ) : (
                <code className="bg-cyber-bg/50 px-1.5 py-0.5 rounded text-cyber-cyan" {...rest}>
                  {children}
                </code>
              );
            },
            // Style links with neon cyan
            a(props: ComponentPropsWithoutRef<'a'>) {
              const { children, ...rest } = props;
              return (
                <a className="text-cyber-cyan hover:text-cyber-magenta transition-colors" {...rest}>
                  {children}
                </a>
              );
            },
          }}
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
