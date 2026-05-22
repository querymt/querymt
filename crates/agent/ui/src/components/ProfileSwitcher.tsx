import type { UiProfileInfo } from '../types';

interface ProfileSwitcherProps {
  profiles?: UiProfileInfo[];
  activeProfileId: string | null;
  currentSessionProfileId?: string;
  connected: boolean;
  onSelectProfile: (profileId: string) => void;
}

function profileLabel(profile: UiProfileInfo | undefined, fallback?: string | null): string {
  return profile?.name || fallback || 'Unknown profile';
}

function profileMeta(profile: UiProfileInfo | undefined): string {
  if (!profile) return 'Profile not in current list';
  return [profile.config_kind, profile.source].filter(Boolean).join(' / ');
}

function profileTags(profile: UiProfileInfo | undefined): string | undefined {
  if (!profile || profile.tags.length === 0) return undefined;
  return `Tags: ${profile.tags.join(', ')}`;
}

export function ProfileSwitcher({
  profiles = [],
  activeProfileId,
  currentSessionProfileId,
  connected,
  onSelectProfile,
}: ProfileSwitcherProps) {
  const activeProfile = profiles.find(profile => profile.id === activeProfileId);
  const currentSessionProfile = profiles.find(profile => profile.id === currentSessionProfileId);
  const sessionUsesDifferentProfile = Boolean(
    currentSessionProfileId && activeProfileId && currentSessionProfileId !== activeProfileId,
  );
  const disabled = !connected || profiles.length <= 1;
  const value = activeProfileId ?? '';

  if (profiles.length === 0) {
    return (
      <div
        className="h-8 flex items-center rounded-full border border-surface-border/50 px-3 text-xs text-ui-muted"
        title="No profiles are available from the backend"
        aria-label="No profiles"
      >
        No profiles
      </div>
    );
  }

  const titleParts = [
    `Default profile: ${profileLabel(activeProfile, activeProfileId)}`,
    activeProfile?.description,
    profileTags(activeProfile),
    profileMeta(activeProfile),
    currentSessionProfileId
      ? `Current session: ${profileLabel(currentSessionProfile, currentSessionProfileId)}`
      : 'Current session: active default',
    sessionUsesDifferentProfile ? 'Existing session stays on its original profile.' : undefined,
  ].filter(Boolean);

  return (
    <div className="hidden lg:flex h-8 items-center gap-1 rounded-full border border-surface-border/50 bg-surface-elevated/40 px-2 text-xs text-ui-secondary">
      <span className="text-ui-muted">Profile</span>
      <select
        aria-label="Active profile"
        className="max-w-40 bg-transparent text-ui-primary outline-none disabled:text-ui-muted"
        disabled={disabled}
        value={value}
        title={titleParts.join('\n')}
        onChange={(event) => onSelectProfile(event.target.value)}
      >
        {!activeProfileId && <option value="">Select profile</option>}
        {profiles.map(profile => (
          <option key={profile.id} value={profile.id}>
            {profile.name || profile.id}
          </option>
        ))}
      </select>
      {sessionUsesDifferentProfile && (
        <span
          className="max-w-36 truncate text-ui-muted"
          title={`Current session: ${profileLabel(currentSessionProfile, currentSessionProfileId)}`}
        >
          session: {profileLabel(currentSessionProfile, currentSessionProfileId)}
        </span>
      )}
    </div>
  );
}
