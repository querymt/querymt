import { useState, useEffect, useMemo } from 'react';
import { EventItem } from '../types';

interface AgentWorkingState {
  isWorking: boolean;
  accumulatedMs: number;
  workStartedAt?: number;
  activeDelegationIds: Set<string>;
}

interface GlobalTimerState {
  hasStarted: boolean;
  accumulatedMs: number;
  lastActiveAt?: number;
}

interface SessionTimerResult {
  globalElapsedMs: number;
  agentElapsedMs: Map<string, number>;
  isSessionActive: boolean;
}

/**
 * Live timer hook that tracks:
 * - Global session elapsed time (wall-clock from first prompt, paused when waiting for user)
 * - Per-agent active time (accumulated work time, excluding pauses for delegation and user wait)
 * 
 * Global timer:
 * - Starts from first prompt_received
 * - Pauses when ALL agents are waiting for user (conversation complete)
 * - Resumes when any agent starts working
 * 
 * Per-agent timers:
 * - Start when agent receives prompt
 * - Pause when:
 *   - Delegating to another agent (delegation_requested)
 *   - Waiting for user (llm_request_end with finish_reason 'stop')
 * - Resume when:
 *   - Delegation completes (delegation_completed)
 *   - Next prompt received
 * - Keep running when:
 *   - llm_request_end with finish_reason 'tool_calls' (about to execute tools)
 */
export function useSessionTimer(
  events: EventItem[],
  thinkingAgentId: string | null,
  isConversationComplete: boolean
): SessionTimerResult {
  const [currentTime, setCurrentTime] = useState(Date.now());
  
  // Update current time every second for live display
  useEffect(() => {
    const interval = setInterval(() => {
      setCurrentTime(Date.now());
    }, 1000);
    
    return () => clearInterval(interval);
  }, []);
  
  // Calculate timing state from events
  const { agentStates, globalState } = useMemo(() => {
    const agentStates = new Map<string, AgentWorkingState>();
    const globalState: GlobalTimerState = {
      hasStarted: false,
      accumulatedMs: 0,
    };
    let lastEventTimestamp = 0;
    
    // Process events to reconstruct timing state
    for (const event of events) {
      const agentId = event.agentId || 'unknown';
      const timestamp = event.timestamp;
      lastEventTimestamp = Math.max(lastEventTimestamp, timestamp);
      
      if (!agentStates.has(agentId)) {
        agentStates.set(agentId, {
          isWorking: false,
          accumulatedMs: 0,
          activeDelegationIds: new Set(),
        });
      }
      
      const state = agentStates.get(agentId)!;
      
      // Detect event types
      const eventContent = event.content?.toLowerCase() || '';
      const isPromptReceived = event.type === 'user';
      const isLlmRequestEnd = eventContent.includes('llm_request_end');
      const isDelegationRequested = eventContent.includes('delegation_requested');
      const isDelegationCompleted = eventContent.includes('delegation_completed');
      const finishReason = event.finishReason?.toLowerCase();
      
      // GLOBAL TIMER: Start from first prompt_received
      if (isPromptReceived && !globalState.hasStarted) {
        globalState.hasStarted = true;
        globalState.lastActiveAt = timestamp;
      }
      
      // PER-AGENT: Start working when prompt received
      if (isPromptReceived) {
        if (!state.isWorking && state.activeDelegationIds.size === 0) {
          state.isWorking = true;
          state.workStartedAt = timestamp;
        }
      }
      
      // PER-AGENT: Pause when delegation requested
      if (isDelegationRequested && event.delegationId) {
        state.activeDelegationIds.add(event.delegationId);
        if (state.isWorking && state.workStartedAt !== undefined) {
          // Accumulate time up to this point and pause
          const elapsed = timestamp - state.workStartedAt;
          state.accumulatedMs += elapsed;
          state.isWorking = false;
          state.workStartedAt = undefined;
        }
      }
      
      // PER-AGENT: Resume when delegation completed
      if (isDelegationCompleted && event.delegationId) {
        state.activeDelegationIds.delete(event.delegationId);
        // If no more active delegations and not already working, resume
        if (state.activeDelegationIds.size === 0 && !state.isWorking) {
          state.isWorking = true;
          state.workStartedAt = timestamp;
        }
      }
      
      // PER-AGENT: Only pause on llm_request_end with finish_reason 'stop'
      if (isLlmRequestEnd && state.isWorking && state.activeDelegationIds.size === 0) {
        if (finishReason === 'stop') {
          // Conversation turn complete, waiting for user - pause
          if (state.workStartedAt !== undefined) {
            const elapsed = timestamp - state.workStartedAt;
            state.accumulatedMs += elapsed;
          }
          state.isWorking = false;
          state.workStartedAt = undefined;
        }
        // If finishReason === 'tool_calls' or 'toolcalls', keep timer running
        // The agent will execute tools next
      }
    }
    
    // GLOBAL TIMER: Pause when all agents stopped working
    const anyAgentWorking = Array.from(agentStates.values()).some(s => s.isWorking);
    if (!anyAgentWorking && globalState.lastActiveAt !== undefined) {
      // All agents have stopped - accumulate time and pause
      const elapsed = lastEventTimestamp - globalState.lastActiveAt;
      globalState.accumulatedMs += elapsed;
      globalState.lastActiveAt = undefined;
    }
    
    return { agentStates, globalState };
  }, [events]);
  
  // Calculate live elapsed times
  // GLOBAL TIMER: Add live delta only if session is active
  const anyAgentWorking = Array.from(agentStates.values()).some(s => s.isWorking);
  const isSessionActive = anyAgentWorking || (thinkingAgentId !== null && !isConversationComplete);
  
  let globalElapsedMs = globalState.accumulatedMs;
  if (globalState.lastActiveAt !== undefined && isSessionActive) {
    globalElapsedMs += (currentTime - globalState.lastActiveAt);
  }
  
  // PER-AGENT TIMERS: Add live delta for agents currently working
  const agentElapsedMs = new Map<string, number>();
  for (const [agentId, state] of agentStates.entries()) {
    let elapsed = state.accumulatedMs;
    // If agent is currently working, add live delta
    if (state.isWorking && state.workStartedAt !== undefined) {
      elapsed += (currentTime - state.workStartedAt);
    }
    agentElapsedMs.set(agentId, elapsed);
  }
  
  return {
    globalElapsedMs,
    agentElapsedMs,
    isSessionActive,
  };
}
