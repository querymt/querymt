import { useEffect, useRef, useCallback, useMemo, useState } from 'react';
import { Outlet, Link, useLocation, useNavigate } from 'react-router-dom';
import { Home, Copy, Check, Palette } from 'lucide-react';
import { useUiClientContext } from '../context/UiClientContext';
import { useUiStore } from '../store/uiStore';
import { useSessionTimer } from '../hooks/useSessionTimer';
import { GlitchText } from './GlitchText';
import { ModelPickerPopover } from './ModelPickerPopover';
import { HeaderStatsBar } from './HeaderStatsBar';
import { SessionSwitcher } from './SessionSwitcher';
import { StatsDrawer } from './StatsDrawer';
import { ThemeSwitcher } from './ThemeSwitcher';
import { ShortcutGateway } from './ShortcutGateway';
import { ProviderAuthSwitcher } from './ProviderAuthSwitcher';
import { WorkspacePathDialog } from './WorkspacePathDialog';
import { RemoteNodeIndicator } from './RemoteNodeIndicator';
import { copyToClipboard } from '../utils/clipboard';
import { getModeColors, getModeDisplayName } from '../utils/modeColors';
import {
  applyDashboardTheme,
  getDashboardThemes,
} from '../utils/dashboardThemes';

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
    authProviders,
    oauthFlow,
    oauthResult,
    refreshAllModels,
    requestAuthProviders,
    startOAuthLogin,
    completeOAuthLogin,
    disconnectOAuth,
    clearOAuthState,
    setSessionModel,
    sessionGroups,
    thinkingBySession,
    agentMode,
    cycleAgentMode,
    workspacePathDialogOpen,
    workspacePathDialogDefaultValue,
    submitWorkspacePathDialog,
    cancelWorkspacePathDialog,
    remoteNodes,
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
    selectedTheme,
    setSelectedTheme,
  } = useUiStore();
  
  const location = useLocation();
  const isHomePage = location.pathname === '/';
  const copyTimeoutRef = useRef<number | null>(null);
  const [shortcutGatewayOpen, setShortcutGatewayOpen] = useState(false);
  const [themeSwitcherOpen, setThemeSwitcherOpen] = useState(false);
  const [providerAuthOpen, setProviderAuthOpen] = useState(false);
  const prevAgentModeRef = useRef(agentMode);
  const availableThemes = useMemo(() => getDashboardThemes(), []);
  const shortcutGatewayPrefix = useMemo(
    () => (navigator.platform.includes('Mac') ? '⌘+X' : 'Ctrl+X'),
    [],
  );
  const selectedThemeLabel = useMemo(
    () => availableThemes.find((theme) => theme.id === selectedTheme)?.label ?? selectedTheme,
    [availableThemes, selectedTheme],
  );

  // Live timer hook (per-session)
  const { globalElapsedMs, agentElapsedMs, isSessionActive } = useSessionTimer(
    events,
    thinkingAgentIds,
    sessionId
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
      const normalizedKey = e.key.toLowerCase();

      // Intentionally global: the gateway works even when an input is focused.
      // If this conflicts with editing workflows later, add an editable-target guard.
      // Ctrl/Cmd+X gateway for command chords (e.g., Ctrl/Cmd+X T)
      if ((e.ctrlKey || e.metaKey) && !e.altKey && !e.shiftKey && normalizedKey === 'x') {
        e.preventDefault();
        setShortcutGatewayOpen((open) => !open);
        return;
      }

      if (shortcutGatewayOpen && !e.altKey && !e.shiftKey && normalizedKey === 't') {
        e.preventDefault();
        setShortcutGatewayOpen(false);
        setThemeSwitcherOpen(true);
        return;
      }

      if (shortcutGatewayOpen && !e.altKey && !e.shiftKey && normalizedKey === 'n') {
        e.preventDefault();
        setShortcutGatewayOpen(false);
        if (connected && !loading) {
          handleNewSession();
        }
        return;
      }

      if (shortcutGatewayOpen && !e.altKey && !e.shiftKey && normalizedKey === 'a') {
        e.preventDefault();
        setShortcutGatewayOpen(false);
        setThemeSwitcherOpen(false);
        setProviderAuthOpen(true);
        requestAuthProviders();
        return;
      }

      if (shortcutGatewayOpen) {
        return;
      }

      // Ctrl+E / Cmd+E - Cycle agent mode
      if ((e.metaKey || e.ctrlKey) && normalizedKey === 'e') {
        e.preventDefault();
        cycleAgentMode();
        return;
      }
      
      // Cmd+Shift+M / Ctrl+Shift+M - Toggle model picker
      if ((e.metaKey || e.ctrlKey) && e.shiftKey && normalizedKey === 'm') {
        e.preventDefault();
        setModelPickerOpen(!modelPickerOpen);
        return;
      }
      
      // Cmd+/ or Ctrl+/ - Toggle session switcher (open/close)
      if ((e.metaKey || e.ctrlKey) && e.key === '/') {
        e.preventDefault();
        setSessionSwitcherOpen(!sessionSwitcherOpen);
      }
      
    };
    
    window.addEventListener('keydown', handleKeyDown);
    return () => window.removeEventListener('keydown', handleKeyDown);
  }, [
    connected,
    loading,
    shortcutGatewayOpen,
    sessionSwitcherOpen,
    setSessionSwitcherOpen,
    cycleAgentMode,
    modelPickerOpen,
    setModelPickerOpen,
    setShortcutGatewayOpen,
    setThemeSwitcherOpen,
    setProviderAuthOpen,
    requestAuthProviders,
  ]);
  
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
        if (workspacePathDialogOpen) {
          e.preventDefault();
          e.stopImmediatePropagation();
          cancelWorkspacePathDialog();
          return;
        }
        if (shortcutGatewayOpen) {
          e.preventDefault();
          e.stopImmediatePropagation();
          setShortcutGatewayOpen(false);
          return;
        }
        if (themeSwitcherOpen) {
          e.preventDefault();
          e.stopImmediatePropagation();
          setThemeSwitcherOpen(false);
          return;
        }
        if (providerAuthOpen) {
          e.preventDefault();
          e.stopImmediatePropagation();
          setProviderAuthOpen(false);
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
  }, [
    thinkingAgentId,
    cancelSession,
    shortcutGatewayOpen,
    themeSwitcherOpen,
    providerAuthOpen,
    workspacePathDialogOpen,
    cancelWorkspacePathDialog,
    setSessionSwitcherOpen,
    setModelPickerOpen,
    setShortcutGatewayOpen,
    setProviderAuthOpen,
    setStatsDrawerOpen,
  ]);
  
  // Set CSS custom properties for mode theming
  useEffect(() => {
    const colors = getModeColors(agentMode, selectedTheme);
    const root = document.documentElement;
    
    root.style.setProperty('--mode-rgb', colors.rgb);
    root.style.setProperty('--mode-color', colors.cssColor);
    
    return () => {
      root.style.removeProperty('--mode-rgb');
      root.style.removeProperty('--mode-color');
    };
  }, [agentMode, selectedTheme]);

  // Set CSS custom properties for dashboard theme
  useEffect(() => {
    applyDashboardTheme(selectedTheme);
  }, [selectedTheme]);

  // Refresh provider/model data after successful OAuth login.
  useEffect(() => {
    if (!oauthResult?.success) {
      return;
    }

    refreshAllModels();
    requestAuthProviders();
  }, [oauthResult, refreshAllModels, requestAuthProviders]);

  // Clear transient OAuth UI state when auth modal closes.
  useEffect(() => {
    if (!providerAuthOpen) {
      clearOAuthState();
    }
  }, [providerAuthOpen, clearOAuthState]);
  
  // Auto-switch model when agent mode changes (if preference exists)
  useEffect(() => {
    // Only auto-switch when agentMode actually changes, not when agentModels updates
    // This prevents infinite loop when user manually switches model via ModelPickerPopover
    if (prevAgentModeRef.current === agentMode) {
      return;
    }
    prevAgentModeRef.current = agentMode;
    
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
    <div className="flex flex-col h-screen bg-surface-canvas text-ui-primary">
      {/* Header */}
      <header className="flex items-center justify-between gap-4 px-6 py-4 bg-surface-elevated border-b border-surface-border shadow-[0_0_20px_rgba(var(--accent-primary-rgb),0.05)]">
        {/* Left section */}
        <div className="flex items-center gap-3">
          <Link
            to="/"
            className={`p-2 rounded-lg transition-colors ${
              isHomePage
                ? 'text-accent-primary/50 cursor-default'
                : 'text-accent-primary hover:bg-surface-canvas'
            }`}
            title="Home"
          >
            <Home className="w-5 h-5" />
          </Link>
          <h1 className="text-xl font-semibold glow-text-primary">
            <GlitchText text="QueryMT" variant="3" hoverOnly />
          </h1>
          
          {/* Session chip (when active) - now includes mode */}
          {sessionId && (
            <div className="flex items-center gap-2">
              {/* Combined session chip with mode */}
              {/* Fixed width sized for a full UUIDv7 (36 chars) + status dot + mode */}
              <div className="flex items-center rounded-lg border border-surface-border bg-surface-canvas overflow-hidden flex-shrink-0">
                {/* Session ID part - click to open session switcher */}
                <button
                  type="button"
                  onClick={() => setSessionSwitcherOpen(true)}
                  title={`Click to switch sessions (${navigator.platform.includes('Mac') ? 'Cmd' : 'Ctrl'}+/)`}
                  className="flex items-center gap-1.5 px-3 py-1.5 hover:bg-surface-elevated/50 transition-colors group"
                >
                  <span
                    className={`w-2 h-2 rounded-full flex-shrink-0 ${
                      isSessionActive
                        ? 'bg-accent-primary animate-pulse'
                        : isConversationComplete
                        ? 'bg-ui-muted'
                        : 'bg-status-success'
                    }`}
                    title={
                      isSessionActive
                        ? 'Active (thinking)'
                        : isConversationComplete
                        ? 'Complete'
                        : 'Idle'
                    }
                  />
                  {/* Fixed-width slot sized for a full UUIDv7 (36 chars) */}
                  <span className="text-xs font-mono text-ui-secondary group-hover:text-accent-primary transition-colors w-[22ch] truncate">
                    {String(sessionId).substring(0, 20)}...
                  </span>
                  <span className="text-ui-muted">·</span>
                </button>
                
                {/* Mode part - click to cycle mode, fixed width for largest mode name */}
                <button
                  type="button"
                  onClick={cycleAgentMode}
                  title={`Mode: ${agentMode} (${navigator.platform.includes('Mac') ? '⌘E' : 'Ctrl+E'} to cycle)`}
                  className="px-2.5 py-1.5 text-xs font-medium transition-colors hover:bg-surface-elevated/50 w-[7ch] text-center flex-shrink-0 truncate"
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
                className="p-1.5 rounded-lg border border-surface-border bg-surface-canvas hover:border-accent-primary/60 hover:bg-surface-elevated/50 transition-colors"
              >
                {sessionCopied ? (
                  <Check className="w-3.5 h-3.5 text-status-success" />
                ) : (
                  <Copy className="w-3.5 h-3.5 text-ui-muted hover:text-accent-primary transition-colors" />
                )}
              </button>
            </div>
          )}
        </div>
        
        {/* Right section: flexible stats on the left, fixed controls pinned to the right */}
        <div className="flex items-center gap-3 min-w-0">
          {/* Inline stats bar (when session has events) — can grow/shrink */}
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

          {/* Fixed controls group — never reflows regardless of stats bar visibility */}
          <div className="flex items-center gap-3 flex-shrink-0 ml-auto">
            {/* Model picker — fixed width set on the trigger button itself */}
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
              currentNode={agentModels[activeAgentId]?.node}
              currentWorkspace={currentWorkspace}
              recentModelsByWorkspace={recentModelsByWorkspace}
              agentMode={agentMode}
              onRefresh={refreshAllModels}
              onSetSessionModel={setSessionModel}
            />

            {/* Dashboard theme picker */}
            <button
              type="button"
              onClick={() => setThemeSwitcherOpen(true)}
              className="h-8 w-8 inline-flex items-center justify-center rounded-lg border border-surface-border bg-surface-canvas/60 transition-colors hover:border-accent-primary/40"
              title={`Dashboard theme: ${selectedThemeLabel} (${shortcutGatewayPrefix} then T)`}
              aria-label="Open theme switcher"
            >
              <Palette className="w-3.5 h-3.5 text-accent-primary" />
            </button>

            {/* Remote node mesh indicator — fixed rightmost slot before conn dot */}
            <RemoteNodeIndicator remoteNodes={remoteNodes} />

            {/* Connection status dot */}
            <div
              className={`w-3 h-3 rounded-full flex-shrink-0 transition-colors ${
                connected ? 'bg-status-success' : 'bg-status-warning'
              }`}
              title={connected ? 'Connected' : 'Disconnected'}
            />
          </div>
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

      <ShortcutGateway
        open={shortcutGatewayOpen}
        onOpenChange={setShortcutGatewayOpen}
        onStartNewSession={() => {
          setShortcutGatewayOpen(false);
          if (connected && !loading) {
            handleNewSession();
          }
        }}
        onSelectTheme={() => {
          setShortcutGatewayOpen(false);
          setThemeSwitcherOpen(true);
        }}
        onAuthenticateProvider={() => {
          setShortcutGatewayOpen(false);
          setThemeSwitcherOpen(false);
          setProviderAuthOpen(true);
          requestAuthProviders();
        }}
      />

      <ThemeSwitcher
        open={themeSwitcherOpen}
        onOpenChange={setThemeSwitcherOpen}
        themes={availableThemes}
        selectedTheme={selectedTheme}
        onSelectTheme={setSelectedTheme}
      />

      <ProviderAuthSwitcher
        open={providerAuthOpen}
        onOpenChange={(open) => {
          setProviderAuthOpen(open);
          if (!open) {
            clearOAuthState();
          }
        }}
        providers={authProviders}
        oauthFlow={oauthFlow}
        oauthResult={oauthResult}
        onRequestProviders={requestAuthProviders}
        onStartOAuthLogin={startOAuthLogin}
        onCompleteOAuthLogin={completeOAuthLogin}
        onClearOAuthState={clearOAuthState}
        onDisconnectOAuth={disconnectOAuth}
      />

      <WorkspacePathDialog
        open={workspacePathDialogOpen}
        defaultValue={workspacePathDialogDefaultValue}
        remoteNodes={remoteNodes}
        onSubmit={submitWorkspacePathDialog}
        onCancel={cancelWorkspacePathDialog}
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
