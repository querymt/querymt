import { useEffect, useCallback, useMemo, useState } from 'react';
import { Outlet, useLocation, useNavigate } from 'react-router-dom';
import { useUiClientActions, useUiClientSession, useUiClientConfig } from '../context/UiClientContext';
import { useUiStore } from '../store/uiStore';
import { useIsMobile } from '../hooks/useIsMobile';
import { useGlobalKeyboardShortcuts } from '../hooks/useGlobalKeyboardShortcuts';
import { useThemeSync } from '../hooks/useThemeSync';
import { useAutoModelSwitch } from '../hooks/useAutoModelSwitch';
import { SessionTimerProvider } from '../context/SessionTimerContext';
import { getDashboardThemes } from '../utils/dashboardThemes';

import { AppHeader } from './AppHeader';
import { MobileDropdownMenu } from './MobileDropdownMenu';
import { GlobalOverlays } from './GlobalOverlays';
import { ToastStack } from './ToastStack';
import { MeshInvitePanel } from './MeshInvitePanel';

/**
 * AppShell - Main layout wrapper for all routes.
 *
 * Orchestrates the header, mobile menu, route outlet, global overlays, and toasts.
 * Heavy logic (keyboard shortcuts, theme sync, model switching) is delegated to hooks.
 */
export function AppShell() {
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
    cycleReasoningEffort,
    setReasoningEffort,
    submitWorkspacePathDialog,
    cancelWorkspacePathDialog,
    createMeshInvite,
    listMeshInvites,
    revokeMeshInvite,
    dismissConnectionError,
    dismissSessionActionNotice,
    updatePlugins,
    createSchedule,
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
    reasoningEffort,
    remoteNodes,
    meshInvites,
    lastCreatedMeshInvite,
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
    modelPickerOpen,
    setModelPickerOpen,
    statsDrawerOpen,
    setStatsDrawerOpen,
    delegationDrawerOpen,
    selectedToolEvent,
    selectedTheme,
    setSelectedTheme,
    createScheduleDialogOpen,
    setCreateScheduleDialogOpen,
  } = useUiStore();

  const location = useLocation();
  const isHomePage = location.pathname === '/';
  const isMobile = useIsMobile();
  const [shortcutGatewayOpen, setShortcutGatewayOpen] = useState(false);
  const [themeSwitcherOpen, setThemeSwitcherOpen] = useState(false);
  const [providerAuthOpen, setProviderAuthOpen] = useState(false);
  const [mobileMenuOpen, setMobileMenuOpen] = useState(false);
  const isSessionActive = thinkingAgentIds.size > 0;
  const availableThemes = useMemo(() => getDashboardThemes(), []);
  // --- Extracted hooks ---

  // Handle new session creation with navigation
  const handleNewSession = useCallback(async (): Promise<void> => {
    try {
      const newSessionId = await newSession();
      navigate(`/session/${newSessionId}`, { replace: true });
    } catch (err) {
      console.log('Session creation cancelled or failed:', err);
    }
  }, [newSession, navigate]);

  useGlobalKeyboardShortcuts({
    connected,
    thinkingAgentId,
    workspacePathDialogOpen,
    shortcutGatewayOpen,
    setShortcutGatewayOpen,
    themeSwitcherOpen,
    setThemeSwitcherOpen,
    providerAuthOpen,
    setProviderAuthOpen,
    handleNewSession,
    cancelSession,
    cycleAgentMode,
    cycleReasoningEffort,
    requestAuthProviders,
    updatePlugins,
    cancelWorkspacePathDialog,
  });

  useThemeSync(agentMode, selectedTheme);

  useAutoModelSwitch({
    agentMode,
    sessionId,
    activeAgentId,
    agentModels,
    setSessionModel,
  });

  // --- Effects ---

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
      workspacePathDialogOpen ||
      createScheduleDialogOpen;

    document.body.classList.toggle('modal-open', hasOpenOverlay);
    return () => { document.body.classList.remove('modal-open'); };
  }, [
    sessionSwitcherOpen, modelPickerOpen, statsDrawerOpen,
    delegationDrawerOpen, selectedToolEvent, shortcutGatewayOpen,
    themeSwitcherOpen, providerAuthOpen, workspacePathDialogOpen,
    createScheduleDialogOpen, isMobile,
  ]);

  // Load persisted UI state on mount
  useEffect(() => { useUiStore.getState().loadPersistedState(); }, []);

  // Refresh provider/model data after successful OAuth login.
  useEffect(() => {
    if (!oauthResult?.success) return;
    refreshAllModels();
    requestAuthProviders();
  }, [oauthResult, refreshAllModels, requestAuthProviders]);

  // Clear transient OAuth UI state when auth modal closes.
  useEffect(() => {
    if (!providerAuthOpen) clearOAuthState();
  }, [providerAuthOpen, clearOAuthState]);

  // --- Derived values ---

  const currentWorkspace = useMemo(() => {
    if (!sessionId) return null;
    for (const group of sessionGroups) {
      if (group.sessions.some(s => s.session_id === sessionId)) {
        return group.cwd || null;
      }
    }
    return null;
  }, [sessionId, sessionGroups]);

  const activeAgentModel = useMemo(
    () => agentModels[activeAgentId],
    [agentModels, activeAgentId],
  );

  // --- Callbacks ---

  const handleSelectSession = useCallback((sid: string) => {
    navigate(`/session/${sid}`);
  }, [navigate]);

  const handleDeleteSession = useCallback((targetSessionId: string, sessionLabel?: string) => {
    deleteSession(targetSessionId, sessionLabel);
    if (targetSessionId === sessionId) navigate('/');
  }, [deleteSession, sessionId, navigate]);

  const handleMobilePickerOpenChange = useCallback(
    (open: boolean) => {
      setModelPickerOpen(open);
      if (!open) setMobileMenuOpen(false);
    },
    [setModelPickerOpen],
  );

  // --- Render ---

  const meshInvitePanel = (
    <MeshInvitePanel
      connected={connected}
      meshInvites={meshInvites}
      lastCreatedMeshInvite={lastCreatedMeshInvite}
      createMeshInvite={createMeshInvite}
      listMeshInvites={listMeshInvites}
      revokeMeshInvite={revokeMeshInvite}
    />
  );

  return (
    <SessionTimerProvider>
    <div className="flex flex-col h-screen bg-surface-canvas text-ui-primary">
      <AppHeader
        isHomePage={isHomePage}
        isMobile={isMobile}
        sessionId={sessionId}
        connected={connected}
        isSessionActive={isSessionActive}
        isConversationComplete={isConversationComplete}
        agentMode={agentMode}
        cycleAgentMode={cycleAgentMode}
        setSessionSwitcherOpen={setSessionSwitcherOpen}
        agentModels={agentModels}
        sessionLimits={sessionLimits}
        statsDrawerOpen={statsDrawerOpen}
        setStatsDrawerOpen={setStatsDrawerOpen}
        modelPickerOpen={modelPickerOpen}
        setModelPickerOpen={setModelPickerOpen}
        mobileMenuOpen={mobileMenuOpen}
        setMobileMenuOpen={setMobileMenuOpen}
        routingMode={routingMode}
        activeAgentId={activeAgentId}
        sessionsByAgent={sessionsByAgent}
        agents={agents}
        allModels={allModels}
        activeAgentModel={activeAgentModel}
        remoteNodes={remoteNodes}
        currentWorkspace={currentWorkspace}
        recentModelsByWorkspace={recentModelsByWorkspace}
        reasoningEffort={reasoningEffort}
        refreshAllModels={refreshAllModels}
        setSessionModel={setSessionModel}
        setReasoningEffort={setReasoningEffort}
        cycleReasoningEffort={cycleReasoningEffort}
        providerCapabilities={providerCapabilities}
        modelDownloads={modelDownloads}
        addCustomModelFromHf={addCustomModelFromHf}
        addCustomModelFromFile={addCustomModelFromFile}
        deleteCustomModel={deleteCustomModel}
        desktopActions={meshInvitePanel}
      />

      {/* Mobile dropdown menu */}
      {isMobile && mobileMenuOpen && (
        <MobileDropdownMenu
          modelPickerOpen={modelPickerOpen}
          handleMobilePickerOpenChange={handleMobilePickerOpenChange}
          setShortcutGatewayOpen={setShortcutGatewayOpen}
          setMobileMenuOpen={setMobileMenuOpen}
          connected={connected}
          routingMode={routingMode}
          activeAgentId={activeAgentId}
          sessionId={sessionId}
          sessionsByAgent={sessionsByAgent}
          agents={agents}
          allModels={allModels}
          activeAgentModel={activeAgentModel}
          remoteNodes={remoteNodes}
          currentWorkspace={currentWorkspace}
          recentModelsByWorkspace={recentModelsByWorkspace}
          agentMode={agentMode}
          reasoningEffort={reasoningEffort}
          refreshAllModels={refreshAllModels}
          setSessionModel={setSessionModel}
          setReasoningEffort={setReasoningEffort}
          cycleReasoningEffort={cycleReasoningEffort}
          providerCapabilities={providerCapabilities}
          modelDownloads={modelDownloads}
          addCustomModelFromHf={addCustomModelFromHf}
          addCustomModelFromFile={addCustomModelFromFile}
          deleteCustomModel={deleteCustomModel}
          mobileExtras={meshInvitePanel}
        />
      )}

      {/* Mode accent line */}
      <div
        className="h-px w-full transition-colors duration-200"
        style={{ backgroundColor: `rgba(var(--mode-rgb), 0.6)` }}
      />

      {/* Route content */}
      <div className="flex-1 min-h-0 flex flex-col overflow-hidden">
        <Outlet />
      </div>

      {/* Global overlays */}
      <GlobalOverlays
        sessionSwitcherOpen={sessionSwitcherOpen}
        setSessionSwitcherOpen={setSessionSwitcherOpen}
        sessionGroups={sessionGroups}
        sessionId={sessionId}
        thinkingBySession={thinkingBySession}
        handleNewSession={handleNewSession}
        handleSelectSession={handleSelectSession}
        handleDeleteSession={handleDeleteSession}
        connected={connected}
        shortcutGatewayOpen={shortcutGatewayOpen}
        setShortcutGatewayOpen={setShortcutGatewayOpen}
        loading={loading}
        setThemeSwitcherOpen={setThemeSwitcherOpen}
        setProviderAuthOpen={setProviderAuthOpen}
        requestAuthProviders={requestAuthProviders}
        updatePlugins={updatePlugins}
        setCreateScheduleDialogOpen={setCreateScheduleDialogOpen}
        isUpdatingPlugins={isUpdatingPlugins}
        themeSwitcherOpen={themeSwitcherOpen}
        availableThemes={availableThemes}
        selectedTheme={selectedTheme}
        setSelectedTheme={setSelectedTheme}
        providerAuthOpen={providerAuthOpen}
        authProviders={authProviders}
        oauthFlow={oauthFlow}
        oauthResult={oauthResult}
        apiTokenResult={apiTokenResult}
        startOAuthLogin={startOAuthLogin}
        completeOAuthLogin={completeOAuthLogin}
        clearOAuthState={clearOAuthState}
        disconnectOAuth={disconnectOAuth}
        setApiToken={setApiToken}
        clearApiToken={clearApiToken}
        setAuthMethodPref={setAuthMethodPref}
        clearApiTokenResult={clearApiTokenResult}
        workspacePathDialogOpen={workspacePathDialogOpen}
        workspacePathDialogDefaultValue={workspacePathDialogDefaultValue}
        remoteNodes={remoteNodes}
        submitWorkspacePathDialog={submitWorkspacePathDialog}
        cancelWorkspacePathDialog={cancelWorkspacePathDialog}
        createScheduleDialogOpen={createScheduleDialogOpen}
        createSchedule={createSchedule}
        statsDrawerOpen={statsDrawerOpen}
        setStatsDrawerOpen={setStatsDrawerOpen}
        agents={agents}
        agentModels={agentModels}
        sessionLimits={sessionLimits}
        pluginUpdateStatus={pluginUpdateStatus}
        pluginUpdateResults={pluginUpdateResults}
      />

      {/* Toasts */}
      <ToastStack
        sessionActionNotices={sessionActionNotices}
        connectionErrors={connectionErrors}
        onDismissNotice={dismissSessionActionNotice}
        onDismissError={dismissConnectionError}
      />
    </div>
    </SessionTimerProvider>
  );
}
