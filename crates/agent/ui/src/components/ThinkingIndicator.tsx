import { UiAgentInfo } from '../types';
import { getAgentColor } from '../utils/agentColors';
import { getAgentShortName } from '../utils/agentNames';

interface ThinkingIndicatorProps {
  agentId: string | null;
  agents: UiAgentInfo[];
}

export function ThinkingIndicator({ agentId, agents }: ThinkingIndicatorProps) {
  const color = agentId ? getAgentColor(agentId) : 'rgb(var(--agent-accent-1-rgb))';
  const name = agentId ? getAgentShortName(agentId, agents) : 'Agent';

  return (
    <div className="flex items-center gap-2.5 px-5 py-1.5 border-t border-surface-border/40 animate-fade-in">
      <div
        className="w-2 h-2 rounded-full animate-pulse"
        style={{ backgroundColor: color }}
      />
      <span className="text-xs text-ui-muted">
        <span style={{ color }} className="font-medium">{name}</span>
        {' '}is thinking...
      </span>
    </div>
  );
}
