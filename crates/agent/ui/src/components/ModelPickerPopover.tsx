import { useCallback, useMemo, useState, useRef } from 'react';
import { Command } from 'cmdk';
import * as Popover from '@radix-ui/react-popover';
import { RefreshCw, Search, X, ChevronRight, ChevronDown } from 'lucide-react';
import type { ModelEntry, RecentModelEntry, RoutingMode, UiAgentInfo } from '../types';
import { useUiStore } from '../store/uiStore';

/* ------------------------------------------------------------------ */
/*  Types                                                              */
/* ------------------------------------------------------------------ */

interface ModelPickerPopoverProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  connected: boolean;
  routingMode: RoutingMode;
  activeAgentId: string;
  sessionId: string | null;
  sessionsByAgent: Record<string, string>;
  agents: UiAgentInfo[];
  allModels: ModelEntry[];
  currentProvider?: string;
  currentModel?: string;
  currentWorkspace: string | null;
  recentModelsByWorkspace: Record<string, RecentModelEntry[]>;
  agentMode: string;
  onRefresh: () => void;
  onSetSessionModel: (sessionId: string, modelId: string) => void;
}

const TARGET_ACTIVE = 'active';
const TARGET_ALL = 'all';

/* ------------------------------------------------------------------ */
/*  Helpers                                                            */
/* ------------------------------------------------------------------ */

const RECENT_PREFIX = 'recent:';
const normalizeValue = (value: string) => 
  value.startsWith(RECENT_PREFIX) ? value.slice(RECENT_PREFIX.length) : value;

interface GroupedModels {
  provider: string;
  models: string[];
}

const localeCompare = new Intl.Collator(undefined, { sensitivity: 'base' }).compare;

function groupByProvider(models: ModelEntry[]): GroupedModels[] {
  const map = new Map<string, string[]>();
  for (const entry of models) {
    const existing = map.get(entry.provider) ?? [];
    existing.push(entry.model);
    map.set(entry.provider, existing);
  }
  return Array.from(map.entries())
    .sort(([a], [b]) => localeCompare(a, b))
    .map(([provider, models]) => ({
      provider,
      models: [...models].sort(localeCompare),
    }));
}

/* ------------------------------------------------------------------ */
/*  Component                                                          */
/* ------------------------------------------------------------------ */

