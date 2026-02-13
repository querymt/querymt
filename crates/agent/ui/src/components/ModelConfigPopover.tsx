/**
 * Popover showing LLM config details (provider, model, parameters)
 * Uses Radix Popover for positioning, outside-click dismiss, and Escape handling.
 */

import { useEffect, useState } from 'react';
import * as Popover from '@radix-ui/react-popover';
import { X, Loader, Cpu } from 'lucide-react';
import type { LlmConfigDetails } from '../types';

interface ModelConfigPopoverProps {
  configId: number;
  open: boolean;
  onOpenChange: (open: boolean) => void;
  requestConfig: (configId: number, callback: (config: LlmConfigDetails) => void) => void;
  cachedConfig?: LlmConfigDetails;
  children: React.ReactNode; // Trigger element
}

export function ModelConfigPopover({
  configId,
  open,
  onOpenChange,
  requestConfig,
  cachedConfig,
  children,
}: ModelConfigPopoverProps) {
  const [config, setConfig] = useState<LlmConfigDetails | null>(cachedConfig || null);
  const [loading, setLoading] = useState(!cachedConfig);

  // Fetch config if not cached
  useEffect(() => {
    if (cachedConfig) {
      setConfig(cachedConfig);
      setLoading(false);
      return;
    }

    if (open) {
      setLoading(true);
      requestConfig(configId, (fetchedConfig) => {
        setConfig(fetchedConfig);
        setLoading(false);
      });
    }
  }, [configId, cachedConfig, requestConfig, open]);

  // Format params for display
  const formatParamValue = (value: unknown): string => {
    if (value === null || value === undefined) return 'null';
    if (typeof value === 'object') return JSON.stringify(value, null, 2);
    return String(value);
  };

  return (
    <Popover.Root open={open} onOpenChange={onOpenChange}>
      <Popover.Trigger asChild>
        {children}
      </Popover.Trigger>

      <Popover.Portal>
        <Popover.Content
          align="end"
          sideOffset={8}
          className="z-50 w-96 rounded-lg border border-accent-primary/30 bg-surface-canvas shadow-lg shadow-accent-primary/25 animate-fade-in"
          onOpenAutoFocus={(e) => e.preventDefault()}
        >
          {/* Header */}
          <div className="flex items-center justify-between px-3 py-2 border-b border-surface-border/40">
            <div className="flex items-center gap-2">
              <Cpu className="w-3.5 h-3.5 text-accent-primary" />
              <span className="text-xs font-semibold text-ui-secondary uppercase tracking-wider">
                Model Config
              </span>
            </div>
            <Popover.Close className="p-1 rounded text-ui-secondary hover:text-ui-primary hover:bg-surface-elevated/60 transition-colors">
              <X className="h-3.5 w-3.5" />
            </Popover.Close>
          </div>

          {/* Content */}
          <div className="px-3 py-2">
            {loading ? (
              <div className="flex items-center justify-center py-4">
                <Loader className="w-5 h-5 animate-spin text-accent-primary" />
              </div>
            ) : config ? (
              <div className="space-y-2">
                {/* Provider & Model */}
                <div className="space-y-1">
                  <div className="flex items-center justify-between gap-4">
                    <span className="text-[10px] uppercase tracking-widest text-ui-muted flex-shrink-0">Provider</span>
                    <span className="text-xs text-ui-primary font-mono">{config.provider}</span>
                  </div>
                  <div className="flex items-center justify-between gap-4">
                    <span className="text-[10px] uppercase tracking-widest text-ui-muted flex-shrink-0">Model</span>
                    <span className="text-xs text-ui-primary font-mono truncate" title={config.model}>
                      {config.model}
                    </span>
                  </div>
                </div>

                {/* Parameters */}
                {config.params && Object.keys(config.params).length > 0 && (
                  <div className="pt-2 border-t border-surface-border/30">
                    <span className="text-[10px] uppercase tracking-widest text-ui-muted block mb-1">
                      Parameters
                    </span>
                    <div className="space-y-1 max-h-40 overflow-y-auto">
                      {Object.entries(config.params).map(([key, value]) => (
                        <div key={key} className="flex items-start justify-between gap-2">
                          <span className="text-[10px] text-ui-secondary font-mono flex-shrink-0">{key}</span>
                          <span className="text-[10px] text-ui-primary font-mono text-right break-all">
                            {formatParamValue(value)}
                          </span>
                        </div>
                      ))}
                    </div>
                  </div>
                )}

                {/* No params message */}
                {(!config.params || Object.keys(config.params).length === 0) && (
                  <div className="pt-2 border-t border-surface-border/30">
                    <span className="text-[10px] text-ui-muted italic">
                      Default parameters
                    </span>
                  </div>
                )}
              </div>
            ) : (
              <div className="py-4 text-center text-xs text-ui-muted">
                Config not found
              </div>
            )}
          </div>
        </Popover.Content>
      </Popover.Portal>
    </Popover.Root>
  );
}
