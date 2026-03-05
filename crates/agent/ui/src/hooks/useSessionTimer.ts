import { useState, useEffect, useRef } from 'react';
import { EventItem } from '../types';
import { useUiStore } from '../store/uiStore';

interface SessionTimerResult {
  globalElapsedMs: number;
  agentElapsedMs: Map<string, number>;
  isSessionActive: boolean;
}

/**
 * Simple stopwatch class for tracking elapsed time.
 * Uses timestamps to calculate elapsed time with millisecond precision.
 */
class AgentStopwatch {
  private running = false;
  private accumulated = 0;
  private startedAt?: number;

  start(): void {
    if (!this.running) {
      this.running = true;
      this.startedAt = Date.now();
    }
  }

  pause(): void {
    if (this.running && this.startedAt !== undefined) {
      this.accumulated += Date.now() - this.startedAt;
      this.running = false;
      this.startedAt = undefined;
    }
  }

  getElapsedMs(): number {
    if (this.running && this.startedAt !== undefined) {
      return this.accumulated + (Date.now() - this.startedAt);
    }
    return this.accumulated;
  }

  isRunning(): boolean {
    return this.running;
  }

  getAccumulated(): number {
    if (this.running && this.startedAt !== undefined) {
      return this.accumulated + (Date.now() - this.startedAt);
    }
    return this.accumulated;
  }

  setAccumulated(value: number): void {
    this.accumulated = value;
    this.running = false;
    this.startedAt = undefined;
  }

  reset(): void {
    this.accumulated = 0;
    this.running = false;
    this.startedAt = undefined;
  }
}

/**
 * Live timer hook that tracks per-session elapsed time.
 *
 * This hook provides:
 * - Global session elapsed time (total wall-clock time while agents are thinking)
 * - Per-agent active time (time each agent spends actively thinking)
 * - Session active state (whether any agent is currently thinking)
 *
 * Key behaviors:
 * - Timer state is stored per-session in Zustand store
 * - When switching sessions, timer state is saved and restored
 * - Global timer starts when first agent begins thinking (thinkingAgentIds becomes non-empty)
 * - Global timer pauses when all agents stop thinking (thinkingAgentIds becomes empty)
 * - Per-agent timers start when agent enters thinkingAgentIds
 * - Per-agent timers pause when agent leaves thinkingAgentIds
 * - Null sessionId (home page) shows 0:00
 *
 * Implementation notes:
 * - Uses a single AgentStopwatch for the global timer (same class as per-agent tracking)
 *   instead of react-timer-hook, eliminating a second independent re-render source.
 * - A single 1-second setInterval drives all display updates while the session is active.
 *   The interval produces a snapshot of the current elapsed values and stores them as
 *   React state, so the Map reference only changes once per second (not on every render).
 * - Saves/restores timer state via Zustand when the session changes.
 *
 * @param _events - Event history (currently unused, reserved for future historical replay)
 * @param thinkingAgentIds - Set of agent IDs currently thinking/processing,
 *   scoped to the currently viewed session (not the global cross-session set).
 * @param sessionId - Current session ID (null when on home page)
 * @returns Session timer state with elapsed times and active status
 */
