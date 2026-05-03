/**
 * GlobalOverlays - Renders all modal/drawer overlays that live at the AppShell level.
 * Extracted from AppShell to reduce its size.
 */

import { SessionSwitcher } from './SessionSwitcher';
import { ShortcutGateway } from './ShortcutGateway';
import { ThemeSwitcher } from './ThemeSwitcher';
import { ProviderAuthSwitcher } from './ProviderAuthSwitcher';
import { WorkspacePathDialog } from './WorkspacePathDialog';
import { CreateScheduleDialog } from './CreateScheduleDialog';
import { StatsDrawer } from './StatsDrawer';
import { PluginUpdateIndicator } from './PluginUpdateIndicator';
import type { DashboardTheme } from '../utils/dashboardThemes';
import type { UiAgentInfo, SessionGroup, SessionLimits, AuthMethod } from '../types';

interface GlobalOverlaysProps {
  // Session switcher
  sessionSwitcherOpen: boolean;
  setSessionSwitcherOpen: (open: boolean) => void;
  sessionGroups: SessionGroup[];
  sessionId: string | null;
  thinkingBySession: Map<string, Set<string>>;
  handleNewSession: () => Promise<void>;
  handleSelectSession: (sessionId: string) => void;
  handleDeleteSession: (sessionId: string, sessionLabel?: string) => void;
  loadSessionChildren: (parentSessionId: string) => void;
  sessionChildrenLoading: Set<string>;
  connected: boolean;

  // Shortcut gateway
  shortcutGatewayOpen: boolean;
  setShortcutGatewayOpen: (open: boolean) => void;
  loading: boolean;
  setThemeSwitcherOpen: (open: boolean) => void;
  setProviderAuthOpen: (open: boolean) => void;
  requestAuthProviders: () => void;
  updatePlugins: () => void;
  setCreateScheduleDialogOpen: (open: boolean) => void;
  isUpdatingPlugins: boolean;

  // Theme switcher
  themeSwitcherOpen: boolean;
  availableThemes: DashboardTheme[];
  selectedTheme: string;
  setSelectedTheme: (themeId: string) => void;

  // Provider auth
  providerAuthOpen: boolean;
  authProviders: any;
  oauthFlow: any;
  oauthResult: any;
  apiTokenResult: any;
  startOAuthLogin: (provider: string) => void;
  completeOAuthLogin: (flowId: string, response: string) => void;
  clearOAuthState: () => void;
  disconnectOAuth: (provider: string) => void;
  setApiToken: (provider: string, token: string) => void;
  clearApiToken: (provider: string) => void;
  setAuthMethodPref: (provider: string, method: AuthMethod) => void;
  clearApiTokenResult: () => void;

  // Workspace path dialog
  workspacePathDialogOpen: boolean;
  workspacePathDialogDefaultValue: string;
  remoteNodes: any;
  submitWorkspacePathDialog: (value: string, node: string | null) => void;
  cancelWorkspacePathDialog: () => void;

  // Create schedule dialog
  createScheduleDialogOpen: boolean;
  createSchedule: (sessionId: string, cron: string, prompt: string) => void;

  // Stats drawer
  statsDrawerOpen: boolean;
  setStatsDrawerOpen: (open: boolean) => void;
  agents: UiAgentInfo[];
  agentModels: Record<string, { provider?: string; model?: string; contextLimit?: number; node?: string }>;
  sessionLimits?: SessionLimits | null;

  // Plugin update indicator
  pluginUpdateStatus: any;
  pluginUpdateResults: any;
}

export function GlobalOverlays(props: GlobalOverlaysProps) {
  return (
    <>
      {/* Session Switcher (Cmd+/) */}
      <SessionSwitcher
        open={props.sessionSwitcherOpen}
        onOpenChange={props.setSessionSwitcherOpen}
        groups={props.sessionGroups}
        activeSessionId={props.sessionId}
        thinkingBySession={props.thinkingBySession}
        onNewSession={props.handleNewSession}
        onSelectSession={props.handleSelectSession}
        onDeleteSession={props.handleDeleteSession}
        onLoadSessionChildren={props.loadSessionChildren}
        sessionChildrenLoading={props.sessionChildrenLoading}
        connected={props.connected}
      />

      <ShortcutGateway
        open={props.shortcutGatewayOpen}
        onOpenChange={props.setShortcutGatewayOpen}
        onStartNewSession={() => {
          props.setShortcutGatewayOpen(false);
          if (props.connected && !props.loading) {
            props.handleNewSession();
          }
        }}
        onSelectTheme={() => {
          props.setShortcutGatewayOpen(false);
          props.setThemeSwitcherOpen(true);
        }}
        onAuthenticateProvider={() => {
          props.setShortcutGatewayOpen(false);
          props.setThemeSwitcherOpen(false);
          props.setProviderAuthOpen(true);
          props.requestAuthProviders();
        }}
        onUpdatePlugins={() => {
          props.setShortcutGatewayOpen(false);
          props.updatePlugins();
        }}
        onCreateSchedule={() => {
          props.setShortcutGatewayOpen(false);
          props.setCreateScheduleDialogOpen(true);
        }}
        isUpdatingPlugins={props.isUpdatingPlugins}
      />

      <ThemeSwitcher
        open={props.themeSwitcherOpen}
        onOpenChange={props.setThemeSwitcherOpen}
        themes={props.availableThemes}
        selectedTheme={props.selectedTheme}
        onSelectTheme={props.setSelectedTheme}
      />

      <ProviderAuthSwitcher
        open={props.providerAuthOpen}
        onOpenChange={(open) => {
          props.setProviderAuthOpen(open);
          if (!open) {
            props.clearOAuthState();
            props.clearApiTokenResult();
          }
        }}
        providers={props.authProviders}
        oauthFlow={props.oauthFlow}
        oauthResult={props.oauthResult}
        apiTokenResult={props.apiTokenResult}
        onRequestProviders={props.requestAuthProviders}
        onStartOAuthLogin={props.startOAuthLogin}
        onCompleteOAuthLogin={props.completeOAuthLogin}
        onClearOAuthState={props.clearOAuthState}
        onDisconnectOAuth={props.disconnectOAuth}
        onSetApiToken={props.setApiToken}
        onClearApiToken={props.clearApiToken}
        onSetAuthMethod={props.setAuthMethodPref}
        onClearApiTokenResult={props.clearApiTokenResult}
      />

      <WorkspacePathDialog
        open={props.workspacePathDialogOpen}
        defaultValue={props.workspacePathDialogDefaultValue}
        remoteNodes={props.remoteNodes}
        onSubmit={props.submitWorkspacePathDialog}
        onCancel={props.cancelWorkspacePathDialog}
      />

      <CreateScheduleDialog
        open={props.createScheduleDialogOpen}
        sessionId={props.sessionId}
        onOpenChange={props.setCreateScheduleDialogOpen}
        onCreate={props.createSchedule}
      />

      {props.sessionId && (
        <StatsDrawer
          open={props.statsDrawerOpen}
          onOpenChange={props.setStatsDrawerOpen}
          agents={props.agents}
          agentModels={props.agentModels}
          sessionLimits={props.sessionLimits}
        />
      )}

      <PluginUpdateIndicator
        isUpdatingPlugins={props.isUpdatingPlugins}
        pluginUpdateStatus={props.pluginUpdateStatus}
        pluginUpdateResults={props.pluginUpdateResults}
      />
    </>
  );
}
