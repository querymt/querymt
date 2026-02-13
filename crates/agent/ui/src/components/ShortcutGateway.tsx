import { useEffect, useRef, useState } from 'react';
import { Command } from 'cmdk';
import { Keyboard, Palette } from 'lucide-react';
import { useUiStore } from '../store/uiStore';

interface ShortcutGatewayProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  onSelectTheme: () => void;
}

export function ShortcutGateway({ open, onOpenChange, onSelectTheme }: ShortcutGatewayProps) {
  const [search, setSearch] = useState('');
  const inputRef = useRef<HTMLInputElement>(null);
  const { focusMainInput } = useUiStore();

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
        className="fixed inset-0 bg-cyber-bg/65 backdrop-blur-sm z-40 animate-fade-in"
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
          className="w-full max-w-lg bg-cyber-surface border-2 border-cyber-cyan/30 rounded-xl shadow-[0_0_40px_rgba(var(--cyber-cyan-rgb),0.22)] overflow-hidden animate-scale-in"
        >
          <div className="flex items-center justify-between gap-3 px-4 py-3 border-b border-cyber-border/60">
            <div className="flex items-center gap-2 text-cyber-cyan">
              <Keyboard className="w-4 h-4" />
              <span className="text-sm font-medium">Shortcut Gateway</span>
            </div>
            <kbd className="px-2 py-1 text-[10px] font-mono bg-cyber-bg border border-cyber-border rounded text-ui-muted">
              ESC
            </kbd>
          </div>

          <div className="flex items-center gap-2 px-4 py-2.5 border-b border-cyber-border/40">
            <span className="text-xs text-ui-muted font-mono">Ctrl+X</span>
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

            <Command.Group heading="Available Commands" className="mb-1">
              <Command.Item
                value="theme selector"
                keywords={['theme', 'themes', 't', 'palette']}
                onSelect={() => onSelectTheme()}
                className="flex items-center gap-3 px-3 py-2.5 rounded-lg border border-cyber-border/20 cursor-pointer transition-colors data-[selected=true]:bg-cyber-cyan/15 data-[selected=true]:border-cyber-cyan/35 hover:bg-cyber-surface/60 hover:border-cyber-border/40"
              >
                <div className="w-7 h-7 rounded-md border border-cyber-cyan/35 bg-cyber-cyan/10 flex items-center justify-center">
                  <Palette className="w-3.5 h-3.5 text-cyber-cyan" />
                </div>
                <div className="flex-1 min-w-0">
                  <div className="text-sm text-ui-primary">Theme Selector</div>
                  <div className="text-xs text-ui-muted">Open searchable theme switcher</div>
                </div>
                <kbd className="px-2 py-1 text-[10px] font-mono bg-cyber-bg border border-cyber-border rounded text-ui-secondary">
                  T
                </kbd>
              </Command.Item>
            </Command.Group>
          </Command.List>
        </Command>
      </div>
    </>
  );
}