export function ModelPickerPopover({
  open,
  onOpenChange,
  connected,
  routingMode,
  activeAgentId: _activeAgentId,
  sessionId,
  sessionsByAgent,
  agents,
  allModels,
  currentProvider,
  currentModel,
  currentWorkspace,
  recentModelsByWorkspace,
  agentMode,
  onRefresh,
  onSetSessionModel,
}: ModelPickerPopoverProps) {
  const [target, setTarget] = useState(TARGET_ACTIVE);
  const [isRefreshing, setIsRefreshing] = useState(false);
  const [selectedValue, setSelectedValue] = useState('');
  const inputRef = useRef<HTMLInputElement>(null);
  const { setModeModelPreference, focusMainInput } = useUiStore();

  const targetAgents = useMemo(
    () => agents.filter((agent) => sessionsByAgent[agent.id]),
    [agents, sessionsByAgent],
  );

  const grouped = useMemo(() => groupByProvider(allModels), [allModels]);

  // Build a lookup map so we can resolve provider from the cmdk value
  const modelMap = useMemo(() => {
    const m = new Map<string, { provider: string; model: string }>();
    for (const entry of allModels) {
      m.set(`${entry.provider}/${entry.model}`, { provider: entry.provider, model: entry.model });
    }
    return m;
  }, [allModels]);
  
  // Get recent models for current workspace from backend data
  const recentModels = useMemo(() => {
    const key = currentWorkspace ?? '';  // Empty string for null workspace
    const recent = recentModelsByWorkspace[key] || [];
    
    // Filter to only show models that are currently available and limit to 5
    return recent
      .filter(entry => 
        allModels.some(m => m.provider === entry.provider && m.model === entry.model)
      )
      .slice(0, 5);
  }, [currentWorkspace, recentModelsByWorkspace, allModels]);

  const hasTargetSession =
    target === TARGET_ALL
      ? Object.values(sessionsByAgent).length > 0
      : target === TARGET_ACTIVE
        ? Boolean(sessionId)
        : Boolean(sessionsByAgent[target]);

  const selectedEntry = modelMap.get(normalizeValue(selectedValue));
  const canSwitch = connected && selectedEntry !== undefined && hasTargetSession;

  /* ---- actions ---- */

  const handleRefresh = useCallback(() => {
    setIsRefreshing(true);
    onRefresh();
    setTimeout(() => setIsRefreshing(false), 1000);
  }, [onRefresh]);

  const switchModel = useCallback(
    (value: string) => {
      const entry = modelMap.get(normalizeValue(value));
      if (!entry) return;

      const modelId = `${entry.provider}/${entry.model}`;
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
      
      // Save the model preference for the current agent mode
      setModeModelPreference(agentMode, entry.provider, entry.model);
      
      // Backend will automatically record model usage and refresh recent models
      
      onOpenChange(false);
      
      // Focus is handled by onCloseAutoFocus in Popover.Content
    },
    [modelMap, target, sessionsByAgent, sessionId, onSetSessionModel, onOpenChange, agentMode, setModeModelPreference],
  );

  const handleSwitchClick = useCallback(() => {
    if (canSwitch) switchModel(selectedValue);
  }, [canSwitch, selectedValue, switchModel]);

  /* ---- trigger label ---- */
  const triggerLabel =
    currentProvider && currentModel
      ? `${currentProvider} / ${currentModel}`
      : 'Select model';

  /* ---- render ---- */

  return (
    <Popover.Root open={open} onOpenChange={onOpenChange}>
      <Popover.Trigger asChild>
        <button
          type="button"
          className="flex items-center gap-2 px-3 py-1.5 rounded-lg border border-cyber-border bg-cyber-bg/60 text-xs text-ui-secondary hover:border-cyber-cyan/60 hover:text-cyber-cyan transition-colors max-w-[280px]"
          title={currentProvider && currentModel 
            ? `${currentProvider} / ${currentModel} (${navigator.platform.includes('Mac') ? '⌘⇧M' : 'Ctrl+Shift+M'} to open)` 
            : `Select model (${navigator.platform.includes('Mac') ? '⌘⇧M' : 'Ctrl+Shift+M'})`}
        >
          <span className="truncate">{triggerLabel}</span>
          <ChevronDown
            className={`w-3.5 h-3.5 flex-shrink-0 transition-transform ${open ? 'rotate-180' : ''}`}
          />
        </button>
      </Popover.Trigger>

      <Popover.Portal>
        <Popover.Content
          align="end"
          sideOffset={8}
          className="z-50 w-[480px] max-h-[420px] flex flex-col rounded-xl border border-cyber-cyan/30 bg-cyber-bg shadow-lg shadow-cyber-cyan/25 animate-fade-in"
          onOpenAutoFocus={(e) => {
            e.preventDefault();
            inputRef.current?.focus();
          }}
          onCloseAutoFocus={(e) => {
            e.preventDefault();
            focusMainInput();
          }}
        >
          {/* Header */}
          <div className="flex items-center justify-between px-3 py-2 border-b border-cyber-border/60">
            <span className="text-xs font-semibold text-ui-secondary uppercase tracking-wider">
              Switch Model
            </span>
            <div className="flex items-center gap-2">
              <button
                type="button"
                onClick={handleRefresh}
                disabled={!connected || isRefreshing}
                className="p-1 rounded text-ui-secondary hover:text-cyber-cyan hover:bg-cyber-cyan/10 transition-colors disabled:opacity-50"
                title="Refresh model list"
              >
                <RefreshCw className={`h-3.5 w-3.5 ${isRefreshing ? 'animate-spin' : ''}`} />
              </button>
              <Popover.Close className="p-1 rounded text-ui-secondary hover:text-ui-primary hover:bg-cyber-surface/60 transition-colors">
                <X className="h-3.5 w-3.5" />
              </Popover.Close>
            </div>
          </div>

          {/* Target selector */}
          <div className="px-3 py-2 border-b border-cyber-border/40">
            <div className="flex items-center gap-2 text-xs">
              <span className="text-[10px] uppercase tracking-widest text-ui-muted">Target</span>
              <select
                value={target}
                onChange={(e) => setTarget(e.target.value)}
                className="flex-1 rounded-lg border border-cyber-border bg-cyber-surface/70 px-2 py-1 text-xs text-ui-primary focus:border-cyber-cyan focus:outline-none"
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

          {/* Command palette (filter + list) */}
          <Command
            label="Model picker"
            value={selectedValue}
            onValueChange={setSelectedValue}
            className="flex flex-col flex-1 min-h-0"
            filter={(value, search) => {
              // value is "provider/model" or "recent:provider/model", search is the user query
              const normalized = normalizeValue(value);
              if (normalized.toLowerCase().includes(search.toLowerCase())) return 1;
              return 0;
            }}
          >
            {/* Filter input */}
            <div className="px-3 py-2 border-b border-cyber-border/40">
              <div className="relative">
                <Search className="absolute left-2.5 top-1/2 -translate-y-1/2 h-3.5 w-3.5 text-ui-muted pointer-events-none" />
                <Command.Input
                  ref={inputRef}
                  placeholder="Filter models..."
                  className="w-full rounded-lg border border-cyber-border bg-cyber-surface/70 pl-8 pr-3 py-1.5 text-xs text-ui-primary placeholder:text-ui-muted focus:border-cyber-cyan focus:outline-none"
                />
              </div>
            </div>

            {/* Model list */}
            <Command.List className="flex-1 overflow-y-auto px-1 py-1 max-h-[240px]">
              {allModels.length === 0 ? (
                <Command.Loading className="px-3 py-6 text-center text-xs text-ui-muted">
                  Loading models...
                </Command.Loading>
              ) : (
                <>
                  <Command.Empty className="px-3 py-6 text-center text-xs text-ui-muted">
                    No models match your search
                  </Command.Empty>

                  {/* Recent Models Section */}
                  {recentModels.length > 0 && (
                    <Command.Group
                      heading="Recent"
                      className="mb-2 [&_[cmdk-group-heading]]:sticky [&_[cmdk-group-heading]]:top-0 [&_[cmdk-group-heading]]:z-10 [&_[cmdk-group-heading]]:px-2 [&_[cmdk-group-heading]]:py-1 [&_[cmdk-group-heading]]:text-[10px] [&_[cmdk-group-heading]]:font-semibold [&_[cmdk-group-heading]]:uppercase [&_[cmdk-group-heading]]:tracking-widest [&_[cmdk-group-heading]]:text-cyber-cyan [&_[cmdk-group-heading]]:bg-cyber-bg/95"
                    >
                      {recentModels.map((entry) => {
                        const itemValue = `${entry.provider}/${entry.model}`;
                        const isCurrent =
                          currentProvider === entry.provider && currentModel === entry.model;

                        return (
                          <Command.Item
                            key={`recent-${itemValue}`}
                            value={`${RECENT_PREFIX}${itemValue}`}
                            keywords={[entry.provider, entry.model]}
                            onSelect={(val) => switchModel(val)}
                            className="w-full flex items-center gap-2 px-2 py-1.5 rounded-lg text-left text-xs transition-colors text-ui-secondary data-[selected=true]:bg-cyber-cyan/20 data-[selected=true]:text-cyber-cyan data-[selected=true]:border data-[selected=true]:border-cyber-cyan/40 hover:bg-cyber-surface/60 cursor-pointer"
                          >
                            <ChevronRight className="cmdk-chevron h-3 w-3 flex-shrink-0 opacity-0 text-cyber-cyan transition-opacity" />
                            <span className="flex-1 truncate">
                              {entry.provider} / {entry.model}
                            </span>
                            {isCurrent && (
                              <span className="flex-shrink-0 px-1.5 py-0.5 rounded text-[9px] uppercase tracking-wider bg-cyber-purple/20 text-cyber-purple border border-cyber-purple/30">
                                current
                              </span>
                            )}
                          </Command.Item>
                        );
                      })}
                    </Command.Group>
                  )}

                  {/* Separator if we have recent models */}
                  {recentModels.length > 0 && (
                    <div className="h-px bg-cyber-border/40 my-2" />
                  )}

                  {/* Provider-grouped models */}
                  {grouped.map((group) => (
                    <Command.Group
                      key={group.provider}
                      heading={group.provider}
                      className="mb-1 [&_[cmdk-group-heading]]:sticky [&_[cmdk-group-heading]]:top-0 [&_[cmdk-group-heading]]:z-10 [&_[cmdk-group-heading]]:px-2 [&_[cmdk-group-heading]]:py-1 [&_[cmdk-group-heading]]:text-[10px] [&_[cmdk-group-heading]]:font-semibold [&_[cmdk-group-heading]]:uppercase [&_[cmdk-group-heading]]:tracking-widest [&_[cmdk-group-heading]]:text-ui-muted [&_[cmdk-group-heading]]:bg-cyber-bg/95"
                    >
                      {group.models.map((model) => {
                        const itemValue = `${group.provider}/${model}`;
                        const isCurrent =
                          currentProvider === group.provider && currentModel === model;

                        return (
                          <Command.Item
                            key={itemValue}
                            value={itemValue}
                            keywords={[group.provider, model]}
                            onSelect={(val) => switchModel(val)}
                            className="w-full flex items-center gap-2 px-2 py-1.5 rounded-lg text-left text-xs transition-colors text-ui-secondary data-[selected=true]:bg-cyber-cyan/20 data-[selected=true]:text-cyber-cyan data-[selected=true]:border data-[selected=true]:border-cyber-cyan/40 hover:bg-cyber-surface/60 cursor-pointer"
                          >
                            <ChevronRight className="cmdk-chevron h-3 w-3 flex-shrink-0 opacity-0 text-cyber-cyan transition-opacity" />
                            <span className="flex-1 truncate">{model}</span>
                            {isCurrent && (
                              <span className="flex-shrink-0 px-1.5 py-0.5 rounded text-[9px] uppercase tracking-wider bg-cyber-purple/20 text-cyber-purple border border-cyber-purple/30">
                                current
                              </span>
                            )}
                          </Command.Item>
                        );
                      })}
                    </Command.Group>
                  ))}
                </>
              )}
            </Command.List>
          </Command>

          {/* Footer with switch button */}
          <div className="px-3 py-2 border-t border-cyber-border/60 bg-cyber-surface/30">
            <div className="flex items-center justify-between gap-3">
              <div className="text-[10px] text-ui-muted truncate flex-1">
                {selectedEntry ? (
                  <span className="text-ui-secondary">
                    {selectedEntry.provider} / {selectedEntry.model}
                  </span>
                ) : (
                  'Select a model above'
                )}
              </div>
              <button
                type="button"
                onClick={handleSwitchClick}
                disabled={!canSwitch}
                className={`px-4 py-1.5 rounded-lg text-xs font-medium transition-all ${
                  canSwitch
                    ? 'bg-cyber-cyan/20 border border-cyber-cyan text-cyber-cyan hover:bg-cyber-cyan/30 hover:shadow-neon-cyan'
                    : 'bg-cyber-surface/50 border border-cyber-border text-ui-muted cursor-not-allowed'
                }`}
              >
                {routingMode === 'broadcast' && target === TARGET_ALL ? 'Switch all' : 'Switch'}
              </button>
            </div>
          </div>
        </Popover.Content>
      </Popover.Portal>
    </Popover.Root>
  );
}
