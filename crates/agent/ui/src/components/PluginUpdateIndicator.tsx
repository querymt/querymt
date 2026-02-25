import { useEffect, useState } from 'react';
import { RefreshCw, CheckCircle, XCircle } from 'lucide-react';
import type { PluginUpdateResult, PluginUpdateStatus } from '../types';

const PHASE_LABELS: Record<string, string> = {
  resolving: 'Resolving',
  verifying_signature: 'Verifying signature',
  downloading: 'Downloading',
  extracting: 'Extracting',
  persisting: 'Persisting',
  completed: 'Completed',
  failed: 'Failed',
};

interface PluginUpdateIndicatorProps {
  isUpdatingPlugins: boolean;
  pluginUpdateStatus: Record<string, PluginUpdateStatus>;
  pluginUpdateResults: PluginUpdateResult[] | null;
}

export function PluginUpdateIndicator({
  isUpdatingPlugins,
  pluginUpdateStatus,
  pluginUpdateResults,
}: PluginUpdateIndicatorProps) {
  // Local dismissed state so the banner can self-dismiss after 5 s
  const [dismissed, setDismissed] = useState(false);

  // Reset dismissed flag whenever new results arrive
  useEffect(() => {
    if (pluginUpdateResults !== null) {
      setDismissed(false);
      const timer = setTimeout(() => setDismissed(true), 5000);
      return () => clearTimeout(timer);
    }
  }, [pluginUpdateResults]);

  // Also reset dismissed when an update starts
  useEffect(() => {
    if (isUpdatingPlugins) {
      setDismissed(false);
    }
  }, [isUpdatingPlugins]);

  const showProgress = isUpdatingPlugins;
  const showResults = !isUpdatingPlugins && pluginUpdateResults !== null && !dismissed;

  if (!showProgress && !showResults) {
    return null;
  }

  // Pick the most-recently-active plugin status entry to display
  const statusEntries = Object.values(pluginUpdateStatus);
  const currentStatus = statusEntries[statusEntries.length - 1] ?? null;

  if (showProgress) {
    const phaseLabel = currentStatus
      ? (PHASE_LABELS[currentStatus.phase] ?? currentStatus.phase)
      : 'Starting…';
    const isDownloading = currentStatus?.phase === 'downloading';
    const percent = currentStatus?.percent ?? null;

    return (
      <div className="fixed bottom-4 left-1/2 -translate-x-1/2 z-50 flex flex-col gap-1 min-w-[280px] max-w-sm px-4 py-3 rounded-lg border border-accent-primary/30 bg-surface-elevated shadow-lg animate-fade-in">
        <div className="flex items-center gap-2">
          <RefreshCw className="w-3.5 h-3.5 text-accent-primary animate-spin flex-shrink-0" />
          <span className="text-xs font-medium text-ui-primary flex-1 truncate">
            Updating plugins
            {currentStatus ? (
              <>
                {' — '}
                <span className="font-mono">{currentStatus.plugin_name}</span>
              </>
            ) : null}
          </span>
        </div>
        <div className="text-xs text-ui-muted">{phaseLabel}</div>
        {isDownloading && percent !== null && (
          <div className="mt-1">
            <div className="flex justify-between text-[10px] text-ui-muted mb-0.5">
              <span>Downloading</span>
              <span>{Math.round(percent)}%</span>
            </div>
            <div className="h-1 w-full bg-surface-canvas rounded-full overflow-hidden">
              <div
                className="h-full bg-accent-primary rounded-full transition-all duration-150"
                style={{ width: `${Math.min(100, Math.round(percent))}%` }}
              />
            </div>
          </div>
        )}
      </div>
    );
  }

  // Show results summary
  const results = pluginUpdateResults!;
  const succeeded = results.filter((r) => r.success).length;
  const failed = results.filter((r) => !r.success).length;

  return (
    <div className="fixed bottom-4 left-1/2 -translate-x-1/2 z-50 flex flex-col gap-1 min-w-[280px] max-w-sm px-4 py-3 rounded-lg border border-surface-border/40 bg-surface-elevated shadow-lg animate-fade-in">
      <div className="flex items-center gap-2">
        {failed === 0 ? (
          <CheckCircle className="w-3.5 h-3.5 text-status-success flex-shrink-0" />
        ) : (
          <XCircle className="w-3.5 h-3.5 text-status-error flex-shrink-0" />
        )}
        <span className="text-xs font-medium text-ui-primary">Plugin update complete</span>
      </div>
      <div className="text-xs text-ui-muted">
        {succeeded} succeeded / {failed} failed
      </div>
      {failed > 0 && (
        <ul className="mt-1 space-y-0.5">
          {results
            .filter((r) => !r.success)
            .map((r) => (
              <li key={r.plugin_name} className="text-[10px] text-status-error font-mono truncate">
                {r.plugin_name}: {r.message ?? 'unknown error'}
              </li>
            ))}
        </ul>
      )}
    </div>
  );
}
