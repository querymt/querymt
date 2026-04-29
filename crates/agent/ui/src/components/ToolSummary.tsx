/**
 * Compact tool card component - shows summary with inline preview for key tools
 */

import { memo, useState, useMemo, useRef, useEffect, Component, ErrorInfo, ReactNode } from 'react';
import { Loader, CheckCircle, XCircle, ChevronRight, ChevronDown, Eye, Pause } from 'lucide-react';
import { PatchDiff } from '@pierre/diffs/react';
import { generateToolSummary, normalizeToolName } from '../utils/toolSummary';
import { EventItem } from '../types';
import { getDashboardThemeVariant, getDiffThemeForDashboard } from '../utils/dashboardThemes';
import { buildToolDiffPreview, type DiffPreviewData } from '../utils/diffPreview';

type DiffRenderGuardProps = {
  children: ReactNode;
  fallback?: ReactNode;
};

type DiffRenderGuardState = {
  hasError: boolean;
};

class DiffRenderGuard extends Component<DiffRenderGuardProps, DiffRenderGuardState> {
  state: DiffRenderGuardState = { hasError: false };

  static getDerivedStateFromError(): DiffRenderGuardState {
    return { hasError: true };
  }

  componentDidCatch(_error: Error, _errorInfo: ErrorInfo) {
    // Keep this local to avoid blanking the whole chat if a single diff is malformed.
  }

  render() {
    if (this.state.hasError) {
      return this.props.fallback ?? (
        <div className="px-3 py-2 text-[10px] text-status-warning">
          Unable to render diff preview. Open details to inspect raw content.
        </div>
      );
    }
    return this.props.children;
  }
}

/**
 * Defers rendering of the expensive PatchDiff component until the container
 * scrolls into (or near) the viewport.  Uses IntersectionObserver with a
 * generous rootMargin so the diff is ready slightly before the user sees it.
 */
function LazyPatchDiff(props: React.ComponentProps<typeof PatchDiff> & { className?: string }) {
  const { className, ...diffProps } = props;
  const containerRef = useRef<HTMLDivElement>(null);
  const [visible, setVisible] = useState(false);

  useEffect(() => {
    const el = containerRef.current;
    if (!el) return;

    const observer = new IntersectionObserver(
      ([entry]) => {
        if (entry.isIntersecting) {
          setVisible(true);
          observer.disconnect();
        }
      },
      { rootMargin: '200px' }, // start rendering 200px before entering viewport
    );
    observer.observe(el);
    return () => observer.disconnect();
  }, []);

  return (
    <div ref={containerRef} className={className}>
      {visible ? (
        <DiffRenderGuard>
          <PatchDiff {...diffProps} />
        </DiffRenderGuard>
      ) : (
        <div className="h-16 flex items-center justify-center text-[10px] text-ui-muted">
          Loading diff...
        </div>
      )}
    </div>
  );
}

export interface ToolSummaryProps {
  event: EventItem & { mergedResult?: EventItem };
  onClick: () => void;
  isDelegate?: boolean;
  isAwaitingInput?: boolean;
  onDelegateClick?: () => void;
  /** Passed down from parent to avoid per-instance store subscription. */
  isMobile?: boolean;
  /** Passed down from parent to avoid per-instance store subscription. */
  selectedTheme?: string;
}

