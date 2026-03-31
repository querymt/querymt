/**
 * VoiceModelPicker -- nested cmdk sub-view for selecting STT/TTS models.
 *
 * Opened from the ShortcutGateway when the user selects "STT Model" or
 * "TTS Model".  Displays available audio models and highlights the
 * currently selected one.
 */

import { useEffect, useRef, useState } from 'react';
import { Command } from 'cmdk';
import { Check, Mic, Volume2, X } from 'lucide-react';

interface AudioModelEntry {
  provider: string;
  model: string;
}

interface VoiceModelPickerProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  mode: 'stt' | 'tts';
  models: AudioModelEntry[];
  selectedProvider: string;
  selectedModel: string;
  onSelect: (provider: string, model: string) => void;
}

export function VoiceModelPicker({
  open,
  onOpenChange,
  mode,
  models,
  selectedProvider,
  selectedModel,
  onSelect,
}: VoiceModelPickerProps) {
  const inputRef = useRef<HTMLInputElement>(null);
  const [search, setSearch] = useState('');

  useEffect(() => {
    if (open) {
      setSearch('');
      setTimeout(() => inputRef.current?.focus(), 0);
    }
  }, [open]);

  if (!open) return null;

  const title = mode === 'stt' ? 'STT Model' : 'TTS Model';
  const Icon = mode === 'stt' ? Mic : Volume2;

  const filtered = search.trim()
    ? models.filter(
        (m) =>
          m.model.toLowerCase().includes(search.toLowerCase()) ||
          m.provider.toLowerCase().includes(search.toLowerCase())
      )
    : models;

  return (
    <>
      <div
        className="fixed inset-0 bg-surface-canvas/80 z-40 animate-fade-in"
        onClick={() => onOpenChange(false)}
      />

      <div
        className="fixed inset-0 z-50 flex items-start justify-center pt-[18vh] px-4"
        onClick={(e) => {
          if (e.target === e.currentTarget) onOpenChange(false);
        }}
      >
        <Command
          label={`Select ${title}`}
          className="w-full max-w-lg bg-surface-elevated border-2 border-accent-primary/30 rounded-xl shadow-[0_0_40px_rgba(var(--accent-primary-rgb),0.22)] overflow-hidden animate-scale-in"
        >
          <div className="flex items-center justify-between gap-3 px-4 py-3 border-b border-surface-border/60">
            <div className="flex items-center gap-2 text-accent-primary">
              <Icon className="w-4 h-4" />
              <span className="text-sm font-medium">Select {title}</span>
            </div>
            <div className="flex items-center gap-2">
              <button
                type="button"
                onClick={() => onOpenChange(false)}
                className="sm:hidden p-1.5 rounded hover:bg-surface-canvas transition-colors text-ui-secondary hover:text-ui-primary"
                aria-label="Close"
              >
                <X className="w-5 h-5" />
              </button>
              <kbd className="hidden sm:inline-block px-2 py-1 text-[10px] font-mono bg-surface-canvas border border-surface-border rounded text-ui-muted">
                ESC
              </kbd>
            </div>
          </div>

          <div className="flex items-center gap-2 px-4 py-2.5 border-b border-surface-border/40">
            <Command.Input
              ref={inputRef}
              value={search}
              onValueChange={setSearch}
              placeholder={`Search ${mode.toUpperCase()} models...`}
              className="flex-1 bg-transparent text-ui-primary placeholder:text-ui-muted text-sm focus:outline-none"
            />
          </div>

          <Command.List className="max-h-[320px] overflow-y-auto p-2 custom-scrollbar">
            <Command.Empty className="px-4 py-6 text-sm text-center text-ui-muted">
              No models found
            </Command.Empty>

            <Command.Group className="mb-1">
              {filtered.map((entry) => {
                const isSelected =
                  entry.provider === selectedProvider && entry.model === selectedModel;

                return (
                  <Command.Item
                    key={`${entry.provider}/${entry.model}`}
                    value={`${entry.provider}/${entry.model}`}
                    onSelect={() => {
                      onSelect(entry.provider, entry.model);
                      onOpenChange(false);
                    }}
                    className="flex items-center gap-3 px-3 py-2.5 rounded-lg border border-surface-border/20 cursor-pointer transition-colors data-[selected=true]:bg-accent-primary/15 data-[selected=true]:border-accent-primary/35 hover:bg-surface-elevated/60 hover:border-surface-border/40"
                  >
                    <div
                      className={`w-7 h-7 rounded-md border flex items-center justify-center ${
                        isSelected
                          ? 'border-accent-primary/50 bg-accent-primary/15'
                          : 'border-surface-border/40 bg-surface-canvas/40'
                      }`}
                    >
                      {isSelected ? (
                        <Check className="w-3.5 h-3.5 text-accent-primary" />
                      ) : (
                        <Icon className="w-3.5 h-3.5 text-ui-muted" />
                      )}
                    </div>
                    <div className="flex-1 min-w-0">
                      <div className="text-sm text-ui-primary truncate">
                        {entry.model}
                      </div>
                      <div className="text-xs text-ui-muted">
                        {entry.provider}
                      </div>
                    </div>
                    {isSelected && (
                      <span className="text-[10px] font-mono text-accent-primary px-1.5 py-0.5 rounded bg-accent-primary/10">
                        active
                      </span>
                    )}
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
