import { useState, useCallback } from 'react';
import { Loader } from 'lucide-react';
import { useSessionManager } from '../hooks/useSessionManager';
import { useUiClientActions, useUiClientSession } from '../context/UiClientContext';
import { SessionPicker } from './SessionPicker';
import { WelcomeScreen } from './WelcomeScreen';

/**
 * HomePage component - displays when navigating to "/"
 * Shows either:
 * - Loading spinner (while waiting for initial connection)
 * - Welcome screen with "New Session" button (if no sessions exist)
 * - SessionPicker (if sessions exist)
 */
export function HomePage() {
  const { selectSession, createSession, goHome } = useSessionManager();
  
  const { deleteSession, loadMoreSessions, loadMoreGroupSessions, searchSessions, loadSessionChildren } = useUiClientActions();
  const {
    connected,
    sessionGroups,
    sessionId,
    thinkingBySession,
    sessionParentMap,
    sessionNextCursor,
    sessionTotalCount,
    sessionPageLoading,
    sessionChildrenLoading,
    sessionsEverLoaded,
  } = useUiClientSession();
  
  const [loading, setLoading] = useState(false);

  const handleLoadMoreSessions = useCallback(() => {
    loadMoreSessions(20);
  }, [loadMoreSessions]);

  const handleLoadMoreGroupSessions = useCallback((cwd: string | null) => {
    loadMoreGroupSessions(cwd, 20);
  }, [loadMoreGroupSessions]);

  const handleSearchSessions = useCallback((q: string) => {
    searchSessions(q, 30);
  }, [searchSessions]);

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

  const handleSelectSession = (sessionId: string) => {
    selectSession(sessionId);
  };

  const handleDeleteSession = (targetSessionId: string, sessionLabel?: string) => {
    deleteSession(targetSessionId, sessionLabel);
    if (targetSessionId === sessionId) {
      goHome();
    }
  };

  // Show a minimal spinner while waiting for the initial connection
  // to avoid flashing the WelcomeScreen before sessions arrive.
  if (!connected) {
    return (
      <div className="flex items-center justify-center h-full">
        <Loader className="w-5 h-5 animate-spin text-ui-muted" />
      </div>
    );
  }

  if (sessionsEverLoaded || sessionGroups.length > 0) {
    return (
      <div className="flex items-center justify-center h-full">
        <SessionPicker
          groups={sessionGroups}
          onSelectSession={handleSelectSession}
          onDeleteSession={handleDeleteSession}
          onNewSession={handleNewSession}
          onLoadMoreSessions={handleLoadMoreSessions}
          onLoadMoreGroupSessions={handleLoadMoreGroupSessions}
          onSearchSessions={handleSearchSessions}
          onLoadSessionChildren={loadSessionChildren}
          sessionChildrenLoading={sessionChildrenLoading}
          disabled={loading}
          activeSessionId={sessionId}
          thinkingBySession={thinkingBySession}
          sessionParentMap={sessionParentMap}
          hasMoreSessions={!!sessionNextCursor || sessionGroups.reduce((n, g) => n + g.sessions.length, 0) < sessionTotalCount}
          sessionPageLoading={sessionPageLoading}
        />
      </div>
    );
  }

  return (
    <div className="flex items-center justify-center h-full">
      <WelcomeScreen
        onNewSession={handleNewSession}
        disabled={loading}
        loading={loading}
      />
    </div>
  );
}
