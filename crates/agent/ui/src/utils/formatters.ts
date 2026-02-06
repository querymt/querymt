/**
 * Shared formatting utilities for stats display
 * Extracted from FloatingStatsPanel for reuse in HeaderStatsBar and StatsDrawer
 */

/**
 * Format percentage for progress indicators
 */
export function formatPercentage(current: number, max: number): string {
  return `${Math.min(100, Math.round((current / max) * 100))}%`;
}

/**
 * Format cost as USD with 2-4 decimal places depending on amount
 */
export function formatCost(usd: number): string {
  if (usd < 0.01) {
    return `$${usd.toFixed(4)}`;
  }
  return `$${usd.toFixed(2)}`;
}

/**
 * Format tokens with abbreviated suffixes (k, M)
 */
export function formatTokensAbbrev(count: number): string {
  if (count >= 1_000_000) return `${(count / 1_000_000).toFixed(1)}M`;
  if (count >= 1_000) return `${(count / 1_000).toFixed(0)}k`;
  return count.toString();
}

/**
 * Format duration as human-readable string
 * Compact version for inline display (e.g., "2m34s")
 */
export function formatDuration(ms: number): string {
  const totalSeconds = Math.floor(ms / 1000);
  const seconds = totalSeconds % 60;
  const minutes = Math.floor(totalSeconds / 60) % 60;
  const hours = Math.floor(totalSeconds / 3600);
  
  if (hours > 0) {
    return `${hours}h${minutes}m${seconds}s`;
  }
  if (minutes > 0) {
    return `${minutes}m${seconds}s`;
  }
  return `${seconds}s`;
}

/**
 * Format duration in compact form (e.g., "2m34s" for inline stats)
 */
export function formatDurationCompact(ms: number): string {
  const totalSeconds = Math.floor(ms / 1000);
  const seconds = totalSeconds % 60;
  const minutes = Math.floor(totalSeconds / 60) % 60;
  const hours = Math.floor(totalSeconds / 3600);
  
  if (hours > 0) {
    return `${hours}h${minutes}m`;
  }
  if (minutes > 0) {
    return `${minutes}m${seconds}s`;
  }
  return `${seconds}s`;
}

/**
 * Format duration from start/end timestamps (e.g., "2h 34m 56s")
 * If endTime is not provided, uses current time
 */
export function formatDurationFromTimestamps(startTime: number, endTime?: number): string {
  const durationMs = (endTime ?? Date.now()) - startTime;
  return formatDuration(Math.max(0, durationMs));
}

/**
 * Format timestamp as locale time string
 */
export function formatTimestamp(ts: number): string {
  return new Date(ts).toLocaleTimeString();
}
