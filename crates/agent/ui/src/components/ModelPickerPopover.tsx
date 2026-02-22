import { useCallback, useEffect, useMemo, useState, useRef } from 'react';
import { Command } from 'cmdk';
import * as Popover from '@radix-ui/react-popover';
import { RefreshCw, Search, X, ChevronRight, ChevronDown, Plus, Trash2 } from 'lucide-react';
import type {
  ModelDownloadStatus,
  ModelEntry,
  ProviderCapabilityEntry,
  RecentModelEntry,
  RoutingMode,
  UiAgentInfo,
} from '../types';
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
  /** Mesh node running the current provider, if any. Absent for local providers. */
  currentNode?: string;
  currentWorkspace: string | null;
  recentModelsByWorkspace: Record<string, RecentModelEntry[]>;
  agentMode: string;
  onRefresh: () => void;
  onSetSessionModel: (sessionId: string, modelId: string, node?: string) => void;
  providerCapabilities: Record<string, ProviderCapabilityEntry>;
  modelDownloads: Record<string, ModelDownloadStatus>;
  onAddCustomModelFromHf: (provider: string, repo: string, filename: string, displayName?: string) => void;
  onAddCustomModelFromFile: (provider: string, filePath: string, displayName?: string) => void;
  onDeleteCustomModel: (provider: string, modelId: string) => void;
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
  /** Display heading — "provider" for local, "provider (node)" for remote */
  heading: string;
  provider: string;
  node?: string;
  models: { value: string; display: string; id?: string; source?: string }[];
}

const localeCompare = new Intl.Collator(undefined, { sensitivity: 'base' }).compare;

/** Build a unique group key that separates local vs. remote providers. */
function groupKey(entry: ModelEntry): string {
  return entry.node ? `${entry.provider}@${entry.node}` : entry.provider;
}

function groupByProvider(models: ModelEntry[]): GroupedModels[] {
  const hasRemote = models.some((m) => m.node);
  const map = new Map<string, GroupedModels>();
  for (const entry of models) {
    const key = groupKey(entry);
    if (!map.has(key)) {
      // When remote models exist, annotate local groups with "(local)" and
      // remote groups with "(node)" so the user can distinguish them.
      const heading = entry.node
        ? `${entry.provider} (${entry.node})`
        : hasRemote
          ? `${entry.provider} (local)`
          : entry.provider;
      map.set(key, { heading, provider: entry.provider, node: entry.node, models: [] });
    }
    map.get(key)!.models.push({
      value: entry.model,
      display: entry.label ?? entry.model,
      id: entry.id,
      source: entry.source,
    });
  }
  return Array.from(map.values())
    .sort((a, b) => localeCompare(a.heading, b.heading))
    .map((g) => ({
      ...g,
      models: [...g.models].sort((a, b) => localeCompare(a.display, b.display)),
    }));
}

/** Build the cmdk item value for a model entry: "provider/model" or "provider@node/model". */
function itemValue(provider: string, model: string, node?: string): string {
  return node ? `${provider}@${node}/${model}` : `${provider}/${model}`;
}

