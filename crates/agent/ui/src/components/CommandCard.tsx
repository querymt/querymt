/**
 * CommandCard — renders a runtime slash command invocation and its output.
 *
 * Runtime commands are control-plane actions (e.g. /status, /plan) that bypass
 * chat history. They appear as ephemeral command cards in the UI, showing the
 * command line and result one below the other.
 */

import { memo, useState } from 'react';
import { ChevronDown, ChevronRight, Terminal, CheckCircle2, AlertTriangle, XCircle, Loader2 } from 'lucide-react';
import { useCopyToClipboard } from '../hooks/useCopyToClipboard';
import { EventItem } from '../types';

export interface CommandCardProps {
  commandEvent: EventItem;
}

const LEVEL_CONFIG = {
  info: {
    border: 'border-surface-border/60',
    bg: 'bg-surface-canvas/40',
    icon: Terminal,
    iconColor: 'text-ui-secondary',
    badgeBg: 'bg-surface-elevated',
    badgeText: 'text-ui-secondary',
  },
  success: {
    border: 'border-status-success/40',
    bg: 'bg-status-success/5',
    icon: CheckCircle2,
    iconColor: 'text-status-success',
    badgeBg: 'bg-status-success/10',
    badgeText: 'text-status-success',
  },
  warning: {
    border: 'border-status-warning/40',
    bg: 'bg-status-warning/5',
    icon: AlertTriangle,
    iconColor: 'text-status-warning',
    badgeBg: 'bg-status-warning/10',
    badgeText: 'text-status-warning',
  },
  error: {
    border: 'border-status-error/40',
    bg: 'bg-status-error/5',
    icon: XCircle,
    iconColor: 'text-status-error',
    badgeBg: 'bg-status-error/10',
    badgeText: 'text-status-error',
  },
};

export const CommandCard = memo(function CommandCard({ commandEvent }: CommandCardProps) {
  const [collapsed, setCollapsed] = useState(false);
  const { copiedValue, copy } = useCopyToClipboard();

  const { commandName, commandLine, commandStatus, commandOutput, commandError } = commandEvent;

  const level = commandOutput?.level ?? (commandStatus === 'failed' ? 'error' : 'info');
  const config = LEVEL_CONFIG[level];
  const Icon = commandStatus === 'running' ? Loader2 : config.icon;

  const body = commandStatus === 'failed'
    ? commandError ?? 'Command failed'
    : commandOutput?.body ?? '';

  const display = commandOutput?.display ?? 'text';

  const handleCopy = () => {
    copy(body);
  };

  return (
    <div className={`my-2 rounded-xl border ${config.border} ${config.bg} overflow-hidden`}>
      {/* Header: command line */}
      <div className="flex items-center gap-2.5 px-3.5 py-2.5 border-b border-surface-border/30">
        <div className={`flex h-7 w-7 items-center justify-center rounded-lg ${config.badgeBg}`}>
          <Icon
            className={`h-3.5 w-3.5 ${config.iconColor} ${commandStatus === 'running' ? 'animate-spin' : ''}`}
          />
        </div>

        <div className="flex-1 min-w-0">
          <div className="flex items-center gap-2">
            <span className="font-mono text-xs font-semibold text-ui-primary truncate">
              {commandLine ?? `/${commandName}`}
            </span>
            {commandStatus === 'running' && (
              <span className="text-[10px] text-accent-tertiary leading-none px-1.5 py-px rounded bg-accent-tertiary/10 border border-accent-tertiary/30">
                running
              </span>
            )}
          </div>
          {commandOutput?.title && (
            <p className="text-[11px] text-ui-secondary truncate mt-0.5">
              {commandOutput.title}
            </p>
          )}
        </div>

        <div className="flex items-center gap-1.5">
          {body && (
            <button
              type="button"
              onClick={handleCopy}
              className="text-[10px] text-ui-muted hover:text-ui-secondary transition-colors px-1.5 py-0.5 rounded border border-surface-border/40 hover:border-surface-border/60"
            >
              {copiedValue === body ? 'Copied' : 'Copy'}
            </button>
          )}
          {body && (
            <button
              type="button"
              onClick={() => setCollapsed((v) => !v)}
              className="text-ui-muted hover:text-ui-secondary transition-colors p-0.5"
              title={collapsed ? 'Expand' : 'Collapse'}
            >
              {collapsed ? (
                <ChevronRight className="h-3.5 w-3.5" />
              ) : (
                <ChevronDown className="h-3.5 w-3.5" />
              )}
            </button>
          )}
        </div>
      </div>

      {/* Body: command output */}
      {!collapsed && body && (
        <div className="px-3.5 py-2.5">
          {display === 'markdown' ? (
            <div className="text-sm text-ui-primary whitespace-pre-wrap font-mono leading-relaxed">
              {body}
            </div>
          ) : (
            <pre className="text-sm text-ui-primary whitespace-pre-wrap font-mono leading-relaxed">
              {body}
            </pre>
          )}
        </div>
      )}
    </div>
  );
});