export const ToolSummary = memo(function ToolSummary({
  event,
  onClick,
  isDelegate,
  isAwaitingInput,
  onDelegateClick,
  isMobile = false,
  selectedTheme = 'cyber-noir',
}: ToolSummaryProps) {
  const diffTheme = getDiffThemeForDashboard(selectedTheme);
  const diffThemeType = getDashboardThemeVariant(selectedTheme);
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
  const normalized = normalizeToolName(toolKind || toolName);
  const isEdit = normalized === 'edit' || normalized === 'multiedit';
  const isPatch = normalized === 'apply_patch';
  const isWrite = normalized === 'write' || normalized === 'write_file';
  const isShell = normalized === 'shell' || normalized === 'bash';
  const hasInlinePreview = isEdit || isPatch || isWrite || isShell;
  const [showPreview, setShowPreview] = useState(!isMobile && (isEdit || isPatch || isWrite)); // Auto-expand diffs (collapsed on mobile)
  const diffContainerClass = `event-diff-container${isMobile ? ' diff-mobile' : ''}`;
  const legacyDiffStats = summary.diffStats;

  // Build preview data
  const previewData = useMemo(() => {
    if (!hasInlinePreview) return null;

    if (isEdit || isPatch || isWrite) {
      return buildToolDiffPreview(toolKind || toolName, rawInput, event.mergedResult);
    }

    if (isShell && hasMergedResult && event.mergedResult) {
      return buildShellPreview(event.mergedResult);
    }

    return null;
  }, [hasInlinePreview, isEdit, isPatch, isWrite, isShell, toolKind, rawInput, event.mergedResult, hasMergedResult]);

  const diffStats = previewData?.type === 'diff' && (previewData.additions !== undefined || previewData.deletions !== undefined)
    ? {
        additions: previewData.additions ?? 0,
        deletions: previewData.deletions ?? 0,
        filePath: previewData.filePath,
      }
    : legacyDiffStats;

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
    <div className="rounded-md border border-surface-border/35 overflow-hidden">
      {/* Summary row */}
      <div
        className={`
          group flex items-center gap-2 px-3 py-1.5 cursor-pointer
          transition-all duration-200
          bg-surface-elevated/60
          hover:bg-surface-elevated hover:border-accent-primary/40
          ${isDelegate ? 'border-l-2 border-l-accent-tertiary' : ''}
          ${isFailed ? 'border-l-2 border-l-status-warning' : ''}
        `}
        onClick={handleClick}
      >
        {/* Preview toggle for tools with inline preview */}
        {hasInlinePreview && previewData && (
          <button
            onClick={handlePreviewToggle}
            className="flex-shrink-0 p-0.5 rounded hover:bg-surface-canvas/50 text-ui-secondary hover:text-accent-primary transition-colors"
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
        <span className="flex-1 text-xs text-ui-secondary truncate font-mono">
          {summary.keyParam ? (
            <>
              <span className="text-accent-primary">{summary.name}</span>
              <span className="text-ui-muted">: </span>
              <span className="text-ui-secondary">{summary.keyParam}</span>
            </>
          ) : (
            <span className="text-accent-primary">{summary.name}</span>
          )}
        </span>

        {/* Diff stats badge */}
        {diffStats && (diffStats.additions > 0 || diffStats.deletions > 0) && (
          <span className="flex-shrink-0 text-[10px] font-mono px-1.5 py-0.5 rounded bg-surface-canvas/80 border border-surface-border/35">
            <span className="text-status-success">+{diffStats.additions}</span>
            <span className="text-ui-muted mx-0.5">/</span>
            <span className="text-accent-secondary">-{diffStats.deletions}</span>
          </span>
        )}


        {/* Shell exit code badge (inline) */}
        {isShell && previewData?.type === 'shell' && previewData.exitCode !== undefined && (
          <span className={`flex-shrink-0 text-[10px] font-mono px-1.5 py-0.5 rounded bg-surface-canvas/80 border border-surface-border/35 ${
            previewData.exitCode === 0 ? 'text-status-success' : 'text-status-warning'
          }`}>
            exit {previewData.exitCode}
          </span>
        )}

        {/* Delegation awaiting-input badge */}
        {isDelegate && isAwaitingInput && (
          <span className="flex-shrink-0 inline-flex items-center gap-1 text-[10px] font-medium px-1.5 py-0.5 rounded bg-status-warning/10 border border-status-warning/35 text-status-warning">
            <Pause className="w-3 h-3" />
            awaiting input
          </span>
        )}

        {/* Status indicator */}
        <span className="flex-shrink-0">
          {isInProgress && (
            <Loader className="w-3.5 h-3.5 text-accent-tertiary animate-spin" />
          )}
          {isCompleted && (
            <CheckCircle className="w-3.5 h-3.5 text-status-success" />
          )}
          {isFailed && (
            <XCircle className="w-3.5 h-3.5 text-status-warning" />
          )}
        </span>

        {/* Expand indicator */}
        <ChevronRight className="w-3.5 h-3.5 text-ui-muted group-hover:text-accent-primary transition-colors flex-shrink-0" />

        {/* Delegate link indicator */}
        {isDelegate && (
          <span
            className="flex-shrink-0 text-[9px] uppercase tracking-wider text-accent-tertiary px-1.5 py-0.5 rounded bg-accent-tertiary/10 border border-accent-tertiary/30"
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
          className="border-t border-surface-border/20 bg-surface-canvas/30 cursor-pointer"
          onClick={handleClick}
          title="Click for full details"
        >
          {previewData.type === 'diff' && previewData.patch && (
            <LazyPatchDiff
              className={diffContainerClass}
              patch={previewData.patch}
              options={{
                theme: diffTheme,
                themeType: diffThemeType,
                diffStyle: isMobile ? 'unified' : 'split',
                diffIndicators: 'bars',
                lineDiffType: 'word-alt',
                overflow: 'wrap',
                disableLineNumbers: false,
                useCSSClasses: true,
                disableBackground: true,
              }}
            />
          )}
          {previewData.type === 'diff' && !previewData.patch && (
            <div className="px-3 py-2 text-[10px] text-status-warning font-mono whitespace-pre-wrap break-words">
              {previewData.fallbackText ?? 'Diff preview unavailable.'}
            </div>
          )}
          {previewData.type === 'shell' && (
            <div className="px-3 py-2 font-mono text-[11px] max-h-64 overflow-auto">
              {previewData.stdout && (
                <pre className="whitespace-pre-wrap break-words text-ui-secondary leading-tight">
                  {truncateOutput(previewData.stdout, 500)}
                </pre>
              )}
              {previewData.stderr && (
                <pre className="whitespace-pre-wrap break-words text-status-warning/70 leading-tight mt-1">
                  {truncateOutput(previewData.stderr, 300)}
                </pre>
              )}
              {!previewData.stdout && !previewData.stderr && (
                <span className="text-ui-muted italic text-[10px]">No output</span>
              )}
            </div>
          )}
        </div>
      )}
    </div>
  );
});


type ShellPreviewData = { type: 'shell'; stdout?: string; stderr?: string; exitCode?: number };
type PreviewData = DiffPreviewData | ShellPreviewData;
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
        ${isCompleted ? 'bg-status-success/10 text-status-success border border-status-success/30' : ''}
        ${isFailed ? 'bg-status-warning/10 text-status-warning border border-status-warning/30' : ''}
        ${!isCompleted && !isFailed ? 'bg-accent-tertiary/10 text-accent-tertiary border border-accent-tertiary/30' : ''}
      `}
    >
      {isCompleted && <CheckCircle className="w-2.5 h-2.5" />}
      {isFailed && <XCircle className="w-2.5 h-2.5" />}
      {status}
    </span>
  );
}
