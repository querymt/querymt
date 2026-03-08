import { useEffect, useRef, useCallback, useMemo, useState } from 'react';
import { Outlet, Link, useLocation, useNavigate } from 'react-router-dom';
import { Home, Copy, Check, Palette, Keyboard, Menu, X } from 'lucide-react';
import { useUiClientActions, useUiClientSession, useUiClientConfig } from '../context/UiClientContext';
import { useUiStore } from '../store/uiStore';

import { useIsMobile } from '../hooks/useIsMobile';
import { GlitchText } from './GlitchText';
import { ModelPickerPopover } from './ModelPickerPopover';
import { HeaderStatsBar } from './HeaderStatsBar';
import { SessionSwitcher } from './SessionSwitcher';
import { StatsDrawer } from './StatsDrawer';
import { ThemeSwitcher } from './ThemeSwitcher';
import { ShortcutGateway } from './ShortcutGateway';
import { PluginUpdateIndicator } from './PluginUpdateIndicator';
import { ProviderAuthSwitcher } from './ProviderAuthSwitcher';
import { WorkspacePathDialog } from './WorkspacePathDialog';
import { RemoteNodeIndicator } from './RemoteNodeIndicator';
import { copyToClipboard } from '../utils/clipboard';
import { SessionTimerProvider } from '../context/SessionTimerContext';
import { getModeColors, getModeDisplayName } from '../utils/modeColors';
import {
  applyDashboardTheme,
  getDashboardThemes,
} from '../utils/dashboardThemes';
import { toggleDebugLog } from '../utils/debugLog';

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
  // Split context subscriptions — AppShell does NOT subscribe to Events context
  // (hot streaming data) so it won't re-render on every streaming delta.
  const {
    newSession,
    cancelSession,
    deleteSession,
    refreshAllModels,
    requestAuthProviders,
    startOAuthLogin,
    completeOAuthLogin,
    disconnectOAuth,
    clearOAuthState,
    setApiToken,
    clearApiToken,
    setAuthMethodPref,
    clearApiTokenResult,
    setSessionModel,
    addCustomModelFromHf,
    addCustomModelFromFile,
    deleteCustomModel,
    cycleAgentMode,
    submitWorkspacePathDialog,
    cancelWorkspacePathDialog,
    dismissConnectionError,
    dismissSessionActionNotice,
    updatePlugins,
  } = useUiClientActions();

  const {
    connected,
    thinkingAgentId,
    thinkingAgentIds,
    sessionId,
    agents,
    agentModels,
    sessionLimits,
    isConversationComplete,
    routingMode,
    activeAgentId,
    sessionsByAgent,
    sessionGroups,
    thinkingBySession,
    agentMode,
    remoteNodes,
  } = useUiClientSession();

  const {
    allModels,
    providerCapabilities,
    recentModelsByWorkspace,
    authProviders,
    oauthFlow,
    oauthResult,
    modelDownloads,
    apiTokenResult,
    workspacePathDialogOpen,
    workspacePathDialogDefaultValue,
    connectionErrors,
    sessionActionNotices,
    isUpdatingPlugins,
    pluginUpdateStatus,
    pluginUpdateResults,
  } = useUiClientConfig();
  
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
    delegationDrawerOpen,
    selectedToolEvent,
    selectedTheme,
    setSelectedTheme,
  } = useUiStore();
  
  const location = useLocation();
  const isHomePage = location.pathname === '/';
  const isMobile = useIsMobile();
  const copyTimeoutRef = useRef<number | null>(null);
  const [shortcutGatewayOpen, setShortcutGatewayOpen] = useState(false);
  const [themeSwitcherOpen, setThemeSwitcherOpen] = useState(false);
  const [providerAuthOpen, setProviderAuthOpen] = useState(false);
  const [mobileMenuOpen, setMobileMenuOpen] = useState(false);
  const prevAgentModeRef = useRef(agentMode);
  const isSessionActive = thinkingAgentIds.size > 0;
  const availableThemes = useMemo(() => getDashboardThemes(), []);
  const shortcutGatewayPrefix = useMemo(
    () => (navigator.platform.includes('Mac') ? '⌘+X' : 'Ctrl+X'),
    [],
  );
  const selectedThemeLabel = useMemo(
    () => availableThemes.find((theme) => theme.id === selectedTheme)?.label ?? selectedTheme,
    [availableThemes, selectedTheme],
  );

  // Prevent page scroll when any modal/drawer overlay is open.
  useEffect(() => {
    const hasOpenOverlay =
      sessionSwitcherOpen ||
      (isMobile && modelPickerOpen) ||
      statsDrawerOpen ||
      delegationDrawerOpen ||
      selectedToolEvent !== null ||
      shortcutGatewayOpen ||
      themeSwitcherOpen ||
      providerAuthOpen ||
      workspacePathDialogOpen;

    document.body.classList.toggle('modal-open', hasOpenOverlay);

    return () => {
      document.body.classList.remove('modal-open');
    };
  }, [
    sessionSwitcherOpen,
    modelPickerOpen,
    statsDrawerOpen,
    delegationDrawerOpen,
    selectedToolEvent,
    shortcutGatewayOpen,
    themeSwitcherOpen,
    providerAuthOpen,
    workspacePathDialogOpen,
    isMobile,
  ]);

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

      if (shortcutGatewayOpen && !e.altKey && !e.shiftKey && normalizedKey === 'u') {
        e.preventDefault();
        setShortcutGatewayOpen(false);
        updatePlugins();
        return;
      }

      if (shortcutGatewayOpen && !e.altKey && !e.shiftKey && normalizedKey === 'd') {
        e.preventDefault();
        setShortcutGatewayOpen(false);
        toggleDebugLog();
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
    updatePlugins,
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

  const handleDeleteSession = useCallback((targetSessionId: string, sessionLabel?: string) => {
    deleteSession(targetSessionId, sessionLabel);
    if (targetSessionId === sessionId) {
      navigate('/');
    }
  }, [deleteSession, sessionId, navigate]);
  
  // Clean up timeout on unmount
  useEffect(() => {
    return () => {
      if (copyTimeoutRef.current !== null) {
        window.clearTimeout(copyTimeoutRef.current);
      }
    };
  }, []);

  // Stable derived model props for ModelPickerPopover — avoids inline property access
  // creating new primitive reads on every AppShell render.
  const activeAgentModel = useMemo(
    () => agentModels[activeAgentId],
    [agentModels, activeAgentId],
  );

  // Stable callback for mobile picker so the inline arrow is not recreated every render.
  const handleMobilePickerOpenChange = useCallback(
    (open: boolean) => {
      setModelPickerOpen(open);
      if (!open) setMobileMenuOpen(false);
    },
    [setModelPickerOpen],
  );
  
  return (
    <SessionTimerProvider>
    <div className="flex flex-col h-screen bg-surface-canvas text-ui-primary">
      {/* Header */}
      <header className="flex items-center justify-between gap-2 md:gap-4 px-3 md:px-6 py-2 md:py-4 bg-surface-elevated border-b border-surface-border shadow-[0_0_20px_rgba(var(--accent-primary-rgb),0.05)]">
        {/* Left section */}
        <div className="flex items-center gap-2 md:gap-3 min-w-0">
          <Link
            to="/"
            className={`p-1.5 md:p-2 rounded-lg transition-colors flex-shrink-0 ${
              isHomePage
                ? 'text-accent-primary/50 cursor-default'
                : 'text-accent-primary hover:bg-surface-canvas'
            }`}
            title="Home"
          >
            <Home className="w-5 h-5" />
          </Link>
          {/* Hide title on mobile to save space */}
          <h1 className="hidden md:block text-xl font-semibold glow-text-primary whitespace-nowrap">
            <GlitchText text="QueryMT" variant="3" hoverOnly />
          </h1>
          
          {/* Session chip (when active) - now includes mode */}
          {sessionId && (
            <div className="flex items-center gap-1.5 md:gap-2 min-w-0">
              {/* Combined session chip with mode */}
              <div className="flex items-center rounded-lg border border-surface-border bg-surface-canvas overflow-hidden flex-shrink min-w-0">
                {/* Session ID part - click to open session switcher */}
                <button
                  type="button"
                  onClick={() => setSessionSwitcherOpen(true)}
                  title={`Click to switch sessions (${navigator.platform.includes('Mac') ? 'Cmd' : 'Ctrl'}+/)`}
                  className="flex items-center gap-1.5 px-2 md:px-3 py-1.5 hover:bg-surface-elevated/50 transition-colors group min-w-0"
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
                  {/* Truncated session ID — narrower on mobile */}
                  <span className="text-xs font-mono text-ui-secondary group-hover:text-accent-primary transition-colors w-[8ch] md:w-[22ch] truncate">
                    {isMobile ? String(sessionId).substring(0, 8) : `${String(sessionId).substring(0, 20)}...`}
                  </span>
                  <span className="text-ui-muted hidden md:inline">·</span>
                </button>
                
                {/* Mode part - click to cycle mode */}
                <button
                  type="button"
                  onClick={cycleAgentMode}
                  title={`Mode: ${agentMode} (${navigator.platform.includes('Mac') ? '⌘E' : 'Ctrl+E'} to cycle)`}
                  className="px-1.5 md:px-2.5 py-1.5 text-xs font-medium transition-colors hover:bg-surface-elevated/50 text-center flex-shrink-0 whitespace-nowrap"
                  style={{ color: 'var(--mode-color)' }}
                >
                  {getModeDisplayName(agentMode)}
                </button>
              </div>
              
              {/* Copy button — hidden on mobile */}
              <button
                type="button"
                onClick={handleCopySessionId}
                title="Copy session ID to clipboard"
                className="inline-flex p-1.5 rounded-lg border border-surface-border bg-surface-canvas hover:border-accent-primary/60 hover:bg-surface-elevated/50 transition-colors"
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
        <div className="flex items-center gap-2 md:gap-3 min-w-0 flex-shrink-0">
          {/* Inline stats bar — compact on mobile, full on desktop */}
          {sessionId && (
            <HeaderStatsBar
              agentModels={agentModels}
              sessionLimits={sessionLimits}
              compact={isMobile}
              onClick={() => setStatsDrawerOpen(!statsDrawerOpen)}
            />
          )}

          {/* Desktop controls — rendered only on desktop to avoid duplicate popover portals on mobile */}
          {!isMobile && (
            <div className="hidden md:flex items-center gap-3 min-w-0 ml-auto">
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
                currentProvider={activeAgentModel?.provider}
                currentModel={activeAgentModel?.model}
                currentNode={activeAgentModel?.node}
                currentWorkspace={currentWorkspace}
                recentModelsByWorkspace={recentModelsByWorkspace}
                agentMode={agentMode}
                onRefresh={refreshAllModels}
                onSetSessionModel={setSessionModel}
                providerCapabilities={providerCapabilities}
                modelDownloads={modelDownloads}
                onAddCustomModelFromHf={addCustomModelFromHf}
                onAddCustomModelFromFile={addCustomModelFromFile}
                onDeleteCustomModel={deleteCustomModel}
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

              {/* Remote node mesh indicator */}
              <RemoteNodeIndicator remoteNodes={remoteNodes} />

              {/* Connection status dot */}
              <div
                className={`w-3 h-3 rounded-full flex-shrink-0 transition-colors ${
                  connected ? 'bg-status-success' : 'bg-status-warning'
                }`}
                title={connected ? 'Connected' : 'Disconnected'}
              />
            </div>
          )}

          {/* Mobile: connection dot + hamburger menu */}
          <div className="flex md:hidden items-center gap-2 flex-shrink-0">
            <div
              className={`w-2.5 h-2.5 rounded-full flex-shrink-0 transition-colors ${
                connected ? 'bg-status-success' : 'bg-status-warning'
              }`}
              title={connected ? 'Connected' : 'Disconnected'}
            />
            <button
              type="button"
              onClick={() => setMobileMenuOpen(!mobileMenuOpen)}
              className="p-1.5 rounded-lg border border-surface-border bg-surface-canvas/60 transition-colors hover:border-accent-primary/40"
              aria-label="Toggle mobile menu"
            >
              {mobileMenuOpen ? (
                <X className="w-4 h-4 text-accent-primary" />
              ) : (
                <Menu className="w-4 h-4 text-accent-primary" />
              )}
            </button>
          </div>
        </div>
      </header>

      {/* Mobile dropdown menu */}
      {isMobile && mobileMenuOpen && (
        <div className="md:hidden bg-surface-elevated border-b border-surface-border px-3 py-3 flex flex-col gap-2 shadow-lg z-30">
          {/* Model picker on mobile */}
          <ModelPickerPopover
            open={modelPickerOpen}
            onOpenChange={handleMobilePickerOpenChange}
            isInMobileMenu
            connected={connected}
            routingMode={routingMode}
            activeAgentId={activeAgentId}
            sessionId={sessionId}
            sessionsByAgent={sessionsByAgent}
            agents={agents}
            allModels={allModels}
            currentProvider={activeAgentModel?.provider}
            currentModel={activeAgentModel?.model}
            currentNode={activeAgentModel?.node}
            currentWorkspace={currentWorkspace}
            recentModelsByWorkspace={recentModelsByWorkspace}
            agentMode={agentMode}
            onRefresh={refreshAllModels}
            onSetSessionModel={setSessionModel}
            providerCapabilities={providerCapabilities}
            modelDownloads={modelDownloads}
            onAddCustomModelFromHf={addCustomModelFromHf}
            onAddCustomModelFromFile={addCustomModelFromFile}
            onDeleteCustomModel={deleteCustomModel}
          />
          <div className="flex items-center gap-2">
            <button
              type="button"
              onClick={() => { setThemeSwitcherOpen(true); setMobileMenuOpen(false); }}
              className="flex-1 h-8 inline-flex items-center justify-center gap-2 rounded-lg border border-surface-border bg-surface-canvas/60 transition-colors hover:border-accent-primary/40 text-xs"
              aria-label="Open theme switcher"
            >
              <Palette className="w-3.5 h-3.5 text-accent-primary" />
              <span className="text-ui-secondary">Theme</span>
            </button>
            <button
              type="button"
              onClick={() => { setShortcutGatewayOpen(true); setMobileMenuOpen(false); }}
              className="flex-1 h-8 inline-flex items-center justify-center gap-2 rounded-lg border border-surface-border bg-surface-canvas/60 transition-colors hover:border-accent-primary/40 text-xs"
              aria-label="Open shortcut gateway"
            >
              <Keyboard className="w-3.5 h-3.5 text-accent-primary" />
              <span className="text-ui-secondary">Shortcuts</span>
            </button>

          </div>
          <RemoteNodeIndicator remoteNodes={remoteNodes} />
        </div>
      )}
      
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
        onDeleteSession={handleDeleteSession}
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
        onUpdatePlugins={() => {
          setShortcutGatewayOpen(false);
          updatePlugins();
        }}
        isUpdatingPlugins={isUpdatingPlugins}
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
            clearApiTokenResult();
          }
        }}
        providers={authProviders}
        oauthFlow={oauthFlow}
        oauthResult={oauthResult}
        apiTokenResult={apiTokenResult}
        onRequestProviders={requestAuthProviders}
        onStartOAuthLogin={startOAuthLogin}
        onCompleteOAuthLogin={completeOAuthLogin}
        onClearOAuthState={clearOAuthState}
        onDisconnectOAuth={disconnectOAuth}
        onSetApiToken={setApiToken}
        onClearApiToken={clearApiToken}
        onSetAuthMethod={setAuthMethodPref}
        onClearApiTokenResult={clearApiTokenResult}
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
            agents={agents}
            agentModels={agentModels}
            sessionLimits={sessionLimits}
          />
      )}

      {/* Plugin update progress / result indicator */}
      <PluginUpdateIndicator
        isUpdatingPlugins={isUpdatingPlugins}
        pluginUpdateStatus={pluginUpdateStatus}
        pluginUpdateResults={pluginUpdateResults}
      />

      {/* Session action + connection toasts */}
      {(sessionActionNotices.length > 0 || connectionErrors.length > 0) && (
        <div className="fixed bottom-4 right-4 z-50 flex flex-col gap-2 max-w-md">
          {sessionActionNotices.map((notice) => (
            <div
              key={notice.id}
              className={`flex items-start gap-2 px-4 py-3 rounded-lg border bg-surface-elevated shadow-lg animate-fade-in ${
                notice.kind === 'success'
                  ? 'border-status-success/40'
                  : 'border-status-warning/40'
              }`}
            >
              <span
                className={`text-xs flex-1 break-words ${
                  notice.kind === 'success' ? 'text-status-success' : 'text-status-warning'
                }`}
              >
                {notice.message}
              </span>
              <button
                type="button"
                onClick={() => dismissSessionActionNotice(notice.id)}
                className="text-ui-muted hover:text-ui-primary transition-colors flex-shrink-0"
                aria-label="Dismiss"
              >
                &times;
              </button>
            </div>
          ))}
          {connectionErrors.map((err) => (
            <div
              key={err.id}
              className="flex items-start gap-2 px-4 py-3 rounded-lg border border-status-warning/40 bg-surface-elevated shadow-lg animate-fade-in"
            >
              <span className="text-xs text-status-warning flex-1 break-words">{err.message}</span>
              <button
                type="button"
                onClick={() => dismissConnectionError(err.id)}
                className="text-ui-muted hover:text-ui-primary transition-colors flex-shrink-0"
                aria-label="Dismiss"
              >
                &times;
              </button>
            </div>
          ))}
        </div>
      )}
    </div>
    </SessionTimerProvider>
  );
}
