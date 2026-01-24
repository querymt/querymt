/**
 * Turn card component - groups user prompt, agent responses, and tool activity
 */

import { useRef, useEffect, useState } from 'react';
import { Turn, UiAgentInfo, EventRow, DelegationGroupInfo, LlmConfigDetails } from '../types';
import { MessageContent } from './MessageContent';
import { ActivitySection } from './ActivitySection';
import { PinnedUserMessage } from './PinnedUserMessage';
import { ModelConfigPopover } from './ModelConfigPopover';
import { getAgentShortName } from '../utils/agentNames';
import { getAgentColor } from '../utils/agentColors';

export interface TurnCardProps {
  turn: Turn;
  agents: UiAgentInfo[];
  onToolClick: (event: EventRow) => void;
  onDelegateClick: (delegationId: string) => void;
  renderEvent: (event: EventRow) => React.ReactNode;
  isLastUserMessage?: boolean;
  showModelLabel?: boolean; // Show model label when session has multiple models
  llmConfigCache?: Record<number, LlmConfigDetails>; // Cached LLM configs
  requestLlmConfig?: (configId: number, callback: (config: LlmConfigDetails) => void) => void;
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

export function TurnCard({
  turn,
  agents,
  onToolClick,
  onDelegateClick,
  renderEvent,
  isLastUserMessage = false,
  showModelLabel = false,
  llmConfigCache = {},
  requestLlmConfig,
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
  const modelLabelRef = useRef<HTMLButtonElement>(null);

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
    <div className="turn-card max-w-4xl mx-auto px-4 py-3">
      {/* Pinned user message (appears when scrolled past) */}
      {isPinned && turn.userMessage && (
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
            <span className="text-xs font-semibold text-cyber-magenta uppercase tracking-wide">
              User
            </span>
            <span className="text-[10px] text-gray-500">
              {new Date(turn.userMessage.timestamp).toLocaleTimeString()}
            </span>
          </div>
          <div className="bg-cyber-surface/60 border border-cyber-magenta/20 rounded-lg px-4 py-3">
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
              style={{ color: agentColor || '#00fff9' }}
            >
              {agentName}
            </span>
            <span className="text-[10px] text-gray-500 leading-none">
              {turn.agentMessages.length > 0
                ? new Date(turn.agentMessages[0].timestamp).toLocaleTimeString()
                : new Date(turn.startTime).toLocaleTimeString()}
            </span>
            {turn.isActive && (
              <span className="text-[10px] text-cyber-purple leading-none px-1.5 py-px rounded bg-cyber-purple/10 border border-cyber-purple/30">
                thinking...
              </span>
            )}
          </div>
          {/* Right: model label */}
          {showModelLabel && turn.modelLabel && (
            <div className="relative flex-shrink-0">
              <button
                ref={modelLabelRef}
                type="button"
                onClick={() => turn.modelConfigId && requestLlmConfig && setShowConfigPopover(true)}
                className={`text-[10px] leading-none px-1.5 py-px rounded bg-cyber-surface/60 border border-cyber-border/40 text-gray-400 truncate max-w-[200px] ${
                  turn.modelConfigId && requestLlmConfig
                    ? 'hover:border-cyber-cyan/40 hover:text-gray-300 cursor-pointer transition-colors'
                    : 'cursor-default'
                }`}
                title={turn.modelLabel}
                disabled={!turn.modelConfigId || !requestLlmConfig}
              >
                {turn.modelLabel}
              </button>
              {showConfigPopover && turn.modelConfigId && requestLlmConfig && (
                <ModelConfigPopover
                  configId={turn.modelConfigId}
                  anchorRef={modelLabelRef}
                  onClose={() => setShowConfigPopover(false)}
                  requestConfig={requestLlmConfig}
                  cachedConfig={llmConfigCache[turn.modelConfigId]}
                />
              )}
            </div>
          )}
        </div>

        <div
          className="bg-cyber-surface/40 border rounded-lg px-4 py-3"
          style={{
            borderColor: agentColor ? `${agentColor}40` : 'rgba(0, 255, 249, 0.2)',
            borderLeftWidth: '3px',
            borderLeftColor: agentColor,
          }}
        >
          {/* Interleaved content: messages and activities in chronological order */}
          {interleaved.length > 0 ? (
            <div className="space-y-3">
              {interleaved.map((item, idx) => {
                if (item.type === 'message') {
                  return (
                    <div key={item.event.id} className={idx > 0 ? 'pt-3 border-t border-cyber-border/30' : ''}>
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
                        renderEvent={renderEvent}
                      />
                    </div>
                  );
                }
              })}
            </div>
          ) : turn.isActive ? (
            <div className="text-sm text-gray-500 italic">Working...</div>
          ) : null}
        </div>
      </div>
    </div>
  );
}
