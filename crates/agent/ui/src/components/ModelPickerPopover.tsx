import { useEffect, useMemo, useRef, useState } from 'react';
import { RefreshCw, Search, X, ChevronRight } from 'lucide-react';
import type { ModelEntry, RoutingMode, UiAgentInfo } from '../types';

interface ModelPickerPopoverProps {
  isOpen: boolean;
  onClose: () => void;
  anchorRef: React.RefObject<HTMLElement>;
  connected: boolean;
  routingMode: RoutingMode;
  activeAgentId: string;
  sessionId: string | null;
  sessionsByAgent: Record<string, string>;
  agents: UiAgentInfo[];
  allModels: ModelEntry[];
  currentProvider?: string;
  currentModel?: string;
  onRefresh: () => void;
  onSetSessionModel: (sessionId: string, modelId: string) => void;
}

const TARGET_ACTIVE = 'active';
const TARGET_ALL = 'all';

interface GroupedModels {
  provider: string;
  models: string[];
}

function groupByProvider(models: ModelEntry[]): GroupedModels[] {
  const map = new Map<string, string[]>();
  for (const entry of models) {
    const existing = map.get(entry.provider) ?? [];
    existing.push(entry.model);
    map.set(entry.provider, existing);
  }
  return Array.from(map.entries()).map(([provider, models]) => ({ provider, models }));
}

