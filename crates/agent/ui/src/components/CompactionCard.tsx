/**
 * CompactionCard - displays AI compaction results and live compacting indicator
 *
 * Shown between turns after the agent response card to indicate that the
 * conversation context was summarized by the AI to free up the context window.
 */

import { useState } from 'react';
import { Layers, ChevronDown, ChevronUp, Loader } from 'lucide-react';
import { MessageContent } from './MessageContent';

// ─── Completed compaction card ────────────────────────────────────────────────

export interface CompactionCardProps {
  tokenEstimate: number;
  summary: string;
  summaryLen: number;
  timestamp: number;
}

export function CompactionCard({
  tokenEstimate,
  summary,
  summaryLen,
  timestamp,
}: CompactionCardProps) {
  const [expanded, setExpanded] = useState(true);

  const formattedTokens = tokenEstimate > 0
    ? `~${tokenEstimate.toLocaleString()} tokens`
    : null;
  const formattedChars = `${summaryLen.toLocaleString()} chars`;
  const time = new Date(timestamp).toLocaleTimeString();

  return (
    <div className="compaction-card mx-auto max-w-6xl px-2 py-1">
      {/* Divider line with centered label */}
      <div className="relative flex items-center gap-3 mb-2">
        <div className="flex-1 h-px bg-accent-tertiary/20" />
        <div className="flex items-center gap-1.5 text-[10px] text-accent-tertiary/70 uppercase tracking-widest font-medium select-none">
          <Layers className="w-3 h-3" />
          <span>Context Compacted</span>
        </div>
        <div className="flex-1 h-px bg-accent-tertiary/20" />
      </div>

      {/* Card body */}
      <div className="rounded-lg border border-accent-tertiary/20 bg-accent-tertiary/5 overflow-hidden">
        {/* Header row */}
        <button
          onClick={() => setExpanded(v => !v)}
          className="w-full flex items-center justify-between gap-3 px-4 py-2.5 hover:bg-accent-tertiary/10 transition-colors text-left"
        >
          <div className="flex items-center gap-2 min-w-0">
            <Layers className="w-3.5 h-3.5 text-accent-tertiary flex-shrink-0" />
            <span className="text-xs text-accent-tertiary font-medium">
              Conversation summarized
            </span>
            {formattedTokens && (
              <span className="text-[10px] text-ui-muted">
                {formattedTokens} → {formattedChars}
              </span>
            )}
          </div>
          <div className="flex items-center gap-2 flex-shrink-0">
            <span className="text-[10px] text-ui-muted">{time}</span>
            {expanded
              ? <ChevronUp className="w-3.5 h-3.5 text-ui-muted" />
              : <ChevronDown className="w-3.5 h-3.5 text-ui-muted" />
            }
          </div>
        </button>

        {/* Expanded summary body */}
        {expanded && (
          <div className="px-4 pb-3 pt-1 border-t border-accent-tertiary/15">
            <div className="text-xs text-ui-secondary/80 mb-1.5 uppercase tracking-widest font-medium">
              Summary
            </div>
            <div className="text-sm text-ui-secondary leading-relaxed">
              <MessageContent content={summary} />
            </div>
          </div>
        )}
      </div>
    </div>
  );
}

// ─── Live compacting indicator ────────────────────────────────────────────────

export interface CompactingIndicatorProps {
  tokenEstimate?: number;
}

export function CompactingIndicator({ tokenEstimate }: CompactingIndicatorProps) {
  const formattedTokens = tokenEstimate && tokenEstimate > 0
    ? ` (~${tokenEstimate.toLocaleString()} tokens)`
    : '';

  return (
    <div className="compacting-indicator mx-auto max-w-6xl px-2 py-1">
      {/* Divider line with centered label */}
      <div className="relative flex items-center gap-3 mb-2">
        <div className="flex-1 h-px bg-accent-tertiary/20" />
        <div className="flex items-center gap-1.5 text-[10px] text-accent-tertiary/70 uppercase tracking-widest font-medium select-none">
          <Loader className="w-3 h-3 animate-spin" />
          <span>Compacting Context</span>
        </div>
        <div className="flex-1 h-px bg-accent-tertiary/20" />
      </div>

      {/* Pulsing status row */}
      <div className="flex items-center gap-2.5 px-4 py-2.5 rounded-lg border border-accent-tertiary/20 bg-accent-tertiary/5">
        <div className="w-2 h-2 rounded-full bg-accent-tertiary animate-pulse flex-shrink-0" />
        <span className="text-xs text-accent-tertiary">
          Summarizing conversation history{formattedTokens}...
        </span>
      </div>
    </div>
  );
}
