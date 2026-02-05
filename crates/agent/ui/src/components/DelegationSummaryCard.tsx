import { CheckCircle, ChevronRight, Clock, Loader, XCircle, Cpu, Wrench } from 'lucide-react';
import { DelegationGroupInfo, UiAgentInfo } from '../types';
import { getAgentColor } from '../utils/agentColors';
import { getAgentShortName } from '../utils/agentNames';
import { calculateDelegationStats } from '../utils/statsCalculator';
import { formatTokensAbbrev, formatCost } from '../utils/formatters';

interface DelegationSummaryCardProps {
  group: DelegationGroupInfo;
  agents: UiAgentInfo[];
  onOpen: (delegationId: string) => void;
}

function formatDuration(startTime: number, endTime?: number): string {
  const durationMs = (endTime ?? Date.now()) - startTime;
  const totalSeconds = Math.max(0, Math.floor(durationMs / 1000));
  const seconds = totalSeconds % 60;
  const minutes = Math.floor(totalSeconds / 60) % 60;
  const hours = Math.floor(totalSeconds / 3600);

  if (hours > 0) return `${hours}h ${minutes}m ${seconds}s`;
  if (minutes > 0) return `${minutes}m ${seconds}s`;
  return `${seconds}s`;
}

export function DelegationSummaryCard({ group, agents, onOpen }: DelegationSummaryCardProps) {
  const agentId = group.targetAgentId ?? group.agentId;
  const agentName = agentId ? getAgentShortName(agentId, agents) : 'Sub-agent';
  const agentColor = agentId ? getAgentColor(agentId) : '#b026ff';
  const durationLabel = formatDuration(group.startTime, group.endTime);
  const stats = calculateDelegationStats(group);
  const objective = group.objective ??
    (group.delegateEvent.toolCall?.raw_input as { objective?: string } | undefined)?.objective;

  return (
    <button
      type="button"
      onClick={() => onOpen(group.id)}
      className="w-full text-left rounded-md border border-cyber-border/50 bg-cyber-surface/40 hover:bg-cyber-surface/60 transition-colors px-3 py-2"
    >
      <div className="flex items-center gap-2">
        <span
          className="text-[11px] font-semibold uppercase tracking-wide px-2 py-0.5 rounded"
          style={{
            color: agentColor,
            backgroundColor: `${agentColor}20`,
            border: `1px solid ${agentColor}40`,
          }}
        >
          {agentName}
        </span>
        <span className="flex-shrink-0">
          {group.status === 'in_progress' && (
            <Loader className="w-3.5 h-3.5 text-cyber-purple animate-spin" />
          )}
          {group.status === 'completed' && (
            <CheckCircle className="w-3.5 h-3.5 text-cyber-lime" />
          )}
          {group.status === 'failed' && (
            <XCircle className="w-3.5 h-3.5 text-cyber-orange" />
          )}
        </span>
        <span className="text-xs text-gray-400 truncate flex-1">
          {objective ?? 'Delegated task'}
        </span>
        <span className="text-[10px] text-gray-500 flex items-center gap-1">
          <Clock className="w-3 h-3" />
          {durationLabel}
        </span>
        <ChevronRight className="w-3.5 h-3.5 text-gray-500" />
      </div>
      <div className="mt-1 text-[11px] text-gray-500 flex items-center gap-3">
        {/* Context usage */}
        <span className={`flex items-center gap-1 ${
          (stats.contextPercent ?? 0) >= 80 ? 'text-cyber-orange' :
          (stats.contextPercent ?? 0) >= 70 ? 'text-yellow-500' :
          'text-gray-500'
        }`}>
          <Cpu className="w-3 h-3" />
          {stats.contextPercent !== undefined
            ? `${stats.contextPercent}%`
            : stats.contextTokens > 0
              ? formatTokensAbbrev(stats.contextTokens)
              : '—'}
        </span>
        <span className="flex items-center gap-1">
          <Wrench className="w-3 h-3" />
          {stats.toolCallCount} tool{stats.toolCallCount === 1 ? '' : 's'}
        </span>
        <span>{stats.messageCount} msg{stats.messageCount === 1 ? '' : 's'}</span>
        {stats.costUsd > 0 && (
          <span className="text-cyber-cyan">{formatCost(stats.costUsd)}</span>
        )}
      </div>
    </button>
  );
}
