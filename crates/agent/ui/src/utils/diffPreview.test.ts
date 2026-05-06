import { describe, expect, it } from 'vitest';

import type { EventItem } from '../types';
import { buildToolDiffPreview } from './diffPreview';

describe('buildToolDiffPreview', () => {
  it('builds diff from edit input', () => {
    const preview = buildToolDiffPreview(
      'functions.edit',
      { filePath: 'file.txt', oldString: 'hello', newString: 'world' },
      {
        id: 'tool-result-1',
        type: 'tool_result',
        content: 'OK paths=1 edits=1 added=1 deleted=1',
        timestamp: Date.now(),
        agentId: 'agent',
        toolCall: {
          tool_call_id: 'functions.edit:1',
          kind: 'functions.edit',
          status: 'completed',
          raw_output: 'OK paths=1 edits=1 added=1 deleted=1',
        },
      } as EventItem,
    );

    expect(preview?.type).toBe('diff');
    expect(preview?.patch).toContain('diff --git');
    expect(preview?.patch).toContain('-hello');
    expect(preview?.patch).toContain('+world');
    expect(preview?.filePath).toBe('file.txt');
  });

  it('builds diff from multiedit input with multiple edits', () => {
    const preview = buildToolDiffPreview(
      'functions.multiedit',
      {
        filePath: 'edit.rs',
        edits: [
          { oldString: 'foo', newString: 'bar' },
          { oldString: 'baz', newString: 'qux' },
        ],
      },
      {
        id: 'tool-result-2',
        type: 'tool_result',
        content: 'OK paths=1 edits=2 added=2 deleted=2',
        timestamp: Date.now(),
        agentId: 'agent',
        toolCall: {
          tool_call_id: 'functions.multiedit:1',
          kind: 'functions.multiedit',
          status: 'completed',
          raw_output: 'OK paths=1 edits=2 added=2 deleted=2',
        },
      } as EventItem,
    );

    expect(preview?.type).toBe('diff');
    const patch = preview?.patch ?? '';
    // Single file → one diff --git section, two hunks
    expect((patch.match(/^diff --git /gm) || []).length).toBe(1);
    expect((patch.match(/^@@ /gm) || []).length).toBe(2);
    expect(patch).toContain('-foo');
    expect(patch).toContain('+bar');
    expect(patch).toContain('-baz');
    expect(patch).toContain('+qux');
  });

  it('builds diff from multiedit input with multi-line edits', () => {
    const oldBlock = 'struct DiffRegionLine {\n    anchor: &str,\n    text: &str,\n    line_number: usize,\n}';
    const newBlock = '#[derive(Clone, Copy)]\nstruct DiffRegionLine {\n    anchor: &str,\n    text: &str,\n    line_number: usize,\n}';

    const preview = buildToolDiffPreview(
      'functions.multiedit',
      {
        filePath: 'edit.rs',
        edits: [{ oldString: oldBlock, newString: newBlock }],
      },
      {
        id: 'tool-result-multi-line',
        type: 'tool_result',
        content: 'OK paths=1 edits=1 added=1 deleted=0',
        timestamp: Date.now(),
        agentId: 'agent',
        toolCall: {
          tool_call_id: 'functions.multiedit:ml',
          kind: 'functions.multiedit',
          status: 'completed',
          raw_output: 'OK paths=1 edits=1 added=1 deleted=0',
        },
      } as EventItem,
    );

    expect(preview?.type).toBe('diff');
    expect(preview?.patch).toContain('diff --git');
    expect(preview?.patch).toContain('-struct DiffRegionLine {');
    expect(preview?.patch).toContain('+#[derive(Clone, Copy)]');
  });

  it('renders write_file as new file diff', () => {
    const preview = buildToolDiffPreview(
      'write_file',
      { filePath: '/workspace/src/new.ts', content: 'line 1\nline 2' },
      undefined,
    );

    expect(preview?.type).toBe('diff');
    expect(preview?.patch).toContain('diff --git a/workspace/src/new.ts');
    expect(preview?.patch).toContain('new file mode 100644');
    expect(preview?.patch).toContain('+line 1');
    expect(preview?.patch).toContain('+line 2');
  });

  it('renders apply_patch with valid unified diff', () => {
    const patch = 'diff --git a/file.ts b/file.ts\n--- a/file.ts\n+++ b/file.ts\n@@ -1,1 +1,1 @@\n-old\n+new';
    const preview = buildToolDiffPreview(
      'apply_patch',
      { patch },
      undefined,
    );

    expect(preview?.type).toBe('diff');
    expect(preview?.patch).toContain('-old');
    expect(preview?.patch).toContain('+new');
  });

  it('returns fallback for malformed apply_patch', () => {
    const preview = buildToolDiffPreview(
      'apply_patch',
      { patch: 'not a diff' },
      undefined,
    );

    expect(preview?.type).toBe('diff');
    expect(preview?.patch).toBeNull();
    expect(preview?.fallbackText).toContain('malformed');
  });

  it('returns null for unknown tool kinds', () => {
    const preview = buildToolDiffPreview(
      'shell',
      { command: 'ls' },
      undefined,
    );

    expect(preview).toBeNull();
  });

  it('returns fallback for error output', () => {
    const preview = buildToolDiffPreview(
      'functions.edit',
      { filePath: 'test.txt', oldString: 'a', newString: 'b' },
      {
        id: 'tool-result-error',
        type: 'tool_result',
        content: 'Error: oldString not found in content',
        timestamp: Date.now(),
        agentId: 'agent',
        toolCall: {
          tool_call_id: 'functions.edit:error',
          kind: 'functions.edit',
          status: 'failed',
          raw_output: 'Error: oldString not found in content',
        },
      } as EventItem,
    );

    expect(preview?.type).toBe('diff');
    expect(preview?.fallbackText).toContain('oldString not found');
  });
});
