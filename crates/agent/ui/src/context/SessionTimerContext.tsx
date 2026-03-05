/**
 * SessionTimerContext
 *
 * Isolates the 1-second tick produced by useSessionTimer so it only
 * rerenders components that actually display timer values
 * (HeaderStatsBar, StatsDrawer) instead of the entire AppShell tree.
 *
 * Usage:
 *   - Mount <SessionTimerProvider> once inside AppShell (after
 *     UiClientContext is available).
 *   - Consume with useSessionTimerContext() in HeaderStatsBar /
 *     StatsDrawer instead of accepting globalElapsedMs etc. as props.
 */

import { createContext, useContext, ReactNode } from 'react';
import { useUiClientContext } from './UiClientContext';
import { useSessionTimer } from '../hooks/useSessionTimer';

interface SessionTimerContextValue {
  globalElapsedMs: number;
  agentElapsedMs: Map<string, number>;
  isSessionActive: boolean;
}

const SessionTimerContext = createContext<SessionTimerContextValue | undefined>(undefined);

interface SessionTimerProviderProps {
  children: ReactNode;
}

/**
 * Renders children once; the 1-second setInterval inside useSessionTimer
 * only rerenders this provider and its subtree, not AppShell.
 */
export function SessionTimerProvider({ children }: SessionTimerProviderProps) {
  const { events, thinkingAgentIds, sessionId } = useUiClientContext();

  const timerResult = useSessionTimer(events, thinkingAgentIds, sessionId);

  return (
    <SessionTimerContext.Provider value={timerResult}>
      {children}
    </SessionTimerContext.Provider>
  );
}

export function useSessionTimerContext(): SessionTimerContextValue {
  const ctx = useContext(SessionTimerContext);
  if (ctx === undefined) {
    throw new Error('useSessionTimerContext must be used within a SessionTimerProvider');
  }
  return ctx;
}
