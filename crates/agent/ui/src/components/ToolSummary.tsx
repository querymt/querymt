/**
 * Compact tool card component - shows minimal summary, click to expand in modal
 */

import { memo } from 'react';
import { Loader, CheckCircle, XCircle, ChevronRight } from 'lucide-react';
import { generateToolSummary } from '../utils/toolSummary';
import { EventItem } from '../types';

export interface ToolSummaryProps {
  event: EventItem & { mergedResult?: EventItem };
  onClick: () => void;
  isDelegate?: boolean;
  onDelegateClick?: () => void;
}

export const ToolSummary = memo(function ToolSummary({ event, onClick, isDelegate, onDelegateClick }: ToolSummaryProps) {
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

  const handleClick = () => {
    if (isDelegate && onDelegateClick) {
      // For delegates, clicking the main card scrolls to delegation
      // but we add a small "details" area for the modal
      onDelegateClick();
    } else {
      onClick();
    }
  };

  return (
    <div
      className={`
        group flex items-center gap-2 px-3 py-1.5 rounded-md cursor-pointer
        transition-all duration-200
        bg-cyber-surface/60 border border-cyber-border/50
        hover:bg-cyber-surface hover:border-cyber-cyan/40 hover:shadow-[0_0_10px_rgba(0,255,249,0.15)]
        ${isDelegate ? 'border-l-2 border-l-cyber-purple' : ''}
        ${isFailed ? 'border-l-2 border-l-cyber-orange' : ''}
      `}
      onClick={handleClick}
    >
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
        <span className="flex-shrink-0 text-[10px] font-mono px-1.5 py-0.5 rounded bg-cyber-bg/80 border border-cyber-border/50">
          <span className="text-cyber-lime">+{summary.diffStats.additions}</span>
          <span className="text-gray-500 mx-0.5">/</span>
          <span className="text-cyber-magenta">-{summary.diffStats.deletions}</span>
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
  );
});

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
