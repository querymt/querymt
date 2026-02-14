import { beforeEach, describe, expect, it, vi } from 'vitest';
import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { ShortcutGateway } from './ShortcutGateway';

describe('ShortcutGateway', () => {
  const defaultProps = {
    open: true,
    onOpenChange: vi.fn(),
    onStartNewSession: vi.fn(),
    onSelectTheme: vi.fn(),
  };

  beforeEach(() => {
    vi.clearAllMocks();
  });

  it('renders when open is true', () => {
    render(<ShortcutGateway {...defaultProps} />);
    expect(screen.getByText('Shortcut Gateway')).toBeInTheDocument();
    expect(screen.getByText('Start New Session')).toBeInTheDocument();
    expect(screen.getByText('Theme Selector')).toBeInTheDocument();
    expect(screen.getByPlaceholderText(/type command/i)).toBeInTheDocument();
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
});
