import { useState } from 'react';
import { Plus, Loader } from 'lucide-react';
import { useSessionManager } from '../hooks/useSessionManager';
import { useUiClientContext } from '../context/UiClientContext';
import { SessionPicker } from './SessionPicker';
import { GlitchText } from './GlitchText';

/**
 * HomePage component - displays when navigating to "/"
 * Shows either:
 * - Welcome screen with "New Session" button (if no sessions exist)
 * - SessionPicker (if sessions exist)
 */
export function HomePage() {
  const { selectSession, createSession } = useSessionManager();
  
  const { 
    connected, 
    sessionGroups, 
    sessionId,
    thinkingBySession,
    sessionParentMap
  } = useUiClientContext();
  
  const [loading, setLoading] = useState(false);

  const handleNewSession = async () => {
    setLoading(true);
    try {
      await createSession();
    } catch (err) {
      console.log('Session creation cancelled or failed:', err);
    } finally {
      setLoading(false);
    }
  };

  // Handle session selection - navigate to the session route
  // The useSessionRoute hook in ChatView will then load the session from URL
  const handleSelectSession = (sessionId: string) => {
    selectSession(sessionId);
  };

  // If sessions exist, show the session picker
  if (sessionGroups.length > 0) {
    return (
      <div className="flex items-center justify-center h-full">
        <SessionPicker
          groups={sessionGroups}
          onSelectSession={handleSelectSession}
          onNewSession={handleNewSession}
          disabled={!connected || loading}
          activeSessionId={sessionId}
          thinkingBySession={thinkingBySession}
          sessionParentMap={sessionParentMap}
        />
      </div>
    );
  }

  // No sessions exist - show welcome screen
  return (
    <div className="flex items-center justify-center h-full">
      <div className="text-center space-y-6 animate-fade-in">
        <div>
          <p className="text-lg text-ui-secondary mb-6">Welcome to QueryMT</p>
          <button
            onClick={handleNewSession}
            disabled={!connected || loading}
            className="
              px-8 py-4 rounded-lg font-medium text-base
              bg-cyber-cyan/10 border-2 border-cyber-cyan
              text-cyber-cyan
              hover:bg-cyber-cyan/20 hover:shadow-neon-cyan
              disabled:opacity-30 disabled:cursor-not-allowed
              transition-all duration-200
              flex items-center justify-center gap-3 mx-auto
            "
          >
            {loading ? (
              <>
                <Loader className="w-6 h-6 animate-spin" />
                <span>Creating Session...</span>
              </>
            ) : (
              <>
                <Plus className="w-6 h-6" />
                <GlitchText text="Start New Session" variant="0" hoverOnly />
              </>
            )}
          </button>
          <p className="text-xs text-ui-muted mt-3">
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
