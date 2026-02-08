import { useEffect, useRef, useCallback, useMemo } from 'react';
import { Outlet, Link, useLocation, useNavigate } from 'react-router-dom';
import { Home, Copy, Check } from 'lucide-react';
import { useUiClientContext } from '../context/UiClientContext';
import { useUiStore } from '../store/uiStore';
import { useSessionTimer } from '../hooks/useSessionTimer';
import { GlitchText } from './GlitchText';
import { ModelPickerPopover } from './ModelPickerPopover';
import { HeaderStatsBar } from './HeaderStatsBar';
import { SessionSwitcher } from './SessionSwitcher';
import { StatsDrawer } from './StatsDrawer';
import { copyToClipboard } from '../utils/clipboard';
import { getModeColors, getModeDisplayName } from '../utils/modeColors';

/**
 * AppShell - Main layout wrapper for all routes
 * 
 * Phase 2: Header redesign complete
 * - Home button, session chip (clickable), inline stats bar
 * - Connection status as colored dot
 * - Model picker
 * 
 * Phase 3: Session Switcher complete
 * - Cmd+K modal with fuzzy search
 * 
 * Phase 4: Stats Drawer complete
 * - Top-sliding drawer with detailed stats
 * - Expert mode toggle for per-agent breakdown
 */