export function ModelPickerPopover({
  isOpen,
  onClose,
  anchorRef,
  connected,
  routingMode,
  activeAgentId: _activeAgentId,
  sessionId,
  sessionsByAgent,
  agents,
  allModels,
  currentProvider,
  currentModel,
  onRefresh,
  onSetSessionModel,
}: ModelPickerPopoverProps) {
  const [target, setTarget] = useState(TARGET_ACTIVE);
  const [filter, setFilter] = useState('');
  const [selectedModel, setSelectedModel] = useState<{ provider: string; model: string } | null>(null);
  const [isRefreshing, setIsRefreshing] = useState(false);
  const [highlightIndex, setHighlightIndex] = useState(0);
  const popoverRef = useRef<HTMLDivElement>(null);
  const inputRef = useRef<HTMLInputElement>(null);
  const listRef = useRef<HTMLDivElement>(null);

  const targetAgents = useMemo(
    () => agents.filter((agent) => sessionsByAgent[agent.id]),
    [agents, sessionsByAgent]
  );

  const grouped = useMemo(() => groupByProvider(allModels), [allModels]);

  const filteredGroups = useMemo(() => {
    const lowerFilter = filter.toLowerCase();
    if (!lowerFilter) return grouped;
    return grouped
      .map((group) => ({
        provider: group.provider,
        models: group.models.filter(
          (model) =>
            model.toLowerCase().includes(lowerFilter) ||
            group.provider.toLowerCase().includes(lowerFilter)
        ),
      }))
      .filter((group) => group.models.length > 0);
  }, [grouped, filter]);

  // Flatten for keyboard navigation
  const flatItems = useMemo(() => {
    const items: { provider: string; model: string }[] = [];
    for (const group of filteredGroups) {
      for (const model of group.models) {
        items.push({ provider: group.provider, model });
      }
    }
    return items;
  }, [filteredGroups]);

  // Reset highlight when filter changes
  useEffect(() => {
    setHighlightIndex(0);
  }, [filter]);

  // Focus input when popover opens
  useEffect(() => {
    if (isOpen && inputRef.current) {
      inputRef.current.focus();
    }
  }, [isOpen]);

  // Close on outside click
  useEffect(() => {
    if (!isOpen) return;
    const handleClick = (e: MouseEvent) => {
      if (
        popoverRef.current &&
        !popoverRef.current.contains(e.target as Node) &&
        anchorRef.current &&
        !anchorRef.current.contains(e.target as Node)
      ) {
        onClose();
      }
    };
    document.addEventListener('mousedown', handleClick);
    return () => document.removeEventListener('mousedown', handleClick);
  }, [isOpen, onClose, anchorRef]);

  // Close on escape
  useEffect(() => {
    if (!isOpen) return;
    const handleKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        onClose();
      }
    };
    document.addEventListener('keydown', handleKey);
    return () => document.removeEventListener('keydown', handleKey);
  }, [isOpen, onClose]);

  const handleRefresh = () => {
    setIsRefreshing(true);
    onRefresh();
    setTimeout(() => setIsRefreshing(false), 1000);
  };

  const hasTargetSession =
    target === TARGET_ALL
      ? Object.values(sessionsByAgent).length > 0
      : target === TARGET_ACTIVE
      ? Boolean(sessionId)
      : Boolean(sessionsByAgent[target]);

  const canSwitch = connected && selectedModel !== null && hasTargetSession;

  const handleSwitch = () => {
    if (!selectedModel) return;
    const modelId = `${selectedModel.provider}/${selectedModel.model}`;
    const sessionIds = new Set<string>();

    if (target === TARGET_ALL) {
      Object.values(sessionsByAgent).forEach((id) => sessionIds.add(id));
    } else if (target === TARGET_ACTIVE) {
      if (sessionId) sessionIds.add(sessionId);
    } else if (sessionsByAgent[target]) {
      sessionIds.add(sessionsByAgent[target]);
    }

    if (sessionIds.size === 0) return;
    sessionIds.forEach((id) => onSetSessionModel(id, modelId));
    onClose();
  };

  const handleKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === 'ArrowDown') {
      e.preventDefault();
      setHighlightIndex((prev) => Math.min(prev + 1, flatItems.length - 1));
    } else if (e.key === 'ArrowUp') {
      e.preventDefault();
      setHighlightIndex((prev) => Math.max(prev - 1, 0));
    } else if (e.key === 'Enter') {
      e.preventDefault();
      const highlightedItem = flatItems[highlightIndex];
      if (highlightedItem && connected && hasTargetSession) {
        // Switch directly to the highlighted model
        const modelId = `${highlightedItem.provider}/${highlightedItem.model}`;
        const sessionIds = new Set<string>();

        if (target === TARGET_ALL) {
          Object.values(sessionsByAgent).forEach((id) => sessionIds.add(id));
        } else if (target === TARGET_ACTIVE) {
          if (sessionId) sessionIds.add(sessionId);
        } else if (sessionsByAgent[target]) {
          sessionIds.add(sessionsByAgent[target]);
        }

        if (sessionIds.size > 0) {
          sessionIds.forEach((id) => onSetSessionModel(id, modelId));
          onClose();
        }
      }
    }
  };

  // Scroll highlighted item into view
  useEffect(() => {
    if (!listRef.current) return;
    const highlighted = listRef.current.querySelector('[data-highlighted="true"]');
    if (highlighted) {
      highlighted.scrollIntoView({ block: 'nearest' });
    }
  }, [highlightIndex]);

  if (!isOpen) return null;

  return (
    <div
      ref={popoverRef}
      className="absolute top-full right-0 mt-2 z-50 w-[480px] max-h-[420px] flex flex-col rounded-xl border border-cyber-border/30 bg-cyber-bg/95 shadow-[0_0_40px_rgba(0,255,249,0.2)] backdrop-blur-md animate-fade-in"
    >
      {/* Header */}
      <div className="flex items-center justify-between px-3 py-2 border-b border-cyber-border/60">
        <span className="text-xs font-semibold text-gray-300 uppercase tracking-wider">
          Switch Model
        </span>
        <div className="flex items-center gap-2">
          <button
            type="button"
            onClick={handleRefresh}
            disabled={!connected || isRefreshing}
            className="p-1 rounded text-gray-400 hover:text-cyber-cyan hover:bg-cyber-cyan/10 transition-colors disabled:opacity-50"
            title="Refresh model list"
          >
            <RefreshCw className={`h-3.5 w-3.5 ${isRefreshing ? 'animate-spin' : ''}`} />
          </button>
          <button
            type="button"
            onClick={onClose}
            className="p-1 rounded text-gray-400 hover:text-gray-200 hover:bg-cyber-surface/60 transition-colors"
          >
            <X className="h-3.5 w-3.5" />
          </button>
        </div>
      </div>

      {/* Target selector */}
      <div className="px-3 py-2 border-b border-cyber-border/40">
        <div className="flex items-center gap-2 text-xs">
          <span className="text-[10px] uppercase tracking-widest text-gray-500">Target</span>
          <select
            value={target}
            onChange={(e) => setTarget(e.target.value)}
            className="flex-1 rounded-lg border border-cyber-border bg-cyber-surface/70 px-2 py-1 text-xs text-gray-200 focus:border-cyber-cyan focus:outline-none"
            disabled={!connected}
          >
            <option value={TARGET_ACTIVE}>Active agent</option>
            <option value={TARGET_ALL}>All agents</option>
            {targetAgents.map((agent) => (
              <option key={agent.id} value={agent.id}>
                {agent.name}
              </option>
            ))}
          </select>
        </div>
      </div>

      {/* Filter input */}
      <div className="px-3 py-2 border-b border-cyber-border/40">
        <div className="relative">
          <Search className="absolute left-2.5 top-1/2 -translate-y-1/2 h-3.5 w-3.5 text-gray-500" />
          <input
            ref={inputRef}
            type="text"
            value={filter}
            onChange={(e) => setFilter(e.target.value)}
            onKeyDown={handleKeyDown}
            placeholder="Filter models..."
            className="w-full rounded-lg border border-cyber-border bg-cyber-surface/70 pl-8 pr-3 py-1.5 text-xs text-gray-200 placeholder:text-gray-500 focus:border-cyber-cyan focus:outline-none"
          />
        </div>
      </div>

      {/* Model list */}
      <div ref={listRef} className="flex-1 overflow-y-auto px-1 py-1">
        {allModels.length === 0 ? (
          <div className="px-3 py-6 text-center text-xs text-gray-500">
            Loading models...
          </div>
        ) : filteredGroups.length === 0 ? (
          <div className="px-3 py-6 text-center text-xs text-gray-500">
            No models match "{filter}"
          </div>
        ) : (
          filteredGroups.map((group) => (
            <div key={group.provider} className="mb-1">
              {/* Provider header */}
              <div className="sticky top-0 z-10 px-2 py-1 text-[10px] font-semibold uppercase tracking-widest text-gray-500 bg-cyber-bg/95">
                {group.provider}
              </div>
              {/* Models */}
              {group.models.map((model) => {
                const flatIndex = flatItems.findIndex(
                  (item) => item.provider === group.provider && item.model === model
                );
                const isHighlighted = flatIndex === highlightIndex;
                const isSelected =
                  selectedModel?.provider === group.provider && selectedModel?.model === model;
                const isCurrent =
                  currentProvider === group.provider && currentModel === model;

                return (
                  <button
                    key={model}
                    type="button"
                    data-highlighted={isHighlighted}
                    onClick={() => setSelectedModel({ provider: group.provider, model })}
                    className={`w-full flex items-center gap-2 px-2 py-1.5 rounded-lg text-left text-xs transition-colors ${
                      isSelected
                        ? 'bg-cyber-cyan/20 text-cyber-cyan border border-cyber-cyan/40'
                        : isHighlighted
                        ? 'bg-cyber-surface/80 text-gray-200'
                        : 'text-gray-300 hover:bg-cyber-surface/60'
                    }`}
                  >
                    <ChevronRight
                      className={`h-3 w-3 flex-shrink-0 transition-opacity ${
                        isSelected ? 'opacity-100 text-cyber-cyan' : 'opacity-0'
                      }`}
                    />
                    <span className="flex-1 truncate">{model}</span>
                    {isCurrent && (
                      <span className="flex-shrink-0 px-1.5 py-0.5 rounded text-[9px] uppercase tracking-wider bg-cyber-purple/20 text-cyber-purple border border-cyber-purple/30">
                        current
                      </span>
                    )}
                  </button>
                );
              })}
            </div>
          ))
        )}
      </div>

      {/* Footer with switch button */}
      <div className="px-3 py-2 border-t border-cyber-border/60 bg-cyber-surface/30">
        <div className="flex items-center justify-between gap-3">
          <div className="text-[10px] text-gray-500 truncate flex-1">
            {selectedModel ? (
              <span className="text-gray-300">
                {selectedModel.provider} / {selectedModel.model}
              </span>
            ) : (
              'Select a model above'
            )}
          </div>
          <button
            type="button"
            onClick={handleSwitch}
            disabled={!canSwitch}
            className={`px-4 py-1.5 rounded-lg text-xs font-medium transition-all ${
              canSwitch
                ? 'bg-cyber-cyan/20 border border-cyber-cyan text-cyber-cyan hover:bg-cyber-cyan/30 hover:shadow-neon-cyan'
                : 'bg-cyber-surface/50 border border-cyber-border text-gray-500 cursor-not-allowed'
            }`}
          >
            {routingMode === 'broadcast' && target === TARGET_ALL ? 'Switch all' : 'Switch'}
          </button>
        </div>
      </div>
    </div>
  );
}
