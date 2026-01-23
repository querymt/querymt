/**
 * Collapsible activity section showing tool calls within a turn
 */

import { useState } from 'react';
import { ChevronDown, ChevronRight, Loader } from 'lucide-react';
import { EventRow, DelegationGroupInfo, UiAgentInfo } from '../types';
import { ToolSummary } from './ToolSummary';
import { DelegationAccordion } from './DelegationAccordion';

export interface ActivitySectionProps {
  toolCalls: EventRow[];
  delegations: DelegationGroupInfo[];
  isActive: boolean;
  agents: UiAgentInfo[];
  onToolClick: (event: EventRow) => void;
  onDelegateClick: (delegationId: string) => void;
  renderEvent: (event: EventRow) => React.ReactNode;
}

export function ActivitySection({
  toolCalls,
  delegations,
  isActive,
  agents,
  onToolClick,
  onDelegateClick,
  renderEvent,
}: ActivitySectionProps) {
  const [isExpanded, setIsExpanded] = useState(isActive); // Auto-expand for active turns
  const [expandedDelegations, setExpandedDelegations] = useState<Set<string>>(new Set());

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

  const handleDelegationToggle = (delegationId: string) => {
    setExpandedDelegations(prev => {
      const next = new Set(prev);
      if (next.has(delegationId)) {
        next.delete(delegationId);
      } else {
        next.add(delegationId);
      }
      return next;
    });
  };

  return (
    <div className="activity-section my-3">
      {/* Header */}
      <button
        onClick={() => setIsExpanded(!isExpanded)}
        className="flex items-center gap-2 w-full text-left px-3 py-2 rounded-md bg-cyber-surface/30 hover:bg-cyber-surface/50 border border-cyber-border/30 transition-colors"
      >
        <span className="text-gray-400 flex-shrink-0">
          {isExpanded ? (
            <ChevronDown className="w-4 h-4" />
          ) : (
            <ChevronRight className="w-4 h-4" />
          )}
        </span>
        <span className="text-xs font-medium text-gray-400 uppercase tracking-wider">
          Activity
        </span>
        <span className="text-xs text-gray-500">
          {totalTools > 0 && `${totalTools} tool${totalTools !== 1 ? 's' : ''}`}
          {totalDelegations > 0 && totalTools > 0 && ', '}
          {totalDelegations > 0 && `${totalDelegations} delegation${totalDelegations !== 1 ? 's' : ''}`}
        </span>
        {isActive && (
          <Loader className="w-3.5 h-3.5 text-cyber-purple animate-spin ml-auto" />
        )}
        {!isActive && failedTools > 0 && (
          <span className="text-[10px] text-cyber-orange ml-auto">
            {failedTools} failed
          </span>
        )}
        {!isActive && failedTools === 0 && completedTools > 0 && (
          <span className="text-[10px] text-cyber-lime ml-auto">
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

            return (
              <div key={toolEvent.id}>
                <ToolSummary
                  event={toolEvent}
                  onClick={() => onToolClick(toolEvent)}
                  isDelegate={isDelegate}
                  onDelegateClick={delegationGroup ? () => onDelegateClick(delegationGroup.id) : undefined}
                />

                {/* Delegation accordion below delegate tool */}
                {isDelegate && delegationGroup && (
                  <div className="mt-1.5 ml-4">
                    <DelegationAccordion
                      group={delegationGroup}
                      agents={agents}
                      isExpanded={expandedDelegations.has(delegationGroup.id)}
                      onToggle={() => handleDelegationToggle(delegationGroup.id)}
                      onToolClick={onToolClick}
                      renderEvent={renderEvent}
                      isActive={delegationGroup.status === 'in_progress'}
                    />
                  </div>
                )}
              </div>
            );
          })}
        </div>
      )}
    </div>
  );
}
