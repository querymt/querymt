import { describe, it, expect, vi, beforeAll, beforeEach } from 'vitest';
import { render, screen } from '@testing-library/react';
import type { EventItem } from '../types';
import { ToolSummary } from './ToolSummary';

const patchDiffSpy = vi.fn();

vi.mock('@pierre/diffs/react', () => ({
  PatchDiff: (props: any) => {
    patchDiffSpy(props);
    return <div data-testid="patch-diff" />;
  },
}));

vi.mock('../utils/dashboardThemes', () => ({
  getDiffThemeForDashboard: () => 'github-dark',
  getDashboardThemeVariant: () => 'dark' as const,
}));

describe('ToolSummary edit visualization', () => {
  beforeAll(() => {
    class MockIntersectionObserver {
      private readonly callback: IntersectionObserverCallback;

      constructor(callback: IntersectionObserverCallback) {
        this.callback = callback;
      }

      observe = () => {
        this.callback(
          [{ isIntersecting: true } as IntersectionObserverEntry],
          this as unknown as IntersectionObserver,
        );
      };

      unobserve = () => {};

      disconnect = () => {};

      takeRecords = () => [];

      readonly root = null;

      readonly rootMargin = '0px';

      readonly thresholds = [0];
    }

    vi.stubGlobal('IntersectionObserver', MockIntersectionObserver);
  });

  beforeEach(() => {
    patchDiffSpy.mockClear();
  });

  it('renders inline diff preview for compact edit output', () => {
    const compactOutput = [
      'OK paths=1 edits=1 added=1 deleted=2',
      'P workspace/src/main.ts',
      'H replace old=42,4 new=42,3',
      ' 00041|  fn foo() {',
      '-00042| old line',
      '-00043| second old line',
      ' 00044|  }',
      '+00042| new line',
    ].join('\n');

    const event = {
      id: 'tool-call-1',
      type: 'tool_call',
      content: 'functions.edit',
      timestamp: Date.now(),
      agentId: 'primary',
      toolCall: {
        tool_call_id: 'functions.edit:1',
        kind: 'functions.edit',
        status: 'in_progress',
        raw_input: {
          filePath: '/workspace/src/main.ts',
          oldString: 'old line\nsecond old line',
          newString: 'new line',
        },
      },
      mergedResult: {
        id: 'tool-result-1',
        type: 'tool_result',
        content: compactOutput,
        timestamp: Date.now(),
        agentId: 'primary',
        toolCall: {
          tool_call_id: 'functions.edit:1',
          kind: 'functions.edit',
          status: 'completed',
          raw_output: compactOutput,
        },
      } as EventItem,
    } as EventItem & { mergedResult: EventItem };

    render(
      <ToolSummary
        event={event}
        onClick={vi.fn()}
        isMobile={false}
        selectedTheme="default-dark"
      />,
    );

    expect(screen.getByTestId('patch-diff')).toBeInTheDocument();
    expect(patchDiffSpy).toHaveBeenCalled();

    const patch = String(patchDiffSpy.mock.calls[0][0].patch);
    expect(patch).toContain('diff --git a/workspace/src/main.ts b/workspace/src/main.ts');
    expect(patch).toContain('-old line');
    expect(patch).toContain('-second old line');
    expect(patch).toContain('+new line');
  });

  it('renders inline diff preview when tool kind is empty but tool_call_id is namespaced', () => {
    const compactOutput = [
      'OK paths=1 edits=1 added=1 deleted=1',
      'P workspace/src/alt.ts',
      'H replace old=1,2 new=1,2',
      '-00001| before',
      '+00001| after',
    ].join('\n');

    const event = {
      id: 'tool-call-2',
      type: 'tool_call',
      content: 'functions.edit',
      timestamp: Date.now(),
      agentId: 'primary',
      toolCall: {
        tool_call_id: 'functions.edit:2',
        kind: '',
        status: 'in_progress',
        raw_input: {
          filePath: '/workspace/src/alt.ts',
          oldString: 'before',
          newString: 'after',
        },
      },
      mergedResult: {
        id: 'tool-result-2',
        type: 'tool_result',
        content: compactOutput,
        timestamp: Date.now(),
        agentId: 'primary',
        toolCall: {
          tool_call_id: 'functions.edit:2',
          kind: 'functions.edit',
          status: 'completed',
          raw_output: compactOutput,
        },
      } as EventItem,
    } as EventItem & { mergedResult: EventItem };

    render(
      <ToolSummary
        event={event}
        onClick={vi.fn()}
        isMobile={false}
        selectedTheme="default-dark"
      />,
    );

    expect(screen.getByTestId('patch-diff')).toBeInTheDocument();
    expect(patchDiffSpy).toHaveBeenCalled();
    const patch = String(patchDiffSpy.mock.calls[patchDiffSpy.mock.calls.length - 1][0].patch);
    expect(patch).toContain('diff --git a/workspace/src/alt.ts b/workspace/src/alt.ts');
    expect(patch).toContain('-before');
    expect(patch).toContain('+after');
  });

  it('shows error text inline instead of diff preview for failed edits', () => {
    const event = {
      id: 'tool-call-error',
      type: 'tool_call',
      content: 'functions.edit',
      timestamp: Date.now(),
      agentId: 'primary',
      toolCall: {
        tool_call_id: 'functions.edit:error',
        kind: 'functions.edit',
        status: 'in_progress',
        raw_input: {
          filePath: '/workspace/src/file.ts',
          oldString: 'nonexistent',
          newString: 'replacement',
        },
      },
      mergedResult: {
        id: 'tool-result-error',
        type: 'tool_result',
        content: 'Error: oldString not found in content',
        timestamp: Date.now(),
        agentId: 'primary',
        toolCall: {
          tool_call_id: 'functions.edit:error',
          kind: 'functions.edit',
          status: 'failed',
          raw_output: 'Error: oldString not found in content',
        },
      } as EventItem,
    } as EventItem & { mergedResult: EventItem };

    render(
      <ToolSummary
        event={event}
        onClick={vi.fn()}
        isMobile={false}
        selectedTheme="default-dark"
      />,
    );

    expect(screen.getByText(/oldString not found/)).toBeInTheDocument();
    expect(screen.queryByTestId('patch-diff')).not.toBeInTheDocument();
    expect(patchDiffSpy).not.toHaveBeenCalled();
  });

});
