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

  it('renders inline diff preview for namespaced edit tool using compact output', () => {
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
        content: 'OK paths=1 edits=1 added=1 deleted=2\nP workspace/src/main.ts\nH replace old=42,2 new=42,1\n-old line\n-second old line\n+new line',
        timestamp: Date.now(),
        agentId: 'primary',
        toolCall: {
          tool_call_id: 'functions.edit:1',
          kind: 'functions.edit',
          status: 'completed',
          raw_output: 'OK paths=1 edits=1 added=1 deleted=2\nP workspace/src/main.ts\nH replace old=42,2 new=42,1\n-old line\n-second old line\n+new line',
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
    expect(patch).toContain('@@ -42,2 +42,1 @@');
    expect(patch).toContain('-old line');
    expect(patch).toContain('-second old line');
    expect(patch).toContain('+new line');
  });

  it('renders inline diff preview when tool kind is empty but tool_call_id is namespaced', () => {
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
        content: 'OK paths=1 edits=1 added=1 deleted=1\nP workspace/src/alt.ts\nH replace old=7,1 new=7,1\n-before\n+after',
        timestamp: Date.now(),
        agentId: 'primary',
        toolCall: {
          tool_call_id: 'functions.edit:2',
          kind: 'functions.edit',
          status: 'completed',
          raw_output: 'OK paths=1 edits=1 added=1 deleted=1\nP workspace/src/alt.ts\nH replace old=7,1 new=7,1\n-before\n+after',
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
    expect(patch).toContain('@@ -7,1 +7,1 @@');
    expect(patch).toContain('-before');
    expect(patch).toContain('+after');
  });

  it('renders anchored edit result without anchors in the visual diff', () => {
    const event = {
      id: 'tool-call-anchor',
      type: 'tool_call',
      content: 'functions.edit',
      timestamp: Date.now(),
      agentId: 'primary',
      toolCall: {
        tool_call_id: 'functions.edit:anchor',
        kind: 'functions.edit',
        status: 'in_progress',
        raw_input: {
          filePath: '/workspace/src/anchored.ts',
          operation: 'replace',
          startAnchor: 'AbC123§const value = 1;',
          newText: 'const value = 2;',
        },
      },
      mergedResult: {
        id: 'tool-result-anchor',
        type: 'tool_result',
        content: 'OK paths=1 edits=1 added=1 deleted=1\nP workspace/src/anchored.ts\nH replace old=1,3 new=1,3\n AbC120§const before = 0;\n-AbC123§const value = 1;\n+XyZ789§const value = 2;\n AbC124§const after = 3;',
        timestamp: Date.now(),
        agentId: 'primary',
        toolCall: {
          tool_call_id: 'functions.edit:anchor',
          kind: 'functions.edit',
          status: 'completed',
          raw_output: 'OK paths=1 edits=1 added=1 deleted=1\nP workspace/src/anchored.ts\nH replace old=1,3 new=1,3\n AbC120§const before = 0;\n-AbC123§const value = 1;\n+XyZ789§const value = 2;\n AbC124§const after = 3;',
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
    expect(patch).toContain('diff --git a/workspace/src/anchored.ts b/workspace/src/anchored.ts');
    expect(patch).toContain('-const value = 1;');
    expect(patch).toContain('+const value = 2;');
    expect(patch).not.toContain('AbC123§');
    expect(patch).not.toContain('XyZ789§');
  });

  it('renders a single file diff with multiple hunks for compact multiedit output', () => {
    const compactOutput = [
      'OK paths=1 edits=2 added=2 deleted=2',
      'P crates/agent/src/tools/builtins/multiedit.rs',
      'H replace old=10,3 new=10,3',
      ' AAA111§use async_trait::async_trait;',
      '-BBB222§use querymt::chat::{Content, FunctionTool, Tool as ChatTool};',
      '+CCC333§use querymt::chat::{Content, FunctionTool};',
      ' DDD444§use serde_json::{Value, json};',
      'H insert_before old=25,2 new=25,3',
      ' EEE555§impl MultiEditTool {',
      '+FFF666§    fn debug_label(&self) -> &str {',
      '+GGG777§        "multiedit"',
      '+HHH888§    }',
      ' III999§    pub fn new() -> Self {',
    ].join('\n');

    const event = {
      id: 'tool-call-multiedit',
      type: 'tool_call',
      content: 'functions.multiedit',
      timestamp: Date.now(),
      agentId: 'primary',
      toolCall: {
        tool_call_id: 'functions.multiedit:1',
        kind: 'functions.multiedit',
        status: 'in_progress',
        raw_input: {
          paths: [
            {
              path: 'crates/agent/src/tools/builtins/multiedit.rs',
              edits: [],
            },
          ],
        },
      },
      mergedResult: {
        id: 'tool-result-multiedit',
        type: 'tool_result',
        content: compactOutput,
        timestamp: Date.now(),
        agentId: 'primary',
        toolCall: {
          tool_call_id: 'functions.multiedit:1',
          kind: 'functions.multiedit',
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
    expect((patch.match(/^diff --git /gm) || []).length).toBe(1);
    expect((patch.match(/^@@ /gm) || []).length).toBe(2);
    expect(patch).toContain('@@ -10,3 +10,3 @@');
    expect(patch).toContain('@@ -25,2 +25,5 @@');
    expect(patch).toContain('-use querymt::chat::{Content, FunctionTool, Tool as ChatTool};');
    expect(patch).toContain('+use querymt::chat::{Content, FunctionTool};');
    expect(patch).not.toContain('BBB222§');
    expect(patch).not.toContain('FFF666§');
  });

  it('renders overlapping same-file multiedit hunks as separate diff sections', () => {
    const compactOutput = [
      'OK paths=1 edits=2 added=1 deleted=5',
      'P crates/agent/src/anchors/edit.rs',
      'H delete old=372,11 new=372,6',
      ' AAA111§    }',
      ' BBB222§',
      '-CCC333§struct DiffRegionLine<' + "'" + 'a> {',
      '-DDD444§    anchor: &' + "'" + 'a str,',
      '-EEE555§    text: &' + "'" + 'a str,',
      '-FFF666§    line_number: usize,',
      '-GGG777§}',
      ' HHH888§',
      ' III999§struct DiffRegionLine<' + "'" + 'a> {',
      ' JJJ000§    anchor: &' + "'" + 'a str,',
      'H insert_before old=373,6 new=373,7',
      ' KKK111§',
      '+LLL222§#[derive(Clone, Copy)]',
      ' MMM333§struct DiffRegionLine<' + "'" + 'a> {',
      ' NNN444§    anchor: &' + "'" + 'a str,',
      ' OOO555§    text: &' + "'" + 'a str,',
      ' PPP666§    line_number: usize,',
    ].join('\n');

    const event = {
      id: 'tool-call-overlap',
      type: 'tool_call',
      content: 'functions.multiedit',
      timestamp: Date.now(),
      agentId: 'primary',
      toolCall: {
        tool_call_id: 'functions.multiedit:overlap',
        kind: 'functions.multiedit',
        status: 'in_progress',
        raw_input: {
          paths: [{ path: 'crates/agent/src/anchors/edit.rs', edits: [] }],
        },
      },
      mergedResult: {
        id: 'tool-result-overlap',
        type: 'tool_result',
        content: compactOutput,
        timestamp: Date.now(),
        agentId: 'primary',
        toolCall: {
          tool_call_id: 'functions.multiedit:overlap',
          kind: 'functions.multiedit',
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
    const patch = String(patchDiffSpy.mock.calls[patchDiffSpy.mock.calls.length - 1][0].patch);
    expect((patch.match(/^diff --git /gm) || []).length).toBe(2);
    expect((patch.match(/^@@ /gm) || []).length).toBe(2);
    expect(patch).not.toContain('CCC333§');
    expect(patch).not.toContain('LLL222§');
  });

  it('passes the exact session-shaped nearby multiedit fixture through as a renderable patch', () => {
    const compactOutput = [
      'OK paths=1 edits=2 added=1 deleted=5',
      'P edit.rs',
      'H delete old=1,11 new=1,6',
      ' AAA111§head',
      ' BBB222§keep before',
      ' CCC333§',
      '-DDD444§struct DiffRegionLine<' + "'" + 'a> {',
      '-EEE555§    anchor: &' + "'" + 'a str,',
      '-FFF666§    text: &' + "'" + 'a str,',
      '-GGG777§    line_number: usize,',
      '-HHH888§}',
      ' III999§',
      ' JJJ000§#[derive(Clone, Copy)]',
      ' KKK111§struct DiffRegionLine<' + "'" + 'a> {',
      'H insert_before old=2,6 new=2,7',
      ' LLL222§keep before',
      ' MMM333§',
      ' NNN444§',
      '+OOO555§#[derive(Clone, Copy)]',
      ' PPP666§struct DiffRegionLine<' + "'" + 'a> {',
      ' QQQ777§    anchor: &' + "'" + 'a str,',
      ' RRR888§    text: &' + "'" + 'a str,',
    ].join('\n');

    const event = {
      id: 'tool-call-session-shaped',
      type: 'tool_call',
      content: 'functions.multiedit',
      timestamp: Date.now(),
      agentId: 'primary',
      toolCall: {
        tool_call_id: 'functions.multiedit:session-shaped',
        kind: 'functions.multiedit',
        status: 'in_progress',
        raw_input: {
          paths: [{ path: 'edit.rs', edits: [] }],
        },
      },
      mergedResult: {
        id: 'tool-result-session-shaped',
        type: 'tool_result',
        content: compactOutput,
        timestamp: Date.now(),
        agentId: 'primary',
        toolCall: {
          tool_call_id: 'functions.multiedit:session-shaped',
          kind: 'functions.multiedit',
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
    const patch = String(patchDiffSpy.mock.calls[patchDiffSpy.mock.calls.length - 1][0].patch);
    expect((patch.match(/^diff --git /gm) || []).length).toBe(2);
    expect((patch.match(/^@@ /gm) || []).length).toBe(2);
    expect(patch).toContain('@@ -1,11 +1,6 @@');
    expect(patch).toContain('@@ -2,6 +2,7 @@');
    expect(patch).not.toContain('DDD444§');
    expect(patch).not.toContain('OOO555§');
  });

  it('keeps inline anchored edit preview focused on the diff only', () => {
    const compactOutput = [
      'OK paths=1 edits=1 added=1 deleted=1 anchors=fresh',
      'P workspace/src/anchored.ts',
      'H replace old=1,3 new=1,3',
      ' AbC120§const before = 0;',
      '-AbC123§const value = 1;',
      '+XyZ789§const value = 2;',
      ' AbC124§const after = 3;',
    ].join('\n');

    const event = {
      id: 'tool-call-anchor-fresh',
      type: 'tool_call',
      content: 'functions.edit',
      timestamp: Date.now(),
      agentId: 'primary',
      toolCall: {
        tool_call_id: 'functions.edit:fresh',
        kind: 'functions.edit',
        status: 'in_progress',
        raw_input: {
          filePath: '/workspace/src/anchored.ts',
          operation: 'replace',
          startAnchor: 'AbC123§const value = 1;',
          newText: 'const value = 2;',
        },
      },
      mergedResult: {
        id: 'tool-result-anchor-fresh',
        type: 'tool_result',
        content: compactOutput,
        timestamp: Date.now(),
        agentId: 'primary',
        toolCall: {
          tool_call_id: 'functions.edit:fresh',
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
    expect(screen.queryByText('Anchored output')).not.toBeInTheDocument();
    expect(screen.queryByText('anchors returned')).not.toBeInTheDocument();
    expect(screen.queryByText(/AbC123§const value = 1;/)).not.toBeInTheDocument();
  });

  it('shows stale-anchor failures inline instead of diff preview', () => {
    const event = {
      id: 'tool-call-stale-anchor',
      type: 'tool_call',
      content: 'functions.multiedit',
      timestamp: Date.now(),
      agentId: 'primary',
      toolCall: {
        tool_call_id: 'functions.multiedit:stale',
        kind: 'functions.multiedit',
        status: 'in_progress',
        raw_input: {
          paths: [{ path: 'workspace/src/file.ts', edits: [] }],
        },
      },
      mergedResult: {
        id: 'tool-result-stale-anchor',
        type: 'tool_result',
        content: "Error: Provider error: No changes written (validation failed): Anchor 'FHCHkR' is missing or stale.",
        timestamp: Date.now(),
        agentId: 'primary',
        toolCall: {
          tool_call_id: 'functions.multiedit:stale',
          kind: 'functions.multiedit',
          status: 'failed',
          raw_output: "Error: Provider error: No changes written (validation failed): Anchor 'FHCHkR' is missing or stale.",
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

    expect(screen.getByText(/Anchor 'FHCHkR' is missing or stale/)).toBeInTheDocument();
    expect(screen.queryByTestId('patch-diff')).not.toBeInTheDocument();
    expect(patchDiffSpy).not.toHaveBeenCalled();
  });

  it('shows fallback text and skips PatchDiff for malformed apply_patch payloads', () => {
    const event = {
      id: 'tool-call-3',
      type: 'tool_call',
      content: 'apply_patch',
      timestamp: Date.now(),
      agentId: 'primary',
      toolCall: {
        tool_call_id: 'apply_patch:1',
        kind: 'apply_patch',
        status: 'completed',
        raw_input: {
          patch: 'this is not a unified diff',
        },
      },
    } as EventItem;

    render(
      <ToolSummary
        event={event}
        onClick={vi.fn()}
        isMobile={false}
        selectedTheme="default-dark"
      />,
    );

    expect(screen.getByText('Patch payload is malformed. Open details to inspect raw content.')).toBeInTheDocument();
    expect(patchDiffSpy).not.toHaveBeenCalled();
  });
});
