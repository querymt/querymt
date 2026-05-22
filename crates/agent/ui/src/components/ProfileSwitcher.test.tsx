import { describe, it, expect, vi } from 'vitest';
import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { ProfileSwitcher } from './ProfileSwitcher';
import type { UiProfileInfo } from '../types';

const profiles: UiProfileInfo[] = [
  {
    id: 'default',
    name: 'Default',
    description: 'Default profile',
    tags: ['coding', 'planner'],
    config_kind: 'builtin',
    source: 'memory',
  },
  {
    id: 'review',
    name: 'Review',
    tags: [],
    config_kind: 'file',
    source: 'memory',
  },
];

describe('ProfileSwitcher', () => {
  it('renders an empty disabled state when no profiles are available', () => {
    render(
      <ProfileSwitcher
        profiles={[]}
        activeProfileId={null}
        connected
        onSelectProfile={vi.fn()}
      />,
    );

    expect(screen.getByLabelText('No profiles')).toHaveTextContent('No profiles');
  });

  it('renders backend profiles and selects the active profile', () => {
    render(
      <ProfileSwitcher
        profiles={profiles}
        activeProfileId="default"
        connected
        onSelectProfile={vi.fn()}
      />,
    );

    const select = screen.getByLabelText('Active profile') as HTMLSelectElement;
    expect(select.value).toBe('default');
    expect(screen.getByRole('option', { name: 'Default' })).toBeInTheDocument();
    expect(screen.getByRole('option', { name: 'Review' })).toBeInTheDocument();
    expect(select).toHaveAttribute('title', expect.stringContaining('Tags:'));
    expect(select).toHaveAttribute('title', expect.stringContaining('coding, planner'));
  });

  it('sends selected profile changes without optimistic state changes', async () => {
    const user = userEvent.setup();
    const onSelectProfile = vi.fn();

    render(
      <ProfileSwitcher
        profiles={profiles}
        activeProfileId="default"
        connected
        onSelectProfile={onSelectProfile}
      />,
    );

    await user.selectOptions(screen.getByLabelText('Active profile'), 'review');

    expect(onSelectProfile).toHaveBeenCalledWith('review');
  });

  it('shows the bound session profile when it differs from the active default', () => {
    render(
      <ProfileSwitcher
        profiles={profiles}
        activeProfileId="default"
        currentSessionProfileId="review"
        connected
        onSelectProfile={vi.fn()}
      />,
    );

    expect(screen.getByText('session: Review')).toBeInTheDocument();
    expect(screen.getByLabelText('Active profile')).toHaveAttribute(
      'title',
      expect.stringContaining('Existing session stays on its original profile.'),
    );
  });

  it('disables the selector when disconnected even with multiple profiles', () => {
    render(
      <ProfileSwitcher
        profiles={profiles}
        activeProfileId="default"
        connected={false}
        onSelectProfile={vi.fn()}
      />,
    );

    expect(screen.getByLabelText('Active profile')).toBeDisabled();
  });

  it('disables the selector when only one profile is available', () => {
    render(
      <ProfileSwitcher
        profiles={[profiles[0]]}
        activeProfileId="default"
        connected
        onSelectProfile={vi.fn()}
      />,
    );

    expect(screen.getByLabelText('Active profile')).toBeDisabled();
  });
});
