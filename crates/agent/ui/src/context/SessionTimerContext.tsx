/**
 * SessionTimerContext
 *
 * Isolates the 1-second tick produced by useSessionTimer so it only
 * re-renders components that actually display timer values
 * (HeaderStatsBar, StatsDrawer) instead of the entire AppShell tree.
 *
 * The context is split into three independent atoms so that consumers
 * subscribe only to what they need:
 *   - SessionTimerElapsedContext  — globalElapsedMs (changes every second while active)
 *   - SessionTimerAgentContext    — agentElapsedMs Map (changes every second while active)
 *   - SessionTimerActiveContext   — isSessionActive boolean (changes only on start/stop)
 *
 * This means HeaderStatsBar (which reads globalElapsedMs + isSessionActive but NOT
 * agentElapsedMs) will not be touched when the agents Map reference changes. Similarly,
 * StatsDrawer can subscribe to all three without any extra overhead.
 *
 * Usage:
 *   - Mount <SessionTimerProvider> once inside AppShell (after UiClientContext is available).
 *   - Consume with the granular hooks:
 *       useSessionTimerElapsed()  → number
 *       useSessionTimerAgents()   → Map<string, number>
 *       useSessionTimerActive()   → boolean
 *   - Or use useSessionTimerContext() for the combined object (StatsDrawer / tests).
 */

import { createContext, useContext, useMemo, ReactNode } from 'react';
import { useUiClientContext } from './UiClientContext';
import { useSessionTimer } from '../hooks/useSessionTimer';

// ---------------------------------------------------------------------------
// Split context atoms
// ---------------------------------------------------------------------------

const SessionTimerElapsedContext = createContext<number>(0);
const SessionTimerAgentContext = createContext<Map<string, number>>(new Map());
const SessionTimerActiveContext = createContext<boolean>(false);

// ---------------------------------------------------------------------------
// Provider
// ---------------------------------------------------------------------------

interface SessionTimerProviderProps {
  children: ReactNode;
}

/**
 * Renders children once; the 1-second setInterval inside useSessionTimer
 * only re-renders this provider and its subtree, not AppShell.
 */
export function SessionTimerProvider({ children }: SessionTimerProviderProps) {
  const { events, thinkingBySession, sessionId } = useUiClientContext();

  // Derive thinking agents scoped to the currently viewed session only.
  // Using the global thinkingAgentIds would start/run the timer whenever *any*
  // session (including delegated children) has an active LLM request, causing
  // the timer to accumulate time from unrelated sessions.
  const sessionThinkingAgentIds = useMemo<Set<string>>(() => {
    if (!sessionId) return new Set();
    return thinkingBySession.get(sessionId) ?? new Set();
  }, [thinkingBySession, sessionId]);

  const { globalElapsedMs, agentElapsedMs, isSessionActive } = useSessionTimer(
    events,
    sessionThinkingAgentIds,
    sessionId,
  );

  return (
    <SessionTimerActiveContext.Provider value={isSessionActive}>
      <SessionTimerElapsedContext.Provider value={globalElapsedMs}>
        <SessionTimerAgentContext.Provider value={agentElapsedMs}>
          {children}
        </SessionTimerAgentContext.Provider>
      </SessionTimerElapsedContext.Provider>
    </SessionTimerActiveContext.Provider>
  );
}

// ---------------------------------------------------------------------------
// Granular hooks — prefer these in new consumers
// ---------------------------------------------------------------------------

/** Returns the global session elapsed time in milliseconds. Updates every second while active. */
export function useSessionTimerElapsed(): number {
  return useContext(SessionTimerElapsedContext);
}

/** Returns per-agent elapsed time map. Updates every second while active. */
export function useSessionTimerAgents(): Map<string, number> {
  return useContext(SessionTimerAgentContext);
}

/** Returns whether the current session has any actively thinking agents. */
export function useSessionTimerActive(): boolean {
  return useContext(SessionTimerActiveContext);
}

// ---------------------------------------------------------------------------
// Combined hook — kept for backward compatibility and convenience
// ---------------------------------------------------------------------------

interface SessionTimerContextValue {
  globalElapsedMs: number;
  agentElapsedMs: Map<string, number>;
  isSessionActive: boolean;
}

/**
 * @deprecated Prefer the granular hooks (useSessionTimerElapsed,
 * useSessionTimerAgents, useSessionTimerActive) so that components only
 * subscribe to the values they actually render.
 */
export function useSessionTimerContext(): SessionTimerContextValue {
  return {
    globalElapsedMs: useSessionTimerElapsed(),
    agentElapsedMs: useSessionTimerAgents(),
    isSessionActive: useSessionTimerActive(),
  };
}
