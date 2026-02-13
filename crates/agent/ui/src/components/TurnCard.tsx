/**
 * Turn card component - groups user prompt, agent responses, and tool activity
 */

import { useRef, useEffect, useState, memo } from 'react';
import { Turn, UiAgentInfo, EventRow, DelegationGroupInfo, LlmConfigDetails } from '../types';
import { MessageContent } from './MessageContent';
import { ActivitySection } from './ActivitySection';
import { PinnedUserMessage } from './PinnedUserMessage';
import { ModelConfigPopover } from './ModelConfigPopover';
import { ElicitationCard } from './ElicitationCard';
import { getAgentShortName } from '../utils/agentNames';
import { colorWithAlpha, getAgentColor } from '../utils/agentColors';
import { useCopyToClipboard } from '../hooks/useCopyToClipboard';
import { Undo2, Redo2, Copy, Check } from 'lucide-react';

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
  onRedo?: () => void; // Callback to redo this turn
  isUndone?: boolean; // This turn was undone
  revertedFiles?: string[]; // Files that were reverted
  canUndo?: boolean; // Whether undo button should be shown
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

type InterleavedItem = InterleavedMessage | InterleavedActivity;

/**
 * Interleave agent messages and tool calls chronologically
 */
function interleaveEvents(
  messages: EventRow[],
  toolCalls: EventRow[],
  delegations: DelegationGroupInfo[]
): InterleavedItem[] {
  // Combine all events with type tags
  const combined: Array<{ event: EventRow; isMessage: boolean }> = [
    ...messages.map(e => ({ event: e, isMessage: true })),
    ...toolCalls.map(e => ({ event: e, isMessage: false })),
  ];

  // Sort by timestamp
  combined.sort((a, b) => a.event.timestamp - b.event.timestamp);

  // Group consecutive tool calls into activity blocks
  const result: InterleavedItem[] = [];
  let currentActivityBlock: EventRow[] = [];
  let currentActivityDelegations: DelegationGroupInfo[] = [];

  for (const item of combined) {
    if (item.isMessage) {
      // Flush any pending activity block
      if (currentActivityBlock.length > 0) {
        result.push({
          type: 'activity',
          events: currentActivityBlock,
          delegations: currentActivityDelegations,
        });
        currentActivityBlock = [];
        currentActivityDelegations = [];
      }
      // Add message
      result.push({
        type: 'message',
        event: item.event,
      });
    } else {
      // Add to activity block
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

  // Flush final activity block
  if (currentActivityBlock.length > 0) {
    result.push({
      type: 'activity',
      events: currentActivityBlock,
      delegations: currentActivityDelegations,
    });
  }

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
  onRedo,
  isUndone = false,
  revertedFiles = [],
  canUndo = false,
}: TurnCardProps) {
  const agentName = turn.agentId ? getAgentShortName(turn.agentId, agents) : 'Agent';
  const agentColor = turn.agentId ? getAgentColor(turn.agentId) : undefined;

  // Interleave messages and tool calls chronologically
  const interleaved = interleaveEvents(turn.agentMessages, turn.toolCalls, turn.delegations);

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
    <div className="turn-card max-w-6xl mx-auto px-2 py-3 group">
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
                      <MessageContent content={item.event.content} />
                    </div>
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

          {/* Undone state overlay */}
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
        </div>

        {/* Undo button - subtly visible, fully visible on hover */}
        {canUndo && onUndo && !isUndone && !turn.isActive && (
          <div className="mt-2 flex justify-end opacity-60 group-hover:opacity-100 transition-opacity">
            <button
              onClick={onUndo}
              className="flex items-center gap-1.5 px-2 py-1 rounded text-xs text-ui-secondary hover:text-status-warning hover:bg-status-warning/10 border border-transparent hover:border-status-warning/40 transition-colors"
              title="Undo changes from this turn"
            >
              <Undo2 className="w-3.5 h-3.5" />
              <span>Undo</span>
            </button>
          </div>
        )}
      </div>
    </div>
  );
});
