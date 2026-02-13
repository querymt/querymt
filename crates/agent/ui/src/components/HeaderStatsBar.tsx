/**
 * HeaderStatsBar - Compact inline stats display for the header
 * Shows: elapsed time | context usage | tool calls | cost
 * Click to open StatsDrawer (Phase 4)
 */

import { useMemo } from 'react';
import { Clock, Cpu, Wrench, DollarSign } from 'lucide-react';
import { EventItem, SessionLimits } from '../types';
import { calculateStats } from '../utils/statsCalculator';
import { formatDurationCompact, formatTokensAbbrev, formatCost, formatPercentage } from '../utils/formatters';

interface HeaderStatsBarProps {
  events: EventItem[];
  globalElapsedMs: number;
  isSessionActive: boolean;
  agentModels: Record<string, { provider?: string; model?: string; contextLimit?: number }>;
  sessionLimits?: SessionLimits | null;
  onClick?: () => void;
}

export function HeaderStatsBar({
  events,
  globalElapsedMs,
  isSessionActive,
  agentModels,
  sessionLimits,
  onClick,
}: HeaderStatsBarProps) {
  const { session, perAgent } = useMemo(() => calculateStats(events, sessionLimits), [events, sessionLimits]);
  
  // Enhance perAgent stats with context limits from agentModels if missing
  const enrichedPerAgent = useMemo(() => {
    return perAgent.map(agentStats => ({
      ...agentStats,
      maxContextTokens: agentStats.maxContextTokens ?? agentModels[agentStats.agentId]?.contextLimit
    }));
  }, [perAgent, agentModels]);
  
  // Don't show stats bar if there are no actual session events
  if (session.totalMessages === 0 && session.totalToolCalls === 0) {
    return null;
  }
  
  // Calculate context usage (single agent or aggregate)
  const totalContextTokens = enrichedPerAgent.reduce((sum, a) => sum + a.currentContextTokens, 0);
  const totalMaxContext = enrichedPerAgent.reduce((sum, a) => sum + (a.maxContextTokens || 0), 0);
  
  // Context display - show percentage instead of raw tokens
  // TODO: The 80% warning threshold is hardcoded to match the backend's ContextMiddleware default.
  // This should be exposed via SessionLimits (context_warn_at_percent) in a future enhancement.
  const contextDisplay = totalMaxContext > 0
    ? formatPercentage(totalContextTokens, totalMaxContext)
    : formatTokensAbbrev(totalContextTokens);
  
  // Cost display with limit if configured
  const costDisplay = session.limits?.max_cost_usd
    ? `${formatCost(session.totalCostUsd)}/${formatCost(session.limits.max_cost_usd)}`
    : formatCost(session.totalCostUsd);
  
  // Context usage percentage for color coding
  // Use 80% as the critical threshold (matches backend ContextConfig.warn_at_percent default)
  const contextPercent = totalMaxContext > 0 ? (totalContextTokens / totalMaxContext) * 100 : 0;
  const costPercent = session.limits?.max_cost_usd ? (session.totalCostUsd / session.limits.max_cost_usd) * 100 : 0;
  
  return (
    <div
      onClick={onClick}
      className={`flex items-center gap-3 px-3 py-1.5 rounded-lg border border-cyber-border/40 bg-cyber-surface/50 text-xs font-mono transition-colors ${
        onClick ? 'cursor-pointer hover:border-cyber-cyan/60 hover:bg-cyber-surface/80' : ''
      }`}
      title="Click for detailed stats"
    >
      {/* Elapsed Time */}
      <div className="flex items-center gap-1.5">
        <Clock className={`w-3.5 h-3.5 ${isSessionActive ? 'text-cyber-cyan animate-pulse' : 'text-ui-muted'}`} />
        <span className="text-ui-secondary">{formatDurationCompact(globalElapsedMs)}</span>
      </div>
      
      <span className="text-cyber-border">│</span>
      
      {/* Context Usage */}
      <div className="flex items-center gap-1.5">
        <Cpu className={`w-3.5 h-3.5 ${
          contextPercent >= 80 ? 'text-cyber-orange' : 
          contextPercent >= 70 ? 'text-cyber-cyan' : 
          'text-ui-muted'
        }`} />
        <span className={`${
          contextPercent >= 80 ? 'text-cyber-orange' : 
          contextPercent >= 70 ? 'text-cyber-cyan' : 
          'text-ui-secondary'
        }`}>
          {contextDisplay}
        </span>
      </div>
      
      <span className="text-cyber-border">│</span>
      
      {/* Tool Calls */}
      <div className="flex items-center gap-1.5">
        <Wrench className="w-3.5 h-3.5 text-ui-muted" />
        <span className="text-ui-secondary">{session.totalToolCalls}</span>
      </div>
      
      <span className="text-cyber-border">│</span>
      
      {/* Cost */}
      <div className="flex items-center gap-1.5">
        <DollarSign className={`w-3.5 h-3.5 ${
          costPercent > 90 ? 'text-cyber-orange' : 
          costPercent > 70 ? 'text-cyber-cyan' : 
          'text-ui-muted'
        }`} />
        <span className={`${
          costPercent > 90 ? 'text-cyber-orange' : 
          costPercent > 70 ? 'text-cyber-cyan' : 
          'text-cyber-cyan'
        }`}>
          {costDisplay}
        </span>
      </div>
    </div>
  );
}
