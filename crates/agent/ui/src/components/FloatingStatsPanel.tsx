import { useMemo, useState, useEffect, useRef, memo } from 'react';
import { ChevronDown, ChevronRight, BarChart3, GripVertical, Minimize2 } from 'lucide-react';
import { EventItem, UiAgentInfo, SessionLimits } from '../types';
import { calculateStats } from '../utils/statsCalculator';
import { getAgentColor } from '../utils/agentColors';
import { getAgentDisplayName } from '../utils/agentNames';

interface FloatingStatsPanelProps {
  events: EventItem[];
  agents: UiAgentInfo[];
  expertMode?: boolean;
  globalElapsedMs: number;
  agentElapsedMs: Map<string, number>;
  isSessionActive: boolean;
  agentModels: Record<string, { provider?: string; model?: string; contextLimit?: number }>;
  sessionLimits?: SessionLimits | null;
}

// Format percentage for progress indicators
function formatPercentage(current: number, max: number): string {
  return `${Math.min(100, Math.round((current / max) * 100))}%`;
}

// Format cost as USD with 2 decimal places
function formatCost(usd: number): string {
  return `$${usd.toFixed(2)}`;
}

// Format tokens with abbreviated suffixes (k, M)
function formatTokensAbbrev(count: number): string {
  if (count >= 1_000_000) return `${(count / 1_000_000).toFixed(1)}M`;
  if (count >= 1_000) return `${(count / 1_000).toFixed(0)}k`;
  return count.toString();
}

// Format duration as human-readable string
function formatDuration(ms: number): string {
  const totalSeconds = Math.floor(ms / 1000);
  const seconds = totalSeconds % 60;
  const minutes = Math.floor(totalSeconds / 60) % 60;
  const hours = Math.floor(totalSeconds / 3600);
  
  if (hours > 0) {
    return `${hours}h ${minutes}m ${seconds}s`;
  }
  if (minutes > 0) {
    return `${minutes}m ${seconds}s`;
  }
  return `${seconds}s`;
}

const STORAGE_KEY_POSITION = 'floatingStatsPosition';
const STORAGE_KEY_COLLAPSED = 'floatingStatsCollapsed';
const STORAGE_KEY_MINIMIZED = 'floatingStatsMinimized';

