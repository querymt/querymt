import { useEffect, useRef, useState } from 'react';
import { Command } from 'cmdk';
import { Check, User, X } from 'lucide-react';
import { useUiStore } from '../store/uiStore';
import type { UiProfileInfo } from '../types';

interface ProfilePickerProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  profiles: UiProfileInfo[];
  activeProfileId: string | null;
  currentSessionProfileId?: string;
  connected: boolean;
  onSelectProfile: (profileId: string) => void;
}

function profileDisplayName(profile: UiProfileInfo | undefined, fallback?: string | null): string {
  return profile?.name || fallback || 'Unknown profile';
}

function profileSubtitle(profile: UiProfileInfo): string {
  const parts = [
    profile.description,
    profile.tags.length > 0 ? `Tags: ${profile.tags.join(', ')}` : undefined,
    profile.config_kind,
    profile.source,
  ].filter(Boolean);
  return parts.join(' / ') || profile.id;
}

export function ProfilePicker({
  open,
  onOpenChange,
  profiles,
  activeProfileId,
  currentSessionProfileId,
  connected,
  onSelectProfile,
}: ProfilePickerProps) {
  const [search, setSearch] = useState('');
  const inputRef = useRef<HTMLInputElement>(null);
  const { focusMainInput } = useUiStore();
  const currentSessionProfile = profiles.find(profile => profile.id === currentSessionProfileId);
  const sessionUsesDifferentProfile = Boolean(
    currentSessionProfileId && activeProfileId && currentSessionProfileId !== activeProfileId,
  );

  const close = () => {
    onOpenChange(false);
    focusMainInput();
  };

  const handleSelectProfile = (profileId: string) => {
    if (!connected) return;
    onSelectProfile(profileId);
    close();
  };

  useEffect(() => {
    if (!open) return;
    setSearch('');
    window.setTimeout(() => inputRef.current?.focus(), 0);
  }, [open]);

  if (!open) return null;

  return (
    <>
      <div
        data-testid="profile-picker-backdrop"
        className="fixed inset-0 bg-surface-canvas/80 z-40 animate-fade-in"
        onClick={close}
      />

      <div
        data-testid="profile-picker-container"
        className="fixed inset-0 z-50 flex items-start justify-center pt-[15vh] px-4"
        onClick={(e) => {
          if (e.target === e.currentTarget) close();
        }}
      >
        <Command
          label="Profile picker"
          className="w-full max-w-xl bg-surface-elevated border-2 border-accent-primary/30 rounded-xl shadow-[0_0_40px_rgba(var(--accent-primary-rgb),0.25)] overflow-hidden animate-scale-in"
        >
          <div className="flex items-center gap-3 px-4 py-3 border-b border-surface-border/60">
            <User className="w-4 h-4 text-accent-primary" />
            <Command.Input
              ref={inputRef}
              value={search}
              onValueChange={setSearch}
              placeholder={`Search profiles (${profiles.length})...`}
              className="flex-1 bg-transparent text-ui-primary placeholder:text-ui-muted text-sm focus:outline-none"
            />
            <button
              type="button"
              onClick={close}
              className="sm:hidden p-1.5 rounded hover:bg-surface-canvas transition-colors text-ui-secondary hover:text-ui-primary"
              aria-label="Close"
            >
              <X className="w-5 h-5" />
            </button>
            <kbd className="hidden sm:inline-block px-2 py-1 text-[10px] font-mono bg-surface-canvas border border-surface-border rounded text-ui-muted">
              ESC
            </kbd>
          </div>

          {(sessionUsesDifferentProfile || !connected) && (
            <div className="px-4 py-2 border-b border-surface-border/40 text-xs text-ui-muted">
              {sessionUsesDifferentProfile && (
                <div>
                  Existing session stays on {profileDisplayName(currentSessionProfile, currentSessionProfileId)}; active profile controls new sessions.
                </div>
              )}
              {!connected && <div>Reconnect to switch active profile.</div>}
            </div>
          )}

          <Command.List className="max-h-[400px] overflow-y-auto p-2 custom-scrollbar">
            <Command.Empty className="px-4 py-8 text-center text-sm text-ui-muted">
              No profiles found
            </Command.Empty>

            <Command.Group className="mb-1">
              {profiles.map(profile => {
                const isActive = profile.id === activeProfileId;
                const isSession = profile.id === currentSessionProfileId;

                return (
                  <Command.Item
                    key={profile.id}
                    value={`${profile.name} ${profile.id}`}
                    keywords={[profile.id, profile.name, profile.description ?? '', profile.source, profile.config_kind ?? '', ...profile.tags].filter(Boolean)}
                    disabled={!connected}
                    onSelect={() => handleSelectProfile(profile.id)}
                    className="flex items-center gap-3 px-3 py-2.5 rounded-lg border border-surface-border/20 cursor-pointer transition-colors data-[disabled=true]:cursor-not-allowed data-[disabled=true]:opacity-50 data-[selected=true]:bg-accent-primary/15 data-[selected=true]:border-accent-primary/35 hover:bg-surface-elevated/60 hover:border-surface-border/40"
                  >
                    <div className="w-7 h-7 rounded-md border border-accent-primary/35 bg-accent-primary/10 flex items-center justify-center">
                      <User className="w-3.5 h-3.5 text-accent-primary" />
                    </div>
                    <div className="flex-1 min-w-0">
                      <div className="text-sm text-ui-primary truncate">{profileDisplayName(profile)}</div>
                      <div className="text-xs text-ui-muted truncate">
                        {isSession ? `Session profile / ${profileSubtitle(profile)}` : profileSubtitle(profile)}
                      </div>
                    </div>
                    <div className="flex items-center gap-1 flex-shrink-0">
                      {isActive && (
                        <span className="px-2 py-1 text-[10px] font-mono bg-surface-canvas border border-surface-border rounded text-ui-secondary">
                          Active
                        </span>
                      )}
                      {isSession && (
                        <span className="px-2 py-1 text-[10px] font-mono bg-surface-canvas border border-surface-border rounded text-ui-secondary">
                          Session
                        </span>
                      )}
                      {isActive && <Check className="w-3.5 h-3.5 text-accent-primary" />}
                    </div>
                  </Command.Item>
                );
              })}
            </Command.Group>
          </Command.List>
        </Command>
      </div>
    </>
  );
}
