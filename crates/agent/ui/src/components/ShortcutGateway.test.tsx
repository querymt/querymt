import { beforeEach, describe, expect, it, vi } from 'vitest';
import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { ShortcutGateway } from './ShortcutGateway';
import { useUiStore } from '../store/uiStore';

describe('ShortcutGateway', () => {
  const defaultProps = {
    open: true,
    onOpenChange: vi.fn(),
    onStartNewSession: vi.fn(),
    onSelectTheme: vi.fn(),
    onAuthenticateProvider: vi.fn(),
    onUpdatePlugins: vi.fn(),
    isUpdatingPlugins: false,
  };

  beforeEach(() => {
    vi.clearAllMocks();
    localStorage.clear();
    useUiStore.setState({ followNewMessages: true });
  });

  it('renders when open is true', () => {
    render(<ShortcutGateway {...defaultProps} />);
    expect(screen.getByText('Shortcut Gateway')).toBeInTheDocument();
    expect(screen.getByText('Start New Session')).toBeInTheDocument();
    expect(screen.getByText('Follow New Messages')).toBeInTheDocument();
    expect(screen.getByText('Theme Selector')).toBeInTheDocument();
    expect(screen.getByText('Authenticate Provider')).toBeInTheDocument();
    expect(screen.getByPlaceholderText(/type command/i)).toBeInTheDocument();
  });

  it('toggles follow new messages command state and persists', async () => {
    const user = userEvent.setup();
    render(<ShortcutGateway {...defaultProps} />);

    expect(screen.getByText('On')).toBeInTheDocument();
    const followItem = screen.getByText('Follow New Messages').closest('[cmdk-item]');
    expect(followItem).toBeTruthy();
    await user.click(followItem!);

    expect(useUiStore.getState().followNewMessages).toBe(false);
    expect(localStorage.getItem('followNewMessages')).toBe('false');
    expect(screen.getByText('Off')).toBeInTheDocument();
  });

  it('calls onStartNewSession when clicking start new session command', async () => {
    const user = userEvent.setup();
    render(<ShortcutGateway {...defaultProps} />);

    const startSessionItem = screen.getByText('Start New Session').closest('[cmdk-item]');
    expect(startSessionItem).toBeTruthy();
    await user.click(startSessionItem!);

    expect(defaultProps.onStartNewSession).toHaveBeenCalledTimes(1);
  });

  it('does not render when open is false', () => {
    render(<ShortcutGateway {...defaultProps} open={false} />);
    expect(screen.queryByText('Shortcut Gateway')).not.toBeInTheDocument();
  });

  it('closes when backdrop is clicked', async () => {
    const user = userEvent.setup();
    render(<ShortcutGateway {...defaultProps} />);

    await user.click(screen.getByTestId('shortcut-gateway-backdrop'));

    expect(defaultProps.onOpenChange).toHaveBeenCalledWith(false);
  });

  it('calls onSelectTheme when clicking theme command', async () => {
    const user = userEvent.setup();
    render(<ShortcutGateway {...defaultProps} />);

    const themeItem = screen.getByText('Theme Selector').closest('[cmdk-item]');
    expect(themeItem).toBeTruthy();
    await user.click(themeItem!);

    expect(defaultProps.onSelectTheme).toHaveBeenCalledTimes(1);
  });

  it('supports keyboard selection with arrow and enter', async () => {
    const user = userEvent.setup();
    render(<ShortcutGateway {...defaultProps} />);

    const input = screen.getByPlaceholderText(/type command/i);
    await user.click(input);
    await user.type(input, 'theme');
    await user.keyboard('{ArrowDown}{Enter}');

    expect(defaultProps.onSelectTheme).toHaveBeenCalledTimes(1);
  });

  it('calls onAuthenticateProvider when clicking auth command', async () => {
    const user = userEvent.setup();
    render(<ShortcutGateway {...defaultProps} />);

    const authItem = screen.getByText('Authenticate Provider').closest('[cmdk-item]');
    expect(authItem).toBeTruthy();
    await user.click(authItem!);

    expect(defaultProps.onAuthenticateProvider).toHaveBeenCalledTimes(1);
  });

  it('renders "Update Provider Plugins" text when open', () => {
    render(<ShortcutGateway {...defaultProps} />);
    expect(screen.getByText('Update Provider Plugins')).toBeInTheDocument();
  });

  it('calls onUpdatePlugins when clicking update plugins command', async () => {
    const user = userEvent.setup();
    render(<ShortcutGateway {...defaultProps} />);

    const updateItem = screen.getByText('Update Provider Plugins').closest('[cmdk-item]');
    expect(updateItem).toBeTruthy();
    await user.click(updateItem!);

    expect(defaultProps.onUpdatePlugins).toHaveBeenCalledTimes(1);
  });

  it('shows updating state with spinner when isUpdatingPlugins is true', () => {
    render(<ShortcutGateway {...defaultProps} isUpdatingPlugins={true} />);
    const updateItem = screen.getByText('Update Provider Plugins').closest('[cmdk-item]');
    expect(updateItem).toBeTruthy();
    // Badge should show "Updating..." instead of "U"
    expect(updateItem).toHaveTextContent('Updating...');
  });
});