export function useSessionTimer(
  _events: EventItem[],
  thinkingAgentIds: Set<string>,
  sessionId: string | null
): SessionTimerResult {
  const { saveSessionTimer, getSessionTimer } = useUiStore();

  // Track previous session ID for save/restore
  const prevSessionIdRef = useRef<string | null>(null);

  // Global stopwatch — same lightweight class as per-agent tracking.
  // Stored in a ref so start/pause never cause a re-render themselves.
  const globalStopwatch = useRef(new AgentStopwatch());

  // Per-agent stopwatches stored in ref (persists across renders)
  const agentStopwatches = useRef(new Map<string, AgentStopwatch>());

  // Displayed state — only updated once per second by the interval below.
  // Keeping globalElapsedMs and agentElapsedMs as a single state object means
  // a single setState call per tick instead of two, halving the number of
  // re-renders triggered by the interval.
  const [timerState, setTimerState] = useState<{
    globalElapsedMs: number;
    agentElapsedMs: Map<string, number>;
  }>({ globalElapsedMs: 0, agentElapsedMs: new Map() });

  // Determine if session is active (any agents thinking)
  const isSessionActive = thinkingAgentIds.size > 0;

  // Keep a ref to the latest thinkingAgentIds so the session-switch effect can
  // inspect it without adding it as a dependency (which would re-run the effect
  // on every thinking-state change, defeating the session-switch guard).
  const thinkingAgentIdsRef = useRef(thinkingAgentIds);
  thinkingAgentIdsRef.current = thinkingAgentIds;

  // Session switching: save previous session state and restore new session state
  useEffect(() => {
    const prevSessionId = prevSessionIdRef.current;

    // Only process when sessionId actually changes
    if (prevSessionId === sessionId) return;

    // Save timer state when leaving a session — pause running timers first so
    // the accumulated value includes the current running segment.
    if (prevSessionId) {
      globalStopwatch.current.pause();
      const agentAccumulatedMs: Record<string, number> = {};
      for (const [agentId, sw] of agentStopwatches.current) {
        sw.pause();
        agentAccumulatedMs[agentId] = sw.getAccumulated();
      }
      saveSessionTimer(prevSessionId, {
        globalAccumulatedMs: globalStopwatch.current.getAccumulated(),
        agentAccumulatedMs,
      });
    }

    // Restore or reset timer state when entering a session
    const savedState = sessionId ? getSessionTimer(sessionId) : null;

    globalStopwatch.current.reset();
    agentStopwatches.current.clear();

    if (savedState) {
      globalStopwatch.current.setAccumulated(savedState.globalAccumulatedMs);
      for (const [agentId, accumulated] of Object.entries(savedState.agentAccumulatedMs)) {
        const sw = new AgentStopwatch();
        sw.setAccumulated(accumulated as number);
        agentStopwatches.current.set(agentId, sw);
      }
    }

    // If the new session is already active (thinkingAgentIds non-empty at the
    // time of the switch), start the stopwatches immediately. The isSessionActive
    // effect won't fire when isSessionActive stays true across the session change,
    // so we handle it here to avoid the global timer remaining paused.
    const currentThinking = thinkingAgentIdsRef.current;
    if (currentThinking.size > 0) {
      globalStopwatch.current.start();
      for (const agentId of currentThinking) {
        if (!agentStopwatches.current.has(agentId)) {
          agentStopwatches.current.set(agentId, new AgentStopwatch());
        }
        agentStopwatches.current.get(agentId)!.start();
      }
    }

    // Immediately reflect restored/reset state in displayed values
    setTimerState({
      globalElapsedMs: globalStopwatch.current.getElapsedMs(),
      agentElapsedMs: new Map(
        Array.from(agentStopwatches.current.entries()).map(([id, sw]) => [id, sw.getElapsedMs()])
      ),
    });

    prevSessionIdRef.current = sessionId;
  }, [sessionId, saveSessionTimer, getSessionTimer]);

  // Control global stopwatch based on session active state
  useEffect(() => {
    if (isSessionActive) {
      globalStopwatch.current.start();
    } else {
      globalStopwatch.current.pause();
    }
  }, [isSessionActive]);

  // Control per-agent stopwatches based on thinkingAgentIds membership
  useEffect(() => {
    // Start stopwatches for agents that are now thinking
    for (const agentId of thinkingAgentIds) {
      if (!agentStopwatches.current.has(agentId)) {
        agentStopwatches.current.set(agentId, new AgentStopwatch());
      }
      const sw = agentStopwatches.current.get(agentId)!;
      if (!sw.isRunning()) sw.start();
    }

    // Pause stopwatches for agents that stopped thinking
    for (const [agentId, sw] of agentStopwatches.current) {
      if (!thinkingAgentIds.has(agentId) && sw.isRunning()) {
        sw.pause();
      }
    }
  }, [thinkingAgentIds]);

  // Single interval: snapshot displayed values once per second while active.
  // We only create a new Map when at least one value has actually changed
  // (rounded to seconds) so that consumers memoised on agentElapsedMs can
  // bail out of re-renders when the displayed second hasn't ticked.
  useEffect(() => {
    if (!isSessionActive) return;

    const interval = setInterval(() => {
      setTimerState((prev) => {
        const nextGlobal = globalStopwatch.current.getElapsedMs();

        // Check whether any agent-level value changed (second granularity).
        let agentChanged = false;
        const nextAgentEntries: [string, number][] = [];
        for (const [agentId, sw] of agentStopwatches.current) {
          const ms = sw.getElapsedMs();
          nextAgentEntries.push([agentId, ms]);
          const prevMs = prev.agentElapsedMs.get(agentId);
          if (prevMs === undefined || Math.floor(ms / 1000) !== Math.floor(prevMs / 1000)) {
            agentChanged = true;
          }
        }
        // Also detect removed agents.
        if (nextAgentEntries.length !== prev.agentElapsedMs.size) {
          agentChanged = true;
        }

        const globalChanged = Math.floor(nextGlobal / 1000) !== Math.floor(prev.globalElapsedMs / 1000);

        if (!globalChanged && !agentChanged) return prev; // no state update

        return {
          globalElapsedMs: nextGlobal,
          agentElapsedMs: agentChanged ? new Map(nextAgentEntries) : prev.agentElapsedMs,
        };
      });
    }, 1000);

    return () => clearInterval(interval);
  }, [isSessionActive]);

  return {
    globalElapsedMs: timerState.globalElapsedMs,
    agentElapsedMs: timerState.agentElapsedMs,
    isSessionActive,
  };
}
