import { useMemo, useRef, useEffect, useState } from 'react';
import { Command } from 'cmdk';
import { useNavigate } from 'react-router-dom';
import Fuse from 'fuse.js';
import { Plus, GitBranch, Clock } from 'lucide-react';
import { SessionGroup, SessionSummary } from '../types';

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
  connected: boolean;
}

interface FlatSession extends SessionSummary {
  workspace: string; // cwd from the group
  isChild: boolean;
  isDelegation: boolean;
}

export function SessionSwitcher({
  open,
  onOpenChange,
  groups,
  activeSessionId,
  thinkingBySession,
  onNewSession,
  connected,
}: SessionSwitcherProps) {
  const navigate = useNavigate();
  const inputRef = useRef<HTMLInputElement>(null);
  const [search, setSearch] = useState('');
  
  // Flatten all sessions from all groups into a single searchable list
  const flatSessions = useMemo(() => {
    const flat: FlatSession[] = [];
    
    for (const group of groups) {
      const cwd = group.cwd || 'No workspace';
      
      for (const session of group.sessions) {
        flat.push({
          ...session,
          workspace: cwd,
          isChild: !!session.parent_session_id,
          isDelegation: session.fork_origin === 'delegation',
        });
      }
    }
    
    // Sort by updated_at (most recent first)
    return flat.sort((a, b) => {
      if (!a.updated_at) return 1;
      if (!b.updated_at) return -1;
      return new Date(b.updated_at).getTime() - new Date(a.updated_at).getTime();
    });
  }, [groups]);
  
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
    navigate(`/session/${sessionId}`);
    onOpenChange(false);
  };
  
  // Handle new session
  const handleNewSession = async () => {
    onOpenChange(false);
    await onNewSession();
  };
  
  if (!open) return null;
  
  return (
    <>
      {/* Backdrop */}
      <div 
        className="fixed inset-0 bg-black/60 backdrop-blur-sm z-40 animate-fade-in"
        onClick={() => onOpenChange(false)}
      />
      
      {/* Command Palette - Wrapper for click-outside handling */}
      <div 
        className="fixed inset-0 z-50 flex items-start justify-center pt-[15vh] px-4"
        onClick={(e) => {
          // Close on click outside the command palette
          if (e.target === e.currentTarget) {
            onOpenChange(false);
          }
        }}
      >
        <Command
          className="w-full max-w-2xl bg-cyber-surface border-2 border-cyber-cyan/40 rounded-xl shadow-[0_0_40px_rgba(0,255,249,0.3)] overflow-hidden animate-scale-in"
          shouldFilter={false} // We handle filtering manually with fuse.js
        >
          {/* Search input */}
          <div className="flex items-center gap-3 px-4 py-3 border-b border-cyber-border/60">
            <div className="text-cyber-cyan text-sm font-mono">🔍</div>
            <Command.Input
              ref={inputRef}
              value={search}
              onValueChange={setSearch}
              placeholder="Search sessions by ID, title, or workspace..."
              className="flex-1 bg-transparent text-gray-100 placeholder-gray-500 text-sm focus:outline-none"
            />
            <kbd className="hidden sm:inline-block px-2 py-1 text-[10px] font-mono bg-cyber-bg border border-cyber-border rounded text-gray-500">
              ESC
            </kbd>
          </div>
          
          {/* Results */}
          <Command.List className="max-h-[400px] overflow-y-auto p-2 custom-scrollbar">
            <Command.Empty className="px-4 py-8 text-center text-sm text-gray-500">
              No sessions found
            </Command.Empty>
            
            {/* Recent Sessions */}
            {!search && (
              <Command.Group heading="Recent Sessions" className="mb-2">
                {filteredSessions.map((session) => {
                  const isActive = activeSessionId === session.session_id;
                  const isThinking = (thinkingBySession.get(session.session_id)?.size ?? 0) > 0;
                  
                  return (
                    <Command.Item
                      key={session.session_id}
                      value={session.session_id}
                      onSelect={() => handleSelectSession(session.session_id)}
                      className="flex items-start gap-3 px-3 py-2.5 rounded-lg cursor-pointer transition-colors data-[selected=true]:bg-cyber-cyan/20 data-[selected=true]:border data-[selected=true]:border-cyber-cyan/40 hover:bg-cyber-surface/60 group"
                    >
                      {/* Status indicator */}
                      <div className="flex-shrink-0 pt-1">
                        {isActive ? (
                          <span className="w-2 h-2 rounded-full bg-cyber-cyan animate-pulse" />
                        ) : isThinking ? (
                          <span className="w-2 h-2 rounded-full bg-cyber-purple animate-pulse" />
                        ) : (
                          <span className="w-2 h-2 rounded-full bg-gray-600" />
                        )}
                      </div>
                      
                      {/* Content */}
                      <div className="flex-1 min-w-0">
                        {/* Title */}
                        <div className="flex items-center gap-2 mb-1">
                          {session.isChild && (
                            <GitBranch className="w-3 h-3 text-cyber-cyan/70 flex-shrink-0" />
                          )}
                          <span className="text-sm text-gray-200 font-medium truncate group-data-[selected=true]:text-cyber-cyan">
                            {session.title || session.name || 'Untitled session'}
                          </span>
                          {isActive && (
                            <span className="text-[10px] px-1.5 py-0.5 bg-cyber-cyan/20 text-cyber-cyan rounded border border-cyber-cyan/30 flex-shrink-0">
                              active
                            </span>
                          )}
                          {isThinking && (
                            <span className="text-[10px] px-1.5 py-0.5 bg-purple-500/20 text-purple-400 rounded border border-purple-500/30 flex-shrink-0">
                              thinking
                            </span>
                          )}
                          {session.isDelegation && (
                            <span className="text-[10px] px-1.5 py-0.5 bg-purple-500/20 text-purple-400 rounded border border-purple-500/30 flex-shrink-0">
                              delegated
                            </span>
                          )}
                        </div>
                        
                        {/* Metadata */}
                        <div className="flex items-center gap-3 text-xs text-gray-500">
                          <span className="font-mono truncate">
                            {session.session_id.slice(0, 12)}...
                          </span>
                          {session.updated_at && (
                            <span className="flex items-center gap-1 flex-shrink-0">
                              <Clock className="w-3 h-3" />
                              {formatTimestamp(session.updated_at)}
                            </span>
                          )}
                          <span className="truncate text-gray-600">
                            {session.workspace}
                          </span>
                        </div>
                      </div>
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
                  
                  return (
                    <Command.Item
                      key={session.session_id}
                      value={session.session_id}
                      onSelect={() => handleSelectSession(session.session_id)}
                      className="flex items-start gap-3 px-3 py-2.5 rounded-lg cursor-pointer transition-colors data-[selected=true]:bg-cyber-cyan/20 data-[selected=true]:border data-[selected=true]:border-cyber-cyan/40 hover:bg-cyber-surface/60 group"
                    >
                      {/* Status indicator */}
                      <div className="flex-shrink-0 pt-1">
                        {isActive ? (
                          <span className="w-2 h-2 rounded-full bg-cyber-cyan animate-pulse" />
                        ) : isThinking ? (
                          <span className="w-2 h-2 rounded-full bg-cyber-purple animate-pulse" />
                        ) : (
                          <span className="w-2 h-2 rounded-full bg-gray-600" />
                        )}
                      </div>
                      
                      {/* Content */}
                      <div className="flex-1 min-w-0">
                        {/* Title */}
                        <div className="flex items-center gap-2 mb-1">
                          {session.isChild && (
                            <GitBranch className="w-3 h-3 text-cyber-cyan/70 flex-shrink-0" />
                          )}
                          <span className="text-sm text-gray-200 font-medium truncate group-data-[selected=true]:text-cyber-cyan">
                            {session.title || session.name || 'Untitled session'}
                          </span>
                          {isActive && (
                            <span className="text-[10px] px-1.5 py-0.5 bg-cyber-cyan/20 text-cyber-cyan rounded border border-cyber-cyan/30 flex-shrink-0">
                              active
                            </span>
                          )}
                          {isThinking && (
                            <span className="text-[10px] px-1.5 py-0.5 bg-purple-500/20 text-purple-400 rounded border border-purple-500/30 flex-shrink-0">
                              thinking
                            </span>
                          )}
                          {session.isDelegation && (
                            <span className="text-[10px] px-1.5 py-0.5 bg-purple-500/20 text-purple-400 rounded border border-purple-500/30 flex-shrink-0">
                              delegated
                            </span>
                          )}
                        </div>
                        
                        {/* Metadata */}
                        <div className="flex items-center gap-3 text-xs text-gray-500">
                          <span className="font-mono truncate">
                            {session.session_id.slice(0, 12)}...
                          </span>
                          {session.updated_at && (
                            <span className="flex items-center gap-1 flex-shrink-0">
                              <Clock className="w-3 h-3" />
                              {formatTimestamp(session.updated_at)}
                            </span>
                          )}
                          <span className="truncate text-gray-600">
                            {session.workspace}
                          </span>
                        </div>
                      </div>
                    </Command.Item>
                  );
                })}
              </Command.Group>
            )}
            
            {/* Actions */}
            <Command.Separator className="my-2 border-t border-cyber-border/40" />
            
            <Command.Group heading="Actions">
              <Command.Item
                onSelect={handleNewSession}
                disabled={!connected}
                className="flex items-center gap-3 px-3 py-2.5 rounded-lg cursor-pointer transition-colors data-[selected=true]:bg-cyber-cyan/20 data-[selected=true]:border data-[selected=true]:border-cyber-cyan/40 hover:bg-cyber-surface/60 data-[disabled=true]:opacity-50 data-[disabled=true]:cursor-not-allowed"
              >
                <Plus className="w-4 h-4 text-cyber-cyan flex-shrink-0" />
                <span className="flex-1 text-sm text-gray-200">New Session</span>
                <kbd className="hidden sm:inline-block px-2 py-1 text-[10px] font-mono bg-cyber-bg border border-cyber-border rounded text-gray-500">
                  {navigator.platform.includes('Mac') ? '⌘' : 'Ctrl'}N
                </kbd>
              </Command.Item>
            </Command.Group>
          </Command.List>
        </Command>
      </div>
    </>
  );
}
