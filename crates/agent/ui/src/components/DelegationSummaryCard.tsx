import { CheckCircle, ChevronRight, Clock, Loader, XCircle } from 'lucide-react';
import { DelegationGroupInfo, UiAgentInfo } from '../types';
import { getAgentColor } from '../utils/agentColors';
import { getAgentShortName } from '../utils/agentNames';

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
  const messageCount = group.events.filter((event) => event.type === 'agent' && event.isMessage).length;
  const toolCallCount = group.events.filter((event) => event.type === 'tool_call').length;
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
        <span>{messageCount} message{messageCount === 1 ? '' : 's'}</span>
        <span>{toolCallCount} tool{toolCallCount === 1 ? '' : 's'}</span>
      </div>
    </button>
  );
}
