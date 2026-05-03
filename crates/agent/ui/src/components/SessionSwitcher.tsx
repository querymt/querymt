import { useMemo, useRef, useEffect, useState } from 'react';
import { Command } from 'cmdk';
import Fuse from 'fuse.js';
import { Plus, GitBranch, Clock, Globe, Trash2, X, ChevronDown, ChevronRight } from 'lucide-react';
import { SessionGroup, SessionSummary } from '../types';
import { useUiStore } from '../store/uiStore';

/**
 * SessionSwitcher - Cmd+K modal for quickly switching sessions
 * 
 * Uses fuzzy search (fuse.js) to filter sessions by ID, title, or name
 * Renders as a command palette overlay
 */

interface SessionSwitcherProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  groups: SessionGroup[];
  activeSessionId: string | null;
  thinkingBySession: Map<string, Set<string>>;
  onNewSession: () => Promise<void>;
  onSelectSession: (sessionId: string) => void;
  onDeleteSession: (sessionId: string, sessionLabel?: string) => void;
  onLoadSessionChildren?: (parentSessionId: string) => void;
  sessionChildrenLoading?: Set<string>;
  connected: boolean;
}

type SessionWithChildren = SessionSummary & { children?: SessionSummary[] };

interface FlatSession extends SessionWithChildren {
  workspace: string; // cwd from the group
  isChild: boolean;
  isRecurring: boolean;
  isMemory: boolean;
  isRemote: boolean;
}

