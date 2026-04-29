import type { EventItem } from '../types';
import { normalizeToolName } from './toolSummary';

export type DiffPreviewData = {
  type: 'diff';
  patch: string | null;
  fallbackText?: string;
  additions?: number;
  deletions?: number;
  filePath?: string;
  rawCompactText?: string;
  anchorsFresh?: boolean;
};

const ANCHORED_DIFF_LINE_RE = /^([ +\-])([A-Za-z0-9]{5,8})§/;

export function buildToolDiffPreview(
  toolKind: string | undefined,
  rawInput: unknown,
  resultEvent?: EventItem,
): DiffPreviewData | null {
  if (!rawInput || typeof rawInput !== 'object') return null;

  const normalized = normalizeToolName(toolKind);
  const input = rawInput as Record<string, unknown>;

  if (normalized === 'edit' || normalized === 'multiedit') {
    const rawOutput = typeof resultEvent?.toolCall?.raw_output === 'string'
      ? resultEvent.toolCall.raw_output
      : typeof resultEvent?.content === 'string'
        ? resultEvent.content
        : null;

    if (!rawOutput) {
      return { type: 'diff', patch: null, fallbackText: 'No compact edit diff payload available.' };
    }

    if (!rawOutput.startsWith('OK ')) {
      return { type: 'diff', patch: null, fallbackText: rawOutput };
    }

    const patch = buildPatchFromCompactOutput(rawOutput);
    if (patch) {
      return {
        type: 'diff',
        patch,
        additions: countPatchAdditions(patch),
        deletions: countPatchDeletions(patch),
        filePath: extractFirstCompactPath(rawOutput),
        rawCompactText: rawOutput,
        anchorsFresh: rawOutput.includes('anchors=fresh'),
      };
    }

    return {
      type: 'diff',
      patch: null,
      fallbackText: 'No compact edit diff payload available.',
      rawCompactText: rawOutput,
      anchorsFresh: rawOutput.includes('anchors=fresh'),
    };
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

  if (normalized === 'apply_patch') {
    const rawPatch = extractPatchString(input);
    if (!rawPatch) {
      return { type: 'diff', patch: null, fallbackText: 'No patch payload available.' };
    }

    const sanitizedPatch = stripAnchorsFromPatch(rawPatch);
    if (!isLikelyRenderablePatch(sanitizedPatch)) {
      return { type: 'diff', patch: null, fallbackText: 'Patch payload is malformed. Open details to inspect raw content.' };
    }

    return {
      type: 'diff',
      patch: sanitizedPatch,
      additions: countPatchAdditions(sanitizedPatch),
      deletions: countPatchDeletions(sanitizedPatch),
      filePath: extractPatchFilePath(input),
    };
  }

  return null;
}

export function isLikelyRenderablePatch(patch: string): boolean {
  const trimmed = patch.trim();
  if (trimmed.length === 0) return false;
  return /diff --git\s+.+\s+.+/.test(trimmed) && /@@\s+-\d/.test(trimmed);
}

function extractPatchString(input: Record<string, unknown>): string | null {
  if (typeof input.patch === 'string') return input.patch;
  const args = parseJsonMaybe(input.arguments);
  if (typeof args?.patch === 'string') return args.patch;
  return null;
}

function parseJsonMaybe(value: unknown): any | undefined {
  if (typeof value === 'string') {
    try {
      return JSON.parse(value);
    } catch {
      return undefined;
    }
  }
  if (typeof value === 'object' && value !== null) return value;
  return undefined;
}

function stripAnchorsFromPatch(patch: string): string {
  return patch
    .split('\n')
    .map((line) => stripVisualAnchorFromDiffLine(line))
    .join('\n');
}

function stripVisualAnchorFromDiffLine(line: string): string {
  return line.replace(ANCHORED_DIFF_LINE_RE, '$1');
}

function extractFilePath(input: Record<string, unknown>): string | undefined {
  const path = input.filePath || input.file_path || input.path || input.file;
  return typeof path === 'string' ? path : undefined;
}

function extractPatchFilePath(input: Record<string, unknown>): string | undefined {
  const direct = extractFilePath(input);
  if (direct) return direct;

  const patch = extractPatchString(input);
  if (!patch) return undefined;

  const match = patch.match(/^(?:---|\+\+\+)\s+[ab]\/([^\n\r]+)/m);
  return match?.[1];
}

function extractFirstCompactPath(output: string): string | undefined {
  for (const line of output.split('\n')) {
    if (line.startsWith('P ')) return line.slice(2).trim();
  }
  return undefined;
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
 * Parse canonical compact anchored edit output into a unified diff patch.
 *
 * Format:
 *   OK paths=N edits=N added=N deleted=N
 *   P <path>
 *   H <op> old=<start>,<count> new=<start>,<count>
 *    <anchor>§<context line>
 *   -<anchor>§<old line>
 *   +<anchor>§<new line>
 */
type CompactHunk = {
  oldStart: number;
  oldCount: number;
  newStart: number;
  newCount: number;
  lines: string[];
};

type CompactFilePatch = {
  path: string;
  hunks: CompactHunk[];
};

function buildPatchFromCompactOutput(output: string): string | null {
  const lines = output.split('\n');
  const files: CompactFilePatch[] = [];

  let currentFile: CompactFilePatch | null = null;
  let currentHunk: CompactHunk | null = null;

  function finalizeCurrentHunk() {
    if (!currentFile || !currentHunk || currentHunk.lines.length === 0) return;
    currentHunk.oldCount = currentHunk.lines.filter((line) => line.startsWith(' ') || line.startsWith('-')).length;
    currentHunk.newCount = currentHunk.lines.filter((line) => line.startsWith(' ') || line.startsWith('+')).length;
    currentFile.hunks.push(currentHunk);
    currentHunk = null;
  }

  function finalizeCurrentFile() {
    finalizeCurrentHunk();
    if (!currentFile || currentFile.hunks.length === 0) return;
    files.push(currentFile);
    currentFile = null;
  }

  for (const line of lines) {
    if (line.startsWith('P ')) {
      finalizeCurrentFile();
      currentFile = { path: line.slice(2).trim(), hunks: [] };
      continue;
    }

    if (line.startsWith('H ')) {
      finalizeCurrentHunk();
      const match = line.match(/^H \S+ old=(\d+),(\d+) new=(\d+),(\d+)/);
      if (!match) {
        currentHunk = null;
        continue;
      }
      currentHunk = {
        oldStart: parseInt(match[1], 10),
        oldCount: 0,
        newStart: parseInt(match[3], 10),
        newCount: 0,
        lines: [],
      };
      continue;
    }

    if (line.startsWith(' ') || line.startsWith('-') || line.startsWith('+')) {
      if (!currentHunk) continue;
      currentHunk.lines.push(stripVisualAnchorFromDiffLine(line));
    }
  }

  finalizeCurrentFile();

  const patches: string[] = [];
  for (const file of files) {
    const normalizedPath = normalizePatchPath(file.path);
    const groups = splitIntoRenderableGroups(file.hunks);
    for (const hunks of groups) {
      patches.push(`diff --git a/${normalizedPath} b/${normalizedPath}`);
      patches.push(`--- a/${normalizedPath}`);
      patches.push(`+++ b/${normalizedPath}`);
      for (const hunk of hunks) {
        patches.push(`@@ -${hunk.oldStart},${hunk.oldCount} +${hunk.newStart},${hunk.newCount} @@`);
        patches.push(...hunk.lines);
      }
    }
  }

  if (patches.length === 0) return null;
  return patches.join('\n');
}

function splitIntoRenderableGroups(hunks: CompactHunk[]): CompactHunk[][] {
  if (hunks.length === 0) return [];
  const sorted = [...hunks].sort((left, right) => {
    if (left.oldStart !== right.oldStart) return left.oldStart - right.oldStart;
    return left.newStart - right.newStart;
  });

  const groups: CompactHunk[][] = [];
  let currentGroup: CompactHunk[] = [];
  let previous: CompactHunk | null = null;

  for (const hunk of sorted) {
    if (!previous || hunksCanSharePatch(previous, hunk)) {
      currentGroup.push(hunk);
    } else {
      groups.push(currentGroup);
      currentGroup = [hunk];
    }
    previous = hunk;
  }

  if (currentGroup.length > 0) groups.push(currentGroup);
  return groups;
}

function hunksCanSharePatch(left: CompactHunk, right: CompactHunk): boolean {
  const leftOldEnd = left.oldStart + Math.max(0, left.oldCount - 1);
  const rightOldStart = right.oldStart;
  const leftNewEnd = left.newStart + Math.max(0, left.newCount - 1);
  const rightNewStart = right.newStart;
  return rightOldStart > leftOldEnd && rightNewStart > leftNewEnd;
}
