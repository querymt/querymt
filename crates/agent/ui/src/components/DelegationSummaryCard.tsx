import { CheckCircle, ChevronRight, Clock, Loader, XCircle, Cpu, Wrench } from 'lucide-react';
import { DelegationGroupInfo, UiAgentInfo } from '../types';
import { colorWithAlpha, getAgentColor } from '../utils/agentColors';
import { getAgentShortName } from '../utils/agentNames';
import { calculateDelegationStats } from '../utils/statsCalculator';
import { formatTokensAbbrev, formatCost, formatDurationFromTimestamps } from '../utils/formatters';

interface DelegationSummaryCardProps {
  group: DelegationGroupInfo;
  agents: UiAgentInfo[];
  onOpen: (delegationId: string) => void;
}



export function DelegationSummaryCard({ group, agents, onOpen }: DelegationSummaryCardProps) {
  const agentId = group.targetAgentId ?? group.agentId;
  const agentName = agentId ? getAgentShortName(agentId, agents) : 'Sub-agent';
  const agentColor = agentId ? getAgentColor(agentId) : 'rgb(var(--accent-tertiary-rgb))';
  const durationLabel = formatDurationFromTimestamps(group.startTime, group.endTime);
  const stats = calculateDelegationStats(group);
  const objective = group.objective ??
    (group.delegateEvent.toolCall?.raw_input as { objective?: string } | undefined)?.objective;

  return (
    <button
      type="button"
      onClick={() => onOpen(group.id)}
      className="w-full text-left rounded-md border border-surface-border/50 bg-surface-elevated/40 hover:bg-surface-elevated/60 transition-colors px-3 py-2"
    >
      <div className="flex items-center gap-2">
        <span
          className="text-[11px] font-semibold uppercase tracking-wide px-2 py-0.5 rounded"
          style={{
            color: agentColor,
            backgroundColor: colorWithAlpha(agentColor, 0.12),
            border: `1px solid ${colorWithAlpha(agentColor, 0.24)}`,
          }}
        >
          {agentName}
        </span>
        <span className="flex-shrink-0">
          {group.status === 'in_progress' && (
            <Loader className="w-3.5 h-3.5 text-accent-tertiary animate-spin" />
          )}
          {group.status === 'completed' && (
            <CheckCircle className="w-3.5 h-3.5 text-status-success" />
          )}
          {group.status === 'failed' && (
            <XCircle className="w-3.5 h-3.5 text-status-warning" />
          )}
        </span>
        <span className="text-xs text-ui-secondary truncate flex-1">
          {objective ?? 'Delegated task'}
        </span>
        <span className="text-[10px] text-ui-muted flex items-center gap-1">
          <Clock className="w-3 h-3" />
          {durationLabel}
        </span>
        <ChevronRight className="w-3.5 h-3.5 text-ui-muted" />
      </div>
      <div className="mt-1 text-[11px] text-ui-muted flex items-center gap-3">
        {/* Context usage */}
        <span className={`flex items-center gap-1 ${
          (stats.contextPercent ?? 0) >= 80 ? 'text-status-warning' :
          (stats.contextPercent ?? 0) >= 70 ? 'text-accent-primary' :
          'text-ui-muted'
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
          <span className="text-accent-primary">{formatCost(stats.costUsd)}</span>
        )}
      </div>
    </button>
  );
}
