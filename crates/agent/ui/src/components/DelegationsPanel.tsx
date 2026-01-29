import { DelegationGroupInfo, UiAgentInfo } from '../types';
import { DelegationSummaryCard } from './DelegationSummaryCard';

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
  if (delegations.length === 0) {
    return (
      <div className="flex items-center justify-center h-full text-gray-500">
        No delegations yet.
      </div>
    );
  }

  return (
    <div className="px-6 py-4 space-y-3 h-full overflow-y-auto">
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
