import { describe, expect, it } from 'vitest';

import type { EventItem } from '../types';
import { buildToolDiffPreview } from './diffPreview';

describe('buildToolDiffPreview', () => {
  it('parses compact output with insert_after hunk', () => {
    const rawOutput = [
      'OK paths=1 edits=1 added=1 deleted=0',
      'P file.txt',
      'H insert_after old=1,5 new=1,6',
      ' 00001| a',
      ' 00002| b',
      '+00003| bb',
      ' 00004| c',
      ' 00005| d',
      ' 00006| e',
    ].join('\n');

    const preview = buildToolDiffPreview(
      'functions.edit',
      { filePath: 'file.txt', oldString: 'b', newString: 'bb' },
      {
        id: 'tool-result-1',
        type: 'tool_result',
        content: rawOutput,
        timestamp: Date.now(),
        agentId: 'agent',
        toolCall: {
          tool_call_id: 'functions.edit:1',
          kind: 'functions.edit',
          status: 'completed',
          raw_output: rawOutput,
        },
      } as EventItem,
    );

    expect(preview?.type).toBe('diff');
    expect(preview?.patch).toContain('@@ -1,5 +1,6 @@');
    expect(preview?.patch).toContain(' a\n b\n+bb\n c');
  });

  it('splits overlapping multiedit hunks into separate diff sections', () => {
    const rawOutput = [
      'OK paths=1 edits=2 added=1 deleted=5',
      'P edit.rs',
      'H delete old=372,11 new=372,6',
      ' 00372|     }',
      ' 00373| ',
      '-00374| struct DiffRegionLine {',
      '-00375|     anchor: &str,',
      '-00376|     text: &str,',
      '-00377|     line_number: usize,',
      '-00378| }',
      ' 00379| ',
      ' 00380| struct DiffRegionLine {',
      ' 00381|     anchor: &str,',
      'H insert_before old=373,6 new=373,7',
      ' 00373| ',
      '+00374| #[derive(Clone, Copy)]',
      ' 00375| struct DiffRegionLine {',
      ' 00376|     anchor: &str,',
      ' 00377|     text: &str,',
      ' 00378|     line_number: usize,',
    ].join('\n');

    const preview = buildToolDiffPreview(
      'functions.multiedit',
      { paths: [{ path: 'edit.rs', edits: [] }] },
      {
        id: 'tool-result-2',
        type: 'tool_result',
        content: rawOutput,
        timestamp: Date.now(),
        agentId: 'agent',
        toolCall: {
          tool_call_id: 'functions.multiedit:1',
          kind: 'functions.multiedit',
          status: 'completed',
          raw_output: rawOutput,
        },
      } as EventItem,
    );

    expect(preview?.type).toBe('diff');
    const patch = preview?.patch ?? '';
    expect((patch.match(/^diff --git /gm) || []).length).toBe(2);
    expect((patch.match(/^@@ /gm) || []).length).toBe(2);
    expect(patch).toContain('@@ -372,10 +372,5 @@');
    expect(patch).toContain('@@ -373,5 +373,6 @@');
  });

  it('renders the exact session-shaped multiedit fixture coherently', () => {
    const rawOutput = [
      'OK paths=1 edits=2 added=1 deleted=5',
      'P edit.rs',
      'H delete old=1,11 new=1,6',
      ' 00001| head',
      ' 00002| keep before',
      ' 00003| ',
      '-00004| struct DiffRegionLine {',
      '-00005|     anchor: &str,',
      '-00006|     text: &str,',
      '-00007|     line_number: usize,',
      '-00008| }',
      ' 00009| ',
      ' 00010| #[derive(Clone, Copy)]',
      ' 00011| struct DiffRegionLine {',
      'H insert_before old=2,6 new=2,7',
      ' 00002| keep before',
      ' 00003| ',
      ' 00004| ',
      '+00005| #[derive(Clone, Copy)]',
      ' 00006| struct DiffRegionLine {',
      ' 00007|     anchor: &str,',
      ' 00008|     text: &str,',
    ].join('\n');

    const preview = buildToolDiffPreview(
      'functions.multiedit',
      { paths: [{ path: 'edit.rs', edits: [] }] },
      {
        id: 'tool-result-session-shaped',
        type: 'tool_result',
        content: rawOutput,
        timestamp: Date.now(),
        agentId: 'agent',
        toolCall: {
          tool_call_id: 'functions.multiedit:session-shaped',
          kind: 'functions.multiedit',
          status: 'completed',
          raw_output: rawOutput,
        },
      } as EventItem,
    );

    expect(preview?.type).toBe('diff');
    const patch = preview?.patch ?? '';
    expect((patch.match(/^diff --git /gm) || []).length).toBe(2);
    expect((patch.match(/^@@ /gm) || []).length).toBe(2);
    expect(patch).toContain('@@ -1,11 +1,6 @@');
    expect(patch).toContain('@@ -2,6 +2,7 @@');
    expect(patch).toContain('-struct DiffRegionLine {');
    expect(patch).toContain('+#[derive(Clone, Copy)]');
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
