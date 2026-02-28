/**
 * Turn card component - groups user prompt, agent responses, and tool activity
 */

import { useRef, useEffect, useState, memo } from 'react';
import { Turn, UiAgentInfo, EventRow, DelegationGroupInfo, LlmConfigDetails, TurnCompaction } from '../types';
import { MessageContent } from './MessageContent';
import { ActivitySection } from './ActivitySection';
import { PinnedUserMessage } from './PinnedUserMessage';
import { ModelConfigPopover } from './ModelConfigPopover';
import { ElicitationCard } from './ElicitationCard';
import { CompactionCard, CompactingIndicator } from './CompactionCard';
import { getAgentShortName } from '../utils/agentNames';
import { colorWithAlpha, getAgentColor } from '../utils/agentColors';
import { useCopyToClipboard } from '../hooks/useCopyToClipboard';
import { Undo2, Redo2, Copy, Check, GitBranchPlus } from 'lucide-react';

export interface TurnCardProps {
  turn: Turn;
  agents: UiAgentInfo[];
  onToolClick: (event: EventRow) => void;
  onDelegateClick: (delegationId: string) => void;
  isLastUserMessage?: boolean;
  showModelLabel?: boolean; // Show model label when session has multiple models
  llmConfigCache?: Record<number, LlmConfigDetails>; // Cached LLM configs
  requestLlmConfig?: (configId: number, callback: (config: LlmConfigDetails) => void) => void;
  activeView?: 'chat' | 'delegations'; // Current view - only show pinned message in chat view
  onUndo?: () => void; // Callback to undo this turn
  onFork?: () => void; // Callback to fork this turn into a new session
  onRedo?: () => void; // Callback to redo this turn
  isUndone?: boolean; // This turn is the top confirmed undone frame (redo available)
  isUndoPending?: boolean; // This turn is the top undo frame waiting for backend confirmation
  isStackedUndone?: boolean; // This turn is undone but blocked by newer undo frames
  revertedFiles?: string[]; // Files that were reverted for the top confirmed frame
  canUndo?: boolean; // Whether undo button should be shown
  isCompacting?: boolean; // Compaction is currently running after this turn
  compactingTokenEstimate?: number; // Estimated tokens being compacted (for live indicator)
}

// Interleaved event item types
interface InterleavedMessage {
  type: 'message';
  event: EventRow;
}

interface InterleavedActivity {
  type: 'activity';
  events: EventRow[];
  delegations: DelegationGroupInfo[];
}

interface InterleavedCompaction {
  type: 'compaction';
  compaction: TurnCompaction;
}

type InterleavedItem = InterleavedMessage | InterleavedActivity | InterleavedCompaction;

/**
 * Interleave agent messages, tool calls, and an optional compaction chronologically
 */
function interleaveEvents(
  messages: EventRow[],
  toolCalls: EventRow[],
  delegations: DelegationGroupInfo[],
  compaction?: TurnCompaction
): InterleavedItem[] {
  // Combine all events with type tags
  type CombinedItem =
    | { kind: 'message'; event: EventRow }
    | { kind: 'tool'; event: EventRow }
    | { kind: 'compaction'; compaction: TurnCompaction };

  const combined: CombinedItem[] = [
    ...messages.map(e => ({ kind: 'message' as const, event: e })),
    ...toolCalls.map(e => ({ kind: 'tool' as const, event: e })),
    ...(compaction ? [{ kind: 'compaction' as const, compaction }] : []),
  ];

  // Sort by timestamp
  combined.sort((a, b) => {
    const ta = a.kind === 'compaction' ? a.compaction.timestamp : a.event.timestamp;
    const tb = b.kind === 'compaction' ? b.compaction.timestamp : b.event.timestamp;
    return ta - tb;
  });

  // Group consecutive tool calls into activity blocks; insert compaction inline
  const result: InterleavedItem[] = [];
  let currentActivityBlock: EventRow[] = [];
  let currentActivityDelegations: DelegationGroupInfo[] = [];

  const flushActivity = () => {
    if (currentActivityBlock.length > 0) {
      result.push({
        type: 'activity',
        events: currentActivityBlock,
        delegations: currentActivityDelegations,
      });
      currentActivityBlock = [];
      currentActivityDelegations = [];
    }
  };

  for (const item of combined) {
    if (item.kind === 'compaction') {
      flushActivity();
      result.push({ type: 'compaction', compaction: item.compaction });
    } else if (item.kind === 'message') {
      flushActivity();
      result.push({ type: 'message', event: item.event });
    } else {
      // tool — add to activity block
      currentActivityBlock.push(item.event);

      // Find matching delegation for this tool call
      if (item.event.isDelegateToolCall && item.event.delegationGroupId) {
        const delegation = delegations.find(d => d.id === item.event.delegationGroupId);
        if (delegation && !currentActivityDelegations.find(d => d.id === delegation.id)) {
          currentActivityDelegations.push(delegation);
        }
      }
    }
  }

  flushActivity();

  return result;
}

