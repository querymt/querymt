import { useMemo, useState } from 'react';
import { X, ChevronDown, ChevronRight } from 'lucide-react';
import { EventItem, UiAgentInfo, SessionLimits } from '../types';
import { calculateStats } from '../utils/statsCalculator';
import { getAgentColor } from '../utils/agentColors';
import { getAgentDisplayName } from '../utils/agentNames';
import { TodoStats } from '../hooks/useTodoState';
import { formatDuration, formatCost, formatTokensAbbrev, formatPercentage } from '../utils/formatters';

/**
 * StatsDrawer - Top-sliding drawer for detailed session statistics
 * 
 * Phase 4: Slides down from below header when stats bar is clicked
 * - Shows session overview (elapsed time, context, tools, cost)
 * - Expert mode toggle shows per-agent breakdown
 * - Replaces FloatingStatsPanel
 */

interface StatsDrawerProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  events: EventItem[];
  agents: UiAgentInfo[];
  globalElapsedMs: number;
  agentElapsedMs: Map<string, number>;
  isSessionActive: boolean;
  agentModels: Record<string, { provider?: string; model?: string; contextLimit?: number }>;
  sessionLimits?: SessionLimits | null;
  todoStats?: TodoStats | null;
  hasTodos?: boolean;
}

export function StatsDrawer({
  open,
  onOpenChange,
  events,
  agents,
  globalElapsedMs,
  agentElapsedMs,
  isSessionActive,
  agentModels,
  sessionLimits,
  todoStats = null,
  hasTodos = false,
}: StatsDrawerProps) {
  const [expertMode, setExpertMode] = useState(false);
  const [expandedAgents, setExpandedAgents] = useState<Set<string>>(new Set());
  
  const { session, perAgent } = useMemo(() => calculateStats(events, sessionLimits), [events, sessionLimits]);
  
  // Enhance perAgent stats with context limits from agentModels if missing
  const enrichedPerAgent = useMemo(() => {
    return perAgent.map(agentStats => ({
      ...agentStats,
      maxContextTokens: agentStats.maxContextTokens ?? agentModels[agentStats.agentId]?.contextLimit
    }));
  }, [perAgent, agentModels]);
  
  // Toggle agent expansion
  const toggleAgent = (agentId: string) => {
    setExpandedAgents(prev => {
      const next = new Set(prev);
      if (next.has(agentId)) {
        next.delete(agentId);
      } else {
        next.add(agentId);
      }
      return next;
    });
  };
  
  // Don't render if no events
  if (session.totalMessages === 0 && session.totalToolCalls === 0) return null;
  
  if (!open) return null;
  
  return (
    <>
      {/* Backdrop */}
      <div 
        className="fixed inset-0 bg-cyber-bg/40 z-30 animate-fade-in"
        onClick={() => onOpenChange(false)}
      />
      
      {/* Drawer */}
      <div className="fixed left-0 right-0 top-[72px] z-40 animate-slide-down">
        <div className="max-w-6xl mx-auto px-6">
          <div className="bg-cyber-surface/95 backdrop-blur-sm border-2 border-cyber-cyan/30 rounded-b-xl shadow-[0_8px_40px_rgba(var(--cyber-cyan-rgb),0.2)]">
            {/* Header */}
            <div className="flex items-center justify-between px-6 py-3 border-b border-cyber-border/40">
              <div className="flex items-center gap-3">
                <span className="text-sm font-semibold text-cyber-cyan uppercase tracking-wider">
                  Session Statistics
                </span>
                {isSessionActive && (
                  <div className="flex items-center gap-1.5 px-2 py-1 bg-cyber-cyan/10 border border-cyber-cyan/30 rounded-md">
                    <span className="w-2 h-2 rounded-full bg-cyber-cyan animate-pulse" />
                    <span className="text-[10px] text-cyber-cyan font-medium uppercase tracking-wide">
                      Active
                    </span>
                  </div>
                )}
              </div>
              
              <div className="flex items-center gap-3">
                {/* Expert Mode Toggle */}
                <label className="flex items-center gap-2 cursor-pointer group">
                  <span className="text-xs text-gray-400 group-hover:text-gray-300 transition-colors">
                    Expert Mode
                  </span>
                  <div className="relative">
                    <input
                      type="checkbox"
                      checked={expertMode}
                      onChange={(e) => setExpertMode(e.target.checked)}
                      className="sr-only peer"
                    />
                    <div className="w-9 h-5 bg-cyber-bg border border-cyber-border rounded-full peer-checked:bg-cyber-purple/20 peer-checked:border-cyber-purple transition-colors" />
                    <div className="absolute left-0.5 top-0.5 w-4 h-4 bg-gray-500 rounded-full transition-all peer-checked:translate-x-4 peer-checked:bg-cyber-purple" />
                  </div>
                </label>
                
                {/* Close button */}
                <button
                  onClick={() => onOpenChange(false)}
                  className="p-1.5 hover:bg-cyber-bg/50 rounded-lg transition-colors"
                  title="Close"
                >
                  <X className="w-4 h-4 text-gray-400 hover:text-gray-200" />
                </button>
              </div>
            </div>
            
            {/* Content */}
            <div className="px-6 py-4 max-h-[60vh] overflow-y-auto custom-scrollbar">
              {/* Session Overview */}
              <div className="grid grid-cols-4 gap-4 mb-4">
                {/* Elapsed Time */}
                <div className="p-4 rounded-lg bg-cyber-bg/50 border border-cyber-border/50">
                  <div className="text-[10px] uppercase tracking-wider text-gray-500 mb-1">
                    Elapsed Time
                  </div>
                  <div className="text-2xl font-mono font-semibold text-cyber-cyan">
                    {formatDuration(globalElapsedMs)}
                  </div>
                  {isSessionActive && (
                    <div className="mt-1 text-[10px] text-gray-500">
                      (active)
                    </div>
                  )}
                </div>
                
                {/* Context Usage */}
                <div className="p-4 rounded-lg bg-cyber-bg/50 border border-cyber-border/50">
                  <div className="text-[10px] uppercase tracking-wider text-gray-500 mb-1">
                    Context
                  </div>
                  {enrichedPerAgent.length === 1 ? (
                    <>
                      <div className="text-2xl font-mono font-semibold text-gray-200">
                        {formatTokensAbbrev(enrichedPerAgent[0].currentContextTokens)}
                        {enrichedPerAgent[0].maxContextTokens && (
                          <span className="text-sm text-gray-500">
                            /{formatTokensAbbrev(enrichedPerAgent[0].maxContextTokens)}
                          </span>
                        )}
                      </div>
                      {enrichedPerAgent[0].maxContextTokens && (
                        <>
                          <div className="mt-2 w-full bg-cyber-bg rounded-full h-1.5 overflow-hidden">
                            <div
                              className="h-full bg-gradient-to-r from-cyber-cyan to-cyber-purple transition-all"
                              style={{
                                width: formatPercentage(
                                  enrichedPerAgent[0].currentContextTokens,
                                  enrichedPerAgent[0].maxContextTokens
                                ),
                              }}
                            />
                          </div>
                          <div className="mt-1 text-[10px] text-gray-500">
                            {formatPercentage(
                              enrichedPerAgent[0].currentContextTokens,
                              enrichedPerAgent[0].maxContextTokens
                            )} used
                          </div>
                        </>
                      )}
                    </>
                  ) : (
                    <div className="space-y-2">
                      {enrichedPerAgent.map((agentStats) => (
                        <div
                          key={agentStats.agentId}
                          className="flex items-center gap-2 text-xs"
                        >
                          <span
                            className="w-2 h-2 rounded-full flex-shrink-0"
                            style={{ backgroundColor: getAgentColor(agentStats.agentId) }}
                          />
                          <span className="font-mono text-gray-300">
                            {formatTokensAbbrev(agentStats.currentContextTokens)}
                            {agentStats.maxContextTokens && (
                              <span className="text-gray-500">
                                /{formatTokensAbbrev(agentStats.maxContextTokens)}
                              </span>
                            )}
                          </span>
                        </div>
                      ))}
                    </div>
                  )}
                </div>
                
                {/* Tool Calls */}
                <div className="p-4 rounded-lg bg-cyber-bg/50 border border-cyber-border/50">
                  <div className="text-[10px] uppercase tracking-wider text-gray-500 mb-1">
                    Tools
                  </div>
                  <div className="text-2xl font-mono font-semibold text-gray-200">
                    {session.totalToolCalls}
                  </div>
                  <div className="mt-1 text-[10px] text-gray-500">
                    {session.totalToolCalls === 1 ? 'call' : 'calls'}
                  </div>
                </div>
                
                {/* Cost */}
                <div className="p-4 rounded-lg bg-cyber-bg/50 border border-cyber-border/50">
                  <div className="text-[10px] uppercase tracking-wider text-gray-500 mb-1">
                    Cost
                  </div>
                  <div className="text-2xl font-mono font-semibold text-cyber-cyan">
                    {formatCost(session.totalCostUsd)}
                    {session.limits?.max_cost_usd && (
                      <span className="text-sm text-gray-500">
                        /{formatCost(session.limits.max_cost_usd)}
                      </span>
                    )}
                  </div>
                  {session.limits?.max_cost_usd && (
                    <>
                      <div className="mt-2 w-full bg-cyber-bg rounded-full h-1.5 overflow-hidden">
                        <div
                          className="h-full bg-gradient-to-r from-cyber-lime to-cyber-orange transition-all"
                          style={{
                            width: formatPercentage(
                              session.totalCostUsd,
                              session.limits.max_cost_usd
                            ),
                          }}
                        />
                      </div>
                      <div className="mt-1 text-[10px] text-gray-500">
                        {formatPercentage(
                          session.totalCostUsd,
                          session.limits.max_cost_usd
                        )} of limit
                      </div>
                    </>
                  )}
                </div>
              </div>
              
              {/* Session Metadata */}
              <div className="flex items-center gap-6 text-xs text-gray-500 mb-4 px-4 py-2 bg-cyber-bg/30 rounded-lg">
                <span>Messages: <span className="text-gray-300 font-mono">{session.totalMessages}</span></span>
                <span>Steps: <span className="text-gray-300 font-mono">{session.totalSteps}</span></span>
                <span>Turns: <span className="text-gray-300 font-mono">{session.totalTurns}</span></span>
                {hasTodos && todoStats && (
                  <span>
                    📋 Tasks: <span className="text-gray-300 font-mono">
                      {todoStats.completed}/{todoStats.total} completed
                    </span>
                    {todoStats.inProgress > 0 && (
                      <span className="ml-1">({todoStats.inProgress} active)</span>
                    )}
                  </span>
                )}
              </div>
              
              {/* Per-Agent Breakdown (Expert Mode) */}
              {expertMode && enrichedPerAgent.length > 0 && (
                <div className="space-y-2">
                  <div className="text-xs font-semibold text-gray-400 uppercase tracking-wider px-2 mb-3">
                    Per-Agent Breakdown
                  </div>
                  {enrichedPerAgent.map((agentStats) => {
                    const isExpanded = expandedAgents.has(agentStats.agentId);
                    const agentElapsed = agentElapsedMs.get(agentStats.agentId) || 0;
                    const agentColor = getAgentColor(agentStats.agentId);
                    const agentName = getAgentDisplayName(agentStats.agentId, agents);
                    const model = agentModels[agentStats.agentId];
                    
                    return (
                      <div
                        key={agentStats.agentId}
                        className="rounded-lg border border-cyber-border/50 bg-cyber-bg/30 overflow-hidden"
                      >
                        {/* Agent Header */}
                        <button
                          onClick={() => toggleAgent(agentStats.agentId)}
                          className="w-full px-4 py-3 flex items-center gap-3 hover:bg-cyber-bg/50 transition-colors text-left"
                        >
                          <div className="flex-shrink-0">
                            {isExpanded ? (
                              <ChevronDown className="w-4 h-4 text-gray-400" />
                            ) : (
                              <ChevronRight className="w-4 h-4 text-gray-400" />
                            )}
                          </div>
                          <span
                            className="w-1 h-8 rounded-full flex-shrink-0"
                            style={{ backgroundColor: agentColor }}
                          />
                          <div className="flex-1 min-w-0">
                            <div className="flex items-center gap-2 mb-1">
                              <span className="font-medium text-gray-200">{agentName}</span>
                              {model && (
                                <span className="text-[10px] text-gray-500 font-mono">
                                  {model.provider}/{model.model}
                                </span>
                              )}
                            </div>
                            <div className="flex items-center gap-4 text-xs text-gray-500">
                              <span>ctx:{formatTokensAbbrev(agentStats.currentContextTokens)}</span>
                              <span>{formatCost(agentStats.costUsd)}</span>
                              <span>🔧{agentStats.toolCallCount}</span>
                              <span>{formatDuration(agentElapsed)}</span>
                            </div>
                          </div>
                        </button>
                        
                        {/* Expanded Agent Details */}
                        {isExpanded && (
                          <div className="px-4 pb-3 space-y-2 border-t border-cyber-border/30">
                            {/* Tool breakdown */}
                            {agentStats.toolBreakdown && Object.keys(agentStats.toolBreakdown).length > 0 && (
                              <div className="pt-3">
                                <div className="text-[10px] text-gray-500 uppercase tracking-wider mb-2">
                                  Tools Used
                                </div>
                                <div className="flex flex-wrap gap-2">
                                  {Object.entries(agentStats.toolBreakdown)
                                    .sort(([, a], [, b]) => (b as number) - (a as number))
                                    .map(([toolName, count]) => (
                                      <div
                                        key={toolName}
                                        className="px-2 py-1 bg-cyber-surface/60 border border-cyber-border/40 rounded text-xs"
                                      >
                                        <span className="text-gray-400">{toolName}</span>
                                        <span className="text-gray-600 mx-1">×</span>
                                        <span className="text-gray-300 font-mono">{count as number}</span>
                                      </div>
                                    ))}
                                </div>
                              </div>
                            )}
                            
                            {/* Stats grid */}
                            <div className="grid grid-cols-3 gap-3 pt-2">
                              <div>
                                <div className="text-[10px] text-gray-500 uppercase">Messages</div>
                                <div className="text-sm font-mono text-gray-300">{agentStats.messageCount}</div>
                              </div>
                              <div>
                                <div className="text-[10px] text-gray-500 uppercase">Steps</div>
                                <div className="text-sm font-mono text-gray-300">{agentStats.steps}</div>
                              </div>
                              <div>
                                <div className="text-[10px] text-gray-500 uppercase">Turns</div>
                                <div className="text-sm font-mono text-gray-300">{agentStats.turns}</div>
                              </div>
                            </div>
                          </div>
                        )}
                      </div>
                    );
                  })}
                </div>
              )}
            </div>
          </div>
        </div>
      </div>
    </>
  );
}
