/**
 * Collapsible activity section showing tool calls within a turn
 */

import { useState } from 'react';
import { ChevronDown, ChevronRight, Loader } from 'lucide-react';
import { EventRow, DelegationGroupInfo, UiAgentInfo } from '../types';
import { isDelegationAwaitingInput } from '../logic/chatViewLogic';
import { ToolSummary } from './ToolSummary';
import { DelegationSummaryCard } from './DelegationSummaryCard';

export interface ActivitySectionProps {
  toolCalls: EventRow[];
  delegations: DelegationGroupInfo[];
  isActive: boolean;
  agents: UiAgentInfo[];
  onToolClick: (event: EventRow) => void;
  onDelegateClick: (delegationId: string) => void;
}

export function ActivitySection({
  toolCalls,
  delegations,
  isActive,
  agents,
  onToolClick,
  onDelegateClick,
}: ActivitySectionProps) {
  const [isExpanded, setIsExpanded] = useState(true); // Always expanded by default

  // Calculate summary
  const totalTools = toolCalls.length;
  const totalDelegations = delegations.length;
  const completedTools = toolCalls.filter(
    t => t.mergedResult?.toolCall?.status === 'completed'
  ).length;
  const failedTools = toolCalls.filter(
    t => t.mergedResult?.toolCall?.status === 'failed'
  ).length;

  if (totalTools === 0 && totalDelegations === 0) {
    return null; // No activity to show
  }

  const toolCallIds = new Set(toolCalls.map((call) => call.toolCall?.tool_call_id ?? call.id));
  const unanchoredDelegations = delegations.filter(
    (delegation) => !toolCallIds.has(delegation.delegateToolCallId)
  );

  return (
    <div className="activity-section my-3">
      {/* Header */}
      <button
        onClick={() => setIsExpanded(!isExpanded)}
        className="flex items-center gap-2 w-full text-left px-3 py-2 rounded-md bg-surface-elevated/30 hover:bg-surface-elevated/50 border border-surface-border/20 transition-colors"
      >
        <span className="text-ui-secondary flex-shrink-0">
          {isExpanded ? (
            <ChevronDown className="w-4 h-4" />
          ) : (
            <ChevronRight className="w-4 h-4" />
          )}
        </span>
        <span className="text-xs font-medium text-ui-secondary uppercase tracking-wider">
          Activity
        </span>
        <span className="text-xs text-ui-muted">
          {totalTools > 0 && `${totalTools} tool${totalTools !== 1 ? 's' : ''}`}
          {totalDelegations > 0 && totalTools > 0 && ', '}
          {totalDelegations > 0 && `${totalDelegations} delegation${totalDelegations !== 1 ? 's' : ''}`}
        </span>
        {isActive && (
          <Loader className="w-3.5 h-3.5 text-accent-tertiary animate-spin ml-auto" />
        )}
        {!isActive && failedTools > 0 && (
          <span className="text-[10px] text-status-warning ml-auto">
            {failedTools} failed
          </span>
        )}
        {!isActive && failedTools === 0 && completedTools > 0 && (
          <span className="text-[10px] text-status-success ml-auto">
            {completedTools}/{totalTools} completed
          </span>
        )}
      </button>

      {/* Expanded content */}
      {isExpanded && (
        <div className="mt-2 space-y-1.5 pl-3">
          {/* Tool calls */}
          {toolCalls.map((toolEvent) => {
            const isDelegate = toolEvent.isDelegateToolCall;
            const delegationGroup = isDelegate && toolEvent.delegationGroupId
              ? delegations.find(d => d.id === toolEvent.delegationGroupId)
              : undefined;
            const isAwaitingInput = delegationGroup
              ? isDelegationAwaitingInput(delegationGroup)
              : false;

            return (
              <div key={toolEvent.id}>
                <ToolSummary
                  event={toolEvent}
                  onClick={() => onToolClick(toolEvent)}
                  isDelegate={isDelegate}
                  isAwaitingInput={isAwaitingInput}
                  onDelegateClick={delegationGroup ? () => onDelegateClick(delegationGroup.id) : undefined}
                />

                {/* Delegation summary below delegate tool */}
                {isDelegate && delegationGroup && (
                  <div className="mt-2 ml-4">
                    <DelegationSummaryCard
                      group={delegationGroup}
                      agents={agents}
                      onOpen={onDelegateClick}
                    />
                  </div>
                )}
              </div>
            );
          })}
          {unanchoredDelegations.length > 0 && (
            <div className="pt-2">
              <div className="text-[10px] text-ui-muted uppercase tracking-wider mb-1">Delegations</div>
              <div className="space-y-2">
                {unanchoredDelegations.map((delegation) => (
                  <DelegationSummaryCard
                    key={delegation.id}
                    group={delegation}
                    agents={agents}
                    onOpen={onDelegateClick}
                  />
                ))}
              </div>
            </div>
          )}
        </div>
      )}
    </div>
  );
}
