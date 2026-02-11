import { useState, useEffect, useRef } from 'react';
import { useStopwatch } from 'react-timer-hook';
import { EventItem } from '../types';
import { useUiStore } from '../store/uiStore';

interface SessionTimerResult {
  globalElapsedMs: number;
  agentElapsedMs: Map<string, number>;
  isSessionActive: boolean;
}

/**
 * Simple stopwatch class for tracking per-agent elapsed time.
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

  // For save/restore functionality
  getAccumulated(): number {
    // Capture current state including running time
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
 * - Uses useStopwatch from react-timer-hook for smooth global timer
 * - Uses custom AgentStopwatch class in refs for per-agent tracking
 * - Re-renders every second while active to update displayed times
 * - Saves/restores timer state via Zustand when session changes
 * 
 * @param events - Event history (currently unused, for future historical replay)
 * @param thinkingAgentIds - Set of agent IDs currently thinking/processing
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
  
  // Global stopwatch using react-timer-hook
  // We'll use reset() with offset to restore saved time
  const { totalSeconds, start, pause, reset, isRunning } = useStopwatch({ autoStart: false });
  
  // Store latest totalSeconds in ref for save operations
  const totalSecondsRef = useRef(totalSeconds);
  totalSecondsRef.current = totalSeconds;
  
  // Per-agent stopwatches stored in ref (persists across renders)
  const agentStopwatches = useRef(new Map<string, AgentStopwatch>());
  
  // Tick state to force re-render every second while active
  // This ensures agentElapsedMs Map reflects current elapsed time
  const [, setTick] = useState(0);
  
  // Track restored time to handle reset() async behavior
  // useStopwatch doesn't update totalSeconds immediately after reset()
  const [restoredTimeMs, setRestoredTimeMs] = useState<number | null>(null);
  
  // Determine if session is active (any agents thinking)
  const isSessionActive = thinkingAgentIds.size > 0;
  
  // Session switching: save previous session state and restore new session state
  useEffect(() => {
    const prevSessionId = prevSessionIdRef.current;
    
    // Only process when sessionId actually changes
    if (prevSessionId === sessionId) return;
    
    // Save timer state when leaving a session
    if (prevSessionId) {
      const globalAccumulatedMs = totalSecondsRef.current * 1000;
      const agentAccumulatedMs: Record<string, number> = {};
      
      for (const [agentId, stopwatch] of agentStopwatches.current) {
        agentAccumulatedMs[agentId] = stopwatch.getAccumulated();
      }
      
      saveSessionTimer(prevSessionId, {
        globalAccumulatedMs,
        agentAccumulatedMs,
      });
      
      console.log(`[useSessionTimer] Saved timer state for session ${prevSessionId}:`, {
        globalAccumulatedMs,
        agentCount: Object.keys(agentAccumulatedMs).length,
      });
    }
    
    // Restore or reset timer state when entering a session
    if (sessionId) {
      const savedState = getSessionTimer(sessionId);
      
      if (savedState) {
        // Restore from saved state
        const offsetTimestamp = new Date();
        offsetTimestamp.setSeconds(offsetTimestamp.getSeconds() + savedState.globalAccumulatedMs / 1000);
        reset(offsetTimestamp, false); // Set offset, don't auto-start
        setRestoredTimeMs(savedState.globalAccumulatedMs);
        
        // Restore agent stopwatches
        const newAgentStopwatches = new Map<string, AgentStopwatch>();
        for (const [agentId, accumulated] of Object.entries(savedState.agentAccumulatedMs)) {
          const stopwatch = new AgentStopwatch();
          stopwatch.setAccumulated(accumulated as number);
          newAgentStopwatches.set(agentId, stopwatch);
        }
        agentStopwatches.current = newAgentStopwatches;
        
        console.log(`[useSessionTimer] Restored timer state for session ${sessionId}:`, {
          globalAccumulatedMs: savedState.globalAccumulatedMs,
          agentCount: Object.keys(savedState.agentAccumulatedMs).length,
        });
      } else {
        // Fresh session - reset to 0
        reset(undefined, false);
        setRestoredTimeMs(0);
        agentStopwatches.current.clear();
        
        console.log(`[useSessionTimer] Fresh timer for session ${sessionId}`);
      }
    } else {
      // Navigated to home page - reset timer
      reset(undefined, false);
      setRestoredTimeMs(0);
      agentStopwatches.current.clear();
      
      console.log(`[useSessionTimer] Reset timer (navigated to home)`);
    }
    
    prevSessionIdRef.current = sessionId;
  }, [sessionId, reset, saveSessionTimer, getSessionTimer]);
  
  // Control global stopwatch based on session active state
  useEffect(() => {
    if (isSessionActive && !isRunning) {
      // Session became active - start or resume global timer
      start();
    } else if (!isSessionActive && isRunning) {
      // Session became inactive - pause global timer
      pause();
    }
  }, [isSessionActive, isRunning, start, pause]);
  
  // Control per-agent stopwatches based on thinkingAgentIds membership
  useEffect(() => {
    // Start stopwatches for agents that are thinking
    for (const agentId of thinkingAgentIds) {
      if (!agentStopwatches.current.has(agentId)) {
        // First time seeing this agent - create stopwatch
        agentStopwatches.current.set(agentId, new AgentStopwatch());
      }
      const stopwatch = agentStopwatches.current.get(agentId)!;
      if (!stopwatch.isRunning()) {
        stopwatch.start();
      }
    }
    
    // Pause stopwatches for agents that stopped thinking
    for (const [agentId, stopwatch] of agentStopwatches.current) {
      if (!thinkingAgentIds.has(agentId) && stopwatch.isRunning()) {
        stopwatch.pause();
      }
    }
  }, [thinkingAgentIds]);
  
  // Update tick every second while session is active
  // This triggers re-render to update displayed elapsed times
  useEffect(() => {
    if (!isSessionActive) return;
    
    const interval = setInterval(() => {
      setTick(t => t + 1);
    }, 1000);
    
    return () => clearInterval(interval);
  }, [isSessionActive]);
  
  // Build agentElapsedMs map from current stopwatch states
  const agentElapsedMs = new Map<string, number>();
  for (const [agentId, stopwatch] of agentStopwatches.current) {
    agentElapsedMs.set(agentId, stopwatch.getElapsedMs());
  }
  
  // Convert totalSeconds to milliseconds for global elapsed time
  // Use restoredTimeMs if available (handles reset() async behavior)
  // Clear restoredTimeMs once totalSeconds catches up
  const currentTimeMs = totalSeconds * 1000;
  const globalElapsedMs = restoredTimeMs !== null && Math.abs(currentTimeMs - restoredTimeMs) > 100
    ? restoredTimeMs
    : currentTimeMs;
  
  // Clear restoredTimeMs once useStopwatch has caught up
  useEffect(() => {
    if (restoredTimeMs !== null && Math.abs(currentTimeMs - restoredTimeMs) <= 100) {
      setRestoredTimeMs(null);
    }
  }, [currentTimeMs, restoredTimeMs]);
  
  return {
    globalElapsedMs,
    agentElapsedMs,
    isSessionActive,
  };
}
