import { CheckCircle, Clock, Loader, XCircle } from 'lucide-react';
import { DelegationGroupInfo, Turn, UiAgentInfo } from '../types';
import { TurnCard } from './TurnCard';
import { getAgentColor } from '../utils/agentColors';
import { getAgentShortName } from '../utils/agentNames';

interface DelegationDetailPanelProps {
  delegation?: DelegationGroupInfo;
  turn?: Turn | null;
  agents: UiAgentInfo[];
  onToolClick: (event: Turn['toolCalls'][number]) => void;
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

export function DelegationDetailPanel({
  delegation,
  turn,
  agents,
  onToolClick,
}: DelegationDetailPanelProps) {
  if (!delegation || !turn) {
    return (
      <div className="flex-1 flex items-center justify-center text-gray-500">
        Select a delegation to view details.
      </div>
    );
  }

  const agentId = delegation.targetAgentId ?? delegation.agentId;
  const agentName = agentId ? getAgentShortName(agentId, agents) : 'Sub-agent';
  const agentColor = agentId ? getAgentColor(agentId) : '#b026ff';
  const durationLabel = formatDuration(delegation.startTime, delegation.endTime);
  const objective = delegation.objective ??
    (delegation.delegateEvent.toolCall?.raw_input as { objective?: string } | undefined)?.objective;

  return (
    <div className="flex-1 flex flex-col overflow-hidden">
      <div className="px-6 py-4 border-b border-cyber-border/50 bg-cyber-surface/40">
        <div className="flex items-center gap-2">
          <span
            className="text-xs font-semibold uppercase tracking-wide px-2 py-0.5 rounded"
            style={{
              color: agentColor,
              backgroundColor: `${agentColor}20`,
              border: `1px solid ${agentColor}40`,
            }}
          >
            {agentName}
          </span>
          <span className="flex-shrink-0">
            {delegation.status === 'in_progress' && (
              <Loader className="w-3.5 h-3.5 text-cyber-purple animate-spin" />
            )}
            {delegation.status === 'completed' && (
              <CheckCircle className="w-3.5 h-3.5 text-cyber-lime" />
            )}
            {delegation.status === 'failed' && (
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
        </div>
      </div>
      <div className="flex-1 overflow-y-auto">
        <TurnCard
          turn={turn}
          agents={agents}
          onToolClick={onToolClick}
          onDelegateClick={() => {}}
          isLastUserMessage={false}
          showModelLabel={false}
        />
      </div>
    </div>
  );
}
