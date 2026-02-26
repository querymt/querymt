import { describe, expect, it, vi, beforeEach } from 'vitest';
import { render, screen, act } from '@testing-library/react';
import { HighlightedCode, clearHighlightCache } from './HighlightedCode';

// Stub shiki — returns a deterministic HTML string based on inputs.
vi.mock('shiki', () => ({
  codeToHtml: vi.fn(async (code: string, opts: { lang: string; theme: string }) =>
    `<pre class="shiki"><code>${opts.lang}:${code}</code></pre>`
  ),
}));

// Stub the theme helpers so the component renders without a real store.
vi.mock('../store/uiStore', () => ({
  useUiStore: (selector: (s: { selectedTheme: string }) => string) =>
    selector({ selectedTheme: 'test-theme' }),
}));
vi.mock('../utils/dashboardThemes', () => ({
  getShikiThemeForDashboard: (t: string) => t,
}));

// Pull in the mock so we can inspect call counts.
import { codeToHtml } from 'shiki';
const mockCodeToHtml = vi.mocked(codeToHtml);

beforeEach(() => {
  clearHighlightCache();
  mockCodeToHtml.mockClear();
});

describe('HighlightedCode', () => {
  it('shows plain code as fallback then highlighted output', async () => {
    const { container } = render(
      <HighlightedCode code="let x = 1;" language="ts" maxHeight="none" />
    );

    // Initially shows raw code as plain monospace (no layout jump)
    expect(container.querySelector('pre code')).toBeTruthy();
    expect(container.textContent).toContain('let x = 1;');

    // Wait for async highlight to complete
    await act(async () => {});

    expect(container.querySelector('.shiki')).toBeTruthy();
    expect(container.textContent).toContain('ts:let x = 1;');
    expect(mockCodeToHtml).toHaveBeenCalledTimes(1);
  });

  it('renders plain monospace when isStreaming is true', () => {
    const { container } = render(
      <HighlightedCode code="partial code" language="rs" isStreaming maxHeight="none" />
    );

    // Should render <pre><code> immediately, no loading, no shiki call
    expect(container.querySelector('pre code')).toBeTruthy();
    expect(container.textContent).toContain('partial code');
    expect(mockCodeToHtml).not.toHaveBeenCalled();
  });

  it('serves cached HTML synchronously on remount — no loading flash', async () => {
    // First mount: populates cache
    const { unmount } = render(
      <HighlightedCode code="cached()" language="py" maxHeight="none" />
    );
    await act(async () => {});
    expect(mockCodeToHtml).toHaveBeenCalledTimes(1);
    unmount();

    // Second mount with identical props: should render from cache instantly
    const { container } = render(
      <HighlightedCode code="cached()" language="py" maxHeight="none" />
    );

    // No loading state — cached HTML rendered synchronously on first paint
    expect(screen.queryByText('Highlighting code...')).toBeNull();
    expect(container.querySelector('.shiki')).toBeTruthy();
    // Shiki should NOT have been called again
    expect(mockCodeToHtml).toHaveBeenCalledTimes(1);
  });

  it('cache is keyed by code content — different code triggers new highlight', async () => {
    render(<HighlightedCode code="v1" language="py" maxHeight="none" />);
    await act(async () => {});

    render(<HighlightedCode code="v2" language="py" maxHeight="none" />);
    await act(async () => {});

    expect(mockCodeToHtml).toHaveBeenCalledTimes(2);
  });

  it('cache is keyed by language — different language triggers new highlight', async () => {
    render(<HighlightedCode code="x" language="py" maxHeight="none" />);
    await act(async () => {});

    render(<HighlightedCode code="x" language="ts" maxHeight="none" />);
    await act(async () => {});

    expect(mockCodeToHtml).toHaveBeenCalledTimes(2);
  });
});
