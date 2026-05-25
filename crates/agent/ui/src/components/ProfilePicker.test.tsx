import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { ProfilePicker } from './ProfilePicker';
import { useUiStore } from '../store/uiStore';
import type { UiProfileInfo } from '../types';

const profiles: UiProfileInfo[] = [
  { id: 'default', name: 'Default', description: 'General work', tags: ['core'], source: 'user' },
  { id: 'research', name: 'Research', description: 'Deep dives', tags: ['long'], source: 'user' },
];

describe('ProfilePicker', () => {
  const defaultProps = {
    open: true,
    onOpenChange: vi.fn(),
    profiles,
    activeProfileId: 'default',
    currentSessionProfileId: undefined,
    connected: true,
    onSelectProfile: vi.fn(),
  };

  beforeEach(() => {
    vi.clearAllMocks();
    useUiStore.setState({ focusMainInput: vi.fn() });
  });

  it('renders profiles when open', () => {
    render(<ProfilePicker {...defaultProps} />);

    expect(screen.getByPlaceholderText(/search profiles/i)).toBeInTheDocument();
    expect(screen.getByText('Default')).toBeInTheDocument();
    expect(screen.getByText('Research')).toBeInTheDocument();
    expect(screen.getByText(/General work/)).toBeInTheDocument();
  });

  it('does not render when closed', () => {
    render(<ProfilePicker {...defaultProps} open={false} />);

    expect(screen.queryByPlaceholderText(/search profiles/i)).not.toBeInTheDocument();
  });

  it('marks active and session profiles and shows session note', () => {
    render(
      <ProfilePicker
        {...defaultProps}
        activeProfileId="default"
        currentSessionProfileId="research"
      />,
    );

    expect(screen.getByText(/Existing session stays on Research/i)).toBeInTheDocument();
    const defaultItem = screen.getByText('Default').closest('[cmdk-item]');
    const researchItem = screen.getByText('Research').closest('[cmdk-item]');
    expect(defaultItem).toHaveTextContent('Active');
    expect(researchItem).toHaveTextContent('Session');
    expect(researchItem).toHaveTextContent('Session profile / Deep dives');
  });

  it('selects a profile and closes', async () => {
    const user = userEvent.setup();
    const onSelectProfile = vi.fn();
    const onOpenChange = vi.fn();
    render(
      <ProfilePicker
        {...defaultProps}
        onSelectProfile={onSelectProfile}
        onOpenChange={onOpenChange}
      />,
    );

    const researchItem = screen.getByText('Research').closest('[cmdk-item]');
    expect(researchItem).toBeTruthy();
    await user.click(researchItem!);

    expect(onSelectProfile).toHaveBeenCalledWith('research');
    expect(onOpenChange).toHaveBeenCalledWith(false);
  });

  it('disables selection when disconnected', async () => {
    const user = userEvent.setup();
    const onSelectProfile = vi.fn();
    render(<ProfilePicker {...defaultProps} connected={false} onSelectProfile={onSelectProfile} />);

    expect(screen.getByText('Reconnect to switch active profile.')).toBeInTheDocument();
    const researchItem = screen.getByText('Research').closest('[cmdk-item]');
    expect(researchItem).toHaveAttribute('aria-disabled', 'true');
    await user.click(researchItem!);

    expect(onSelectProfile).not.toHaveBeenCalled();
  });
});
