import { useEffect } from 'react';
import { useUiStore } from '../store/uiStore';
import { toggleDebugLog } from '../utils/debugLog';

/**
 * Centralizes all global keyboard shortcuts previously inline in AppShell.
 *
 * - Ctrl/Cmd+X gateway (chords: T, N, A, U, S, D)
 * - Ctrl/Cmd+E cycle agent mode
 * - Ctrl/Cmd+; cycle reasoning effort
 * - Ctrl/Cmd+Shift+M toggle model picker
 * - Ctrl/Cmd+/ toggle session switcher
 * - ESC cascade: close modals -> double-escape cancel session
 */
export function useGlobalKeyboardShortcuts({
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
}: {
  connected: boolean;
  thinkingAgentId: string | null;
  workspacePathDialogOpen: boolean;
  shortcutGatewayOpen: boolean;
  setShortcutGatewayOpen: (open: boolean | ((prev: boolean) => boolean)) => void;
  themeSwitcherOpen: boolean;
  setThemeSwitcherOpen: (open: boolean) => void;
  providerAuthOpen: boolean;
  setProviderAuthOpen: (open: boolean) => void;
  handleNewSession: () => void;
  cancelSession: () => void;
  cycleAgentMode: () => void;
  cycleReasoningEffort: () => void;
  requestAuthProviders: () => void;
  updatePlugins: () => void;
  cancelWorkspacePathDialog: () => void;
}) {
  const {
    loading,
    sessionSwitcherOpen,
    setSessionSwitcherOpen,
    modelPickerOpen,
    setModelPickerOpen,
    setStatsDrawerOpen,
    createScheduleDialogOpen,
    setCreateScheduleDialogOpen,
  } = useUiStore();

  // Global keyboard shortcuts
  useEffect(() => {
    const handleKeyDown = (e: KeyboardEvent) => {
      const normalizedKey = e.key.toLowerCase();

      // Ctrl/Cmd+X gateway
      if ((e.ctrlKey || e.metaKey) && !e.altKey && !e.shiftKey && normalizedKey === 'x') {
        e.preventDefault();
        setShortcutGatewayOpen((open: boolean) => !open);
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

      if (shortcutGatewayOpen && !e.altKey && !e.shiftKey && normalizedKey === 's') {
        e.preventDefault();
        setShortcutGatewayOpen(false);
        setCreateScheduleDialogOpen(true);
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

      // Ctrl+; / Cmd+; - Cycle reasoning effort
      if ((e.metaKey || e.ctrlKey) && !e.shiftKey && !e.altKey && e.key === ';') {
        e.preventDefault();
        cycleReasoningEffort();
        return;
      }

      // Cmd+Shift+M / Ctrl+Shift+M - Toggle model picker
      if ((e.metaKey || e.ctrlKey) && e.shiftKey && normalizedKey === 'm') {
        e.preventDefault();
        setModelPickerOpen(!modelPickerOpen);
        return;
      }

      // Cmd+/ or Ctrl+/ - Toggle session switcher
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
    cycleReasoningEffort,
    modelPickerOpen,
    setModelPickerOpen,
    setShortcutGatewayOpen,
    setThemeSwitcherOpen,
    setProviderAuthOpen,
    requestAuthProviders,
    updatePlugins,
    setCreateScheduleDialogOpen,
    handleNewSession,
  ]);

  // ESC handling: close modals first, then double-escape to cancel session
  useEffect(() => {
    let lastEscapeTime = 0;

    const handleKeyDown = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        const { sessionSwitcherOpen, modelPickerOpen, statsDrawerOpen } = useUiStore.getState();

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
        if (createScheduleDialogOpen) {
          e.preventDefault();
          e.stopImmediatePropagation();
          setCreateScheduleDialogOpen(false);
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

        // Double-escape to cancel session (when agent is thinking)
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
    createScheduleDialogOpen,
    setSessionSwitcherOpen,
    setModelPickerOpen,
    setShortcutGatewayOpen,
    setProviderAuthOpen,
    setStatsDrawerOpen,
    setCreateScheduleDialogOpen,
    setThemeSwitcherOpen,
  ]);
}
