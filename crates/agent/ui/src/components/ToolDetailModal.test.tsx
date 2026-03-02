import { describe, it, expect, vi, afterEach } from 'vitest';
import { render } from '@testing-library/react';

// Capture PatchDiff props to inspect diffStyle
const patchDiffSpy = vi.fn();
vi.mock('@pierre/diffs/react', () => ({
  PatchDiff: (props: any) => {
    patchDiffSpy(props);
    return <div data-testid="patch-diff" data-diff-style={props.options?.diffStyle ?? 'unknown'} />;
  },
}));

vi.mock('../store/uiStore', () => ({
  useUiStore: (selector: (s: any) => any) => selector({ selectedTheme: 'default-dark' }),
}));
vi.mock('../utils/dashboardThemes', () => ({
  getDiffThemeForDashboard: () => 'github-dark',
  getDashboardThemeVariant: () => 'dark' as const,
}));

// We need to test DiffView indirectly via ToolDetailModal, but it's easier
// to extract and test the diffStyle logic. Since DiffView is not exported,
// we'll render ToolDetailModal with an edit event and check PatchDiff props.
vi.mock('@radix-ui/react-dialog', () => ({
  Root: ({ children }: any) => <div>{children}</div>,
  Portal: ({ children }: any) => <div>{children}</div>,
  Overlay: () => <div />,
  Content: ({ children }: any) => <div>{children}</div>,
  Title: ({ children }: any) => <div>{children}</div>,
  Close: ({ children }: any) => <div>{children}</div>,
}));

vi.mock('./HighlightedCode', () => ({
  HighlightedCode: () => <div data-testid="highlighted-code" />,
}));
vi.mock('./MessageContent', () => ({
  MessageContent: () => <div data-testid="message-content" />,
}));

// Import after mocks
import { ToolDetailModal } from './ToolDetailModal';

function setViewportWidth(width: number) {
  Object.defineProperty(window, 'innerWidth', {
    writable: true,
    configurable: true,
    value: width,
  });
}

describe('ToolDetailModal diff style', () => {
  const originalWidth = window.innerWidth;

  afterEach(() => {
    setViewportWidth(originalWidth);
    patchDiffSpy.mockClear();
  });

  const editEvent = {
    id: 'e1',
    type: 'tool_call' as const,
    content: '',
    timestamp: 1000,
    agentId: 'agent-0',
    toolCall: {
      name: 'Edit',
      kind: 'edit',
      raw_input: {
        filePath: '/test/file.ts',
        oldString: 'const a = 1;',
        newString: 'const a = 2;',
      },
      status: 'completed',
    },
  } as any;

  it('uses split diffStyle on desktop viewport', () => {
    setViewportWidth(1024);
    window.dispatchEvent(new Event('resize'));

    render(<ToolDetailModal event={editEvent} onClose={vi.fn()} />);

    expect(patchDiffSpy).toHaveBeenCalled();
    const lastCall = patchDiffSpy.mock.calls[patchDiffSpy.mock.calls.length - 1][0];
    expect(lastCall.options.diffStyle).toBe('split');
  });

  it('uses unified diffStyle on mobile viewport', () => {
    setViewportWidth(375);
    window.dispatchEvent(new Event('resize'));

    render(<ToolDetailModal event={editEvent} onClose={vi.fn()} />);

    expect(patchDiffSpy).toHaveBeenCalled();
    const lastCall = patchDiffSpy.mock.calls[patchDiffSpy.mock.calls.length - 1][0];
    expect(lastCall.options.diffStyle).toBe('unified');
  });
});