export const FloatingStatsPanel = memo(function FloatingStatsPanel({ 
  events, 
  agents, 
  expertMode = false,
  globalElapsedMs,
  agentElapsedMs,
  isSessionActive,
  agentModels,
  sessionLimits,
}: FloatingStatsPanelProps) {
  const [isCollapsed, setIsCollapsed] = useState(() => {
    const stored = localStorage.getItem(STORAGE_KEY_COLLAPSED);
    return stored ? JSON.parse(stored) : false;
  });
  
  const [isMinimized, setIsMinimized] = useState(() => {
    const stored = localStorage.getItem(STORAGE_KEY_MINIMIZED);
    return stored ? JSON.parse(stored) : false;
  });
  
  const [position, setPosition] = useState(() => {
    const stored = localStorage.getItem(STORAGE_KEY_POSITION);
    if (stored) {
      return JSON.parse(stored);
    }
    return { x: window.innerWidth - 320, y: 120 };
  });
  
  const [isDragging, setIsDragging] = useState(false);
  const [dragStart, setDragStart] = useState({ x: 0, y: 0 });
  const panelRef = useRef<HTMLDivElement>(null);
  
  const { session, perAgent } = useMemo(() => calculateStats(events, sessionLimits), [events, sessionLimits]);
  
  // Enhance perAgent stats with context limits from agentModels if missing
  const enrichedPerAgent = useMemo(() => {
    return perAgent.map(agentStats => ({
      ...agentStats,
      maxContextTokens: agentStats.maxContextTokens ?? agentModels[agentStats.agentId]?.contextLimit
    }));
  }, [perAgent, agentModels]);
  
  useEffect(() => {
    localStorage.setItem(STORAGE_KEY_POSITION, JSON.stringify(position));
  }, [position]);
  
  useEffect(() => {
    localStorage.setItem(STORAGE_KEY_COLLAPSED, JSON.stringify(isCollapsed));
  }, [isCollapsed]);
  
  useEffect(() => {
    localStorage.setItem(STORAGE_KEY_MINIMIZED, JSON.stringify(isMinimized));
  }, [isMinimized]);
  
  const handleMouseDown = (e: React.MouseEvent) => {
    if ((e.target as HTMLElement).closest('.drag-handle')) {
      setIsDragging(true);
      setDragStart({
        x: e.clientX - position.x,
        y: e.clientY - position.y,
      });
      e.preventDefault();
    }
  };
  
  useEffect(() => {
    const handleMouseMove = (e: MouseEvent) => {
      if (isDragging) {
        const newX = e.clientX - dragStart.x;
        const newY = e.clientY - dragStart.y;
        
        // Constrain to viewport
        const maxX = window.innerWidth - (panelRef.current?.offsetWidth || 300);
        const maxY = window.innerHeight - (panelRef.current?.offsetHeight || 200);
        
        setPosition({
          x: Math.max(0, Math.min(newX, maxX)),
          y: Math.max(0, Math.min(newY, maxY)),
        });
      }
    };
    
    const handleMouseUp = () => {
      setIsDragging(false);
    };
    
    if (isDragging) {
      document.addEventListener('mousemove', handleMouseMove);
      document.addEventListener('mouseup', handleMouseUp);
      return () => {
        document.removeEventListener('mousemove', handleMouseMove);
        document.removeEventListener('mouseup', handleMouseUp);
      };
    }
  }, [isDragging, dragStart]);
  
  // Don't show panel if there are no actual session events (only system events)
  if (session.totalMessages === 0 && session.totalToolCalls === 0) return null;
  
  // Minimized state - just a floating button
  if (isMinimized) {
    return (
      <div
        ref={panelRef}
        className="fixed z-30 select-none"
        style={{
          left: `${position.x}px`,
          top: `${position.y}px`,
        }}
        onMouseDown={handleMouseDown}
      >
        <button
          onClick={() => setIsMinimized(false)}
          className="drag-handle flex items-center gap-2 px-3 py-2 rounded-lg bg-cyber-surface/95 border border-cyber-cyan/30 shadow-lg shadow-cyber-cyan/25 cursor-move"
        >
          <BarChart3 className="w-4 h-4 text-cyber-cyan" />
          <span className="text-xs font-medium text-gray-300">Stats</span>
        </button>
      </div>
    );
  }
  
  return (
    <div
      ref={panelRef}
      className="fixed z-30 select-none"
      style={{
        left: `${position.x}px`,
        top: `${position.y}px`,
        width: '320px',
      }}
      onMouseDown={handleMouseDown}
    >
      <div className="rounded-lg bg-cyber-surface/95 border border-cyber-cyan/30 shadow-lg shadow-cyber-cyan/25">
        {/* Header with drag handle */}
        <div className="drag-handle flex items-center justify-between px-3 py-2 border-b border-cyber-border/30 cursor-move">
          <div className="flex items-center gap-2">
            <GripVertical className="w-4 h-4 text-gray-500" />
            <BarChart3 className="w-4 h-4 text-cyber-cyan" />
            <span className="text-sm font-medium text-gray-300">Session Stats</span>
          </div>
          <div className="flex items-center gap-1">
            <button
              onClick={(e) => {
                e.stopPropagation();
                setIsCollapsed(!isCollapsed);
              }}
              className="p-1 hover:bg-cyber-bg/50 rounded transition-colors"
              title={isCollapsed ? "Expand" : "Collapse"}
            >
              {isCollapsed ? (
                <ChevronRight className="w-4 h-4 text-gray-400" />
              ) : (
                <ChevronDown className="w-4 h-4 text-gray-400" />
              )}
            </button>
            <button
              onClick={(e) => {
                e.stopPropagation();
                setIsMinimized(true);
              }}
              className="p-1 hover:bg-cyber-bg/50 rounded transition-colors"
              title="Minimize"
            >
              <Minimize2 className="w-4 h-4 text-gray-400" />
            </button>
          </div>
        </div>
        
        {/* Content */}
        {!isCollapsed && (
          <div className="px-3 py-3 space-y-3 max-h-[60vh] overflow-y-auto">
            {/* Session Summary - Always visible in normal mode */}
            {!expertMode && (
              <div className="p-3 rounded-lg border border-cyber-cyan/30 bg-cyber-cyan/5">
                <div className="space-y-2 text-xs">
                  <div className="flex justify-between items-center">
                    <span className="text-gray-500">Elapsed Time</span>
                    <div className="flex items-center gap-2">
                      {isSessionActive && (
                        <div className="w-2 h-2 rounded-full bg-cyber-cyan animate-pulse" title="Session active" />
                      )}
                      <span className="text-cyber-cyan font-mono font-semibold">
                        {formatDuration(globalElapsedMs)}
                      </span>
                    </div>
                  </div>
                  <div className="flex justify-between items-center">
                    <span className="text-gray-500">Context</span>
                    <div className="flex items-center gap-1.5 font-mono text-gray-300">
                      {enrichedPerAgent.length === 1 ? (
                        // Single agent: no dot, just show context
                        <span>
                          {formatTokensAbbrev(enrichedPerAgent[0].currentContextTokens)}
                          {enrichedPerAgent[0].maxContextTokens && (
                            <span className="text-gray-500">
                              {' '}({((enrichedPerAgent[0].currentContextTokens / enrichedPerAgent[0].maxContextTokens) * 100).toFixed(0)}%)
                            </span>
                          )}
                        </span>
                      ) : (
                        // Multiple agents: colored dots with context
                        enrichedPerAgent.map((agentStats) => (
                          <span
                            key={agentStats.agentId}
                            className="flex items-center gap-0.5"
                            title={getAgentDisplayName(agentStats.agentId, agents)}
                          >
                            <span
                              className="w-2 h-2 rounded-full inline-block"
                              style={{ backgroundColor: getAgentColor(agentStats.agentId) }}
                            />
                            <span>
                              {formatTokensAbbrev(agentStats.currentContextTokens)}
                              {agentStats.maxContextTokens && (
                                <span className="text-gray-500">
                                  ({((agentStats.currentContextTokens / agentStats.maxContextTokens) * 100).toFixed(0)}%)
                                </span>
                              )}
                            </span>
                          </span>
                        ))
                      )}
                    </div>
                  </div>
                  <div className="flex justify-between">
                    <span className="text-gray-500">Cost</span>
                    <span className="text-cyber-cyan font-mono font-semibold">
                      {formatCost(session.totalCostUsd)}
                      {session.limits?.max_cost_usd && (
                        <span className="text-gray-500 ml-1">
                          / {formatCost(session.limits.max_cost_usd)}
                        </span>
                      )}
                    </span>
                  </div>
                  {/* Show cost progress bar if limit is set */}
                  {session.limits?.max_cost_usd && (
                    <div className="pt-1">
                      <div className="w-full bg-cyber-bg/50 rounded-full h-1.5 overflow-hidden">
                        <div
                          className={`h-full rounded-full transition-all ${
                            session.totalCostUsd / session.limits.max_cost_usd > 0.9
                              ? 'bg-cyber-orange'
                              : session.totalCostUsd / session.limits.max_cost_usd > 0.7
                              ? 'bg-yellow-500'
                              : 'bg-cyber-cyan'
                          }`}
                          style={{
                            width: formatPercentage(session.totalCostUsd, session.limits.max_cost_usd),
                          }}
                        />
                      </div>
                    </div>
                  )}
                  <div className="flex justify-between">
                    <span className="text-gray-500">Messages</span>
                    <span className="text-gray-300">{session.totalMessages}</span>
                  </div>
                  <div className="flex justify-between">
                    <span className="text-gray-500">Tool Calls</span>
                    <span className="text-gray-300">{session.totalToolCalls}</span>
                  </div>
                  {/* Show steps/turns with limits if available */}
                  {(session.totalSteps > 0 || session.limits?.max_steps) && (
                    <div className="flex justify-between">
                      <span className="text-gray-500">Steps</span>
                      <span className="text-gray-300">
                        {session.totalSteps}
                        {session.limits?.max_steps && (
                          <span className="text-gray-500"> / {session.limits.max_steps}</span>
                        )}
                      </span>
                    </div>
                  )}
                  {(session.totalTurns > 0 || session.limits?.max_turns) && (
                    <div className="flex justify-between">
                      <span className="text-gray-500">Turns</span>
                      <span className="text-gray-300">
                        {session.totalTurns}
                        {session.limits?.max_turns && (
                          <span className="text-gray-500"> / {session.limits.max_turns}</span>
                        )}
                      </span>
                    </div>
                  )}
                </div>
              </div>
            )}
            
            {/* Expert Mode: Session Total + Per-Agent Breakdown */}
            {expertMode && (
              <>
                {/* Session Total */}
                <div className="p-3 rounded-lg border border-cyber-cyan/30 bg-cyber-cyan/5">
                  <div className="text-sm font-medium text-gray-200 mb-2">Session Total</div>
                  <div className="space-y-1 text-xs">
                    <div className="flex justify-between items-center">
                      <span className="text-gray-500">Elapsed Time</span>
                      <div className="flex items-center gap-2">
                        {isSessionActive && (
                          <div className="w-2 h-2 rounded-full bg-cyber-cyan animate-pulse" title="Session active" />
                        )}
                        <span className="text-cyber-cyan font-mono font-semibold">
                          {formatDuration(globalElapsedMs)}
                        </span>
                      </div>
                    </div>
                    <div className="flex justify-between">
                      <span className="text-gray-500">Cost</span>
                      <span className="text-cyber-cyan font-mono font-semibold">
                        {formatCost(session.totalCostUsd)}
                        {session.limits?.max_cost_usd && (
                          <span className="text-gray-500 ml-1">
                            / {formatCost(session.limits.max_cost_usd)}
                          </span>
                        )}
                      </span>
                    </div>
                    {/* Show cost progress bar if limit is set */}
                    {session.limits?.max_cost_usd && (
                      <div className="pt-1">
                        <div className="w-full bg-cyber-bg/50 rounded-full h-1.5 overflow-hidden">
                          <div
                            className={`h-full rounded-full transition-all ${
                              session.totalCostUsd / session.limits.max_cost_usd > 0.9
                                ? 'bg-cyber-orange'
                                : session.totalCostUsd / session.limits.max_cost_usd > 0.7
                                ? 'bg-yellow-500'
                                : 'bg-cyber-cyan'
                            }`}
                            style={{
                              width: formatPercentage(session.totalCostUsd, session.limits.max_cost_usd),
                            }}
                          />
                        </div>
                        <div className="text-[10px] text-gray-500 mt-0.5">
                          {formatPercentage(session.totalCostUsd, session.limits.max_cost_usd)} of budget used
                        </div>
                      </div>
                    )}
                    {/* Steps with limit */}
                    {(session.totalSteps > 0 || session.limits?.max_steps) && (
                      <>
                        <div className="flex justify-between">
                          <span className="text-gray-500">Steps (LLM calls)</span>
                          <span className="text-gray-300 font-mono">
                            {session.totalSteps}
                            {session.limits?.max_steps && (
                              <span className="text-gray-500"> / {session.limits.max_steps}</span>
                            )}
                          </span>
                        </div>
                        {session.limits?.max_steps && (
                          <div className="pt-1">
                            <div className="w-full bg-cyber-bg/50 rounded-full h-1.5 overflow-hidden">
                              <div
                                className={`h-full rounded-full transition-all ${
                                  session.totalSteps / session.limits.max_steps > 0.9
                                    ? 'bg-cyber-orange'
                                    : session.totalSteps / session.limits.max_steps > 0.7
                                    ? 'bg-yellow-500'
                                    : 'bg-cyber-purple'
                                }`}
                                style={{
                                  width: formatPercentage(session.totalSteps, session.limits.max_steps),
                                }}
                              />
                            </div>
                          </div>
                        )}
                      </>
                    )}
                    {/* Turns with limit */}
                    {(session.totalTurns > 0 || session.limits?.max_turns) && (
                      <>
                        <div className="flex justify-between">
                          <span className="text-gray-500">Turns (exchanges)</span>
                          <span className="text-gray-300 font-mono">
                            {session.totalTurns}
                            {session.limits?.max_turns && (
                              <span className="text-gray-500"> / {session.limits.max_turns}</span>
                            )}
                          </span>
                        </div>
                        {session.limits?.max_turns && (
                          <div className="pt-1">
                            <div className="w-full bg-cyber-bg/50 rounded-full h-1.5 overflow-hidden">
                              <div
                                className={`h-full rounded-full transition-all ${
                                  session.totalTurns / session.limits.max_turns > 0.9
                                    ? 'bg-cyber-orange'
                                    : session.totalTurns / session.limits.max_turns > 0.7
                                    ? 'bg-yellow-500'
                                    : 'bg-cyber-lime'
                                }`}
                                style={{
                                  width: formatPercentage(session.totalTurns, session.limits.max_turns),
                                }}
                              />
                            </div>
                          </div>
                        )}
                      </>
                    )}
                  </div>
                </div>
                
                {/* Per-Agent Stats */}
                {enrichedPerAgent.map((agentStats) => {
                  const displayName = getAgentDisplayName(agentStats.agentId, agents);
                  const liveAgentTime = agentElapsedMs.get(agentStats.agentId) ?? agentStats.activeTimeMs;
                  return (
                    <div
                      key={agentStats.agentId}
                      className="p-3 rounded-lg border border-cyber-border/40"
                      style={{ borderLeftColor: getAgentColor(agentStats.agentId), borderLeftWidth: '3px' }}
                    >
                      <div className="text-sm font-medium text-gray-200 mb-2">
                        {displayName}
                      </div>
                      <div className="grid grid-cols-3 gap-2 text-xs mb-2">
                        <div>
                          <span className="text-gray-500">Messages</span>
                          <div className="text-gray-300">{agentStats.messageCount}</div>
                        </div>
                        <div>
                          <span className="text-gray-500">Tool Calls</span>
                          <div className="text-gray-300">{agentStats.toolCallCount}</div>
                        </div>
                        <div>
                          <span className="text-gray-500">Results</span>
                          <div className="text-gray-300">{agentStats.toolResultCount}</div>
                        </div>
                      </div>
                      <div className="space-y-1 text-xs">
                        <div className="flex justify-between">
                          <span className="text-gray-500">Active Time</span>
                          <span className="text-gray-300 font-mono">
                            {formatDuration(liveAgentTime)}
                          </span>
                        </div>
                        <div className="flex justify-between">
                          <span className="text-gray-500">Context</span>
                          <span className="text-gray-300 font-mono text-[11px]">
                            {formatTokensAbbrev(agentStats.currentContextTokens)}
                            {agentStats.maxContextTokens && (
                              <span className="text-gray-500">
                                {' '}({((agentStats.currentContextTokens / agentStats.maxContextTokens) * 100).toFixed(1)}%)
                              </span>
                            )}
                          </span>
                        </div>
                        <div className="flex justify-between">
                          <span className="text-gray-500">Cost</span>
                          <span className="text-cyber-cyan font-mono font-semibold">
                            {formatCost(agentStats.costUsd)}
                          </span>
                        </div>
                        {/* Agent-level steps/turns in expert mode */}
                        {agentStats.steps > 0 && (
                          <div className="flex justify-between">
                            <span className="text-gray-500">Steps</span>
                            <span className="text-gray-300 font-mono text-[11px]">
                              {agentStats.steps}
                            </span>
                          </div>
                        )}
                        {agentStats.turns > 0 && (
                          <div className="flex justify-between">
                            <span className="text-gray-500">Turns</span>
                            <span className="text-gray-300 font-mono text-[11px]">
                              {agentStats.turns}
                            </span>
                          </div>
                        )}
                      </div>
                      {Object.keys(agentStats.toolBreakdown).length > 0 && (
                        <div className="mt-2 pt-2 border-t border-cyber-border/50">
                          <span className="text-[10px] text-gray-500 uppercase">Tools Used</span>
                          <div className="flex flex-wrap gap-1 mt-1">
                            {Object.entries(agentStats.toolBreakdown).map(([tool, count]) => (
                              <span
                                key={tool}
                                className="text-[10px] px-1.5 py-0.5 rounded bg-cyber-bg border border-cyber-border text-gray-400"
                              >
                                {tool}: {count}
                              </span>
                            ))}
                          </div>
                        </div>
                      )}
                    </div>
                  );
                })}
              </>
            )}
          </div>
        )}
      </div>
    </div>
  );
});
