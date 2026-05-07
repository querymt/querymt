import type { EventItem } from '../types';
import { normalizeToolName } from './toolSummary';

export type DiffPreviewData = {
  type: 'diff';
  patch: string | null;
  fallbackText?: string;
  additions?: number;
  deletions?: number;
  filePath?: string;
};

export function buildToolDiffPreview(
  toolKind: string | undefined,
  rawInput: unknown,
  resultEvent?: EventItem,
): DiffPreviewData | null {
  if (!rawInput || typeof rawInput !== 'object') return null;

  const normalized = normalizeToolName(toolKind);
  const input = rawInput as Record<string, unknown>;

  if (normalized === 'edit' || normalized === 'multiedit') {
    // Check for error in output first
    const rawOutput = typeof resultEvent?.toolCall?.raw_output === 'string'
      ? resultEvent.toolCall.raw_output
      : typeof resultEvent?.content === 'string'
        ? resultEvent.content
        : null;

    if (rawOutput && !rawOutput.startsWith('OK ')) {
      return { type: 'diff', patch: null, fallbackText: rawOutput };
    }

    // Build diff from tool input (oldString / newString)
    const patch = buildPatchFromEditInput(input);
    if (patch) {
      return {
        type: 'diff',
        patch,
        additions: countPatchAdditions(patch),
        deletions: countPatchDeletions(patch),
        filePath: extractFilePath(input),
      };
    }

    return { type: 'diff', patch: null, fallbackText: rawOutput || 'Edit diff unavailable.' };
  }

  if (normalized === 'write' || normalized === 'write_file') {
    const filePath = String(input.filePath || input.file_path || input.path || 'file');
    const content = input.content;
    if (typeof content !== 'string') return null;

    const normalizedPath = normalizePatchPath(filePath);
    const newLines = content.split('\n').length;
    const newBlock = content.split('\n').map((line) => `+${line}`).join('\n');
    const patch = [
      `diff --git a/${normalizedPath} b/${normalizedPath}`,
      'new file mode 100644',
      '--- /dev/null',
      `+++ b/${normalizedPath}`,
      `@@ -0,0 +1,${newLines} @@`,
      newBlock,
    ].join('\n');

    return {
      type: 'diff',
      patch,
      additions: countPatchAdditions(patch),
      deletions: countPatchDeletions(patch),
      filePath,
    };
  }

  return null;
}

function extractFilePath(input: Record<string, unknown>): string | undefined {
  const path = input.filePath || input.file_path || input.path || input.file;
  return typeof path === 'string' ? path : undefined;
}

function normalizePatchPath(path: string): string {
  return path.replace(/^\/+/, '') || 'file';
}

function countPatchAdditions(patch: string): number {
  return (patch.match(/^\+[^+]/gm) || []).length;
}

function countPatchDeletions(patch: string): number {
  return (patch.match(/^-[^-]/gm) || []).length;
}

/**
 * Build a unified diff patch from edit/multiedit tool input.
 * Uses oldString/newString from the tool arguments.
 */
function buildPatchFromEditInput(input: Record<string, unknown>): string | null {
  const filePath = extractFilePath(input);
  if (!filePath) return null;
  const normalizedPath = normalizePatchPath(filePath);

  const edits: Array<{ oldString: string; newString: string }> = [];

  // multiedit: { filePath, edits: [{ oldString, newString }] }
  const editsArray = input.edits;
  if (Array.isArray(editsArray)) {
    for (const edit of editsArray) {
      if (edit && typeof edit === 'object') {
        const oldStr = String(edit.oldString ?? '');
        const newStr = String(edit.newString ?? '');
        edits.push({ oldString: oldStr, newString: newStr });
      }
    }
  } else {
    // edit: { filePath, oldString, newString }
    const oldStr = String(input.oldString ?? '');
    const newStr = String(input.newString ?? '');
    if (!oldStr && !newStr) return null;
    edits.push({ oldString: oldStr, newString: newStr });
  }

  if (edits.length === 0) return null;

  const hunks: string[] = [];
  let lineOffset = 0;

  for (const { oldString, newString } of edits) {
    const oldLines = oldString.split('\n');
    const newLines = newString.split('\n');
    const oldStart = lineOffset + 1;
    const newStart = lineOffset + 1;

    const hunkLines: string[] = [];
    for (const line of oldLines) hunkLines.push(`-${line}`);
    for (const line of newLines) hunkLines.push(`+${line}`);

    hunks.push(
      `@@ -${oldStart},${oldLines.length} +${newStart},${newLines.length} @@`,
      ...hunkLines,
    );

    lineOffset += Math.max(oldLines.length, newLines.length);
  }

  return [
    `diff --git a/${normalizedPath} b/${normalizedPath}`,
    `--- a/${normalizedPath}`,
    `+++ b/${normalizedPath}`,
    ...hunks,
  ].join('\n');
}


