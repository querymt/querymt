import { useState, useMemo } from 'react';
import * as Collapsible from '@radix-ui/react-collapsible';
import { Search, X, Filter, ChevronDown, Code } from 'lucide-react';
import { EventItem, EventFilters, UiAgentInfo } from '../types';
import { getAgentDisplayName } from '../utils/agentNames';

interface EventFiltersProps {
  events: EventItem[];
  filters: EventFilters;
  onFiltersChange: (filters: EventFilters) => void;
  filteredCount: number;
  totalCount: number;
  expertMode: boolean;
  onExpertModeChange: (enabled: boolean) => void;
  agents: UiAgentInfo[];
}

const EVENT_TYPES: EventItem['type'][] = ['user', 'agent', 'tool_call', 'tool_result', 'system'];

export function EventFiltersBar({
  events,
  filters,
  onFiltersChange,
  filteredCount,
  totalCount,
  expertMode,
  onExpertModeChange,
  agents,
}: EventFiltersProps) {
  const [showFilters, setShowFilters] = useState(false);
  
  const { agentIds, tools } = useMemo(() => {
    const agentSet = new Set<string>();
    const toolSet = new Set<string>();
    
    for (const event of events) {
      if (event.agentId) agentSet.add(event.agentId);
      if (event.toolCall?.kind) toolSet.add(event.toolCall.kind);
    }
    
    return {
      agentIds: Array.from(agentSet).sort(),
      tools: Array.from(toolSet).sort(),
    };
  }, [events]);
  
  const hasActiveFilters = 
    filters.types.size < EVENT_TYPES.length ||
    filters.agents.size > 0 ||
    filters.tools.size > 0 ||
    filters.searchQuery.length > 0;
  
  const toggleType = (type: EventItem['type']) => {
    const newTypes = new Set(filters.types);
    if (newTypes.has(type)) {
      newTypes.delete(type);
    } else {
      newTypes.add(type);
    }
    onFiltersChange({ ...filters, types: newTypes });
  };
  
  const toggleAgent = (agent: string) => {
    const newAgents = new Set(filters.agents);
    if (newAgents.has(agent)) {
      newAgents.delete(agent);
    } else {
      newAgents.add(agent);
    }
    onFiltersChange({ ...filters, agents: newAgents });
  };
  
  const toggleTool = (tool: string) => {
    const newTools = new Set(filters.tools);
    if (newTools.has(tool)) {
      newTools.delete(tool);
    } else {
      newTools.add(tool);
    }
    onFiltersChange({ ...filters, tools: newTools });
  };
  
  const clearFilters = () => {
    onFiltersChange({
      types: new Set(EVENT_TYPES),
      agents: new Set(),
      tools: new Set(),
      searchQuery: '',
    });
  };
  
  return (
    <Collapsible.Root open={showFilters} onOpenChange={setShowFilters} className="px-4 py-2 bg-surface-elevated/50 border-b border-surface-border">
      <div className="flex items-center gap-2">
        <div className="flex-1 relative">
          <Search className="absolute left-3 top-1/2 -translate-y-1/2 w-4 h-4 text-ui-muted" />
          <input
            type="text"
            placeholder="Search events..."
            value={filters.searchQuery}
            onChange={(e) => onFiltersChange({ ...filters, searchQuery: e.target.value })}
            className="w-full pl-9 pr-3 py-1.5 bg-surface-canvas border border-surface-border/60 rounded text-sm text-ui-primary placeholder:text-ui-muted focus:outline-none focus:border-accent-primary"
          />
          {filters.searchQuery && (
            <button
              onClick={() => onFiltersChange({ ...filters, searchQuery: '' })}
              className="absolute right-2 top-1/2 -translate-y-1/2 text-ui-muted hover:text-ui-secondary"
            >
              <X className="w-4 h-4" />
            </button>
          )}
        </div>
        
        <button
          onClick={() => onExpertModeChange(!expertMode)}
          className={`flex items-center gap-1 px-3 py-1.5 rounded border text-sm transition-colors ${
            expertMode
              ? 'border-accent-tertiary text-accent-tertiary bg-accent-tertiary/10'
              : 'border-surface-border/60 text-ui-secondary hover:border-accent-tertiary/40'
          }`}
          title="Toggle expert mode (show all internal events)"
        >
          <Code className="w-4 h-4" />
          <span>Expert</span>
        </button>

        <Collapsible.Trigger
          className={`flex items-center gap-1 px-3 py-1.5 rounded border text-sm transition-colors ${
            hasActiveFilters
              ? 'border-accent-primary text-accent-primary bg-accent-primary/10'
              : 'border-surface-border/60 text-ui-secondary hover:border-accent-primary/40'
          }`}
        >
          <Filter className="w-4 h-4" />
          <span>Filters</span>
          {hasActiveFilters && (
            <span className="ml-1 px-1.5 py-0.5 text-[10px] bg-accent-primary/20 rounded">
              {filteredCount}/{totalCount}
            </span>
          )}
          <ChevronDown className={`w-4 h-4 transition-transform ${showFilters ? 'rotate-180' : ''}`} />
        </Collapsible.Trigger>
        
        {hasActiveFilters && (
          <button
            onClick={clearFilters}
            className="text-xs text-ui-muted hover:text-status-warning"
          >
            Clear
          </button>
        )}
      </div>
      
      <Collapsible.Content className="mt-3 pt-3 border-t border-surface-border/50 space-y-3">
        <div>
          <span className="text-[10px] text-ui-muted uppercase">Event Types</span>
          <div className="flex flex-wrap gap-1 mt-1">
            {EVENT_TYPES.map((type) => (
              <button
                key={type}
                onClick={() => toggleType(type)}
                className={`text-xs px-2 py-1 rounded border transition-colors ${
                  filters.types.has(type)
                    ? 'border-accent-primary text-accent-primary bg-accent-primary/10'
                    : 'border-surface-border/50 text-ui-muted hover:border-surface-border/70 hover:text-ui-secondary'
                }`}
              >
                {type.replace('_', ' ')}
              </button>
            ))}
          </div>
        </div>
        
        {agentIds.length > 1 && (
          <div>
            <span className="text-[10px] text-ui-muted uppercase">Agents</span>
            <div className="flex flex-wrap gap-1 mt-1">
              {agentIds.map((agentId) => {
                const displayName = getAgentDisplayName(agentId, agents);
                return (
                  <button
                    key={agentId}
                    onClick={() => toggleAgent(agentId)}
                    className={`text-xs px-2 py-1 rounded border transition-colors ${
                      filters.agents.has(agentId)
                        ? 'border-accent-secondary text-accent-secondary bg-accent-secondary/10'
                        : 'border-surface-border/50 text-ui-muted hover:border-surface-border/70 hover:text-ui-secondary'
                    }`}
                  >
                    {displayName}
                  </button>
                );
              })}
            </div>
          </div>
        )}
        
        {tools.length > 0 && (
          <div>
            <span className="text-[10px] text-ui-muted uppercase">Tools</span>
            <div className="flex flex-wrap gap-1 mt-1">
              {tools.map((tool) => (
                <button
                  key={tool}
                  onClick={() => toggleTool(tool)}
                  className={`text-xs px-2 py-1 rounded border transition-colors ${
                    filters.tools.has(tool)
                      ? 'border-accent-tertiary text-accent-tertiary bg-accent-tertiary/10'
                      : 'border-surface-border/50 text-ui-muted hover:border-surface-border/70 hover:text-ui-secondary'
                  }`}
                >
                  {tool}
                </button>
              ))}
            </div>
          </div>
        )}
      </Collapsible.Content>
    </Collapsible.Root>
  );
}
