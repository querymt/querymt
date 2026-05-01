import { describe, it, expect, vi, afterEach } from 'vitest';
import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';

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
  Content: ({ children, ...rest }: any) => <div {...rest}>{children}</div>,
  Title: ({ children, ...rest }: any) => <div {...rest}>{children}</div>,
  Close: ({ children, ...rest }: any) => <button data-testid="close-button" {...rest}>{children}</button>,
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
      kind: '',
      tool_call_id: 'functions.edit:99',
      raw_input: {
        filePath: '/test/file.ts',
        oldString: 'const a = 1;',
        newString: 'const a = 2;',
      },
      status: 'completed',
    },
    mergedResult: {
      id: 'e2',
      type: 'tool_result' as const,
      content: 'OK paths=1 edits=1 added=1 deleted=1\nP test/file.ts\nH replace old=3,1 new=3,1\n-00003| const a = 1;\n+00003| const a = 2;',
      timestamp: 1001,
      agentId: 'agent-0',
      toolCall: {
        tool_call_id: 'functions.edit:99',
        kind: 'functions.edit',
        status: 'completed',
        raw_output: 'OK paths=1 edits=1 added=1 deleted=1\nP test/file.ts\nH replace old=3,1 new=3,1\n-00003| const a = 1;\n+00003| const a = 2;',
      },
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

  it('renders edit diff preview from oldString/newString input', () => {
    setViewportWidth(1024);
    const editEvent = {
      id: 'e-edit',
      type: 'tool_call' as const,
      content: '',
      timestamp: 1002,
      agentId: 'agent-0',
      toolCall: {
        name: 'Edit',
        kind: '',
        tool_call_id: 'functions.edit:edit',
        raw_input: {
          filePath: '/test/file.ts',
          oldString: 'const value = 1;',
          newString: 'const value = 2;',
        },
        status: 'completed',
      },
      mergedResult: {
        id: 'e-edit-result',
        type: 'tool_result' as const,
        content: 'OK paths=1 edits=1 added=1 deleted=1\nP test/file.ts\nH replace old=1,2 new=1,2\n-00001| const value = 1;\n+00001| const value = 2;',
        timestamp: 1003,
        agentId: 'agent-0',
        toolCall: {
          tool_call_id: 'functions.edit:edit',
          kind: 'functions.edit',
          status: 'completed',
          raw_output: 'OK paths=1 edits=1 added=1 deleted=1\nP test/file.ts\nH replace old=1,2 new=1,2\n-00001| const value = 1;\n+00001| const value = 2;',
        },
      },
    } as any;

    render(<ToolDetailModal event={editEvent} onClose={vi.fn()} />);

    expect(patchDiffSpy).toHaveBeenCalled();
    const patch = String(patchDiffSpy.mock.calls[patchDiffSpy.mock.calls.length - 1][0].patch);
    expect(patch).toContain('-const value = 1;');
    expect(patch).toContain('+const value = 2;');
  });

  it('shows error text for failed edit operations', () => {
    setViewportWidth(1024);
    const failedEvent = {
      id: 'e-edit-failed',
      type: 'tool_call' as const,
      content: '',
      timestamp: 1006,
      agentId: 'agent-0',
      toolCall: {
        name: 'Edit',
        kind: '',
        tool_call_id: 'functions.edit:failed',
        raw_input: {
          filePath: '/test/file.ts',
          oldString: 'nonexistent',
          newString: 'replacement',
        },
        status: 'failed',
      },
      mergedResult: {
        id: 'e-edit-failed-result',
        type: 'tool_result' as const,
        content: 'Error: oldString not found in content',
        timestamp: 1007,
        agentId: 'agent-0',
        toolCall: {
          tool_call_id: 'functions.edit:failed',
          kind: 'functions.edit',
          status: 'failed',
          raw_output: 'Error: oldString not found in content',
        },
      },
    } as any;

    render(<ToolDetailModal event={failedEvent} onClose={vi.fn()} />);

    expect(screen.getAllByText(/oldString not found/).length).toBeGreaterThan(0);
    expect(screen.queryByTestId('patch-diff')).not.toBeInTheDocument();
  });

  it('shows malformed patch warning and skips PatchDiff for invalid apply_patch input', () => {
    const malformedPatchEvent = {
      id: 'e3',
      type: 'tool_call' as const,
      content: '',
      timestamp: 1002,
      agentId: 'agent-0',
      toolCall: {
        kind: 'apply_patch',
        tool_call_id: 'apply_patch:1',
        raw_input: {
          patch: 'definitely not a unified diff',
        },
        status: 'completed',
      },
    } as any;

    render(<ToolDetailModal event={malformedPatchEvent} onClose={vi.fn()} />);

    expect(screen.getByText('Patch payload is malformed. Open details to inspect raw content.')).toBeInTheDocument();
    expect(patchDiffSpy).not.toHaveBeenCalled();
  });
});

describe('ToolDetailModal mobile header layout', () => {
  const originalWidth = window.innerWidth;

  afterEach(() => {
    setViewportWidth(originalWidth);
  });

  const completedEvent = {
    id: 'e1',
    type: 'tool_call' as const,
    content: '',
    timestamp: 1000,
    agentId: 'agent-0',
    toolCall: {
      name: 'Shell',
      kind: 'shell',
      raw_input: { command: 'ls' },
      status: 'completed',
    },
    mergedResult: {
      id: 'e2',
      type: 'tool_result' as const,
      content: 'file.txt',
      timestamp: 1001,
      agentId: 'agent-0',
      toolCall: { status: 'completed' },
    },
  } as any;

  it('on mobile, close button is in a separate row from status and timestamp', () => {
    setViewportWidth(375);
    window.dispatchEvent(new Event('resize'));

    render(<ToolDetailModal event={completedEvent} onClose={vi.fn()} />);

    const closeButton = screen.getByTestId('close-button');
    // On mobile, the close button's parent row should NOT contain status/timestamp text
    const closeRow = closeButton.closest('[data-testid="header-top-row"]');
    expect(closeRow).not.toBeNull();

    // Status and timestamp should be in a separate row
    const metaRow = screen.getByTestId('header-meta-row');
    expect(metaRow).toBeInTheDocument();
    expect(metaRow.textContent).toContain('Completed');
  });

  it('on desktop, close button is in the same row as status and timestamp', () => {
    setViewportWidth(1024);
    window.dispatchEvent(new Event('resize'));

    render(<ToolDetailModal event={completedEvent} onClose={vi.fn()} />);

    const closeButton = screen.getByTestId('close-button');
    // On desktop, the close button should be inside the right-side group
    const rightGroup = closeButton.closest('[data-testid="header-right-group"]');
    expect(rightGroup).not.toBeNull();
    // The right group should contain status text too
    expect(rightGroup!.textContent).toContain('Completed');
  });

  it('on mobile, close button has flex-shrink-0 to prevent clipping', () => {
    setViewportWidth(375);
    window.dispatchEvent(new Event('resize'));

    render(<ToolDetailModal event={completedEvent} onClose={vi.fn()} />);

    const closeButton = screen.getByTestId('close-button');
    expect(closeButton.className).toContain('shrink-0');
  });
});
