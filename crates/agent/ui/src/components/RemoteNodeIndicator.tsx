/**
 * RemoteNodeIndicator — header pill showing connected mesh nodes.
 *
 * Renders nothing when there are no remote nodes (mesh not active / no peers).
 * When peers are present, shows a Globe icon with a green status dot and a
 * count badge. Clicking opens a popover listing each node with its hostname,
 * capabilities, and active session count.
 */

import { useState } from 'react';
import * as Popover from '@radix-ui/react-popover';
import { Globe, Wifi } from 'lucide-react';
import type { RemoteNodeInfo } from '../types';

interface RemoteNodeIndicatorProps {
  remoteNodes: RemoteNodeInfo[];
}

export function RemoteNodeIndicator({ remoteNodes }: RemoteNodeIndicatorProps) {
  const [open, setOpen] = useState(false);

  // Nothing to show when mesh has no peers
  if (remoteNodes.length === 0) {
    return null;
  }

  return (
    <Popover.Root open={open} onOpenChange={setOpen}>
      <Popover.Trigger asChild>
        <button
          type="button"
          title={`${remoteNodes.length} remote node${remoteNodes.length !== 1 ? 's' : ''} connected`}
          className="relative h-8 inline-flex items-center gap-1.5 px-2.5 rounded-lg border border-surface-border bg-surface-canvas/60 hover:border-accent-secondary/40 hover:bg-surface-elevated/50 transition-colors"
        >
          <Globe className="w-3.5 h-3.5 text-accent-secondary" />
          <span className="text-xs font-mono text-accent-secondary">
            {remoteNodes.length}
          </span>
          {/* Online dot */}
          <span className="absolute -top-0.5 -right-0.5 w-2 h-2 rounded-full bg-status-success ring-1 ring-surface-canvas" />
        </button>
      </Popover.Trigger>

      <Popover.Portal>
        <Popover.Content
          side="bottom"
          align="end"
          sideOffset={8}
          className="z-50 w-72 rounded-xl border border-surface-border bg-surface-elevated shadow-[0_8px_30px_rgba(0,0,0,0.4),0_0_20px_rgba(var(--accent-secondary-rgb),0.1)] animate-fade-in"
        >
          {/* Header */}
          <div className="flex items-center gap-2 px-4 py-3 border-b border-surface-border/50">
            <Wifi className="w-3.5 h-3.5 text-accent-secondary" />
            <span className="text-xs font-semibold text-accent-secondary uppercase tracking-wider">
              Mesh Nodes
            </span>
            <span className="ml-auto text-[10px] text-ui-muted">
              {remoteNodes.length} online
            </span>
          </div>

          {/* Node list */}
          <div className="py-2 max-h-64 overflow-y-auto custom-scrollbar">
            {remoteNodes.map((node) => (
              <div
                key={node.label}
                className="flex items-start gap-3 px-4 py-2.5 hover:bg-surface-canvas/40 transition-colors"
              >
                {/* Status dot */}
                <span className="mt-1 w-2 h-2 flex-shrink-0 rounded-full bg-status-success" />

                <div className="flex-1 min-w-0">
                  <div className="flex items-center gap-2">
                    <span className="text-sm font-mono text-ui-primary truncate">
                      {node.label}
                    </span>
                    {node.active_sessions > 0 && (
                      <span className="text-[10px] px-1.5 py-0.5 rounded bg-accent-secondary/15 text-accent-secondary border border-accent-secondary/25">
                        {node.active_sessions} session{node.active_sessions !== 1 ? 's' : ''}
                      </span>
                    )}
                  </div>
                  {node.capabilities.length > 0 && (
                    <p className="text-[11px] text-ui-muted mt-0.5 truncate">
                      {node.capabilities.join(', ')}
                    </p>
                  )}
                </div>
              </div>
            ))}
          </div>

          <Popover.Arrow className="fill-surface-border" />
        </Popover.Content>
      </Popover.Portal>
    </Popover.Root>
  );
}
