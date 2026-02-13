import { useEffect, useRef, useState } from 'react';
import { Command } from 'cmdk';
import { Check, Palette } from 'lucide-react';
import { useUiStore } from '../store/uiStore';
import type { DashboardTheme, DashboardThemeId } from '../utils/dashboardThemes';

interface ThemeSwitcherProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  themes: DashboardTheme[];
  selectedTheme: DashboardThemeId;
  onSelectTheme: (themeId: DashboardThemeId) => void;
}

export function ThemeSwitcher({
  open,
  onOpenChange,
  themes,
  selectedTheme,
  onSelectTheme,
}: ThemeSwitcherProps) {
  const [search, setSearch] = useState('');
  const inputRef = useRef<HTMLInputElement>(null);
  const { focusMainInput } = useUiStore();

  const close = () => {
    onOpenChange(false);
    focusMainInput();
  };

  const handleSelectTheme = (themeId: DashboardThemeId) => {
    onSelectTheme(themeId);
    close();
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
        data-testid="theme-switcher-backdrop"
        className="fixed inset-0 bg-cyber-bg/70 backdrop-blur-sm z-40 animate-fade-in"
        onClick={close}
      />

      <div
        data-testid="theme-switcher-container"
        className="fixed inset-0 z-50 flex items-start justify-center pt-[15vh] px-4"
        onClick={(e) => {
          if (e.target === e.currentTarget) {
            close();
          }
        }}
      >
        <Command
          label="Theme switcher"
          className="w-full max-w-xl bg-cyber-surface border-2 border-cyber-cyan/30 rounded-xl shadow-[0_0_40px_rgba(var(--cyber-cyan-rgb),0.25)] overflow-hidden animate-scale-in"
        >
          <div className="flex items-center gap-3 px-4 py-3 border-b border-cyber-border/60">
            <Palette className="w-4 h-4 text-cyber-cyan" />
            <Command.Input
              ref={inputRef}
              value={search}
              onValueChange={setSearch}
              placeholder={`Search dashboard themes (${themes.length})...`}
              className="flex-1 bg-transparent text-ui-primary placeholder:text-ui-muted text-sm focus:outline-none"
            />
            <kbd className="hidden sm:inline-block px-2 py-1 text-[10px] font-mono bg-cyber-bg border border-cyber-border rounded text-ui-muted">
              ESC
            </kbd>
          </div>

          <Command.List className="max-h-[400px] overflow-y-auto p-2 custom-scrollbar">
            <Command.Empty className="px-4 py-8 text-center text-sm text-ui-muted">
              No themes found
            </Command.Empty>

            <Command.Group className="mb-1">
              {themes.map((theme) => (
                <Command.Item
                  key={theme.id}
                  value={`${theme.label} ${theme.variant}`}
                  keywords={[theme.id, theme.description, theme.variant]}
                  onSelect={() => handleSelectTheme(theme.id)}
                  className="flex items-start gap-3 px-3 py-2.5 rounded-lg border border-cyber-border/20 cursor-pointer transition-colors data-[selected=true]:bg-cyber-cyan/15 data-[selected=true]:border-cyber-cyan/35 hover:bg-cyber-surface/60 hover:border-cyber-border/40"
                >
                  <div className="flex-1 min-w-0">
                    <div className="text-sm text-ui-primary truncate">{theme.label}</div>
                    <div className="text-xs text-ui-muted truncate">{theme.description}</div>
                  </div>
                  <div className="flex items-center gap-2 flex-shrink-0">
                    <span
                      className={`inline-flex items-center rounded border px-1.5 py-0.5 text-[9px] font-semibold uppercase tracking-wider ${
                        theme.variant === 'light'
                          ? 'border-cyber-orange/45 bg-cyber-orange/10 text-cyber-orange'
                          : 'border-cyber-cyan/45 bg-cyber-cyan/10 text-cyber-cyan'
                      }`}
                    >
                      {theme.variant}
                    </span>
                    <span className="text-[10px] text-ui-muted font-mono">{theme.id}</span>
                    {selectedTheme === theme.id && (
                      <span className="inline-flex items-center justify-center w-5 h-5 rounded border border-cyber-cyan/35 bg-cyber-cyan/10">
                        <Check className="w-3 h-3 text-cyber-cyan" />
                      </span>
                    )}
                  </div>
                </Command.Item>
              ))}
            </Command.Group>
          </Command.List>
        </Command>
      </div>
    </>
  );
}