export function AppShell() {
  const {
    connected,
    newSession,
    cancelSession,
    thinkingAgentId,
    thinkingAgentIds,
    sessionId,
    events,
    agents,
    agentModels,
    sessionLimits,
    isConversationComplete,
    routingMode,
    activeAgentId,
    sessionsByAgent,
    allModels,
    recentModelsByWorkspace,
    refreshAllModels,
    setSessionModel,
    sessionGroups,
    thinkingBySession,
    agentMode,
    cycleAgentMode,
  } = useUiClientContext();
  
  const navigate = useNavigate();
  
  const {
    loading,
    sessionSwitcherOpen,
    setSessionSwitcherOpen,
    sessionCopied,
    setSessionCopied,
    modelPickerOpen,
    setModelPickerOpen,
    statsDrawerOpen,
    setStatsDrawerOpen,
  } = useUiStore();
  
  const location = useLocation();
  const isHomePage = location.pathname === '/';
  const copyTimeoutRef = useRef<number | null>(null);
  
  // Live timer hook
  const { globalElapsedMs, agentElapsedMs, isSessionActive } = useSessionTimer(
    events,
    thinkingAgentIds,
    isConversationComplete
  );
  
  // Load persisted UI state on mount
  useEffect(() => {
    useUiStore.getState().loadPersistedState();
  }, []);
  
  // Extract current workspace from active session
  const currentWorkspace = useMemo(() => {
    if (!sessionId) return null;
    // Find workspace from sessionGroups
    for (const group of sessionGroups) {
      if (group.sessions.some(s => s.session_id === sessionId)) {
        return group.cwd || null;
      }
    }
    return null;
  }, [sessionId, sessionGroups]);
  
  // Global keyboard shortcuts
  useEffect(() => {
    const handleKeyDown = (e: KeyboardEvent) => {
      // Ctrl+E / Cmd+E - Cycle agent mode
      if ((e.metaKey || e.ctrlKey) && e.key === 'e') {
        e.preventDefault();
        cycleAgentMode();
        return;
      }
      
      // Cmd+Shift+M / Ctrl+Shift+M - Toggle model picker
      if ((e.metaKey || e.ctrlKey) && e.shiftKey && e.key.toLowerCase() === 'm') {
        e.preventDefault();
        setModelPickerOpen(!modelPickerOpen);
        return;
      }
      
      // Cmd+/ or Ctrl+/ - Toggle session switcher (open/close)
      if ((e.metaKey || e.ctrlKey) && e.key === '/') {
        e.preventDefault();
        setSessionSwitcherOpen(!sessionSwitcherOpen);
      }
      
      // Cmd+N / Ctrl+N - New session
      if ((e.metaKey || e.ctrlKey) && e.key === 'n') {
        e.preventDefault();
        if (connected && !loading) {
          handleNewSession();
        }
      }
    };
    
    window.addEventListener('keydown', handleKeyDown);
    return () => window.removeEventListener('keydown', handleKeyDown);
  }, [connected, loading, newSession, sessionSwitcherOpen, setSessionSwitcherOpen, cycleAgentMode, modelPickerOpen, setModelPickerOpen]);
  
  // ESC handling: close modals first, then double-escape to cancel session
  // Reads state directly from Zustand store for guaranteed latest values
  useEffect(() => {
    let lastEscapeTime = 0;
    
    const handleKeyDown = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        const { sessionSwitcherOpen, modelPickerOpen, statsDrawerOpen } = useUiStore.getState();
        
        // Priority 1: Close any open modal/overlay
        if (sessionSwitcherOpen) {
          e.preventDefault();
          e.stopImmediatePropagation();
          setSessionSwitcherOpen(false);
          return;
        }
        if (modelPickerOpen) {
          e.preventDefault();
          e.stopImmediatePropagation();
          setModelPickerOpen(false);
          return;
        }
        if (statsDrawerOpen) {
          e.preventDefault();
          e.stopImmediatePropagation();
          setStatsDrawerOpen(false);
          return;
        }
        
        // Priority 2: Double-escape to cancel session (when agent is thinking)
        const now = Date.now();
        const timeSinceLastEsc = now - lastEscapeTime;
        
        if (timeSinceLastEsc < 500 && thinkingAgentId !== null) {
          e.preventDefault();
          e.stopImmediatePropagation();
          cancelSession();
          lastEscapeTime = 0;
        } else {
          lastEscapeTime = now;
        }
      }
    };
    
    window.addEventListener('keydown', handleKeyDown, { capture: true });
    return () => window.removeEventListener('keydown', handleKeyDown, { capture: true });
  }, [thinkingAgentId, cancelSession, setSessionSwitcherOpen, setModelPickerOpen, setStatsDrawerOpen]);
  
  // Set CSS custom properties for mode theming
  useEffect(() => {
    const colors = getModeColors(agentMode);
    const root = document.documentElement;
    
    root.style.setProperty('--mode-rgb', colors.rgb);
    root.style.setProperty('--mode-color', colors.hex);
    
    return () => {
      root.style.removeProperty('--mode-rgb');
      root.style.removeProperty('--mode-color');
    };
  }, [agentMode]);
  
  // Auto-switch model when agent mode changes (if preference exists)
  useEffect(() => {
    const { modeModelPreferences } = useUiStore.getState();
    const preference = modeModelPreferences[agentMode];
    
    // Only auto-switch if:
    // 1. We have a stored preference for this mode
    // 2. We have an active session
    // 3. The current model is different from the preference
    if (
      preference &&
      sessionId &&
      (agentModels[activeAgentId]?.provider !== preference.provider ||
       agentModels[activeAgentId]?.model !== preference.model)
    ) {
      const modelId = `${preference.provider}/${preference.model}`;
      console.log(`[AppShell] Auto-switching to ${modelId} for mode "${agentMode}"`);
      setSessionModel(sessionId, modelId);
    }
  }, [agentMode, sessionId, activeAgentId, agentModels, setSessionModel]);
  
  // Handle session ID copy
  const handleCopySessionId = () => {
    if (!sessionId) return;
    
    copyToClipboard(String(sessionId));
    setSessionCopied(true);
    
    if (copyTimeoutRef.current !== null) {
      window.clearTimeout(copyTimeoutRef.current);
    }
    
    copyTimeoutRef.current = window.setTimeout(() => {
      setSessionCopied(false);
    }, 2000);
  };
  
  // Handle new session creation with navigation
  const handleNewSession = async () => {
    try {
      const newSessionId = await newSession();
      navigate(`/session/${newSessionId}`, { replace: true });
    } catch (err) {
      console.log('Session creation cancelled or failed:', err);
    }
  };
  
  // Handle session selection
  const handleSelectSession = useCallback((sessionId: string) => {
    navigate(`/session/${sessionId}`);
  }, [navigate]);
  
  // Clean up timeout on unmount
  useEffect(() => {
    return () => {
      if (copyTimeoutRef.current !== null) {
        window.clearTimeout(copyTimeoutRef.current);
      }
    };
  }, []);
  
  return (
    <div className="flex flex-col h-screen bg-cyber-bg text-gray-100">
      {/* Header */}
      <header className="flex items-center justify-between gap-4 px-6 py-4 bg-cyber-surface border-b border-cyber-border shadow-[0_0_20px_rgba(0,255,249,0.05)]">
        {/* Left section */}
        <div className="flex items-center gap-3">
          <Link
            to="/"
            className={`p-2 rounded-lg transition-colors ${
              isHomePage
                ? 'text-cyber-cyan/50 cursor-default'
                : 'text-cyber-cyan hover:bg-cyber-bg'
            }`}
            title="Home"
          >
            <Home className="w-5 h-5" />
          </Link>
          <h1 className="text-xl font-semibold neon-text-cyan">
            <GlitchText text="QueryMT" variant="3" hoverOnly />
          </h1>
          
          {/* Session chip (when active) - now includes mode */}
          {sessionId && (
            <div className="flex items-center gap-2">
              {/* Combined session chip with mode */}
              <div className="flex items-center rounded-lg border border-cyber-border bg-cyber-bg overflow-hidden">
                {/* Session ID part - click to open session switcher */}
                <button
                  type="button"
                  onClick={() => setSessionSwitcherOpen(true)}
                  title={`Click to switch sessions (${navigator.platform.includes('Mac') ? 'Cmd' : 'Ctrl'}+/)`}
                  className="flex items-center gap-1.5 px-3 py-1.5 hover:bg-cyber-surface/50 transition-colors group"
                >
                  <span
                    className={`w-2 h-2 rounded-full ${
                      isSessionActive
                        ? 'bg-cyber-cyan animate-pulse'
                        : isConversationComplete
                        ? 'bg-gray-500'
                        : 'bg-cyber-lime'
                    }`}
                    title={
                      isSessionActive
                        ? 'Active (thinking)'
                        : isConversationComplete
                        ? 'Complete'
                        : 'Idle'
                    }
                  />
                  <span className="text-xs font-mono text-gray-400 group-hover:text-cyber-cyan transition-colors">
                    {String(sessionId).substring(0, 12)}...
                  </span>
                  <span className="text-gray-600">·</span>
                </button>
                
                {/* Mode part - click to cycle mode */}
                <button
                  type="button"
                  onClick={cycleAgentMode}
                  title={`Mode: ${agentMode} (${navigator.platform.includes('Mac') ? '⌘E' : 'Ctrl+E'} to cycle)`}
                  className="px-2.5 py-1.5 text-xs font-medium transition-colors hover:bg-cyber-surface/50"
                  style={{ color: 'var(--mode-color)' }}
                >
                  {getModeDisplayName(agentMode)}
                </button>
              </div>
              
              {/* Copy button */}
              <button
                type="button"
                onClick={handleCopySessionId}
                title="Copy session ID to clipboard"
                className="p-1.5 rounded-lg border border-cyber-border bg-cyber-bg hover:border-cyber-cyan/60 hover:bg-cyber-surface/50 transition-colors"
              >
                {sessionCopied ? (
                  <Check className="w-3.5 h-3.5 text-cyber-lime" />
                ) : (
                  <Copy className="w-3.5 h-3.5 text-gray-500 hover:text-cyber-cyan transition-colors" />
                )}
              </button>
            </div>
          )}
        </div>
        
        {/* Right section */}
        <div className="flex items-center gap-3">
          {/* Inline stats bar (when session has events) */}
          {sessionId && (
            <HeaderStatsBar
              events={events}
              globalElapsedMs={globalElapsedMs}
              isSessionActive={isSessionActive}
              agentModels={agentModels}
              sessionLimits={sessionLimits}
              onClick={() => setStatsDrawerOpen(!statsDrawerOpen)}
            />
          )}
          
          {/* Model picker */}
          <ModelPickerPopover
            open={modelPickerOpen}
            onOpenChange={setModelPickerOpen}
            connected={connected}
            routingMode={routingMode}
            activeAgentId={activeAgentId}
            sessionId={sessionId}
            sessionsByAgent={sessionsByAgent}
            agents={agents}
            allModels={allModels}
            currentProvider={agentModels[activeAgentId]?.provider}
            currentModel={agentModels[activeAgentId]?.model}
            currentWorkspace={currentWorkspace}
            recentModelsByWorkspace={recentModelsByWorkspace}
            agentMode={agentMode}
            onRefresh={refreshAllModels}
            onSetSessionModel={setSessionModel}
          />
          
          {/* Connection status dot */}
          <div
            className={`w-3 h-3 rounded-full transition-colors ${
              connected ? 'bg-cyber-lime' : 'bg-cyber-orange'
            }`}
            title={connected ? 'Connected' : 'Disconnected'}
          />
        </div>
      </header>
      
      {/* Mode accent line */}
      <div 
        className="h-0.5 w-full transition-colors duration-200"
        style={{ backgroundColor: `rgba(var(--mode-rgb), 0.6)` }}
      />
      
      {/* Route content */}
      <div className="flex-1 min-h-0 flex flex-col overflow-hidden">
        <Outlet />
      </div>
      
      {/* Global overlays */}
      {/* SystemLog and ThinkingIndicator are rendered by ChatView */}
      
      {/* Session Switcher (Cmd+K) */}
      <SessionSwitcher
        open={sessionSwitcherOpen}
        onOpenChange={setSessionSwitcherOpen}
        groups={sessionGroups}
        activeSessionId={sessionId}
        thinkingBySession={thinkingBySession}
        onNewSession={handleNewSession}
        onSelectSession={handleSelectSession}
        connected={connected}
      />
      
      {/* Stats Drawer - Phase 4 */}
      {sessionId && (
        <StatsDrawer
          open={statsDrawerOpen}
          onOpenChange={setStatsDrawerOpen}
          events={events}
          agents={agents}
          globalElapsedMs={globalElapsedMs}
          agentElapsedMs={agentElapsedMs}
          isSessionActive={isSessionActive}
          agentModels={agentModels}
          sessionLimits={sessionLimits}
        />
      )}
    </div>
  );
}