export function SessionSwitcher({
  open,
  onOpenChange,
  groups,
  activeSessionId,
  thinkingBySession,
  onNewSession,
  onSelectSession,
  onDeleteSession,
  onLoadSessionChildren,
  sessionChildrenLoading = new Set(),
  connected,
}: SessionSwitcherProps) {
  const inputRef = useRef<HTMLInputElement>(null);
  const [search, setSearch] = useState('');
  const [selectedValue, setSelectedValue] = useState('');
  const [expandedSessions, setExpandedSessions] = useState<Set<string>>(new Set());
  const { focusMainInput } = useUiStore();
  
  // Flatten all sessions from all groups into a single searchable list.
  const flatSessions = useMemo(() => {
    const toFlat = (session: SessionWithChildren, workspace: string): FlatSession => ({
      ...session,
      workspace,
      isChild: !!session.parent_session_id,
      isRecurring: session.session_kind === 'recurring',
      isMemory: session.session_kind === 'memory',
      isRemote: !!session.node,
    });
    const roots = groups
      .flatMap((group) => {
        const cwd = group.cwd || 'No workspace';
        return (group.sessions as SessionWithChildren[])
          .filter((session) => session.fork_origin !== 'delegation')
          .map((session) => ({ session, cwd }));
      })
      .sort((a, b) => {
        const aTime = a.session.updated_at ? Date.parse(a.session.updated_at) : 0;
        const bTime = b.session.updated_at ? Date.parse(b.session.updated_at) : 0;
        return bTime - aTime;
      });

    const flat: FlatSession[] = [];
    for (const { session, cwd } of roots) {
      flat.push(toFlat(session, cwd));
      if (expandedSessions.has(session.session_id) && session.children) {
        for (const child of session.children) {
          if (child.fork_origin !== 'delegation') {
            flat.push(toFlat(child, cwd));
          }
        }
      }
    }
    
    return flat;
  }, [groups, expandedSessions]);
  
  // Setup fuse.js for fuzzy search
  const fuse = useMemo(() => {
    return new Fuse(flatSessions, {
      keys: [
        { name: 'session_id', weight: 2 },
        { name: 'title', weight: 3 },
        { name: 'name', weight: 3 },
        { name: 'workspace', weight: 1 },
      ],
      threshold: 0.4,
      includeScore: true,
    });
  }, [flatSessions]);
  
  // Filter sessions based on search input
  const filteredSessions = useMemo(() => {
    if (!search.trim()) {
      // No search: show recent sessions (limit to 10)
      return flatSessions.slice(0, 10);
    }
    
    // Fuzzy search
    const results = fuse.search(search);
    return results.map(r => r.item).slice(0, 10);
  }, [search, flatSessions, fuse]);
  
  // Reset search when modal opens
  useEffect(() => {
    if (open) {
      setSearch('');
      // Auto-focus input
      setTimeout(() => inputRef.current?.focus(), 0);
    }
  }, [open]);

  // Keep command selection on a valid item as list updates after deletes/search.
  useEffect(() => {
    if (!open) {
      return;
    }

    if (filteredSessions.length === 0) {
      if (selectedValue !== '') {
        setSelectedValue('');
      }
      inputRef.current?.focus();
      return;
    }

    const stillValid = filteredSessions.some((session) => session.session_id === selectedValue);
    if (!stillValid) {
      setSelectedValue(filteredSessions[0].session_id);
    }
  }, [open, filteredSessions, selectedValue]);
  
  // Format timestamp helper
  const formatTimestamp = (timestamp?: string) => {
    if (!timestamp) return '';
    const date = new Date(timestamp);
    const now = new Date();
    const diffMs = now.getTime() - date.getTime();
    const diffMins = Math.floor(diffMs / 60000);
    const diffHours = Math.floor(diffMs / 3600000);
    const diffDays = Math.floor(diffMs / 86400000);
    
    if (diffMins < 1) return 'just now';
    if (diffMins < 60) return `${diffMins} min${diffMins > 1 ? 's' : ''} ago`;
    if (diffHours < 24) return `${diffHours} hour${diffHours > 1 ? 's' : ''} ago`;
    if (diffDays === 1) return 'yesterday';
    if (diffDays < 7) return `${diffDays} days ago`;
    
    return date.toLocaleDateString();
  };
  
  // Handle session selection
  const handleSelectSession = (sessionId: string) => {
    if (sessionId !== activeSessionId) {
      onSelectSession(sessionId);
    }
    onOpenChange(false);
    // Return focus to the main input after closing
    focusMainInput();
  };
  
  // Handle new session
  const handleNewSession = async () => {
    onOpenChange(false);
    await onNewSession();
    // Return focus to the main input after creating new session
    focusMainInput();
  };

  const handleDeleteSession = (sessionId: string, label: string, closeAfterDelete: boolean = true) => {
    // TODO: consider a user setting to require confirmation before deleting sessions.
    onDeleteSession(sessionId, label);
    if (closeAfterDelete) {
      onOpenChange(false);
      focusMainInput();
    }
  };

  const getNeighborSessionId = (sessionId: string): string => {
    const index = filteredSessions.findIndex((session) => session.session_id === sessionId);
    if (index < 0) {
      return filteredSessions[0]?.session_id ?? '';
    }
    if (filteredSessions.length === 1) {
      return '';
    }
    const fallbackIndex = index < filteredSessions.length - 1 ? index + 1 : index - 1;
    return filteredSessions[fallbackIndex]?.session_id ?? '';
  };
  
  const handleDeleteKey = (event: React.KeyboardEvent<HTMLDivElement>) => {
    if (event.key !== 'Delete') {
      return;
    }

    const targetSessionId = selectedValue || filteredSessions[0]?.session_id;
    if (!targetSessionId) {
      return;
    }

    event.preventDefault();
    event.stopPropagation();

    const targetSession = filteredSessions.find((session) => session.session_id === targetSessionId);
    const sessionLabel = targetSession?.title || targetSession?.name || 'Untitled session';

    setSelectedValue(getNeighborSessionId(targetSessionId));
    handleDeleteSession(targetSessionId, sessionLabel, false);
  };

  if (!open) return null;
  
  return (
    <>
      {/* Backdrop */}
      <div 
        data-testid="session-switcher-backdrop"
        className="fixed inset-0 bg-surface-canvas/80 z-40 animate-fade-in"
        onClick={() => onOpenChange(false)}
      />
      
      {/* Command Palette - Wrapper for click-outside handling */}
      <div 
        data-testid="session-switcher-container"
        className="fixed inset-0 z-50 flex items-start justify-center pt-[10vh] md:pt-[15vh] px-3 md:px-0"
        onClick={(e) => {
          // Close on click outside the command palette
          if (e.target === e.currentTarget) {
            onOpenChange(false);
          }
        }}
      >
        <Command
          className="w-full max-w-2xl bg-surface-elevated border-2 border-accent-primary/40 rounded-xl shadow-[0_0_40px_rgba(var(--accent-primary-rgb),0.3)] overflow-hidden animate-scale-in"
          shouldFilter={false} // We handle filtering manually with fuse.js
          value={selectedValue}
          onValueChange={setSelectedValue}
          onKeyDownCapture={handleDeleteKey}
        >
          {/* Search input */}
          <div className="flex items-center gap-3 px-4 py-3 border-b border-surface-border/60">
            <div className="text-accent-primary text-sm font-mono">🔍</div>
            <Command.Input
              ref={inputRef}
              value={search}
              onValueChange={setSearch}
              placeholder="Search sessions by ID, title, or workspace..."
              className="flex-1 bg-transparent text-ui-primary placeholder:text-ui-muted text-sm focus:outline-none"
            />
            <button
              type="button"
              onClick={() => onOpenChange(false)}
              className="sm:hidden p-1.5 rounded hover:bg-surface-canvas transition-colors text-ui-secondary hover:text-ui-primary"
              aria-label="Close"
            >
              <X className="w-5 h-5" />
            </button>
            <kbd className="hidden sm:inline-block px-2 py-1 text-[10px] font-mono bg-surface-canvas border border-surface-border rounded text-ui-muted">
              ESC
            </kbd>
          </div>
          
          {/* Results */}
          <Command.List className="max-h-[400px] overflow-y-auto p-2 custom-scrollbar">
            <Command.Empty className="px-4 py-8 text-center text-sm text-ui-muted">
              No sessions found
            </Command.Empty>
            
            {/* Recent Sessions */}
            {!search && (
              <Command.Group heading="Recent Sessions" className="mb-2">
                {filteredSessions.map((session) => {
                  const isActive = activeSessionId === session.session_id;
                  const isThinking = (thinkingBySession.get(session.session_id)?.size ?? 0) > 0;
                  const sessionLabel = session.title || session.name || 'Untitled session';
                  const hasLoadedChildren = !!session.children && session.children.length > 0;
                  const isExpanded = expandedSessions.has(session.session_id);
                  const isLoadingChildren = sessionChildrenLoading.has(session.session_id);
                  const forkCount = session.fork_count ?? 0;
                  const canExpandForks = !session.isRemote && forkCount > 0 && !session.isChild;
                  
                  return (
                    <Command.Item
                      key={session.session_id}
                      value={session.session_id}
                      onSelect={() => handleSelectSession(session.session_id)}
                      className="flex items-start gap-3 px-3 py-2.5 rounded-lg border border-surface-border/20 cursor-pointer transition-colors data-[selected=true]:bg-accent-primary/15 data-[selected=true]:border-accent-primary/35 hover:bg-surface-elevated/60 hover:border-surface-border/40 group"
                    >
                      {/* Status indicator */}
                      <div className="flex-shrink-0 pt-1">
                        {isActive ? (
                          <span className="w-2 h-2 rounded-full bg-accent-primary animate-pulse" />
                        ) : isThinking ? (
                          <span className="w-2 h-2 rounded-full bg-accent-tertiary animate-pulse" />
                        ) : (
                          <span className="w-2 h-2 rounded-full bg-ui-muted" />
                        )}
                      </div>
                      
                      {/* Content */}
                      <div className="flex-1 min-w-0">
                        {/* Title */}
                        <div className="flex items-center gap-2 mb-1">
                          {canExpandForks && (
                            <button
                              type="button"
                              onClick={(event) => {
                                event.stopPropagation();
                                setExpandedSessions((prev) => {
                                  const next = new Set(prev);
                                  if (next.has(session.session_id)) {
                                    next.delete(session.session_id);
                                  } else {
                                    next.add(session.session_id);
                                    if (!hasLoadedChildren) {
                                      onLoadSessionChildren?.(session.session_id);
                                    }
                                  }
                                  return next;
                                });
                              }}
                              className="inline-flex items-center gap-1 rounded px-1 py-0.5 text-accent-primary/70 hover:text-accent-primary hover:bg-accent-primary/10"
                              aria-label={`${isExpanded ? 'Collapse' : 'Expand'} forks for ${sessionLabel}`}
                            >
                              <GitBranch className="w-3 h-3" />
                              <span className="text-[10px] font-medium leading-none">{forkCount}</span>
                              {isLoadingChildren || isExpanded ? (
                                <ChevronDown className="w-3 h-3" />
                              ) : (
                                <ChevronRight className="w-3 h-3" />
                              )}
                            </button>
                          )}
                          {session.isChild && (
                            <GitBranch className="w-3 h-3 text-accent-primary/70 flex-shrink-0" />
                          )}
                          <span className="text-sm text-ui-primary font-medium truncate group-data-[selected=true]:text-accent-primary">
                            {sessionLabel}
                          </span>
                          {isActive && (
                            <span className="text-[10px] px-1.5 py-0.5 bg-accent-primary/20 text-accent-primary rounded border border-accent-primary/30 flex-shrink-0">
                              active
                            </span>
                          )}
                          {isThinking && (
                            <span className="text-[10px] px-1.5 py-0.5 bg-accent-tertiary/20 text-accent-tertiary rounded border border-accent-tertiary/30 flex-shrink-0">
                              thinking
                            </span>
                          )}
                          {session.isRecurring && !session.isMemory && (
                            <span className="text-[10px] px-1.5 py-0.5 bg-accent-secondary/20 text-accent-secondary rounded border border-accent-secondary/30 flex-shrink-0">
                              recurring
                            </span>
                          )}
                          {session.isMemory && (
                            <span className="text-[10px] px-1.5 py-0.5 bg-purple-500/20 text-purple-400 rounded border border-purple-500/30 flex-shrink-0">
                              memory
                            </span>
                          )}
                          {session.isRemote && (
                            <span className="inline-flex items-center gap-1 text-[10px] px-1.5 py-0.5 bg-accent-secondary/20 text-accent-secondary rounded border border-accent-secondary/30 flex-shrink-0">
                              <Globe className="w-2.5 h-2.5" />
                              {session.node}
                            </span>
                          )}
                        </div>
                        
                        {/* Metadata */}
                        <div className="flex items-center gap-3 text-xs text-ui-muted">
                          <span className="font-mono truncate">
                            {session.session_id.slice(0, 12)}...
                          </span>
                          {session.updated_at && (
                            <span className="flex items-center gap-1 flex-shrink-0">
                              <Clock className="w-3 h-3" />
                              {formatTimestamp(session.updated_at)}
                            </span>
                          )}
                          <span className="truncate text-ui-muted">
                            {session.workspace}
                          </span>
                        </div>
                      </div>
                      {!session.isRemote && (
                        <button
                          type="button"
                          onClick={(event) => {
                            event.stopPropagation();
                            handleDeleteSession(session.session_id, sessionLabel);
                          }}
                          className="self-center p-1.5 rounded-md text-ui-muted hover:text-status-warning hover:bg-status-warning/10"
                          title="Delete session"
                          aria-label={`Delete session ${sessionLabel}`}
                        >
                          <Trash2 className="w-3.5 h-3.5" />
                        </button>
                      )}
                    </Command.Item>
                  );
                })}
              </Command.Group>
            )}
            
            {/* Search Results */}
            {search && filteredSessions.length > 0 && (
              <Command.Group heading="Search Results">
                {filteredSessions.map((session) => {
                  const isActive = activeSessionId === session.session_id;
                  const isThinking = (thinkingBySession.get(session.session_id)?.size ?? 0) > 0;
                  const sessionLabel = session.title || session.name || 'Untitled session';
                  const hasLoadedChildren = !!session.children && session.children.length > 0;
                  const isExpanded = expandedSessions.has(session.session_id);
                  const isLoadingChildren = sessionChildrenLoading.has(session.session_id);
                  const forkCount = session.fork_count ?? 0;
                  const canExpandForks = !session.isRemote && forkCount > 0 && !session.isChild;
                  
                  return (
                    <Command.Item
                      key={session.session_id}
                      value={session.session_id}
                      onSelect={() => handleSelectSession(session.session_id)}
                      className="flex items-start gap-3 px-3 py-2.5 rounded-lg border border-surface-border/20 cursor-pointer transition-colors data-[selected=true]:bg-accent-primary/15 data-[selected=true]:border-accent-primary/35 hover:bg-surface-elevated/60 hover:border-surface-border/40 group"
                    >
                      {/* Status indicator */}
                      <div className="flex-shrink-0 pt-1">
                        {isActive ? (
                          <span className="w-2 h-2 rounded-full bg-accent-primary animate-pulse" />
                        ) : isThinking ? (
                          <span className="w-2 h-2 rounded-full bg-accent-tertiary animate-pulse" />
                        ) : (
                          <span className="w-2 h-2 rounded-full bg-ui-muted" />
                        )}
                      </div>
                      
                      {/* Content */}
                      <div className="flex-1 min-w-0">
                        {/* Title */}
                        <div className="flex items-center gap-2 mb-1">
                          {canExpandForks && (
                            <button
                              type="button"
                              onClick={(event) => {
                                event.stopPropagation();
                                setExpandedSessions((prev) => {
                                  const next = new Set(prev);
                                  if (next.has(session.session_id)) {
                                    next.delete(session.session_id);
                                  } else {
                                    next.add(session.session_id);
                                    if (!hasLoadedChildren) {
                                      onLoadSessionChildren?.(session.session_id);
                                    }
                                  }
                                  return next;
                                });
                              }}
                              className="inline-flex items-center gap-1 rounded px-1 py-0.5 text-accent-primary/70 hover:text-accent-primary hover:bg-accent-primary/10"
                              aria-label={`${isExpanded ? 'Collapse' : 'Expand'} forks for ${sessionLabel}`}
                            >
                              <GitBranch className="w-3 h-3" />
                              <span className="text-[10px] font-medium leading-none">{forkCount}</span>
                              {isLoadingChildren || isExpanded ? (
                                <ChevronDown className="w-3 h-3" />
                              ) : (
                                <ChevronRight className="w-3 h-3" />
                              )}
                            </button>
                          )}
                          {session.isChild && (
                            <GitBranch className="w-3 h-3 text-accent-primary/70 flex-shrink-0" />
                          )}
                          <span className="text-sm text-ui-primary font-medium truncate group-data-[selected=true]:text-accent-primary">
                            {sessionLabel}
                          </span>
                          {isActive && (
                            <span className="text-[10px] px-1.5 py-0.5 bg-accent-primary/20 text-accent-primary rounded border border-accent-primary/30 flex-shrink-0">
                              active
                            </span>
                          )}
                          {isThinking && (
                            <span className="text-[10px] px-1.5 py-0.5 bg-accent-tertiary/20 text-accent-tertiary rounded border border-accent-tertiary/30 flex-shrink-0">
                              thinking
                            </span>
                          )}
                          {session.isRecurring && !session.isMemory && (
                            <span className="text-[10px] px-1.5 py-0.5 bg-accent-secondary/20 text-accent-secondary rounded border border-accent-secondary/30 flex-shrink-0">
                              recurring
                            </span>
                          )}
                          {session.isMemory && (
                            <span className="text-[10px] px-1.5 py-0.5 bg-purple-500/20 text-purple-400 rounded border border-purple-500/30 flex-shrink-0">
                              memory
                            </span>
                          )}
                          {session.isRemote && (
                            <span className="inline-flex items-center gap-1 text-[10px] px-1.5 py-0.5 bg-accent-secondary/20 text-accent-secondary rounded border border-accent-secondary/30 flex-shrink-0">
                              <Globe className="w-2.5 h-2.5" />
                              {session.node}
                            </span>
                          )}
                        </div>
                        
                        {/* Metadata */}
                        <div className="flex items-center gap-3 text-xs text-ui-muted">
                          <span className="font-mono truncate">
                            {session.session_id.slice(0, 12)}...
                          </span>
                          {session.updated_at && (
                            <span className="flex items-center gap-1 flex-shrink-0">
                              <Clock className="w-3 h-3" />
                              {formatTimestamp(session.updated_at)}
                            </span>
                          )}
                          <span className="truncate text-ui-muted">
                            {session.workspace}
                          </span>
                        </div>
                      </div>
                      {!session.isRemote && (
                        <button
                          type="button"
                          onClick={(event) => {
                            event.stopPropagation();
                            handleDeleteSession(session.session_id, sessionLabel);
                          }}
                          className="self-center p-1.5 rounded-md text-ui-muted hover:text-status-warning hover:bg-status-warning/10"
                          title="Delete session"
                          aria-label={`Delete session ${sessionLabel}`}
                        >
                          <Trash2 className="w-3.5 h-3.5" />
                        </button>
                      )}
                    </Command.Item>
                  );
                })}
              </Command.Group>
            )}
            
            {/* Actions */}
            <Command.Separator className="my-2 border-t border-surface-border/40" />
            
            <Command.Group heading="Actions">
              <Command.Item
                onSelect={handleNewSession}
                disabled={!connected}
                className="flex items-center gap-3 px-3 py-2.5 rounded-lg border border-surface-border/20 cursor-pointer transition-colors data-[selected=true]:bg-accent-primary/15 data-[selected=true]:border-accent-primary/35 hover:bg-surface-elevated/60 hover:border-surface-border/40 data-[disabled=true]:opacity-50 data-[disabled=true]:cursor-not-allowed"
              >
                <Plus className="w-4 h-4 text-accent-primary flex-shrink-0" />
                <span className="flex-1 text-sm text-ui-primary">New Session</span>
                <kbd className="hidden sm:inline-block px-2 py-1 text-[10px] font-mono bg-surface-canvas border border-surface-border rounded text-ui-muted">
                  {navigator.platform.includes('Mac') ? '⌘+X N' : 'Ctrl+X N'}
                </kbd>
              </Command.Item>
            </Command.Group>
          </Command.List>
        </Command>
      </div>
    </>
  );
}
