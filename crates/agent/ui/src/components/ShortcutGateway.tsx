import { useEffect, useRef, useState } from 'react';
import { Command } from 'cmdk';
import { Keyboard, KeyRound, MessageSquare, Palette, Plus } from 'lucide-react';
import { useUiStore } from '../store/uiStore';

interface ShortcutGatewayProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  onStartNewSession: () => void;
  onSelectTheme: () => void;
  onAuthenticateProvider: () => void;
}

export function ShortcutGateway({
  open,
  onOpenChange,
  onStartNewSession,
  onSelectTheme,
  onAuthenticateProvider,
}: ShortcutGatewayProps) {
  const [search, setSearch] = useState('');
  const inputRef = useRef<HTMLInputElement>(null);
  const { focusMainInput, followNewMessages, setFollowNewMessages } = useUiStore();
  const shortcutGatewayPrefix = navigator.platform.includes('Mac') ? '⌘+X' : 'Ctrl+X';

  const close = () => {
    onOpenChange(false);
    focusMainInput();
  };

  useEffect(() => {
    if (!open) {
      return;
    }
    setSearch('');
    window.setTimeout(() => inputRef.current?.focus(), 0);
  }, [open]);

  if (!open) {
    return null;
  }

  return (
    <>
      <div
        data-testid="shortcut-gateway-backdrop"
        className="fixed inset-0 bg-surface-canvas/65 backdrop-blur-sm z-40 animate-fade-in"
        onClick={close}
      />

      <div
        data-testid="shortcut-gateway-container"
        className="fixed inset-0 z-50 flex items-start justify-center pt-[18vh] px-4"
        onClick={(e) => {
          if (e.target === e.currentTarget) {
            close();
          }
        }}
      >
        <Command
          label="Shortcut gateway"
          className="w-full max-w-lg bg-surface-elevated border-2 border-accent-primary/30 rounded-xl shadow-[0_0_40px_rgba(var(--accent-primary-rgb),0.22)] overflow-hidden animate-scale-in"
        >
          <div className="flex items-center justify-between gap-3 px-4 py-3 border-b border-surface-border/60">
            <div className="flex items-center gap-2 text-accent-primary">
              <Keyboard className="w-4 h-4" />
              <span className="text-sm font-medium">Shortcut Gateway</span>
            </div>
            <kbd className="px-2 py-1 text-[10px] font-mono bg-surface-canvas border border-surface-border rounded text-ui-muted">
              ESC
            </kbd>
          </div>

          <div className="flex items-center gap-2 px-4 py-2.5 border-b border-surface-border/40">
            <span className="text-xs text-ui-muted font-mono">{shortcutGatewayPrefix}</span>
            <Command.Input
              ref={inputRef}
              value={search}
              onValueChange={setSearch}
              placeholder="Type command or use Up/Down then Enter..."
              className="flex-1 bg-transparent text-ui-primary placeholder:text-ui-muted text-sm focus:outline-none"
            />
          </div>

          <Command.List className="max-h-[280px] overflow-y-auto p-2 custom-scrollbar">
            <Command.Empty className="px-4 py-6 text-sm text-center text-ui-muted">
              No shortcut commands found
            </Command.Empty>

            <Command.Group className="mb-1">
              <Command.Item
                value="start new session"
                keywords={['new', 'session', 'start', 'n']}
                onSelect={() => onStartNewSession()}
                className="flex items-center gap-3 px-3 py-2.5 rounded-lg border border-surface-border/20 cursor-pointer transition-colors data-[selected=true]:bg-accent-primary/15 data-[selected=true]:border-accent-primary/35 hover:bg-surface-elevated/60 hover:border-surface-border/40"
              >
                <div className="w-7 h-7 rounded-md border border-accent-primary/35 bg-accent-primary/10 flex items-center justify-center">
                  <Plus className="w-3.5 h-3.5 text-accent-primary" />
                </div>
                <div className="flex-1 min-w-0">
                  <div className="text-sm text-ui-primary">Start New Session</div>
                  <div className="text-xs text-ui-muted">Create a fresh session and jump in</div>
                </div>
                <kbd className="px-2 py-1 text-[10px] font-mono bg-surface-canvas border border-surface-border rounded text-ui-secondary">
                  N
                </kbd>
              </Command.Item>

              <Command.Item
                value="follow new messages"
                keywords={['follow', 'messages', 'scroll', 'auto']}
                onSelect={() => setFollowNewMessages(!followNewMessages)}
                className="flex items-center gap-3 px-3 py-2.5 rounded-lg border border-surface-border/20 cursor-pointer transition-colors data-[selected=true]:bg-accent-primary/15 data-[selected=true]:border-accent-primary/35 hover:bg-surface-elevated/60 hover:border-surface-border/40"
              >
                <div className="w-7 h-7 rounded-md border border-accent-primary/35 bg-accent-primary/10 flex items-center justify-center">
                  <MessageSquare className="w-3.5 h-3.5 text-accent-primary" />
                </div>
                <div className="flex-1 min-w-0">
                  <div className="text-sm text-ui-primary">Follow New Messages</div>
                  <div className="text-xs text-ui-muted">
                    {followNewMessages ? 'Auto-scroll to newest message is enabled' : 'Auto-scroll is paused'}
                  </div>
                </div>
                <kbd className="px-2 py-1 text-[10px] font-mono bg-surface-canvas border border-surface-border rounded text-ui-secondary">
                  {followNewMessages ? 'On' : 'Off'}
                </kbd>
              </Command.Item>

              <Command.Item
                value="theme selector"
                keywords={['theme', 'themes', 't', 'palette']}
                onSelect={() => onSelectTheme()}
                className="flex items-center gap-3 px-3 py-2.5 rounded-lg border border-surface-border/20 cursor-pointer transition-colors data-[selected=true]:bg-accent-primary/15 data-[selected=true]:border-accent-primary/35 hover:bg-surface-elevated/60 hover:border-surface-border/40"
              >
                <div className="w-7 h-7 rounded-md border border-accent-primary/35 bg-accent-primary/10 flex items-center justify-center">
                  <Palette className="w-3.5 h-3.5 text-accent-primary" />
                </div>
                <div className="flex-1 min-w-0">
                  <div className="text-sm text-ui-primary">Theme Selector</div>
                  <div className="text-xs text-ui-muted">Open searchable theme switcher</div>
                </div>
                <kbd className="px-2 py-1 text-[10px] font-mono bg-surface-canvas border border-surface-border rounded text-ui-secondary">
                  T
                </kbd>
              </Command.Item>

              <Command.Item
                value="authenticate provider"
                keywords={['auth', 'oauth', 'provider', 'login', 'a']}
                onSelect={() => onAuthenticateProvider()}
                className="flex items-center gap-3 px-3 py-2.5 rounded-lg border border-surface-border/20 cursor-pointer transition-colors data-[selected=true]:bg-accent-primary/15 data-[selected=true]:border-accent-primary/35 hover:bg-surface-elevated/60 hover:border-surface-border/40"
              >
                <div className="w-7 h-7 rounded-md border border-accent-primary/35 bg-accent-primary/10 flex items-center justify-center">
                  <KeyRound className="w-3.5 h-3.5 text-accent-primary" />
                </div>
                <div className="flex-1 min-w-0">
                  <div className="text-sm text-ui-primary">Authenticate Provider</div>
                  <div className="text-xs text-ui-muted">Sign in with OAuth and unlock provider models</div>
                </div>
                <kbd className="px-2 py-1 text-[10px] font-mono bg-surface-canvas border border-surface-border rounded text-ui-secondary">
                  A
                </kbd>
              </Command.Item>
            </Command.Group>
          </Command.List>
        </Command>
      </div>
    </>
  );
}
