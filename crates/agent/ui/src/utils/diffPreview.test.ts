import { describe, expect, it } from 'vitest';

import type { EventItem } from '../types';
import { buildToolDiffPreview } from './diffPreview';

describe('buildToolDiffPreview', () => {
  it('keeps insert_after hunks aligned when compact output includes surrounding context', () => {
    const rawOutput = [
      'OK paths=1 edits=1 added=1 deleted=0',
      'P file.txt',
      'H insert_after old=1,5 new=1,6',
      ' AAA111§a',
      ' BBB222§b',
      '+CCC333§bb',
      ' DDD444§c',
      ' EEE555§d',
      ' FFF666§e',
    ].join('\n');

    const preview = buildToolDiffPreview(
      'functions.edit',
      { filePath: 'file.txt', operation: 'insert_after' },
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

  it('splits overlapping multiedit hunks for the same file into separate diff sections', () => {
    const rawOutput = [
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

    const preview = buildToolDiffPreview(
      'functions.multiedit',
      { paths: [{ path: 'crates/agent/src/anchors/edit.rs', edits: [] }] },
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

  it('renders the exact session-shaped nearby multiedit fixture coherently', () => {
    const rawOutput = [
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
    expect(patch).toContain('-struct DiffRegionLine<' + "'" + 'a> {');
    expect(patch).toContain('+#[derive(Clone, Copy)]');
    expect(patch).not.toContain('DDD444§');
    expect(patch).not.toContain('OOO555§');
  });
});
