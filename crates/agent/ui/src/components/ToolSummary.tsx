/**
 * Compact tool card component - shows summary with inline preview for key tools
 */

import { memo, useState, useMemo } from 'react';
import { Loader, CheckCircle, XCircle, ChevronRight, ChevronDown, Eye } from 'lucide-react';
import { PatchDiff } from '@pierre/diffs/react';
import { generateToolSummary } from '../utils/toolSummary';
import { EventItem } from '../types';
import { useUiStore } from '../store/uiStore';
import { getDiffThemeForDashboard } from '../utils/dashboardThemes';

export interface ToolSummaryProps {
  event: EventItem & { mergedResult?: EventItem };
  onClick: () => void;
  isDelegate?: boolean;
  onDelegateClick?: () => void;
}

export const ToolSummary = memo(function ToolSummary({ event, onClick, isDelegate, onDelegateClick }: ToolSummaryProps) {
  const selectedTheme = useUiStore((state) => state.selectedTheme);
  const diffTheme = getDiffThemeForDashboard(selectedTheme);
  const toolKind = event.toolCall?.kind;
  const toolName = inferToolName(event);
  const rawInput = parseJsonMaybe(event.toolCall?.raw_input) ?? event.toolCall?.raw_input;
  
  const summary = generateToolSummary(toolKind, toolName, rawInput);
  
  // Determine status
  const hasMergedResult = 'mergedResult' in event && event.mergedResult;
  const status = hasMergedResult
    ? event.mergedResult?.toolCall?.status
    : event.toolCall?.status;
  const isInProgress = !hasMergedResult && !status;
  const isCompleted = status === 'completed';
  const isFailed = status === 'failed';

  // Inline preview state - auto-expand for edits/patches
  const normalized = (toolKind || toolName || '').toLowerCase().replace(/^mcp_/, '');
  const isEdit = normalized === 'edit';
  const isPatch = normalized === 'apply_patch';
  const isWrite = normalized === 'write' || normalized === 'write_file';
  const isShell = normalized === 'shell' || normalized === 'bash';
  const hasInlinePreview = isEdit || isPatch || isWrite || isShell;
  const [showPreview, setShowPreview] = useState(isEdit || isPatch || isWrite); // Auto-expand diffs

  // Build preview data
  const previewData = useMemo(() => {
    if (!hasInlinePreview) return null;

    if ((isEdit || isPatch || isWrite) && rawInput && typeof rawInput === 'object') {
      return buildDiffPreview(toolKind, rawInput as Record<string, unknown>);
    }

    if (isShell && hasMergedResult && event.mergedResult) {
      return buildShellPreview(event.mergedResult);
    }

    return null;
  }, [hasInlinePreview, isEdit, isPatch, isWrite, isShell, toolKind, rawInput, hasMergedResult, event.mergedResult]);

  const handleClick = () => {
    if (isDelegate && onDelegateClick) {
      onDelegateClick();
    } else {
      onClick();
    }
  };

  const handlePreviewToggle = (e: React.MouseEvent) => {
    e.stopPropagation();
    setShowPreview(!showPreview);
  };

  return (
    <div className="rounded-md border border-cyber-border/35 overflow-hidden">
      {/* Summary row */}
      <div
        className={`
          group flex items-center gap-2 px-3 py-1.5 cursor-pointer
          transition-all duration-200
          bg-cyber-surface/60
          hover:bg-cyber-surface hover:border-cyber-cyan/40
          ${isDelegate ? 'border-l-2 border-l-cyber-purple' : ''}
          ${isFailed ? 'border-l-2 border-l-cyber-orange' : ''}
        `}
        onClick={handleClick}
      >
        {/* Preview toggle for tools with inline preview */}
        {hasInlinePreview && previewData && (
          <button
            onClick={handlePreviewToggle}
            className="flex-shrink-0 p-0.5 rounded hover:bg-cyber-bg/50 text-gray-400 hover:text-cyber-cyan transition-colors"
            title={showPreview ? 'Hide preview' : 'Show preview'}
          >
            {showPreview ? (
              <ChevronDown className="w-3.5 h-3.5" />
            ) : (
              <Eye className="w-3.5 h-3.5" />
            )}
          </button>
        )}

        {/* Icon */}
        <span className="text-base flex-shrink-0" title={summary.name}>
          {summary.icon}
        </span>

        {/* Summary text */}
        <span className="flex-1 text-xs text-gray-300 truncate font-mono">
          {summary.keyParam ? (
            <>
              <span className="text-cyber-cyan">{summary.name}</span>
              <span className="text-gray-500">: </span>
              <span className="text-gray-400">{summary.keyParam}</span>
            </>
          ) : (
            <span className="text-cyber-cyan">{summary.name}</span>
          )}
        </span>

        {/* Diff stats badge */}
        {summary.diffStats && (summary.diffStats.additions > 0 || summary.diffStats.deletions > 0) && (
          <span className="flex-shrink-0 text-[10px] font-mono px-1.5 py-0.5 rounded bg-cyber-bg/80 border border-cyber-border/35">
            <span className="text-cyber-lime">+{summary.diffStats.additions}</span>
            <span className="text-gray-500 mx-0.5">/</span>
            <span className="text-cyber-magenta">-{summary.diffStats.deletions}</span>
          </span>
        )}

        {/* Shell exit code badge (inline) */}
        {isShell && previewData?.type === 'shell' && previewData.exitCode !== undefined && (
          <span className={`flex-shrink-0 text-[10px] font-mono px-1.5 py-0.5 rounded bg-cyber-bg/80 border border-cyber-border/35 ${
            previewData.exitCode === 0 ? 'text-cyber-lime' : 'text-cyber-orange'
          }`}>
            exit {previewData.exitCode}
          </span>
        )}

        {/* Status indicator */}
        <span className="flex-shrink-0">
          {isInProgress && (
            <Loader className="w-3.5 h-3.5 text-cyber-purple animate-spin" />
          )}
          {isCompleted && (
            <CheckCircle className="w-3.5 h-3.5 text-cyber-lime" />
          )}
          {isFailed && (
            <XCircle className="w-3.5 h-3.5 text-cyber-orange" />
          )}
        </span>

        {/* Expand indicator */}
        <ChevronRight className="w-3.5 h-3.5 text-gray-500 group-hover:text-cyber-cyan transition-colors flex-shrink-0" />

        {/* Delegate link indicator */}
        {isDelegate && (
          <span
            className="flex-shrink-0 text-[9px] uppercase tracking-wider text-cyber-purple px-1.5 py-0.5 rounded bg-cyber-purple/10 border border-cyber-purple/30"
            onClick={(e) => {
              e.stopPropagation();
              onClick(); // Show modal for delegate details
            }}
          >
            details
          </span>
        )}
      </div>

      {/* Inline preview */}
      {showPreview && previewData && (
        <div
          className="border-t border-cyber-border/20 bg-cyber-bg/30 cursor-pointer"
          onClick={handleClick}
          title="Click for full details"
        >
          {previewData.type === 'diff' && previewData.patch && (
            <div className="event-diff-container m-0 border-0 text-[11px]">
                <PatchDiff
                  patch={previewData.patch}
                  options={{
                    theme: diffTheme,
                    themeType: 'dark',
                  diffStyle: 'split',
                  diffIndicators: 'bars',
                  lineDiffType: 'word-alt',
                  overflow: 'wrap',
                  disableLineNumbers: false,
                  useCSSClasses: true,
                  disableBackground: true,
                }}
              />
            </div>
          )}
          {previewData.type === 'shell' && (
            <div className="px-3 py-2 font-mono text-[11px] max-h-64 overflow-auto">
              {previewData.stdout && (
                <pre className="whitespace-pre-wrap break-words text-gray-300 leading-tight">
                  {truncateOutput(previewData.stdout, 500)}
                </pre>
              )}
              {previewData.stderr && (
                <pre className="whitespace-pre-wrap break-words text-cyber-orange/70 leading-tight mt-1">
                  {truncateOutput(previewData.stderr, 300)}
                </pre>
              )}
              {!previewData.stdout && !previewData.stderr && (
                <span className="text-gray-500 italic text-[10px]">No output</span>
              )}
            </div>
          )}
        </div>
      )}
    </div>
  );
});

