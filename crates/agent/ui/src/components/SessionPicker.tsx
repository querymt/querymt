import { useState, useMemo } from 'react';
import * as Collapsible from '@radix-ui/react-collapsible';
import { SessionGroup, SessionSummary } from '../types';
import { ChevronDown, ChevronRight, Search, Plus, Clock, GitBranch, Globe, Trash2, Plug } from 'lucide-react';
import { useThinkingSessionIds } from '../hooks/useThinkingSessionIds';

interface SessionPickerProps {
  groups: SessionGroup[];
  onSelectSession: (sessionId: string) => void;
  onDeleteSession: (sessionId: string, sessionLabel?: string) => void;
  onNewSession: () => void;
  disabled?: boolean;
  activeSessionId?: string | null;
  thinkingBySession?: Map<string, Set<string>>;
  sessionParentMap?: Map<string, string>;
}

export function SessionPicker({ groups, onSelectSession, onDeleteSession, onNewSession, disabled, activeSessionId, thinkingBySession, sessionParentMap }: SessionPickerProps) {
  const [filterText, setFilterText] = useState('');
  const [expandedGroups, setExpandedGroups] = useState<Set<string>>(new Set(groups.map((_, i) => `group-${i}`)));
  
  // Build session hierarchy and filter by search text
  const filteredGroups = useMemo(() => {
    const query = filterText.trim().toLowerCase();

    const matchesQuery = (s: SessionSummary) =>
      s.session_id.toLowerCase().includes(query) ||
      s.title?.toLowerCase().includes(query) ||
      s.name?.toLowerCase().includes(query);

    // Build hierarchy using a Map for O(1) child lookup instead of O(n) filter per parent.
    const groupsWithHierarchy = groups.map(group => {
      // Index children by parent id in one O(n) pass.
      const childrenByParent = new Map<string, SessionSummary[]>();
      for (const s of group.sessions) {
        if (s.parent_session_id) {
          let bucket = childrenByParent.get(s.parent_session_id);
          if (!bucket) {
            bucket = [];
            childrenByParent.set(s.parent_session_id, bucket);
          }
          bucket.push(s);
        }
      }

      const buildHierarchy = (session: SessionSummary): SessionSummary & { children?: SessionSummary[] } => {
        const children = childrenByParent.get(session.session_id);
        if (children && children.length > 0) {
          return { ...session, children: children.map(buildHierarchy) };
        }
        return session;
      };

      const topLevel = group.sessions
        .filter(s => !s.parent_session_id)
        .map(buildHierarchy);

      return { ...group, sessions: topLevel };
    });

    if (!query) return groupsWithHierarchy;

    // Filter with hierarchy awareness: keep a node if it or any descendant matches.
    const filterWithHierarchy = (
      session: SessionSummary & { children?: SessionSummary[] }
    ): (SessionSummary & { children?: SessionSummary[] }) | null => {
      const childrenFiltered = session.children
        ?.map(filterWithHierarchy)
        .filter((c): c is SessionSummary & { children?: SessionSummary[] } => c !== null);

      if (matchesQuery(session) || (childrenFiltered && childrenFiltered.length > 0)) {
        return { ...session, children: childrenFiltered?.length ? childrenFiltered : undefined };
      }
      return null;
    };

    return groupsWithHierarchy
      .map(group => ({
        ...group,
        sessions: group.sessions
          .map(filterWithHierarchy)
          .filter((s): s is SessionSummary & { children?: SessionSummary[] } => s !== null),
      }))
      .filter(group => group.sessions.length > 0);
  }, [groups, filterText]);
  
  const thinkingSessionIds = useThinkingSessionIds(thinkingBySession, groups, sessionParentMap);
  
  const toggleGroup = (groupIndex: number) => {
    const groupId = `group-${groupIndex}`;
    setExpandedGroups(prev => {
      const next = new Set(prev);
      if (next.has(groupId)) {
        next.delete(groupId);
      } else {
        next.add(groupId);
      }
      return next;
    });
  };

  const handleDeleteSession = (session: SessionSummary) => {
    if (disabled) {
      return;
    }
    // TODO: consider a user setting to require confirmation before deleting sessions.
    onDeleteSession(session.session_id, session.title || session.name || session.session_id);
  };
  
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

  // Helper to render a single session card with its children
  const renderSessionCard = (session: SessionSummary & { children?: SessionSummary[] }, sessionIndex: number, depth: number = 0) => {
    const isChild = !!session.parent_session_id;
    const isDelegation = session.fork_origin === 'delegation';
    const isRecurring = session.session_kind === 'recurring';
    const isMemory = session.session_kind === 'memory';
    const isUnattached = session.attached === false;
    const indentClass = depth > 0 ? `ml-${depth * 6}` : '';
    const isActive = activeSessionId === session.session_id;
    const isThinking = thinkingSessionIds.has(session.session_id);
    
    return (
      <div
        key={session.session_id}
        className={indentClass}
        style={{ marginLeft: depth > 0 ? `${depth * 1.5}rem` : '0' }}
      >
        <div className="relative">
          <button
            onClick={() => onSelectSession(session.session_id)}
            disabled={disabled}
            className={`w-full text-left px-4 py-3 pr-14 bg-surface-elevated/40 hover:bg-surface-elevated border ${
              isChild ? 'border-l-2 border-l-accent-primary/60' : ''
            } border-surface-border/35 hover:border-accent-primary/30 rounded-lg transition-all duration-200 group session-card disabled:opacity-50 disabled:cursor-not-allowed overflow-visible ${
              isActive ? 'ring-2 ring-accent-primary/35 bg-surface-elevated/60' : ''
            }`}
            style={{
              animation: `session-card-entrance 0.3s ease-out ${sessionIndex * 0.05}s both`,
            }}
          >
            {/* Title with optional child indicator */}
            <div className="font-medium text-ui-primary mb-1 group-hover:text-accent-primary transition-colors flex items-center gap-2">
              {isChild && (
                <GitBranch className="w-3.5 h-3.5 text-accent-primary/70 flex-shrink-0" />
              )}
              <span className={isChild ? 'text-sm' : ''}>
                {session.title || session.name || 'Untitled session'}
              </span>
              {isActive && (
                <span className="flex items-center gap-1.5">
                  <span className="w-2 h-2 rounded-full bg-accent-primary animate-pulse" />
                  <span className="text-[10px] px-1.5 py-0.5 bg-accent-primary/20 text-accent-primary rounded border border-accent-primary/30">
                    active
                  </span>
                </span>
              )}
              {isThinking && (
                <span className="flex items-center gap-1">
                  <span className="w-2 h-2 rounded-full bg-accent-tertiary animate-pulse" />
                  <span className="text-[10px] px-1.5 py-0.5 bg-accent-tertiary/20 text-accent-tertiary rounded border border-accent-tertiary/30">
                    thinking
                  </span>
                </span>
              )}
              {isDelegation && (
                <span className="text-[10px] px-1.5 py-0.5 bg-accent-tertiary/20 text-accent-tertiary rounded border border-accent-tertiary/30">
                  delegated
                </span>
              )}
              {isRecurring && !isMemory && (
                <span className="text-[10px] px-1.5 py-0.5 bg-accent-secondary/20 text-accent-secondary rounded border border-accent-secondary/30">
                  recurring
                </span>
              )}
              {isMemory && (
                <span className="text-[10px] px-1.5 py-0.5 bg-purple-500/20 text-purple-400 rounded border border-purple-500/30">
                  memory
                </span>
              )}
              {session.node && (
                <span className="inline-flex items-center gap-1 text-[10px] px-1.5 py-0.5 bg-accent-secondary/20 text-accent-secondary rounded border border-accent-secondary/30">
                  <Globe className="w-2.5 h-2.5" />
                  {session.node}
                </span>
              )}
              {isUnattached && (
                <span className="inline-flex items-center gap-1 text-[10px] px-1.5 py-0.5 bg-ui-secondary/20 text-ui-secondary rounded border border-ui-secondary/30">
                  <Plug className="w-2.5 h-2.5" />
                  click to attach
                </span>
              )}
            </div>
            
            {/* Metadata */}
            <div className="flex items-center gap-4 text-xs text-ui-muted">
              <span className="font-mono">
                {session.session_id.slice(0, 12)}...
              </span>
              {session.updated_at && (
                <span className="flex items-center gap-1">
                  <Clock className="w-3 h-3" />
                  {formatTimestamp(session.updated_at)}
                </span>
              )}
            </div>
          </button>

          {!session.node && (
            <button
              type="button"
              onClick={(event) => {
                event.stopPropagation();
                handleDeleteSession(session);
              }}
              disabled={disabled}
              className="absolute right-3 top-1/2 -translate-y-1/2 p-1.5 rounded-md text-ui-muted hover:text-status-warning hover:bg-status-warning/10 disabled:opacity-40 disabled:cursor-not-allowed"
              title="Delete session"
              aria-label={`Delete session ${session.title || session.name || session.session_id}`}
            >
              <Trash2 className="w-3.5 h-3.5" />
            </button>
          )}
        </div>
        
        {/* Render children recursively */}
        {session.children && session.children.length > 0 && (
          <div className="mt-2 space-y-2">
            {session.children.map((child, childIndex) => 
              renderSessionCard(child, childIndex, depth + 1)
            )}
          </div>
        )}
      </div>
    );
  };
  
  return (
    <div className="session-picker-container flex items-center justify-center h-full w-full px-8 py-6 relative">
      {/* Main content */}
      <div className="relative z-10 w-full max-w-4xl animate-fade-in">
        {/* Header */}
        <div className="text-center mb-8">
          <h1 className="text-3xl font-bold text-accent-primary glow-text-primary mb-2">
            Select a Session
          </h1>
          <p className="text-ui-secondary text-sm">
            Resume your work or start fresh
          </p>
        </div>
        
        {/* Filter input */}
        <div className="mb-6">
          <div className="relative">
            <Search className="absolute left-4 top-1/2 transform -translate-y-1/2 w-5 h-5 text-accent-primary" />
            <input
              type="text"
              placeholder="Filter by session ID or title..."
              value={filterText}
              onChange={(e) => setFilterText(e.target.value)}
              className="w-full pl-12 pr-4 py-3 bg-surface-elevated border-2 border-surface-border rounded-lg text-ui-primary placeholder:text-ui-muted focus:border-accent-primary focus:outline-none transition-colors session-filter-input"
            />
          </div>
        </div>
        
        {/* Sessions list */}
        <div className="space-y-4 mb-6 max-h-[50vh] overflow-y-auto custom-scrollbar p-1 -m-1">
          {filteredGroups.length === 0 ? (
            <div className="text-center py-8 text-ui-muted">
              <p>No sessions found matching "{filterText}"</p>
            </div>
          ) : (
            filteredGroups.map((group, groupIndex) => {
              const groupId = `group-${groupIndex}`;
              const isExpanded = expandedGroups.has(groupId);
              const groupLabel = group.cwd || 'No workspace';
              
              return (
                <Collapsible.Root
                  key={groupId}
                  className="session-group"
                  open={isExpanded}
                  onOpenChange={() => toggleGroup(groupIndex)}
                >
                  {/* Group header */}
                  <Collapsible.Trigger className="w-full flex items-center gap-3 px-4 py-3 bg-surface-elevated/60 hover:bg-surface-elevated rounded-lg transition-colors session-group-header">
                    {isExpanded ? (
                      <ChevronDown className="w-5 h-5 text-accent-primary" />
                    ) : (
                      <ChevronRight className="w-5 h-5 text-accent-primary" />
                    )}
                    <span className="flex-1 text-left font-mono text-sm text-accent-primary">
                      {groupLabel}
                    </span>
                    <span className="text-xs text-ui-muted">
                      {group.sessions.length} session{group.sessions.length !== 1 ? 's' : ''}
                    </span>
                  </Collapsible.Trigger>
                  
                  {/* Sessions in group */}
                  <Collapsible.Content className="mt-2 space-y-2 pl-4">
                    {group.sessions.map((session, sessionIndex) => 
                      renderSessionCard(session, sessionIndex, 0)
                    )}
                  </Collapsible.Content>
                </Collapsible.Root>
              );
            })
          )}
        </div>
        
        {/* New session button */}
        <div className="text-center space-y-3">
          <button
            onClick={onNewSession}
            disabled={disabled}
            className="px-6 py-3 rounded-full font-medium text-sm bg-accent-primary text-surface-canvas hover:opacity-90 disabled:opacity-30 disabled:cursor-not-allowed transition-all duration-150 flex items-center justify-center gap-2 mx-auto"
          >
            <Plus className="w-4 h-4" />
            <span>New Session</span>
          </button>
          <p className="text-[11px] text-ui-muted">
            <kbd className="px-1.5 py-0.5 bg-surface-canvas border border-surface-border/60 rounded font-mono text-[10px]">
              {navigator.platform.includes('Mac') ? '\u2318+X N' : 'Ctrl+X N'}
            </kbd>
            {' '}<span className="text-ui-muted/60">or</span>{' '}
            <kbd className="px-1.5 py-0.5 bg-surface-canvas border border-surface-border/60 rounded font-mono text-[10px]">
              {navigator.platform.includes('Mac') ? '\u2318' : 'Ctrl'}+/
            </kbd>
          </p>
        </div>
      </div>
    </div>
  );
}
