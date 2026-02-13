import { useState, useMemo } from 'react';
import * as Collapsible from '@radix-ui/react-collapsible';
import { SessionGroup, SessionSummary } from '../types';
import { GlitchText } from './GlitchText';
import { ChevronDown, ChevronRight, Search, Plus, Clock, GitBranch } from 'lucide-react';
import { useThinkingSessionIds } from '../hooks/useThinkingSessionIds';

interface SessionPickerProps {
  groups: SessionGroup[];
  onSelectSession: (sessionId: string) => void;
  onNewSession: () => void;
  disabled?: boolean;
  activeSessionId?: string | null;
  thinkingBySession?: Map<string, Set<string>>;
  sessionParentMap?: Map<string, string>;
}

export function SessionPicker({ groups, onSelectSession, onNewSession, disabled, activeSessionId, thinkingBySession, sessionParentMap }: SessionPickerProps) {
  const [filterText, setFilterText] = useState('');
  const [expandedGroups, setExpandedGroups] = useState<Set<string>>(new Set(groups.map((_, i) => `group-${i}`)));
  
  // Build session hierarchy and filter by search text
  const filteredGroups = useMemo(() => {
    // First, build the hierarchy
    const groupsWithHierarchy = groups.map(group => {
      
      // Find top-level sessions (those without parent_session_id)
      const topLevelSessions = group.sessions.filter(s => !s.parent_session_id);
      
      // Build hierarchy: attach children to parents
      const buildHierarchy = (session: SessionSummary): SessionSummary & { children?: SessionSummary[] } => {
        const children = group.sessions.filter(s => s.parent_session_id === session.session_id);
        if (children.length > 0) {
          return { ...session, children: children.map(buildHierarchy) };
        }
        return session;
      };
      
      return {
        ...group,
        sessions: topLevelSessions.map(buildHierarchy),
      };
    });
    
    // Then apply filtering
    if (!filterText.trim()) return groupsWithHierarchy;
    
    const query = filterText.toLowerCase();
    const matchesQuery = (s: SessionSummary) =>
      s.session_id.toLowerCase().includes(query) ||
      s.title?.toLowerCase().includes(query) ||
      s.name?.toLowerCase().includes(query);
    
    // Filter with hierarchy awareness
    const filterWithHierarchy = (session: SessionSummary & { children?: SessionSummary[] }): (SessionSummary & { children?: SessionSummary[] }) | null => {
      const sessionMatches = matchesQuery(session);
      const childrenFiltered = session.children?.map(filterWithHierarchy).filter(c => c !== null) as (SessionSummary & { children?: SessionSummary[] })[] | undefined;
      
      // Include if session matches OR any children match
      if (sessionMatches || (childrenFiltered && childrenFiltered.length > 0)) {
        return {
          ...session,
          children: childrenFiltered && childrenFiltered.length > 0 ? childrenFiltered : undefined,
        };
      }
      return null;
    };
    
    return groupsWithHierarchy
      .map(group => ({
        ...group,
        sessions: group.sessions.map(filterWithHierarchy).filter(s => s !== null) as SessionSummary[],
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
    const indentClass = depth > 0 ? `ml-${depth * 6}` : '';
    const isActive = activeSessionId === session.session_id;
    const isThinking = thinkingSessionIds.has(session.session_id);
    
    return (
      <div key={session.session_id}>
        <button
          onClick={() => onSelectSession(session.session_id)}
          disabled={disabled}
          className={`w-full text-left px-4 py-3 bg-surface-elevated/40 hover:bg-surface-elevated border ${
            isChild ? 'border-l-2 border-l-accent-primary/60' : ''
          } border-surface-border/35 hover:border-accent-primary/30 rounded-lg transition-all duration-200 group session-card disabled:opacity-50 disabled:cursor-not-allowed overflow-visible ${indentClass} ${
            isActive ? 'ring-2 ring-accent-primary/35 bg-surface-elevated/60' : ''
          }`}
          style={{
            animation: `session-card-entrance 0.3s ease-out ${sessionIndex * 0.05}s both`,
            marginLeft: depth > 0 ? `${depth * 1.5}rem` : '0',
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
        <div className="space-y-4 mb-6 max-h-[50vh] overflow-y-auto custom-scrollbar">
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
        <div className="text-center">
          <button
            onClick={onNewSession}
            disabled={disabled}
            className="px-8 py-4 rounded-lg font-medium text-base bg-accent-primary/10 border-2 border-accent-primary text-accent-primary hover:bg-accent-primary/20 hover:shadow-glow-primary disabled:opacity-30 disabled:cursor-not-allowed transition-all duration-200 flex items-center justify-center gap-3 mx-auto overflow-visible"
          >
            <Plus className="w-6 h-6" />
            <GlitchText text="Start New Session" variant="0" hoverOnly />
          </button>
          <p className="text-xs text-ui-muted mt-2">
            or press{' '}
            <kbd className="px-2 py-1 bg-surface-canvas border border-surface-border rounded text-accent-primary font-mono text-[10px]">
              {navigator.platform.includes('Mac') ? '⌘' : 'Ctrl'}+N
            </kbd>{' '}
            to create a session, or{' '}
            <kbd className="px-2 py-1 bg-surface-canvas border border-surface-border rounded text-accent-primary font-mono text-[10px]">
              {navigator.platform.includes('Mac') ? '⌘' : 'Ctrl'}+/
            </kbd>{' '}
            to open quick switcher
          </p>
        </div>
      </div>
    </div>
  );
}
