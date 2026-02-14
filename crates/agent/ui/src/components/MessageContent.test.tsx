import { describe, expect, it, vi } from 'vitest';
import { render, screen } from '@testing-library/react';
import { MessageContent } from './MessageContent';

vi.mock('./HighlightedCode', () => ({
  HighlightedCode: ({ code, language }: { code: string; language?: string }) => (
    <div data-testid="highlighted-code" data-language={language ?? ''}>
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
});