export const TurnCard = memo(function TurnCard({
  turn,
  agents,
  onToolClick,
  onDelegateClick,
  isLastUserMessage = false,
  showModelLabel = false,
  llmConfigCache = {},
  requestLlmConfig,
  activeView = 'chat',
  onUndo,
  onFork,
  onRedo,
  isUndone = false,
  isUndoPending = false,
  isStackedUndone = false,
  revertedFiles = [],
  canUndo = false,
  isCompacting = false,
  compactingTokenEstimate,
}: TurnCardProps) {
  const agentName = turn.agentId ? getAgentShortName(turn.agentId, agents) : 'Agent';
  const agentColor = turn.agentId ? getAgentColor(turn.agentId) : undefined;
  const hasUndoOverlay = isUndone || isUndoPending || isStackedUndone;
  const canShowUndoButton = !!onUndo && canUndo && !turn.isActive && !hasUndoOverlay;
  const canShowForkButton =
    !!onFork &&
    (!!turn.userMessage?.messageId || turn.agentMessages.some((message) => !!message.messageId)) &&
    !turn.isActive &&
    !hasUndoOverlay;

  // Interleave messages, tool calls, and compaction chronologically
  const interleaved = interleaveEvents(
    turn.agentMessages,
    turn.toolCalls,
    turn.delegations,
    isCompacting ? undefined : turn.compaction, // compaction goes inline once complete
  );

  // Track pinned state for last user message
  const userMessageRef = useRef<HTMLDivElement>(null);
  const [isPinned, setIsPinned] = useState(false);
  
  // Model config popover state
  const [showConfigPopover, setShowConfigPopover] = useState(false);

  // Copy to clipboard hook
  const { copiedValue: copiedSection, copy: copyToClipboard } = useCopyToClipboard();

  useEffect(() => {
    if (!isLastUserMessage || !userMessageRef.current || !turn.userMessage) return;
    
    const observer = new IntersectionObserver(
      ([entry]) => setIsPinned(!entry.isIntersecting),
      { threshold: 0, rootMargin: '-80px 0px 0px 0px' } // Account for header
    );
    
    observer.observe(userMessageRef.current);
    return () => observer.disconnect();
  }, [isLastUserMessage, turn.userMessage]);

  const handleJumpBack = () => {
    if (userMessageRef.current) {
      userMessageRef.current.scrollIntoView({ behavior: 'smooth', block: 'start' });
    }
  };

  return (
    <div
      className={`turn-card max-w-6xl mx-auto px-2 py-3 group transition-opacity ${
        isStackedUndone ? 'opacity-45' : 'opacity-100'
      }`}
      data-stacked-undone={isStackedUndone ? 'true' : 'false'}
    >
      {/* Pinned user message (appears when scrolled past) - only in chat view */}
      {isPinned && turn.userMessage && activeView === 'chat' && (
        <PinnedUserMessage
          message={turn.userMessage.content}
          timestamp={turn.userMessage.timestamp}
          onJumpBack={handleJumpBack}
        />
      )}

      {/* User message (if present) */}
      {turn.userMessage && (
        <div ref={isLastUserMessage ? userMessageRef : null} className="user-message mb-3">
          <div className="flex items-center gap-2 mb-1">
            <span className="text-xs font-semibold text-accent-secondary uppercase tracking-wide">
              User
            </span>
            <span className="text-[10px] text-ui-muted">
              {new Date(turn.userMessage.timestamp).toLocaleTimeString()}
            </span>
            <button
              onClick={() => copyToClipboard(turn.userMessage!.content, 'user-message')}
              className="opacity-0 group-hover:opacity-100 transition-opacity p-1 rounded hover:bg-surface-canvas/50"
              title="Copy message"
            >
              {copiedSection === 'user-message' ? (
                <Check className="w-3.5 h-3.5 text-status-success" />
              ) : (
                <Copy className="w-3.5 h-3.5 text-ui-secondary hover:text-accent-primary" />
              )}
            </button>
          </div>
          <div className="bg-surface-elevated/60 border border-accent-secondary/15 rounded-lg px-4 py-3">
            <MessageContent content={turn.userMessage.content} />
          </div>
        </div>
      )}

      {/* Agent response */}
      <div className="agent-response">
        <div className="flex items-baseline justify-between gap-2 mb-1">
          {/* Left: agent name, timestamp, thinking indicator */}
          <div className="flex items-baseline gap-2">
            <span
              className="text-xs font-semibold uppercase tracking-wide leading-none"
              style={{ color: agentColor || 'rgb(var(--agent-accent-1-rgb))' }}
            >
              {agentName}
            </span>
            <span className="text-[10px] text-ui-muted leading-none">
              {turn.agentMessages.length > 0
                ? new Date(turn.agentMessages[0].timestamp).toLocaleTimeString()
                : new Date(turn.startTime).toLocaleTimeString()}
            </span>
            {turn.isActive && (
              <span className="text-[10px] text-accent-tertiary leading-none px-1.5 py-px rounded bg-accent-tertiary/10 border border-accent-tertiary/30">
                thinking...
              </span>
            )}
            {/* Copy agent turn button */}
            <button
              onClick={() => {
                const agentContent = turn.agentMessages.map(m => m.content).join('\n\n');
                copyToClipboard(agentContent, 'agent-turn');
              }}
              className="opacity-0 group-hover:opacity-100 transition-opacity p-1 rounded hover:bg-surface-canvas/50"
              title="Copy agent response"
            >
              {copiedSection === 'agent-turn' ? (
                <Check className="w-3.5 h-3.5 text-status-success" />
              ) : (
                <Copy className="w-3.5 h-3.5 text-ui-secondary hover:text-accent-primary" />
              )}
            </button>
          </div>
          {/* Right: model label */}
          {showModelLabel && turn.modelLabel && turn.modelConfigId && requestLlmConfig ? (
            <ModelConfigPopover
              configId={turn.modelConfigId}
              open={showConfigPopover}
              onOpenChange={setShowConfigPopover}
              requestConfig={requestLlmConfig}
              cachedConfig={llmConfigCache[turn.modelConfigId]}
            >
              <button
                type="button"
                className="flex-shrink-0 text-[10px] leading-none px-1.5 py-px rounded bg-surface-elevated/60 border border-surface-border/40 text-ui-secondary truncate max-w-[200px] hover:border-accent-primary/40 hover:text-ui-secondary cursor-pointer transition-colors"
                title={turn.modelLabel}
              >
                {turn.modelLabel}
              </button>
            </ModelConfigPopover>
          ) : showModelLabel && turn.modelLabel ? (
            <span
              className="flex-shrink-0 text-[10px] leading-none px-1.5 py-px rounded bg-surface-elevated/60 border border-surface-border/40 text-ui-secondary truncate max-w-[200px] cursor-default"
              title={turn.modelLabel}
            >
              {turn.modelLabel}
            </span>
          ) : null}
        </div>

        <div
          className="bg-surface-elevated/40 border rounded-lg px-4 py-3 relative"
          style={{
            borderColor: agentColor ? colorWithAlpha(agentColor, 0.22) : 'rgba(var(--agent-accent-1-rgb), 0.14)',
            borderLeftWidth: '3px',
            borderLeftColor: agentColor || 'rgb(var(--agent-accent-1-rgb))',
          }}
        >
          {/* Interleaved content: messages and activities in chronological order */}
          {interleaved.length > 0 ? (
            <div className="space-y-3">
              {interleaved.map((item, idx) => {
                if (item.type === 'message') {
                  return (
                    <div key={item.event.id} className={`${idx > 0 ? 'pt-3 border-t border-surface-border/20' : ''} group/message relative`}>
                      <button
                        onClick={() => copyToClipboard(item.event.content, `message-${item.event.id}`)}
                        className="absolute top-2 right-2 opacity-0 group-hover/message:opacity-100 transition-opacity p-1.5 rounded hover:bg-surface-canvas/50"
                        title="Copy message"
                      >
                        {copiedSection === `message-${item.event.id}` ? (
                          <Check className="w-3.5 h-3.5 text-status-success" />
                        ) : (
                          <Copy className="w-3.5 h-3.5 text-ui-secondary hover:text-accent-primary" />
                        )}
                      </button>

                      {/* Thinking block — collapsed by default, open while actively streaming */}
                      {item.event.thinking && !item.event.isStreamDelta && (
                        <details
                          open={turn.isActive || undefined}
                          className="mb-2 group/thinking"
                        >
                          <summary className="cursor-pointer select-none text-xs text-ui-muted/60 uppercase tracking-widest mb-1 list-none flex items-center gap-1.5">
                            <span className="inline-block w-2 h-2 rounded-full bg-accent-tertiary/50" />
                            Thinking
                          </summary>
                          <div className="text-sm text-ui-muted/70 bg-surface-canvas/30 rounded px-3 py-2 border-l-2 border-accent-tertiary/30 mt-1">
                            <MessageContent content={item.event.thinking} />
                          </div>
                        </details>
                      )}

                      {/* Live thinking accumulator (thinking streaming in, no final text yet) */}
                      {item.event.isStreamDelta && item.event.isThinkingDelta && item.event.thinking && (
                        <div className="text-sm text-ui-muted/70 bg-surface-canvas/30 rounded px-3 py-2 border-l-2 border-accent-tertiary/30 mb-2">
                          <MessageContent content={item.event.thinking} isStreaming />
                        </div>
                      )}

                      <MessageContent content={item.event.content} isStreaming={item.event.isStreamDelta} />
                    </div>
                  );
                } else if (item.type === 'compaction') {
                  return (
                    <CompactionCard
                      key={`compaction-${item.compaction.timestamp}`}
                      tokenEstimate={item.compaction.tokenEstimate}
                      summary={item.compaction.summary}
                      summaryLen={item.compaction.summaryLen}
                      timestamp={item.compaction.timestamp}
                    />
                  );
                } else {
                  return (
                    <div key={`activity-${idx}`}>
                      <ActivitySection
                        toolCalls={item.events}
                        delegations={item.delegations}
                        isActive={false}
                        agents={agents}
                        onToolClick={onToolClick}
                        onDelegateClick={onDelegateClick}
                      />
                      {/* Render elicitation cards for any tool calls with elicitation data */}
                      {item.events.map((event) => event.elicitationData && (
                        <ElicitationCard key={`elicitation-${event.id}`} data={event.elicitationData} />
                      ))}
                    </div>
                  );
                }
              })}
            </div>
          ) : turn.isActive ? (
            <div className="text-sm text-ui-muted italic">Working...</div>
          ) : null}

          {/* Pending undo overlay (top stack frame awaiting confirmation) */}
          {isUndoPending && !isUndone && (
            <div className="absolute inset-0 bg-surface-canvas/90 backdrop-blur-sm rounded-lg flex items-center justify-center p-6">
              <div className="max-w-md w-full text-center">
                <h3 className="text-lg font-semibold text-status-warning mb-2">Undoing Changes...</h3>
                <p className="text-sm text-ui-secondary">Waiting for filesystem snapshot restore to finish.</p>
              </div>
            </div>
          )}

          {/* Undone state overlay (top stack frame) */}
          {isUndone && (
            <div className="absolute inset-0 bg-surface-canvas/90 backdrop-blur-sm rounded-lg flex items-center justify-center p-6">
              <div className="max-w-md w-full space-y-4">
                <div className="text-center">
                  <h3 className="text-lg font-semibold text-status-warning mb-2">Changes Undone</h3>
                  <p className="text-sm text-ui-secondary">
                    {revertedFiles.length > 0
                      ? `${revertedFiles.length} file${revertedFiles.length !== 1 ? 's' : ''} reverted`
                      : 'No filesystem changes were made in this turn'}
                  </p>
                </div>

                {/* File list - only show if there are files */}
                {revertedFiles.length > 0 && (
                  <div className="bg-surface-elevated/60 border border-surface-border/40 rounded-lg p-3 max-h-40 overflow-y-auto">
                    <div className="space-y-1">
                      {revertedFiles.slice(0, 5).map((file, idx) => (
                        <div key={idx} className="text-xs text-ui-secondary font-mono truncate" title={file}>
                          {file}
                        </div>
                      ))}
                      {revertedFiles.length > 5 && (
                        <div className="text-xs text-ui-muted italic">
                          +{revertedFiles.length - 5} more file{revertedFiles.length - 5 !== 1 ? 's' : ''}
                        </div>
                      )}
                    </div>
                  </div>
                )}

                {/* Redo button */}
                {onRedo && (
                  <button
                    onClick={onRedo}
                    className="w-full flex items-center justify-center gap-2 px-4 py-2 rounded-lg bg-accent-primary/10 border border-accent-primary/40 text-accent-primary hover:bg-accent-primary/20 hover:border-accent-primary transition-colors"
                  >
                    <Redo2 className="w-4 h-4" />
                    <span className="text-sm font-medium">Redo Changes</span>
                  </button>
                )}
              </div>
            </div>
          )}

          {/* Stacked undo placeholder (older undone frames) */}
          {isStackedUndone && !isUndone && (
            <div className="absolute top-3 right-3 left-3 pointer-events-none">
              <div className="px-3 py-2 rounded-md bg-status-warning/10 border border-status-warning/30 text-xs text-status-warning text-center">
                Undone in stack. Redo newer undo first.
              </div>
            </div>
          )}
        </div>

        {/* Turn actions - subtly visible, fully visible on hover */}
        {(canShowUndoButton || canShowForkButton) && (
          <div className="mt-2 flex justify-end gap-2 opacity-60 group-hover:opacity-100 transition-opacity">
            {canShowForkButton && (
              <button
                onClick={onFork}
                className="flex items-center gap-1.5 px-2 py-1 rounded text-xs text-ui-secondary hover:text-accent-primary hover:bg-accent-primary/10 border border-transparent hover:border-accent-primary/40 transition-colors"
                title="Fork a new session from this turn"
              >
                <GitBranchPlus className="w-3.5 h-3.5" />
                <span>Fork</span>
              </button>
            )}
            {canShowUndoButton && (
              <button
                onClick={onUndo}
                className="flex items-center gap-1.5 px-2 py-1 rounded text-xs text-ui-secondary hover:text-status-warning hover:bg-status-warning/10 border border-transparent hover:border-status-warning/40 transition-colors"
                title="Undo changes from this turn"
              >
                <Undo2 className="w-3.5 h-3.5" />
                <span>Undo</span>
              </button>
            )}
          </div>
        )}

        {/* Live compacting indicator - shown while compaction is running (always at the end) */}
        {isCompacting && (
          <CompactingIndicator tokenEstimate={compactingTokenEstimate} />
        )}
      </div>
    </div>
  );
});
