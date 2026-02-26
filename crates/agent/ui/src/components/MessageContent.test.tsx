import { describe, expect, it, vi } from 'vitest';
import { render, screen } from '@testing-library/react';
import { MessageContent, isLastFenceUnclosed } from './MessageContent';

vi.mock('./HighlightedCode', () => ({
  HighlightedCode: ({ code, language, isStreaming }: { code: string; language?: string; isStreaming?: boolean }) => (
    <div
      data-testid="highlighted-code"
      data-language={language ?? ''}
      data-streaming={isStreaming ? 'true' : 'false'}
    >
      {code}
    </div>
  ),
}));

describe('MessageContent', () => {
  it('renders fenced code blocks without language via the highlighter', () => {
    const longLine = 'x'.repeat(400);
    const markdown = ['```', longLine, '```'].join('\n');
    render(<MessageContent content={markdown} />);

    const highlighted = screen.getByTestId('highlighted-code');
    expect(highlighted).toHaveTextContent(longLine);
    expect(highlighted).toHaveAttribute('data-language', '');
  });

  it('passes fenced block language to the highlighter', () => {
    const markdown = ['```ts', 'const value = 1;', '```'].join('\n');
    const { container } = render(<MessageContent content={markdown} />);

    const highlighted = screen.getByTestId('highlighted-code');
    expect(highlighted).toHaveAttribute('data-language', 'ts');
    expect(container.querySelectorAll('[data-testid="highlighted-code"]')).toHaveLength(1);
  });

  it('keeps inline code styled as inline and does not wrap it in pre', () => {
    const { container } = render(<MessageContent content={'Use `querymt` here.'} />);

    expect(container.querySelectorAll('pre')).toHaveLength(0);
    expect(container.querySelector('code')).toHaveClass('bg-surface-canvas/50');
  });

  // --- Per-block streaming tests ---

  it('marks the only code block as streaming when fence is unclosed', () => {
    const markdown = 'Hello\n```python\nprint("hi';
    render(<MessageContent content={markdown} isStreaming />);

    const block = screen.getByTestId('highlighted-code');
    expect(block).toHaveAttribute('data-streaming', 'true');
  });

  it('does not mark a completed code block as streaming even when isStreaming is true', () => {
    const markdown = '```python\nprint("hi")\n```\n\nMore text coming...';
    render(<MessageContent content={markdown} isStreaming />);

    const block = screen.getByTestId('highlighted-code');
    expect(block).toHaveAttribute('data-streaming', 'false');
  });

  it('highlights completed first block while last unclosed block is streaming', () => {
    const markdown = [
      '```python',
      'print("done")',
      '```',
      '',
      'Some prose.',
      '',
      '```js',
      'console.lo',
    ].join('\n');
    render(<MessageContent content={markdown} isStreaming />);

    const blocks = screen.getAllByTestId('highlighted-code');
    expect(blocks).toHaveLength(2);
    // First block (python) — completed, should be highlighted (not streaming)
    expect(blocks[0]).toHaveAttribute('data-streaming', 'false');
    expect(blocks[0]).toHaveAttribute('data-language', 'python');
    // Second block (js) — unclosed, should defer highlighting
    expect(blocks[1]).toHaveAttribute('data-streaming', 'true');
    expect(blocks[1]).toHaveAttribute('data-language', 'js');
  });

  it('highlights all blocks when stream is complete', () => {
    const markdown = [
      '```python',
      'print("done")',
      '```',
      '',
      '```js',
      'console.log("done")',
      '```',
    ].join('\n');
    // isStreaming is false (default)
    render(<MessageContent content={markdown} />);

    const blocks = screen.getAllByTestId('highlighted-code');
    expect(blocks).toHaveLength(2);
    expect(blocks[0]).toHaveAttribute('data-streaming', 'false');
    expect(blocks[1]).toHaveAttribute('data-streaming', 'false');
  });

  it('handles tilde fences the same as backtick fences', () => {
    const markdown = '~~~rust\nlet x = 1';
    render(<MessageContent content={markdown} isStreaming />);

    const block = screen.getByTestId('highlighted-code');
    expect(block).toHaveAttribute('data-streaming', 'true');
    expect(block).toHaveAttribute('data-language', 'rust');
  });
});

describe('isLastFenceUnclosed', () => {
  it('returns false for no fences', () => {
    expect(isLastFenceUnclosed('just text')).toBe(false);
  });

  it('returns true for a single opening fence', () => {
    expect(isLastFenceUnclosed('```python\ncode')).toBe(true);
  });

  it('returns false for a matched pair', () => {
    expect(isLastFenceUnclosed('```\ncode\n```')).toBe(false);
  });

  it('returns true for two pairs plus one open', () => {
    expect(isLastFenceUnclosed('```\na\n```\n```\nb\n```\n```\nc')).toBe(true);
  });

  it('returns false for two matched pairs', () => {
    expect(isLastFenceUnclosed('```\na\n```\n```\nb\n```')).toBe(false);
  });

  it('handles tilde fences', () => {
    expect(isLastFenceUnclosed('~~~\ncode')).toBe(true);
    expect(isLastFenceUnclosed('~~~\ncode\n~~~')).toBe(false);
  });

  it('handles 4+ backtick fences', () => {
    expect(isLastFenceUnclosed('````\ncode')).toBe(true);
    expect(isLastFenceUnclosed('````\ncode\n````')).toBe(false);
  });
});