function sessionModelId(provider: string, model: string, id?: string): string {
  const canonical = id ?? model;
  return canonical.includes(':') ? `${provider}/${canonical}` : `${provider}/${canonical}`;
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
  currentNode,
  currentWorkspace,
  recentModelsByWorkspace,
  agentMode,
  onRefresh,
  onSetSessionModel,
  providerCapabilities,
  modelDownloads,
  onAddCustomModelFromHf,
  onAddCustomModelFromFile,
  onDeleteCustomModel,
}: ModelPickerPopoverProps) {
  const [target, setTarget] = useState(TARGET_ACTIVE);
  const [isRefreshing, setIsRefreshing] = useState(false);
  const [selectedValue, setSelectedValue] = useState('');
  const [hfRepo, setHfRepo] = useState('');
  const [hfFilename, setHfFilename] = useState('');
  const [filePath, setFilePath] = useState('');
  const inputRef = useRef<HTMLInputElement>(null);
  const commandListRef = useRef<HTMLDivElement>(null);
  const { setModeModelPreference, focusMainInput } = useUiStore();

  // When the popover opens, auto-select the current model in the recent section
  // and scroll the list to the top so the Recent section is visible.
  // When it closes, reset selection for a clean slate next time.
  useEffect(() => {
    if (open && currentProvider && currentModel) {
      setSelectedValue(`${RECENT_PREFIX}${itemValue(currentProvider, currentModel)}`);
      setTimeout(() => {
        if (commandListRef.current) {
          commandListRef.current.scrollTop = 0;
        }
      }, 0);
    } else if (!open) {
      setSelectedValue('');
    }
  }, [open, currentProvider, currentModel]);

  const targetAgents = useMemo(
    () => agents.filter((agent) => sessionsByAgent[agent.id]),
    [agents, sessionsByAgent],
  );

  const grouped = useMemo(() => groupByProvider(allModels), [allModels]);

  const selectedProvider = currentProvider ?? '';
  const selectedProviderCapabilities = providerCapabilities[selectedProvider];
  const canManageCustomModels = Boolean(
    selectedProvider && !currentNode && selectedProviderCapabilities?.supports_custom_models,
  );

  const selectedProviderEntry = useMemo(
    () => allModels.find((m) => m.provider === selectedProvider && m.model === currentModel && !m.node),
    [allModels, selectedProvider, currentModel],
  );

  const activeDownload = useMemo(() => {
    if (!selectedProvider) return undefined;
    const values = Object.values(modelDownloads).filter((d) => d.provider === selectedProvider);
    return values.sort((a, b) => (b.percent ?? 0) - (a.percent ?? 0))[0];
  }, [modelDownloads, selectedProvider]);

  // Build a lookup map so we can resolve provider/model/node from the cmdk value
  const modelMap = useMemo(() => {
    const m = new Map<string, { provider: string; model: string; node?: string }>();
    for (const entry of allModels) {
      m.set(itemValue(entry.provider, entry.model, entry.node), {
        provider: entry.provider,
        model: entry.model,
        node: entry.node,
      });
    }
    return m;
  }, [allModels]);

  const modelIdByValue = useMemo(() => {
    const m = new Map<string, string>();
    for (const entry of allModels) {
      m.set(
        itemValue(entry.provider, entry.model, entry.node),
        sessionModelId(entry.provider, entry.model, entry.id),
      );
    }
    return m;
  }, [allModels]);
  
  // Get recent models for current workspace from backend data
  const recentModels = useMemo(() => {
    const key = currentWorkspace ?? '';  // Empty string for null workspace
    const recent = recentModelsByWorkspace[key] || [];
    
    // Filter to only show models that are currently available and limit to 5
    // Recent models are always local (no node field) — they come from local event history
    return recent
      .filter(entry => 
        allModels.some(m => m.provider === entry.provider && m.model === entry.model && !m.node)
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

      const normalized = normalizeValue(value);
      const modelId = modelIdByValue.get(normalized) ?? `${entry.provider}/${entry.model}`;
      const sessionIds = new Set<string>();

      if (target === TARGET_ALL) {
        Object.values(sessionsByAgent).forEach((id) => sessionIds.add(id));
      } else if (target === TARGET_ACTIVE) {
        if (sessionId) sessionIds.add(sessionId);
      } else if (sessionsByAgent[target]) {
        sessionIds.add(sessionsByAgent[target]);
      }

      if (sessionIds.size === 0) return;
      // Pass node along so the backend can route to the correct provider host
      sessionIds.forEach((id) => onSetSessionModel(id, modelId, entry.node));
      
      // Save the model preference for the current agent mode (local models only)
      if (!entry.node) {
        setModeModelPreference(agentMode, entry.provider, entry.model);
      }
      
      // Backend will automatically record model usage and refresh recent models
      
      onOpenChange(false);
      
      // Focus is handled by onCloseAutoFocus in Popover.Content
    },
    [
      modelMap,
      modelIdByValue,
      target,
      sessionsByAgent,
      sessionId,
      onSetSessionModel,
      onOpenChange,
      agentMode,
      setModeModelPreference,
    ],
  );

  const handleSwitchClick = useCallback(() => {
    if (canSwitch) switchModel(selectedValue);
  }, [canSwitch, selectedValue, switchModel]);

  const handleAddHfModel = useCallback(() => {
    const repo = hfRepo.trim();
    const filename = hfFilename.trim();
    if (!repo || !filename || !selectedProvider) return;
    onAddCustomModelFromHf(selectedProvider, repo, filename, filename);
    setHfRepo('');
    setHfFilename('');
  }, [hfRepo, hfFilename, onAddCustomModelFromHf, selectedProvider]);

  const handleAddFileModel = useCallback(() => {
    const path = filePath.trim();
    if (!path || !selectedProvider) return;
    const label = path.split('/').pop() || path;
    onAddCustomModelFromFile(selectedProvider, path, label);
    setFilePath('');
  }, [filePath, onAddCustomModelFromFile, selectedProvider]);

  const handleDeleteCurrentCustom = useCallback(() => {
    const modelId = selectedProviderEntry?.id;
    if (!modelId || selectedProviderEntry?.source !== 'custom' || !selectedProvider) return;
    onDeleteCustomModel(selectedProvider, modelId);
  }, [onDeleteCustomModel, selectedProviderEntry, selectedProvider]);

  /* ---- trigger label ---- */
  const triggerLabel =
    currentProvider && currentModel
      ? `${currentProvider} / ${currentModel}`
      : 'Select model';

  const triggerTitle = currentProvider && currentModel
    ? `${currentProvider} / ${currentModel}${currentNode ? ` on ${currentNode}` : ''} (${navigator.platform.includes('Mac') ? '⌘⇧M' : 'Ctrl+Shift+M'} to open)`
    : `Select model (${navigator.platform.includes('Mac') ? '⌘⇧M' : 'Ctrl+Shift+M'})`;

  /* ---- render ---- */

  return (
    <Popover.Root open={open} onOpenChange={onOpenChange}>
      <Popover.Trigger asChild>
        <button
          type="button"
          className="flex items-center gap-2 px-3 py-1.5 rounded-lg border border-surface-border bg-surface-canvas/60 text-xs text-ui-secondary hover:border-accent-primary/60 hover:text-accent-primary transition-colors w-[20rem] flex-shrink-0"
          title={triggerTitle}
        >
          <span className="truncate">{triggerLabel}</span>
          {currentNode && (
            <span className="flex-shrink-0 px-1.5 py-0.5 rounded text-[9px] uppercase tracking-wider bg-blue-500/10 text-blue-400 border border-blue-500/20">
              {currentNode}
            </span>
          )}
          <ChevronDown
            className={`w-3.5 h-3.5 flex-shrink-0 transition-transform ${open ? 'rotate-180' : ''}`}
          />
        </button>
      </Popover.Trigger>

      <Popover.Portal>
        <Popover.Content
          align="end"
          sideOffset={8}
          className="z-50 w-[480px] max-h-[420px] flex flex-col rounded-xl border border-accent-primary/30 bg-surface-canvas shadow-lg shadow-accent-primary/25 animate-fade-in"
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
          <div className="flex items-center justify-between px-3 py-2 border-b border-surface-border/60">
            <span className="text-xs font-semibold text-ui-secondary uppercase tracking-wider">
              Switch Model
            </span>
            <div className="flex items-center gap-2">
              <button
                type="button"
                onClick={handleRefresh}
                disabled={!connected || isRefreshing}
                className="p-1 rounded text-ui-secondary hover:text-accent-primary hover:bg-accent-primary/10 transition-colors disabled:opacity-50"
                title="Refresh model list"
              >
                <RefreshCw className={`h-3.5 w-3.5 ${isRefreshing ? 'animate-spin' : ''}`} />
              </button>
              <Popover.Close className="p-1 rounded text-ui-secondary hover:text-ui-primary hover:bg-surface-elevated/60 transition-colors">
                <X className="h-3.5 w-3.5" />
              </Popover.Close>
            </div>
          </div>

          {/* Target selector */}
          <div className="px-3 py-2 border-b border-surface-border/40">
            <div className="flex items-center gap-2 text-xs">
              <span className="text-[10px] uppercase tracking-widest text-ui-muted">Target</span>
              <select
                value={target}
                onChange={(e) => setTarget(e.target.value)}
                className="flex-1 rounded-lg border border-surface-border bg-surface-elevated/70 px-2 py-1 text-xs text-ui-primary focus:border-accent-primary focus:outline-none"
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
            {canManageCustomModels && (
              <div className="mt-2 pt-2 border-t border-surface-border/40 space-y-2">
                <div className="text-[10px] uppercase tracking-widest text-ui-muted">Add custom model ({selectedProvider})</div>
                <div className="flex items-center gap-2">
                  <input
                    value={hfRepo}
                    onChange={(e) => setHfRepo(e.target.value)}
                    placeholder="HF repo (owner/name-GGUF)"
                    className="flex-1 rounded-lg border border-surface-border bg-surface-elevated/70 px-2 py-1 text-xs text-ui-primary"
                  />
                  <input
                    value={hfFilename}
                    onChange={(e) => setHfFilename(e.target.value)}
                    placeholder="filename.gguf"
                    className="flex-1 rounded-lg border border-surface-border bg-surface-elevated/70 px-2 py-1 text-xs text-ui-primary"
                  />
                  <button
                    type="button"
                    onClick={handleAddHfModel}
                    className="px-2 py-1 rounded border border-surface-border text-ui-secondary hover:text-accent-primary"
                    title="Add from Hugging Face"
                  >
                    <Plus className="h-3.5 w-3.5" />
                  </button>
                </div>
                <div className="flex items-center gap-2">
                  <input
                    value={filePath}
                    onChange={(e) => setFilePath(e.target.value)}
                    placeholder="/absolute/path/to/model.gguf"
                    className="flex-1 rounded-lg border border-surface-border bg-surface-elevated/70 px-2 py-1 text-xs text-ui-primary"
                  />
                  <button
                    type="button"
                    onClick={handleAddFileModel}
                    className="px-2 py-1 rounded border border-surface-border text-ui-secondary hover:text-accent-primary"
                    title="Add local GGUF file"
                  >
                    <Plus className="h-3.5 w-3.5" />
                  </button>
                  <button
                    type="button"
                    onClick={handleDeleteCurrentCustom}
                    disabled={selectedProviderEntry?.source !== 'custom'}
                    className="px-2 py-1 rounded border border-surface-border text-ui-secondary hover:text-red-400 disabled:opacity-40"
                    title="Delete selected custom model"
                  >
                    <Trash2 className="h-3.5 w-3.5" />
                  </button>
                </div>
                {activeDownload && (
                  <div className="text-[10px] text-ui-muted">
                    {activeDownload.status}: {activeDownload.model_id}
                    {activeDownload.percent != null ? ` (${Math.round(activeDownload.percent)}%)` : ''}
                    {activeDownload.message ? ` - ${activeDownload.message}` : ''}
                  </div>
                )}
              </div>
            )}
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
            <div className="px-3 py-2 border-b border-surface-border/40">
              <div className="relative">
                <Search className="absolute left-2.5 top-1/2 -translate-y-1/2 h-3.5 w-3.5 text-ui-muted pointer-events-none" />
                <Command.Input
                  ref={inputRef}
                  placeholder="Filter models..."
                  className="w-full rounded-lg border border-surface-border bg-surface-elevated/70 pl-8 pr-3 py-1.5 text-xs text-ui-primary placeholder:text-ui-muted focus:border-accent-primary focus:outline-none"
                />
              </div>
            </div>

            {/* Model list */}
            <Command.List ref={commandListRef} className="flex-1 overflow-y-auto px-1 py-1 max-h-[240px]">
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
                      className="mb-2 [&_[cmdk-group-heading]]:sticky [&_[cmdk-group-heading]]:top-0 [&_[cmdk-group-heading]]:z-10 [&_[cmdk-group-heading]]:px-2 [&_[cmdk-group-heading]]:py-1 [&_[cmdk-group-heading]]:text-[10px] [&_[cmdk-group-heading]]:font-semibold [&_[cmdk-group-heading]]:uppercase [&_[cmdk-group-heading]]:tracking-widest [&_[cmdk-group-heading]]:text-accent-primary [&_[cmdk-group-heading]]:bg-surface-canvas/95"
                    >
                      {recentModels.map((entry) => {
                        const val = itemValue(entry.provider, entry.model);
                        const isCurrent =
                          currentProvider === entry.provider && currentModel === entry.model;

                        return (
                          <Command.Item
                            key={`recent-${val}`}
                            value={`${RECENT_PREFIX}${val}`}
                            keywords={[entry.provider, entry.model]}
                            onSelect={(v) => switchModel(v)}
                            className="w-full flex items-center gap-2 px-2 py-1.5 rounded-lg text-left text-xs transition-colors text-ui-secondary data-[selected=true]:bg-accent-primary/20 data-[selected=true]:text-accent-primary data-[selected=true]:border data-[selected=true]:border-accent-primary/40 hover:bg-surface-elevated/60 cursor-pointer"
                          >
                            <ChevronRight className="cmdk-chevron h-3 w-3 flex-shrink-0 opacity-0 text-accent-primary transition-opacity" />
                            <span className="flex-1 truncate">
                              {entry.provider} / {entry.model}
                            </span>
                            {isCurrent && (
                              <span className="flex-shrink-0 px-1.5 py-0.5 rounded text-[9px] uppercase tracking-wider bg-accent-tertiary/20 text-accent-tertiary border border-accent-tertiary/30">
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
                    <div className="h-px bg-surface-border/40 my-2" />
                  )}

                  {/* Provider-grouped models (local + remote) */}
                  {grouped.map((group) => (
                    <Command.Group
                      key={group.heading}
                      heading={group.heading}
                      className="mb-1 [&_[cmdk-group-heading]]:sticky [&_[cmdk-group-heading]]:top-0 [&_[cmdk-group-heading]]:z-10 [&_[cmdk-group-heading]]:px-2 [&_[cmdk-group-heading]]:py-1 [&_[cmdk-group-heading]]:text-[10px] [&_[cmdk-group-heading]]:font-semibold [&_[cmdk-group-heading]]:uppercase [&_[cmdk-group-heading]]:tracking-widest [&_[cmdk-group-heading]]:text-ui-muted [&_[cmdk-group-heading]]:bg-surface-canvas/95"
                    >
                      {group.models.map((model) => {
                        const val = itemValue(group.provider, model.value, group.node);
                        const isCurrent =
                          currentProvider === group.provider && currentModel === model.value;

                        return (
                          <Command.Item
                            key={val}
                            value={val}
                            keywords={[group.provider, model.display, model.value, ...(group.node ? [group.node] : [])]}
                            onSelect={(v) => switchModel(v)}
                            className="w-full flex items-center gap-2 px-2 py-1.5 rounded-lg text-left text-xs transition-colors text-ui-secondary data-[selected=true]:bg-accent-primary/20 data-[selected=true]:text-accent-primary data-[selected=true]:border data-[selected=true]:border-accent-primary/40 hover:bg-surface-elevated/60 cursor-pointer"
                          >
                            <ChevronRight className="cmdk-chevron h-3 w-3 flex-shrink-0 opacity-0 text-accent-primary transition-opacity" />
                            <span className="flex-1 truncate">{model.display}</span>
                            {group.node ? (
                              <span className="flex-shrink-0 px-1.5 py-0.5 rounded text-[9px] uppercase tracking-wider bg-blue-500/10 text-blue-400 border border-blue-500/20">
                                {group.node}
                              </span>
                            ) : (
                              <span className="flex-shrink-0 px-1.5 py-0.5 rounded text-[9px] uppercase tracking-wider bg-emerald-500/10 text-emerald-400 border border-emerald-500/20">
                                local
                              </span>
                            )}
                            {isCurrent && (
                              <span className="flex-shrink-0 px-1.5 py-0.5 rounded text-[9px] uppercase tracking-wider bg-accent-tertiary/20 text-accent-tertiary border border-accent-tertiary/30">
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
          <div className="px-3 py-2 border-t border-surface-border/60 bg-surface-elevated/30">
            <div className="flex items-center justify-between gap-3">
              <div className="text-[10px] text-ui-muted truncate flex-1 flex items-center gap-1.5">
                {selectedEntry ? (
                  <>
                    <span className="text-ui-secondary">
                      {selectedEntry.provider} / {selectedEntry.model}
                    </span>
                    {selectedEntry.node ? (
                      <span className="px-1 py-0.5 rounded text-[9px] uppercase tracking-wider bg-blue-500/10 text-blue-400 border border-blue-500/20">
                        {selectedEntry.node}
                      </span>
                    ) : (
                      <span className="px-1 py-0.5 rounded text-[9px] uppercase tracking-wider bg-emerald-500/10 text-emerald-400 border border-emerald-500/20">
                        local
                      </span>
                    )}
                  </>
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
                    ? 'bg-accent-primary/20 border border-accent-primary text-accent-primary hover:bg-accent-primary/30 hover:shadow-glow-primary'
                    : 'bg-surface-elevated/50 border border-surface-border text-ui-muted cursor-not-allowed'
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
