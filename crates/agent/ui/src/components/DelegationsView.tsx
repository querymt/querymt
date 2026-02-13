import { PanelLeftClose, PanelLeftOpen } from 'lucide-react';
import { DelegationGroupInfo, Turn, UiAgentInfo, LlmConfigDetails } from '../types';
import { DelegationDetailPanel } from './DelegationDetailPanel';
import { DelegationsPanel } from './DelegationsPanel';
import { useUiStore } from '../store/uiStore';
import { getAgentColor } from '../utils/agentColors';
import { getAgentShortName } from '../utils/agentNames';

interface DelegationsViewProps {
  delegations: DelegationGroupInfo[];
  agents: UiAgentInfo[];
  activeDelegationId?: string | null;
  activeTurn?: Turn | null;
  onSelectDelegation: (delegationId: string) => void;
  onToolClick: (event: Turn['toolCalls'][number]) => void;
  llmConfigCache?: Record<number, LlmConfigDetails>;
  requestLlmConfig?: (configId: number, callback: (config: LlmConfigDetails) => void) => void;
}

export function DelegationsView({
  delegations,
  agents,
  activeDelegationId,
  activeTurn,
  onSelectDelegation,
  onToolClick,
  llmConfigCache,
  requestLlmConfig,
}: DelegationsViewProps) {
  const { delegationsPanelCollapsed: panelCollapsed, setDelegationsPanelCollapsed: setPanelCollapsed } = useUiStore();
  
  return (
    <div className="flex flex-col lg:flex-row h-full overflow-hidden">
      {/* Collapsible left panel */}
      <div className={`
        border-b lg:border-b-0 lg:border-r border-cyber-border/50 bg-cyber-bg/40
        transition-all duration-200 flex-shrink-0
        ${panelCollapsed ? 'w-full lg:w-12' : 'w-full lg:w-80'}
      `}>
        {panelCollapsed ? (
          <div className="h-full flex flex-col items-center pt-2 gap-1.5">
            {/* Expand button */}
            <button
              onClick={() => setPanelCollapsed(false)}
              className="p-1.5 rounded text-ui-muted hover:text-cyber-cyan transition-colors"
              title="Show delegations list"
            >
              <PanelLeftOpen className="w-4 h-4" />
            </button>
            
            {/* Divider */}
            <div className="w-6 border-t border-cyber-border/30 my-1" />
            
            {/* Mini delegation indicators */}
            <div className="flex-1 overflow-y-auto flex flex-col items-center gap-1 px-1">
              {delegations.map((group) => {
                const agentId = group.targetAgentId ?? group.agentId;
                const agentName = agentId ? getAgentShortName(agentId, agents) : 'Sub-agent';
                const agentColor = agentId ? getAgentColor(agentId) : 'rgb(var(--cyber-purple-rgb))';
                const isActive = activeDelegationId === group.id;
                const initial = agentName.charAt(0).toUpperCase();
                const objective = group.objective ?? 'Delegated task';
                const tooltipText = `${agentName}: ${objective.length > 60 ? objective.slice(0, 57) + '...' : objective}`;
                
                return (
                  <button
                    key={group.id}
                    onClick={() => onSelectDelegation(group.id)}
                    title={tooltipText}
                    className={`
                      relative w-8 h-8 rounded-md flex items-center justify-center
                      text-[11px] font-bold transition-all duration-150
                      ${isActive 
                        ? 'ring-1 ring-cyber-cyan bg-cyber-cyan/10' 
                        : 'hover:bg-white/5'}
                    `}
                    style={{ color: agentColor }}
                  >
                    {initial}
                    {/* Status dot */}
                    <span className={`absolute -top-0.5 -right-0.5 w-2 h-2 rounded-full border border-cyber-bg ${
                      group.status === 'completed' ? 'bg-cyber-lime' :
                      group.status === 'failed' ? 'bg-cyber-orange' :
                      'bg-cyber-purple animate-pulse'
                    }`} />
                  </button>
                );
              })}
            </div>
          </div>
        ) : (
          <div className="h-full flex flex-col">
            <div className="flex justify-end px-2 pt-2 flex-shrink-0">
              <button
                onClick={() => setPanelCollapsed(true)}
                className="p-1 rounded text-ui-muted hover:text-cyber-cyan transition-colors"
                title="Hide delegations list"
              >
                <PanelLeftClose className="w-4 h-4" />
              </button>
            </div>
            <div className="flex-1 overflow-hidden">
              <DelegationsPanel
                delegations={delegations}
                agents={agents}
                activeDelegationId={activeDelegationId}
                onOpenDelegation={onSelectDelegation}
              />
            </div>
          </div>
        )}
      </div>
      <DelegationDetailPanel
        delegation={delegations.find((group) => group.id === activeDelegationId)}
        turn={activeTurn}
        agents={agents}
        onToolClick={onToolClick}
        llmConfigCache={llmConfigCache}
        requestLlmConfig={requestLlmConfig}
      />
    </div>
  );
}
