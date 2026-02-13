import { CheckCircle } from 'lucide-react';
import { UiAgentInfo } from '../types';
import { getAgentColor } from '../utils/agentColors';
import { getAgentShortName } from '../utils/agentNames';

interface ThinkingIndicatorProps {
  agentId: string | null;
  agents: UiAgentInfo[];
  isComplete?: boolean;
}

export function ThinkingIndicator({ agentId, agents, isComplete = false }: ThinkingIndicatorProps) {
  const color = agentId ? getAgentColor(agentId) : 'rgb(var(--agent-accent-1-rgb))';
  const name = agentId ? getAgentShortName(agentId, agents) : 'Agent';

  if (isComplete) {
    return (
      <div className="flex items-center gap-3 px-6 py-2 bg-surface-elevated/80 border-t border-surface-border/50 animate-fade-in">
        <CheckCircle 
          className="w-4 h-4"
          style={{ color: 'rgb(var(--status-success-rgb))' }}
        />
        <span className="text-sm text-ui-secondary">
          <span style={{ color }} className="font-medium">{name}</span>
          {' '}response complete
        </span>
      </div>
    );
  }

  return (
    <div className="flex items-center gap-3 px-6 py-2 bg-surface-elevated/80 border-t border-surface-border/50 animate-fade-in">
      <div
        className="w-3 h-3 rounded-full animate-pulse"
        style={{
          backgroundColor: color,
        }}
      />
      <span className="text-sm text-ui-secondary">
        <span style={{ color }} className="font-medium">{name}</span>
        {' '}is thinking...
      </span>
    </div>
  );
}
