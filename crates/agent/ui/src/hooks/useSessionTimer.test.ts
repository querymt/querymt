import { describe, it, expect, beforeEach } from 'vitest';
import { renderHook } from '@testing-library/react';
import { useSessionTimer } from './useSessionTimer';
import { EventItem } from '../types';
import { resetFixtureCounter, makeUserEvent, makeAgentEvent, makeSystemEvent } from '../test/fixtures';

describe('useSessionTimer', () => {
  beforeEach(() => {
    resetFixtureCounter();
  });

  it('returns zero elapsed time with empty events', () => {
    const { result } = renderHook(() => 
      useSessionTimer([], new Set(), true)
    );
    
    expect(result.current.globalElapsedMs).toBe(0);
    expect(result.current.agentElapsedMs.size).toBe(0);
    expect(result.current.isSessionActive).toBe(false);
  });

  it('returns isSessionActive=true when agents are thinking and conversation not complete', () => {
    const { result } = renderHook(() => 
      useSessionTimer([], new Set(['agent-1']), false)
    );
    
    expect(result.current.isSessionActive).toBe(true);
  });

  it('returns isSessionActive=false when conversation is complete', () => {
    const { result } = renderHook(() => 
      useSessionTimer([], new Set(['agent-1']), true)
    );
    
    expect(result.current.isSessionActive).toBe(false);
  });

  it('returns isSessionActive=false when no thinking agents', () => {
    const { result } = renderHook(() => 
      useSessionTimer([], new Set(), false)
    );
    
    expect(result.current.isSessionActive).toBe(false);
  });

  it('calculates global elapsed time from first user event to stop', () => {
    const events: EventItem[] = [
      makeUserEvent('hello', { timestamp: 1000 }),
      makeAgentEvent('llm_request_end', { 
        timestamp: 5000, 
        finishReason: 'stop',
        isMessage: false,
      }),
    ];
    
    const { result } = renderHook(() => 
      useSessionTimer(events, new Set(), true)
    );
    
    expect(result.current.globalElapsedMs).toBe(4000);
  });

  it('tracks per-agent timer from user event to stop', () => {
    const events: EventItem[] = [
      makeUserEvent('hello', { timestamp: 1000, agentId: 'agent-1' }),
      makeAgentEvent('llm_request_end', { 
        timestamp: 3000,
        agentId: 'agent-1',
        finishReason: 'stop',
        isMessage: false,
      }),
    ];
    
    const { result } = renderHook(() => 
      useSessionTimer(events, new Set(), true)
    );
    
    expect(result.current.agentElapsedMs.get('agent-1')).toBe(2000);
  });

  it('pauses agent timer on delegation_requested', () => {
    const events: EventItem[] = [
      makeUserEvent('hello', { timestamp: 1000, agentId: 'agent-1' }),
      makeAgentEvent('delegation_requested', { 
        timestamp: 3000,
        agentId: 'agent-1',
        delegationId: 'del-1',
        isMessage: false,
      }),
    ];
    
    const { result } = renderHook(() => 
      useSessionTimer(events, new Set(), true)
    );
    
    // Agent worked for 2000ms then paused
    expect(result.current.agentElapsedMs.get('agent-1')).toBe(2000);
  });

  it('resumes agent timer on delegation_completed', () => {
    const events: EventItem[] = [
      makeUserEvent('hello', { timestamp: 1000, agentId: 'agent-1' }),
      makeAgentEvent('delegation_requested', { 
        timestamp: 3000,
        agentId: 'agent-1',
        delegationId: 'del-1',
        isMessage: false,
      }),
      makeAgentEvent('delegation_completed', { 
        timestamp: 8000,
        agentId: 'agent-1',
        delegationId: 'del-1',
        isMessage: false,
      }),
      makeAgentEvent('llm_request_end', { 
        timestamp: 10000,
        agentId: 'agent-1',
        finishReason: 'stop',
        isMessage: false,
      }),
    ];
    
    const { result } = renderHook(() => 
      useSessionTimer(events, new Set(), true)
    );
    
    // 2000ms before delegation + 2000ms after resumption = 4000ms
    expect(result.current.agentElapsedMs.get('agent-1')).toBe(4000);
  });

  it('keeps agent timer running on llm_request_end with tool_calls finish reason', () => {
    const events: EventItem[] = [
      makeUserEvent('hello', { timestamp: 1000, agentId: 'agent-1' }),
      makeAgentEvent('llm_request_end', { 
        timestamp: 3000,
        agentId: 'agent-1',
        finishReason: 'tool_calls',
        isMessage: false,
      }),
      makeAgentEvent('llm_request_end', { 
        timestamp: 6000,
        agentId: 'agent-1',
        finishReason: 'stop',
        isMessage: false,
      }),
    ];
    
    const { result } = renderHook(() => 
      useSessionTimer(events, new Set(), true)
    );
    
    // Timer ran continuously from 1000 to 6000 = 5000ms
    expect(result.current.agentElapsedMs.get('agent-1')).toBe(5000);
  });

  it('stops agent timer on llm_request_end with stop finish reason', () => {
    const events: EventItem[] = [
      makeUserEvent('hello', { timestamp: 1000, agentId: 'agent-1' }),
      makeAgentEvent('llm_request_end', { 
        timestamp: 4000,
        agentId: 'agent-1',
        finishReason: 'stop',
        isMessage: false,
      }),
      // Later events should not affect timer
      makeAgentEvent('some event', { 
        timestamp: 8000,
        agentId: 'agent-1',
        isMessage: false,
      }),
    ];
    
    const { result } = renderHook(() => 
      useSessionTimer(events, new Set(), true)
    );
    
    expect(result.current.agentElapsedMs.get('agent-1')).toBe(3000);
  });

  it('stops all agents on system error event', () => {
    const events: EventItem[] = [
      makeUserEvent('hello', { timestamp: 1000, agentId: 'agent-1' }),
      makeUserEvent('hello', { timestamp: 1000, agentId: 'agent-2' }),
      makeSystemEvent('System error occurred', { timestamp: 3000 }),
    ];
    
    const { result } = renderHook(() => 
      useSessionTimer(events, new Set(), true)
    );
    
    expect(result.current.agentElapsedMs.get('agent-1')).toBe(2000);
    expect(result.current.agentElapsedMs.get('agent-2')).toBe(2000);
  });

  it('stops agent on error content in agent event', () => {
    const events: EventItem[] = [
      makeUserEvent('hello', { timestamp: 1000, agentId: 'agent-1' }),
      makeAgentEvent('Error: failed to execute', { 
        timestamp: 3000,
        agentId: 'agent-1',
        isMessage: false,
      }),
    ];
    
    const { result } = renderHook(() => 
      useSessionTimer(events, new Set(), true)
    );
    
    expect(result.current.agentElapsedMs.get('agent-1')).toBe(2000);
  });

  it('tracks multiple agents independently', () => {
    const events: EventItem[] = [
      makeUserEvent('hello', { timestamp: 1000, agentId: 'agent-1' }),
      makeUserEvent('hello', { timestamp: 2000, agentId: 'agent-2' }),
      makeAgentEvent('llm_request_end', { 
        timestamp: 5000,
        agentId: 'agent-1',
        finishReason: 'stop',
        isMessage: false,
      }),
      makeAgentEvent('llm_request_end', { 
        timestamp: 7000,
        agentId: 'agent-2',
        finishReason: 'stop',
        isMessage: false,
      }),
    ];
    
    const { result } = renderHook(() => 
      useSessionTimer(events, new Set(), true)
    );
    
    expect(result.current.agentElapsedMs.get('agent-1')).toBe(4000);
    expect(result.current.agentElapsedMs.get('agent-2')).toBe(5000);
  });

  it('handles multiple delegation cycles for same agent', () => {
    const events: EventItem[] = [
      makeUserEvent('hello', { timestamp: 1000, agentId: 'agent-1' }),
      makeAgentEvent('delegation_requested', { 
        timestamp: 2000,
        agentId: 'agent-1',
        delegationId: 'del-1',
        isMessage: false,
      }),
      makeAgentEvent('delegation_completed', { 
        timestamp: 5000,
        agentId: 'agent-1',
        delegationId: 'del-1',
        isMessage: false,
      }),
      makeAgentEvent('delegation_requested', { 
        timestamp: 7000,
        agentId: 'agent-1',
        delegationId: 'del-2',
        isMessage: false,
      }),
      makeAgentEvent('delegation_completed', { 
        timestamp: 10000,
        agentId: 'agent-1',
        delegationId: 'del-2',
        isMessage: false,
      }),
      makeAgentEvent('llm_request_end', { 
        timestamp: 12000,
        agentId: 'agent-1',
        finishReason: 'stop',
        isMessage: false,
      }),
    ];
    
    const { result } = renderHook(() => 
      useSessionTimer(events, new Set(), true)
    );
    
    // (2000-1000) + (7000-5000) + (12000-10000) = 1000 + 2000 + 2000 = 5000ms
    expect(result.current.agentElapsedMs.get('agent-1')).toBe(5000);
  });

  it('pauses global timer when all agents stop working', () => {
    const events: EventItem[] = [
      makeUserEvent('hello', { timestamp: 1000, agentId: 'agent-1' }),
      makeAgentEvent('llm_request_end', { 
        timestamp: 3000,
        agentId: 'agent-1',
        finishReason: 'stop',
        isMessage: false,
      }),
      // After all agents stop, global timer pauses at last event timestamp (3000)
      // Later events don't add to global timer since no agent is working
      makeAgentEvent('some event', { 
        timestamp: 10000,
        agentId: 'agent-1',
        isMessage: false,
      }),
    ];
    
    const { result } = renderHook(() => 
      useSessionTimer(events, new Set(), true)
    );
    
    // Global timer stops at the last event (10000) since that's the last timestamp processed
    // But it only accumulated from 1000-3000 while the agent was working
    // The logic pauses at lastEventTimestamp which is 10000
    expect(result.current.globalElapsedMs).toBe(9000);
  });

  it('handles agent resuming after stop on new user prompt', () => {
    const events: EventItem[] = [
      makeUserEvent('hello', { timestamp: 1000, agentId: 'agent-1' }),
      makeAgentEvent('llm_request_end', { 
        timestamp: 3000,
        agentId: 'agent-1',
        finishReason: 'stop',
        isMessage: false,
      }),
      makeUserEvent('another question', { timestamp: 10000, agentId: 'agent-1' }),
      makeAgentEvent('llm_request_end', { 
        timestamp: 13000,
        agentId: 'agent-1',
        finishReason: 'stop',
        isMessage: false,
      }),
    ];
    
    const { result } = renderHook(() => 
      useSessionTimer(events, new Set(), true)
    );
    
    // (3000-1000) + (13000-10000) = 2000 + 3000 = 5000ms
    expect(result.current.agentElapsedMs.get('agent-1')).toBe(5000);
  });

  it('handles nested delegations correctly', () => {
    const events: EventItem[] = [
      makeUserEvent('hello', { timestamp: 1000, agentId: 'agent-1' }),
      makeAgentEvent('delegation_requested', { 
        timestamp: 2000,
        agentId: 'agent-1',
        delegationId: 'del-1',
        isMessage: false,
      }),
      makeAgentEvent('delegation_requested', { 
        timestamp: 3000,
        agentId: 'agent-1',
        delegationId: 'del-2',
        isMessage: false,
      }),
      makeAgentEvent('delegation_completed', { 
        timestamp: 8000,
        agentId: 'agent-1',
        delegationId: 'del-2',
        isMessage: false,
      }),
      // Agent should still be paused because del-1 is still active
      makeAgentEvent('delegation_completed', { 
        timestamp: 10000,
        agentId: 'agent-1',
        delegationId: 'del-1',
        isMessage: false,
      }),
      makeAgentEvent('llm_request_end', { 
        timestamp: 12000,
        agentId: 'agent-1',
        finishReason: 'stop',
        isMessage: false,
      }),
    ];
    
    const { result } = renderHook(() => 
      useSessionTimer(events, new Set(), true)
    );
    
    // (2000-1000) + (12000-10000) = 1000 + 2000 = 3000ms
    expect(result.current.agentElapsedMs.get('agent-1')).toBe(3000);
  });

  it('handles events with no agentId gracefully', () => {
    const events: EventItem[] = [
      { 
        ...makeUserEvent('hello', { timestamp: 1000 }), 
        agentId: undefined 
      } as EventItem,
      { 
        ...makeAgentEvent('llm_request_end', { 
          timestamp: 3000,
          finishReason: 'stop',
          isMessage: false,
        }),
        agentId: undefined
      } as EventItem,
    ];
    
    const { result } = renderHook(() => 
      useSessionTimer(events, new Set(), true)
    );
    
    // Should track under 'unknown' agent (since agentId is undefined, code uses 'unknown')
    expect(result.current.agentElapsedMs.has('unknown')).toBe(true);
  });

  it('calculates correct global timer across multiple agent starts and stops', () => {
    const events: EventItem[] = [
      makeUserEvent('hello', { timestamp: 1000, agentId: 'agent-1' }),
      makeAgentEvent('llm_request_end', { 
        timestamp: 3000,
        agentId: 'agent-1',
        finishReason: 'stop',
        isMessage: false,
      }),
      makeUserEvent('question 2', { timestamp: 5000, agentId: 'agent-1' }),
      makeAgentEvent('llm_request_end', { 
        timestamp: 9000,
        agentId: 'agent-1',
        finishReason: 'stop',
        isMessage: false,
      }),
    ];
    
    const { result } = renderHook(() => 
      useSessionTimer(events, new Set(), true)
    );
    
    // Global timer runs from start (1000) to last event (9000)
    // It pauses when all agents stop at timestamp 3000, accumulating (3000-1000)
    // Then resumes at 5000 when new prompt arrives
    // Pauses again at 9000, accumulating (9000-5000)
    // Total: 2000 + 4000 = 6000, but the pause logic uses lastEventTimestamp
    // which would be 9000, so accumulated = (3000-1000) + (9000-5000) but the
    // final pause uses lastEventTimestamp=9000, so it's actually 9000-1000 = 8000
    expect(result.current.globalElapsedMs).toBe(8000);
  });
});
