/**
 * Debug logging utility — gated behind a runtime toggle.
 *
 * All hot-path console.log / console.debug calls should use these helpers
 * so they become zero-cost when debug logging is disabled (the default).
 *
 * Toggle via:
 *   - Keyboard shortcut: Ctrl/Cmd+X then D
 *   - Console: `window.__toggleDebugLog()`
 *   - Persisted in localStorage as `debugLogEnabled`
 */

const STORAGE_KEY = 'debugLogEnabled';

let _enabled =
  typeof localStorage !== 'undefined' && localStorage.getItem(STORAGE_KEY) === 'true';

/** Whether debug logging is currently enabled. */
export function isDebugLogEnabled(): boolean {
  return _enabled;
}

/** Toggle debug logging on/off. Returns the new state. */
export function toggleDebugLog(): boolean {
  _enabled = !_enabled;
  try {
    localStorage.setItem(STORAGE_KEY, String(_enabled));
  } catch {
    // Storage full or unavailable — ignore.
  }
  // Always announce the change regardless of the toggle state.
  // eslint-disable-next-line no-console
  console.log(`[debugLog] Debug logging ${_enabled ? 'ENABLED' : 'DISABLED'}`);
  return _enabled;
}

/**
 * Log at `console.log` level — only when debug logging is enabled.
 *
 * Accepts a thunk so argument evaluation (including object spreading)
 * is entirely skipped when disabled.
 */
export function debugLog(message: string, data?: () => Record<string, unknown>): void {
  if (!_enabled) return;
  if (data) {
    // eslint-disable-next-line no-console
    console.log(message, data());
  } else {
    // eslint-disable-next-line no-console
    console.log(message);
  }
}

/**
 * Log at `console.debug` level — only when debug logging is enabled.
 */
export function debugTrace(message: string, data?: () => Record<string, unknown>): void {
  if (!_enabled) return;
  if (data) {
    // eslint-disable-next-line no-console
    console.debug(message, data());
  } else {
    // eslint-disable-next-line no-console
    console.debug(message);
  }
}

// Expose a global toggle for the browser console.
if (typeof window !== 'undefined') {
  (window as any).__toggleDebugLog = toggleDebugLog;
}
