import { DelegationGroupInfo, Turn, UiAgentInfo } from '../types';
import { DelegationDetailPanel } from './DelegationDetailPanel';
import { DelegationsPanel } from './DelegationsPanel';

interface DelegationsViewProps {
  delegations: DelegationGroupInfo[];
  agents: UiAgentInfo[];
  activeDelegationId?: string | null;
  activeTurn?: Turn | null;
  onSelectDelegation: (delegationId: string) => void;
  onToolClick: (event: Turn['toolCalls'][number]) => void;
}

export function DelegationsView({
  delegations,
  agents,
  activeDelegationId,
  activeTurn,
  onSelectDelegation,
  onToolClick,
}: DelegationsViewProps) {
  return (
    <div className="flex flex-col lg:flex-row h-full overflow-hidden">
      <div className="w-full lg:w-80 border-b lg:border-b-0 lg:border-r border-cyber-border/50 bg-cyber-bg/40">
        <DelegationsPanel
          delegations={delegations}
          agents={agents}
          activeDelegationId={activeDelegationId}
          onOpenDelegation={onSelectDelegation}
        />
      </div>
      <DelegationDetailPanel
        delegation={delegations.find((group) => group.id === activeDelegationId)}
        turn={activeTurn}
        agents={agents}
        onToolClick={onToolClick}
      />
    </div>
  );
}