// Build diff preview data for edit/patch tools
type DiffPreviewData = { type: 'diff'; patch: string | null };
type ShellPreviewData = { type: 'shell'; stdout?: string; stderr?: string; exitCode?: number };
type PreviewData = DiffPreviewData | ShellPreviewData;

function buildDiffPreview(toolKind: string | undefined, input: Record<string, unknown>): PreviewData | null {
  const normalized = (toolKind || '').toLowerCase().replace(/^mcp_/, '');
  
  if (normalized === 'edit') {
    const filePath = (input.filePath || input.file_path || input.path || 'file') as string;
    const oldString = String(input.oldString || input.old_string || '');
    const newString = String(input.newString || input.new_string || '');
    
    if (oldString || newString) {
      const normalizedPath = filePath.replace(/^\/+/, '') || 'file';
      const oldLines = oldString.split('\n').length;
      const newLines = newString.split('\n').length;
      const oldBlock = oldString.split('\n').map(line => `-${line}`).join('\n');
      const newBlock = newString.split('\n').map(line => `+${line}`).join('\n');
      const patch = [
        `diff --git a/${normalizedPath} b/${normalizedPath}`,
        `--- a/${normalizedPath}`,
        `+++ b/${normalizedPath}`,
        `@@ -1,${oldLines} +1,${newLines} @@`,
        oldBlock,
        newBlock,
      ].join('\n');
      return { type: 'diff', patch };
    }
  }
  
  if (normalized === 'write' || normalized === 'write_file') {
    const filePath = (input.filePath || input.file_path || input.path || 'file') as string;
    const content = input.content;
    if (typeof content === 'string') {
      const normalizedPath = filePath.replace(/^\/+/, '') || 'file';
      const newLines = content.split('\n').length;
      const newBlock = content.split('\n').map(line => `+${line}`).join('\n');
      const patch = [
        `diff --git a/${normalizedPath} b/${normalizedPath}`,
        `new file mode 100644`,
        `--- /dev/null`,
        `+++ b/${normalizedPath}`,
        `@@ -0,0 +1,${newLines} @@`,
        newBlock,
      ].join('\n');
      return { type: 'diff', patch };
    }
  }
  
  if (normalized === 'apply_patch') {
    const patch = typeof input.patch === 'string' ? input.patch : null;
    // Also check nested arguments
    if (!patch && input.arguments) {
      const args = parseJsonMaybe(input.arguments);
      if (args?.patch) return { type: 'diff', patch: args.patch };
    }
    return { type: 'diff', patch };
  }
  
  return null;
}

