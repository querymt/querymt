import { describe, it, expect, vi } from 'vitest';
import { render, screen } from '@testing-library/react';
import { MemoryRouter } from 'react-router-dom';
import { AppHeader } from './AppHeader';
import { MobileDropdownMenu } from './MobileDropdownMenu';

vi.mock('./GlitchText', () => ({
  GlitchText: ({ text }: { text: string }) => <span>{text}</span>,
}));

vi.mock('./ModelPickerPopover', () => ({
  ModelPickerPopover: ({ isInMobileMenu }: { isInMobileMenu?: boolean }) => (
    <div data-testid={isInMobileMenu ? 'mobile-model-picker' : 'desktop-model-picker'} />
  ),
}));

vi.mock('./HeaderStatsBar', () => ({
  HeaderStatsBar: () => <div data-testid="header-stats-bar" />,
}));

vi.mock('./RemoteNodeIndicator', () => ({
  RemoteNodeIndicator: () => <div data-testid="remote-node-indicator" />,
}));

const baseModelProps = {
  modelPickerOpen: true,
  connected: true,
  routingMode: 'broadcast' as const,
  activeAgentId: 'agent-1',
  sessionsByAgent: { 'agent-1': 'session-1' },
  agents: [],
  allModels: [],
  activeAgentModel: undefined,
  remoteNodes: [],
  currentWorkspace: null,
  recentModelsByWorkspace: {},
  agentMode: 'standard',
  reasoningEffort: null,
  refreshAllModels: vi.fn(),
  setSessionModel: vi.fn(),
  setReasoningEffort: vi.fn(),
  cycleReasoningEffort: vi.fn(),
  providerCapabilities: {},
  modelDownloads: {},
  addCustomModelFromHf: vi.fn(),
  addCustomModelFromFile: vi.fn(),
  deleteCustomModel: vi.fn(),
};

const appHeaderProps = {
  ...baseModelProps,
  isHomePage: false,
  isMobile: false,
  sessionId: 'session-1',
  reconnecting: false,
  isSessionActive: false,
  isConversationComplete: false,
  cycleAgentMode: vi.fn(),
  setSessionSwitcherOpen: vi.fn(),
  agentModels: {},
  sessionLimits: null,
  statsDrawerOpen: false,
  setStatsDrawerOpen: vi.fn(),
  setModelPickerOpen: vi.fn(),
  mobileMenuOpen: false,
  setMobileMenuOpen: vi.fn(),
};

const mobileDropdownProps = {
  ...baseModelProps,
  sessionId: 'session-1',
  handleMobilePickerOpenChange: vi.fn(),
  setShortcutGatewayOpen: vi.fn(),
  setMobileMenuOpen: vi.fn(),
};

describe('model picker session visibility', () => {
  it('renders the desktop model picker only when a session exists', () => {
    const { rerender } = render(
      <MemoryRouter>
        <AppHeader {...appHeaderProps} />
      </MemoryRouter>
    );

    expect(screen.getByTestId('desktop-model-picker')).toBeInTheDocument();

    rerender(
      <MemoryRouter>
        <AppHeader {...appHeaderProps} sessionId={null} />
      </MemoryRouter>
    );

    expect(screen.queryByTestId('desktop-model-picker')).not.toBeInTheDocument();
    expect(screen.getByTestId('remote-node-indicator')).toBeInTheDocument();
  });

  it('renders the mobile model picker only when a session exists', () => {
    const { rerender } = render(<MobileDropdownMenu {...mobileDropdownProps} />);

    expect(screen.getByTestId('mobile-model-picker')).toBeInTheDocument();

    rerender(<MobileDropdownMenu {...mobileDropdownProps} sessionId={null} />);

    expect(screen.queryByTestId('mobile-model-picker')).not.toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Open shortcut gateway' })).toBeInTheDocument();
    expect(screen.getByTestId('remote-node-indicator')).toBeInTheDocument();
  });
});
