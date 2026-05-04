import { useEffect } from 'react';
import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render, screen, waitFor } from '@testing-library/react';
import { MemoryRouter, Route, Routes } from 'react-router-dom';
import { AppShell } from './AppShell';

const mocks = vi.hoisted(() => {
  const uiStoreState = {
    loading: false,
    sessionSwitcherOpen: false,
    setSessionSwitcherOpen: vi.fn(),
    modelPickerOpen: true,
    setModelPickerOpen: vi.fn(),
    statsDrawerOpen: true,
    setStatsDrawerOpen: vi.fn(),
    mobileMenuOpen: true,
    setMobileMenuOpen: vi.fn(),
    delegationDrawerOpen: false,
    selectedToolEvent: null,
    selectedTheme: 'default-dark',
    setSelectedTheme: vi.fn(),
    createScheduleDialogOpen: false,
    setCreateScheduleDialogOpen: vi.fn(),
  };

  return {
    isMobile: false,
    uiStoreState,
    actions: {
      newSession: vi.fn(),
      cancelSession: vi.fn(),
      deleteSession: vi.fn(),
      refreshAllModels: vi.fn(),
      requestAuthProviders: vi.fn(),
      startOAuthLogin: vi.fn(),
      completeOAuthLogin: vi.fn(),
      disconnectOAuth: vi.fn(),
      clearOAuthState: vi.fn(),
      setApiToken: vi.fn(),
      clearApiToken: vi.fn(),
      setAuthMethodPref: vi.fn(),
      clearApiTokenResult: vi.fn(),
      setSessionModel: vi.fn(),
      addCustomModelFromHf: vi.fn(),
      addCustomModelFromFile: vi.fn(),
      deleteCustomModel: vi.fn(),
      cycleAgentMode: vi.fn(),
      cycleReasoningEffort: vi.fn(),
      setReasoningEffort: vi.fn(),
      submitWorkspacePathDialog: vi.fn(),
      cancelWorkspacePathDialog: vi.fn(),
      createMeshInvite: vi.fn(),
      listMeshInvites: vi.fn(),
      revokeMeshInvite: vi.fn(),
      dismissConnectionError: vi.fn(),
      dismissSessionActionNotice: vi.fn(),
      updatePlugins: vi.fn(),
      createSchedule: vi.fn(),
    },
  };
});

vi.mock('../context/UiClientContext', () => ({
  useUiClientActions: () => mocks.actions,
  useUiClientSession: () => ({
    connected: true,
    reconnecting: false,
    thinkingAgentId: null,
    thinkingAgentIds: new Set(),
    sessionId: 'session-1',
    agents: [],
    agentModels: { 'agent-1': { provider: 'test', model: 'model-1' } },
    sessionLimits: null,
    isConversationComplete: false,
    routingMode: 'broadcast',
    activeAgentId: 'agent-1',
    sessionsByAgent: { 'agent-1': 'session-1' },
    sessionGroups: [{ cwd: '/workspace', sessions: [{ session_id: 'session-1' }] }],
    thinkingBySession: {},
    agentMode: 'standard',
    reasoningEffort: 'medium',
    remoteNodes: [],
    meshInvites: [],
    lastCreatedMeshInvite: null,
  }),
  useUiClientConfig: () => ({
    allModels: [],
    providerCapabilities: {},
    recentModelsByWorkspace: {},
    authProviders: [],
    oauthFlow: null,
    oauthResult: null,
    modelDownloads: {},
    apiTokenResult: null,
    workspacePathDialogOpen: false,
    workspacePathDialogDefaultValue: '',
    connectionErrors: [],
    sessionActionNotices: [],
    isUpdatingPlugins: false,
    pluginUpdateStatus: null,
    pluginUpdateResults: null,
  }),
}));

vi.mock('../store/uiStore', () => {
  const useUiStore = () => mocks.uiStoreState;
  useUiStore.getState = () => ({ loadPersistedState: vi.fn() });
  return { useUiStore };
});