// Build shell output preview
function buildShellPreview(resultEvent: EventItem): PreviewData | null {
  const rawOutput = resultEvent.toolCall?.raw_output ?? resultEvent.content;
  const parsed = parseJsonMaybe(rawOutput);
  
  const stdout = typeof parsed?.stdout === 'string' ? parsed.stdout : '';
  const stderr = typeof parsed?.stderr === 'string' ? parsed.stderr : '';
  const exitCode = typeof parsed?.exit_code === 'number' ? parsed.exit_code : undefined;
  
  return { type: 'shell', stdout, stderr, exitCode };
}

// Truncate long output for preview
function truncateOutput(text: string, maxLen: number): string {
  if (text.length <= maxLen) return text;
  return text.slice(0, maxLen) + '\n… (truncated)';
}

// Helper: parse tool name from event
function inferToolName(event: EventItem): string | undefined {
  const toolCallId = event.toolCall?.tool_call_id;
  if (typeof toolCallId === 'string' && toolCallId.includes(':')) {
    const name = toolCallId.split(':')[0];
    if (name) return name;
  }
  const desc = event.toolCall?.description;
  if (typeof desc === 'string') {
    const match = desc.match(/run\s+([a-z0-9_.:-]+)/i);
    if (match?.[1]) return match[1];
  }
  return event.toolCall?.kind;
}

// Helper: safely parse JSON
function parseJsonMaybe(value: unknown): any | undefined {
  if (typeof value === 'string') {
    try {
      return JSON.parse(value);
    } catch {
      return undefined;
    }
  }
  if (typeof value === 'object' && value !== null) {
    return value;
  }
  return undefined;
}

/**
 * Minimal inline status for tool results (used in delegation summaries)
 */
export function ToolStatusBadge({ status }: { status?: string }) {
  if (!status) return null;
  
  const isCompleted = status === 'completed';
  const isFailed = status === 'failed';
  
  return (
    <span
      className={`
        inline-flex items-center gap-1 text-[10px] px-1.5 py-0.5 rounded
        ${isCompleted ? 'bg-cyber-lime/10 text-cyber-lime border border-cyber-lime/30' : ''}
        ${isFailed ? 'bg-cyber-orange/10 text-cyber-orange border border-cyber-orange/30' : ''}
        ${!isCompleted && !isFailed ? 'bg-cyber-purple/10 text-cyber-purple border border-cyber-purple/30' : ''}
      `}
    >
      {isCompleted && <CheckCircle className="w-2.5 h-2.5" />}
      {isFailed && <XCircle className="w-2.5 h-2.5" />}
      {status}
    </span>
  );
}
