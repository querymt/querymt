import { useState, useMemo } from 'react';
import { SessionGroup } from '../types';
import { GlitchText } from './GlitchText';
import { ChevronDown, ChevronRight, Search, Plus, Clock } from 'lucide-react';

interface SessionPickerProps {
  groups: SessionGroup[];
  onSelectSession: (sessionId: string) => void;
  onNewSession: () => void;
  disabled?: boolean;
}

export function SessionPicker({ groups, onSelectSession, onNewSession, disabled }: SessionPickerProps) {
  const [filterText, setFilterText] = useState('');
  const [expandedGroups, setExpandedGroups] = useState<Set<string>>(new Set(groups.map((_, i) => `group-${i}`)));
  
  // Filter groups and sessions by search text
  const filteredGroups = useMemo(() => {
    if (!filterText.trim()) return groups;
    
    const query = filterText.toLowerCase();
    return groups
      .map(group => ({
        ...group,
        sessions: group.sessions.filter(s =>
          s.session_id.toLowerCase().includes(query) ||
          s.title?.toLowerCase().includes(query) ||
          s.name?.toLowerCase().includes(query)
        ),
      }))
      .filter(group => group.sessions.length > 0);
  }, [groups, filterText]);
  
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
  
  return (
    <div className="session-picker-container flex items-center justify-center h-full w-full px-8 py-6 relative">
      {/* Main content */}
      <div className="relative z-10 w-full max-w-4xl animate-fade-in">
        {/* Header */}
        <div className="text-center mb-8">
          <h1 className="text-3xl font-bold text-cyber-cyan neon-text-cyan mb-2">
            Select a Session
          </h1>
          <p className="text-gray-400 text-sm">
            Resume your work or start fresh
          </p>
        </div>
        
        {/* Filter input */}
        <div className="mb-6">
          <div className="relative">
            <Search className="absolute left-4 top-1/2 transform -translate-y-1/2 w-5 h-5 text-cyber-cyan" />
            <input
              type="text"
              placeholder="Filter by session ID or title..."
              value={filterText}
              onChange={(e) => setFilterText(e.target.value)}
              className="w-full pl-12 pr-4 py-3 bg-cyber-surface border-2 border-cyber-border rounded-lg text-gray-100 placeholder-gray-500 focus:border-cyber-cyan focus:outline-none transition-colors session-filter-input"
            />
          </div>
        </div>
        
        {/* Sessions list */}
        <div className="space-y-4 mb-6 max-h-[50vh] overflow-y-auto custom-scrollbar">
          {filteredGroups.length === 0 ? (
            <div className="text-center py-8 text-gray-500">
              <p>No sessions found matching "{filterText}"</p>
            </div>
          ) : (
            filteredGroups.map((group, groupIndex) => {
              const groupId = `group-${groupIndex}`;
              const isExpanded = expandedGroups.has(groupId);
              const groupLabel = group.cwd || 'No workspace';
              
              return (
                <div key={groupId} className="session-group">
                  {/* Group header */}
                  <button
                    onClick={() => toggleGroup(groupIndex)}
                    className="w-full flex items-center gap-3 px-4 py-3 bg-cyber-surface/60 hover:bg-cyber-surface rounded-lg transition-colors session-group-header"
                  >
                    {isExpanded ? (
                      <ChevronDown className="w-5 h-5 text-cyber-cyan" />
                    ) : (
                      <ChevronRight className="w-5 h-5 text-cyber-cyan" />
                    )}
                    <span className="flex-1 text-left font-mono text-sm text-cyber-cyan">
                      {groupLabel}
                    </span>
                    <span className="text-xs text-gray-500">
                      {group.sessions.length} session{group.sessions.length !== 1 ? 's' : ''}
                    </span>
                  </button>
                  
                  {/* Sessions in group */}
                  {isExpanded && (
                    <div className="mt-2 space-y-2 pl-4">
                      {group.sessions.map((session, sessionIndex) => (
                        <button
                          key={session.session_id}
                          onClick={() => onSelectSession(session.session_id)}
                          disabled={disabled}
                          className="w-full text-left px-4 py-3 bg-cyber-surface/40 hover:bg-cyber-surface border border-cyber-border/50 hover:border-cyber-cyan/40 rounded-lg transition-all duration-200 group session-card disabled:opacity-50 disabled:cursor-not-allowed overflow-visible"
                          style={{
                            animation: `session-card-entrance 0.3s ease-out ${sessionIndex * 0.05}s both`,
                          }}
                        >
                          {/* Title */}
                          <div className="font-medium text-gray-200 mb-1 group-hover:text-cyber-cyan transition-colors">
                            <GlitchText 
                              text={session.title || session.name || 'Untitled session'} 
                              variant="0" 
                              hoverOnly
                            />
                          </div>
                          
                          {/* Metadata */}
                          <div className="flex items-center gap-4 text-xs text-gray-500">
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
                      ))}
                    </div>
                  )}
                </div>
              );
            })
          )}
        </div>
        
        {/* New session button */}
        <div className="text-center">
          <button
            onClick={onNewSession}
            disabled={disabled}
            className="px-8 py-4 rounded-lg font-medium text-base bg-cyber-cyan/10 border-2 border-cyber-cyan text-cyber-cyan hover:bg-cyber-cyan/20 hover:shadow-neon-cyan disabled:opacity-30 disabled:cursor-not-allowed transition-all duration-200 flex items-center justify-center gap-3 mx-auto overflow-visible"
          >
            <Plus className="w-6 h-6" />
            <GlitchText text="Start New Session" variant="0" hoverOnly />
          </button>
          <p className="text-xs text-gray-500 mt-2">
            or press{' '}
            <kbd className="px-2 py-1 bg-cyber-bg border border-cyber-border rounded text-cyber-cyan font-mono text-[10px]">
              {navigator.platform.includes('Mac') ? '⌘' : 'Ctrl'}+N
            </kbd>{' '}
            to create a session
          </p>
        </div>
      </div>
    </div>
  );
}
