import { beforeEach, describe, expect, it, vi } from 'vitest';
import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { ThemeSwitcher } from './ThemeSwitcher';
import { getDashboardThemes } from '../utils/dashboardThemes';

describe('ThemeSwitcher', () => {
  const defaultProps = {
    open: true,
    onOpenChange: vi.fn(),
    themes: getDashboardThemes(),
    selectedTheme: 'base16-ocean' as const,
    onSelectTheme: vi.fn(),
  };

  beforeEach(() => {
    vi.clearAllMocks();
  });

  it('renders when open is true', () => {
    render(<ThemeSwitcher {...defaultProps} />);
    expect(screen.getByPlaceholderText(/search dashboard themes/i)).toBeInTheDocument();
  });

  it('does not render when open is false', () => {
    render(<ThemeSwitcher {...defaultProps} open={false} />);
    expect(screen.queryByPlaceholderText(/search dashboard themes/i)).not.toBeInTheDocument();
  });

  it('selects a theme on click and closes', async () => {
    const user = userEvent.setup();
    render(<ThemeSwitcher {...defaultProps} />);

    const themeItem = screen.getByText('Kanagawa').closest('[cmdk-item]');
    expect(themeItem).toBeTruthy();
    await user.click(themeItem!);

    expect(defaultProps.onSelectTheme).toHaveBeenCalledWith('base16-kanagawa');
    expect(defaultProps.onOpenChange).toHaveBeenCalledWith(false);
  });

  it('selects a filtered theme when pressing enter', async () => {
    const user = userEvent.setup();
    render(<ThemeSwitcher {...defaultProps} />);

    const input = screen.getByPlaceholderText(/search dashboard themes/i);
    await user.type(input, 'kanagawa dragon');
    await user.keyboard('{Enter}');

    expect(defaultProps.onSelectTheme).toHaveBeenCalledWith('base16-kanagawa-dragon');
    expect(defaultProps.onOpenChange).toHaveBeenCalledWith(false);
  });

  it('closes when clicking the backdrop', async () => {
    const user = userEvent.setup();
    render(<ThemeSwitcher {...defaultProps} />);

    const backdrop = screen.getByTestId('theme-switcher-backdrop');
    await user.click(backdrop);

    expect(defaultProps.onOpenChange).toHaveBeenCalledWith(false);
  });
});
