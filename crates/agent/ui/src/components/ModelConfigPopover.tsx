/**
 * Popover showing LLM config details (provider, model, parameters)
 * Anchored to the model label in TurnCard
 */

import { useEffect, useRef, useState } from 'react';
import { X, Loader, Cpu } from 'lucide-react';
import type { LlmConfigDetails } from '../types';

interface ModelConfigPopoverProps {
  configId: number;
  anchorRef: React.RefObject<HTMLElement>;
  onClose: () => void;
  requestConfig: (configId: number, callback: (config: LlmConfigDetails) => void) => void;
  cachedConfig?: LlmConfigDetails;
}

export function ModelConfigPopover({
  configId,
  anchorRef,
  onClose,
  requestConfig,
  cachedConfig,
}: ModelConfigPopoverProps) {
  const [config, setConfig] = useState<LlmConfigDetails | null>(cachedConfig || null);
  const [loading, setLoading] = useState(!cachedConfig);
  const popoverRef = useRef<HTMLDivElement>(null);

  // Fetch config if not cached
  useEffect(() => {
    if (cachedConfig) {
      setConfig(cachedConfig);
      setLoading(false);
      return;
    }

    requestConfig(configId, (fetchedConfig) => {
      setConfig(fetchedConfig);
      setLoading(false);
    });
  }, [configId, cachedConfig, requestConfig]);

  // Close on outside click
  useEffect(() => {
    const handleClick = (e: MouseEvent) => {
      if (
        popoverRef.current &&
        !popoverRef.current.contains(e.target as Node) &&
        anchorRef.current &&
        !anchorRef.current.contains(e.target as Node)
      ) {
        onClose();
      }
    };
    document.addEventListener('mousedown', handleClick);
    return () => document.removeEventListener('mousedown', handleClick);
  }, [onClose, anchorRef]);

  // Close on escape
  useEffect(() => {
    const handleKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        onClose();
      }
    };
    document.addEventListener('keydown', handleKey);
    return () => document.removeEventListener('keydown', handleKey);
  }, [onClose]);

  // Calculate position relative to anchor
  const [position, setPosition] = useState<{ top: number; left: number }>({ top: 0, left: 0 });

  useEffect(() => {
    if (!anchorRef.current) return;
    const rect = anchorRef.current.getBoundingClientRect();
    const popoverWidth = 384; // w-96 = 24rem = 384px
    // Align to right edge of anchor, but don't overflow left side of viewport
    const rightAlignedLeft = rect.right - popoverWidth;
    const left = Math.max(8, rightAlignedLeft);
    setPosition({
      top: rect.bottom + 8,
      left,
    });
  }, [anchorRef]);

  // Format params for display
  const formatParamValue = (value: unknown): string => {
    if (value === null || value === undefined) return 'null';
    if (typeof value === 'object') return JSON.stringify(value, null, 2);
    return String(value);
  };

  return (
    <div
      ref={popoverRef}
      className="fixed z-50 w-96 rounded-lg border border-cyber-border/40 bg-cyber-bg/95 shadow-[0_0_20px_rgba(0,255,249,0.15)] backdrop-blur-md animate-fade-in"
      style={{ top: position.top, left: position.left }}
    >
      {/* Header */}
      <div className="flex items-center justify-between px-3 py-2 border-b border-cyber-border/40">
        <div className="flex items-center gap-2">
          <Cpu className="w-3.5 h-3.5 text-cyber-cyan" />
          <span className="text-xs font-semibold text-gray-300 uppercase tracking-wider">
            Model Config
          </span>
        </div>
        <button
          type="button"
          onClick={onClose}
          className="p-1 rounded text-gray-400 hover:text-gray-200 hover:bg-cyber-surface/60 transition-colors"
        >
          <X className="h-3.5 w-3.5" />
        </button>
      </div>

      {/* Content */}
      <div className="px-3 py-2">
        {loading ? (
          <div className="flex items-center justify-center py-4">
            <Loader className="w-5 h-5 animate-spin text-cyber-cyan" />
          </div>
        ) : config ? (
          <div className="space-y-2">
            {/* Provider & Model */}
            <div className="space-y-1">
              <div className="flex items-center justify-between gap-4">
                <span className="text-[10px] uppercase tracking-widest text-gray-500 flex-shrink-0">Provider</span>
                <span className="text-xs text-gray-200 font-mono">{config.provider}</span>
              </div>
              <div className="flex items-center justify-between gap-4">
                <span className="text-[10px] uppercase tracking-widest text-gray-500 flex-shrink-0">Model</span>
                <span className="text-xs text-gray-200 font-mono truncate" title={config.model}>
                  {config.model}
                </span>
              </div>
            </div>

            {/* Parameters */}
            {config.params && Object.keys(config.params).length > 0 && (
              <div className="pt-2 border-t border-cyber-border/30">
                <span className="text-[10px] uppercase tracking-widest text-gray-500 block mb-1">
                  Parameters
                </span>
                <div className="space-y-1 max-h-40 overflow-y-auto">
                  {Object.entries(config.params).map(([key, value]) => (
                    <div key={key} className="flex items-start justify-between gap-2">
                      <span className="text-[10px] text-gray-400 font-mono flex-shrink-0">{key}</span>
                      <span className="text-[10px] text-gray-200 font-mono text-right break-all">
                        {formatParamValue(value)}
                      </span>
                    </div>
                  ))}
                </div>
              </div>
            )}

            {/* No params message */}
            {(!config.params || Object.keys(config.params).length === 0) && (
              <div className="pt-2 border-t border-cyber-border/30">
                <span className="text-[10px] text-gray-500 italic">
                  Default parameters
                </span>
              </div>
            )}
          </div>
        ) : (
          <div className="py-4 text-center text-xs text-gray-500">
            Config not found
          </div>
        )}
      </div>
    </div>
  );
}
