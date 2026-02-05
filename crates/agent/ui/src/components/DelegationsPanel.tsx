import { useMemo } from 'react';
import { Layers, Wrench, DollarSign } from 'lucide-react';
import { DelegationGroupInfo, UiAgentInfo } from '../types';
import { DelegationSummaryCard } from './DelegationSummaryCard';
import { calculateDelegationStats } from '../utils/statsCalculator';
import { formatCost } from '../utils/formatters';

interface DelegationsPanelProps {
  delegations: DelegationGroupInfo[];
  agents: UiAgentInfo[];
  activeDelegationId?: string | null;
  onOpenDelegation: (delegationId: string) => void;
}

export function DelegationsPanel({
  delegations,
  agents,
  activeDelegationId,
  onOpenDelegation,
}: DelegationsPanelProps) {
  const aggregateStats = useMemo(() => {
    let totalToolCalls = 0;
    let totalCost = 0;
    let totalMessages = 0;
    let maxContextPercent = 0;

    for (const group of delegations) {
      const s = calculateDelegationStats(group);
      totalToolCalls += s.toolCallCount;
      totalCost += s.costUsd;
      totalMessages += s.messageCount;
      if (s.contextPercent !== undefined && s.contextPercent > maxContextPercent) {
        maxContextPercent = s.contextPercent;
      }
    }

    return { totalToolCalls, totalCost, totalMessages, maxContextPercent };
  }, [delegations]);

  if (delegations.length === 0) {
    return (
      <div className="flex items-center justify-center h-full text-gray-500">
        No delegations yet.
      </div>
    );
  }

  return (
    <div className="px-6 py-4 space-y-3 h-full overflow-y-auto">
      {/* Aggregate stats */}
      <div className="flex items-center gap-3 text-[10px] text-gray-500 pb-2 mb-1 border-b border-cyber-border/30">
        <span className="flex items-center gap-1">
          <Layers className="w-3 h-3" />
          {delegations.length} delegation{delegations.length === 1 ? '' : 's'}
        </span>
        <span className="flex items-center gap-1">
          <Wrench className="w-3 h-3" />
          {aggregateStats.totalToolCalls} tool call{aggregateStats.totalToolCalls === 1 ? '' : 's'}
        </span>
        {aggregateStats.totalCost > 0 && (
          <span className="flex items-center gap-1 text-cyber-cyan">
            <DollarSign className="w-3 h-3" />
            {formatCost(aggregateStats.totalCost)}
          </span>
        )}
      </div>
      {delegations.map((group) => (
        <div
          key={group.id}
          className={activeDelegationId === group.id ? 'ring-1 ring-cyber-cyan/50 rounded-md' : ''}
        >
          <DelegationSummaryCard
            group={group}
            agents={agents}
            onOpen={onOpenDelegation}
          />
        </div>
      ))}
    </div>
  );
}
