import { useMemo, useState } from 'react';
import { ChevronDown, ChevronRight, BarChart3 } from 'lucide-react';
import { EventItem, UiAgentInfo } from '../types';
import { calculateStats } from '../utils/statsCalculator';
import { getAgentColor } from '../utils/agentColors';
import { getAgentDisplayName } from '../utils/agentNames';

interface StatsPanelProps {
  events: EventItem[];
  agents: UiAgentInfo[];
}

export function StatsPanel({ events, agents }: StatsPanelProps) {
  const [isOpen, setIsOpen] = useState(true);
  const { perAgent } = useMemo(() => calculateStats(events), [events]);
  
  if (events.length === 0) return null;
  
  return (
    <div className="border-b border-cyber-border">
      <button
        onClick={() => setIsOpen(!isOpen)}
        className="w-full flex items-center justify-between p-4 hover:bg-cyber-bg/50 transition-colors"
      >
        <div className="flex items-center gap-2">
          <BarChart3 className="w-4 h-4 text-cyber-cyan" />
          <span className="text-sm font-medium text-gray-300">Session Stats</span>
        </div>
        {isOpen ? (
          <ChevronDown className="w-4 h-4 text-gray-400" />
        ) : (
          <ChevronRight className="w-4 h-4 text-gray-400" />
        )}
      </button>
      
      {isOpen && (
        <div className="px-4 pb-4 space-y-3">
          {perAgent.map((agentStats) => {
            const displayName = getAgentDisplayName(agentStats.agentId, agents);
            return (
              <div
                key={agentStats.agentId}
                className="p-3 rounded-lg bg-cyber-bg/50 border border-cyber-border"
                style={{ borderLeftColor: getAgentColor(agentStats.agentId), borderLeftWidth: '3px' }}
              >
                <div className="text-sm font-medium text-gray-200 mb-2">
                  {displayName}
                </div>
              <div className="grid grid-cols-3 gap-2 text-xs">
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
        </div>
      )}
    </div>
  );
}