vi.mock('../hooks/useIsMobile', () => ({ useIsMobile: () => mocks.isMobile }));
vi.mock('../hooks/useGlobalKeyboardShortcuts', () => ({ useGlobalKeyboardShortcuts: vi.fn() }));
vi.mock('../hooks/useThemeSync', () => ({ useThemeSync: vi.fn() }));
vi.mock('../hooks/useAutoModelSwitch', () => ({ useAutoModelSwitch: vi.fn() }));
vi.mock('../context/SessionTimerContext', () => ({
  SessionTimerProvider: ({ children }: { children: React.ReactNode }) => <>{children}</>,
}));
vi.mock('./MeshInvitePanel', () => ({ MeshInvitePanel: () => <div data-testid="mesh-invite-panel" /> }));
vi.mock('./ToastStack', () => ({ ToastStack: () => <div data-testid="toast-stack" /> }));
vi.mock('./AppHeader', () => ({
  AppHeader: ({ sessionId, activeAgentModel, currentWorkspace, setMobileMenuOpen }: any) => {
    useEffect(() => {
      if (mocks.isMobile) setMobileMenuOpen(true);
    }, [setMobileMenuOpen]);

    return (
      <div
        data-testid="app-header"
        data-session-id={sessionId ?? ''}
        data-active-model={activeAgentModel?.model ?? ''}
        data-workspace={currentWorkspace ?? ''}
      />
    );
  },
}));
vi.mock('./MobileDropdownMenu', () => ({
  MobileDropdownMenu: ({ sessionId, activeAgentModel, currentWorkspace }: any) => (
    <div
      data-testid="mobile-dropdown-menu"
      data-session-id={sessionId ?? ''}
      data-active-model={activeAgentModel?.model ?? ''}
      data-workspace={currentWorkspace ?? ''}
    />
  ),
}));
vi.mock('./GlobalOverlays', () => ({
  GlobalOverlays: ({ sessionId }: any) => <div data-testid="global-overlays" data-session-id={sessionId ?? ''} />,
}));

function renderShell(path: string) {
  return render(
    <MemoryRouter initialEntries={[path]}>
      <Routes>
        <Route path="/" element={<AppShell />}>
          <Route index element={<div data-testid="home-page" />} />
          <Route path="session/:sessionId" element={<div data-testid="session-page" />} />
        </Route>
      </Routes>
    </MemoryRouter>,
  );
}

describe('AppShell home header session visibility', () => {
  beforeEach(() => {
    mocks.isMobile = false;
    vi.clearAllMocks();
  });

  it('passes no visible session details to header on the home route', async () => {
    renderShell('/');

    expect(screen.getByTestId('app-header')).toHaveAttribute('data-session-id', '');
    expect(screen.getByTestId('app-header')).toHaveAttribute('data-active-model', '');
    expect(screen.getByTestId('app-header')).toHaveAttribute('data-workspace', '');
    expect(screen.getByTestId('global-overlays')).toHaveAttribute('data-session-id', 'session-1');
    await waitFor(() => expect(mocks.uiStoreState.setModelPickerOpen).toHaveBeenCalledWith(false));
    expect(mocks.uiStoreState.setStatsDrawerOpen).toHaveBeenCalledWith(false);
  });

  it('keeps visible session details on session routes', () => {
    renderShell('/session/session-1');

    expect(screen.getByTestId('app-header')).toHaveAttribute('data-session-id', 'session-1');
    expect(screen.getByTestId('app-header')).toHaveAttribute('data-active-model', 'model-1');
    expect(screen.getByTestId('app-header')).toHaveAttribute('data-workspace', '/workspace');
    expect(mocks.uiStoreState.setModelPickerOpen).not.toHaveBeenCalled();
    expect(mocks.uiStoreState.setStatsDrawerOpen).not.toHaveBeenCalled();
  });

  it('passes no visible session details to the mobile menu on the home route', () => {
    mocks.isMobile = true;
    renderShell('/');

    expect(screen.getByTestId('mobile-dropdown-menu')).toHaveAttribute('data-session-id', '');
    expect(screen.getByTestId('mobile-dropdown-menu')).toHaveAttribute('data-active-model', '');
    expect(screen.getByTestId('mobile-dropdown-menu')).toHaveAttribute('data-workspace', '');
  });
});
