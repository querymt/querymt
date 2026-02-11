import { describe, it, expect, beforeEach, vi, afterEach } from 'vitest';
import { renderHook, act } from '@testing-library/react';
import { useSessionTimer } from './useSessionTimer';
import { EventItem } from '../types';
import { resetFixtureCounter } from '../test/fixtures';
import { useUiStore } from '../store/uiStore';

describe('useSessionTimer', () => {
  beforeEach(() => {
    resetFixtureCounter();
    vi.useFakeTimers();
    // Clear the session timer cache before each test
    useUiStore.getState().sessionTimerCache.clear();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  describe('initial state', () => {
    it('returns zero elapsed time with no thinking agents', () => {
      const { result } = renderHook(() => 
        useSessionTimer([], new Set(), 'session-1')
      );
      
      expect(result.current.globalElapsedMs).toBe(0);
      expect(result.current.agentElapsedMs.size).toBe(0);
      expect(result.current.isSessionActive).toBe(false);
    });

    it('returns isSessionActive=true when agents are thinking', () => {
      const { result } = renderHook(() => 
        useSessionTimer([], new Set(['agent-1']), 'session-1')
      );
      
      expect(result.current.isSessionActive).toBe(true);
    });

    it('returns isSessionActive=false when no thinking agents', () => {
      const { result } = renderHook(() => 
        useSessionTimer([], new Set(), 'session-1')
      );
      
      expect(result.current.isSessionActive).toBe(false);
    });

    it('handles null sessionId (home page)', () => {
      const { result } = renderHook(() => 
        useSessionTimer([], new Set(), null)
      );
      
      expect(result.current.globalElapsedMs).toBe(0);
      expect(result.current.agentElapsedMs.size).toBe(0);
      expect(result.current.isSessionActive).toBe(false);
    });
  });

  describe('global timer', () => {
    it('starts global timer when agent begins thinking', async () => {
      const { result, rerender } = renderHook(
        ({ thinking }) => useSessionTimer([], thinking, 'session-1'),
        { initialProps: { thinking: new Set<string>() } }
      );

      expect(result.current.globalElapsedMs).toBe(0);

      // Agent starts thinking
      act(() => {
        rerender({ thinking: new Set(['agent-1']) });
      });

      // Advance time by 3 seconds
      await act(async () => {
        vi.advanceTimersByTime(3000);
      });

      // Global timer should have advanced (approximately 3000ms)
      // Note: react-timer-hook uses seconds, so we check >= 2000ms to account for timing
      expect(result.current.globalElapsedMs).toBeGreaterThanOrEqual(2000);
      expect(result.current.globalElapsedMs).toBeLessThanOrEqual(4000);
    });

    it('pauses global timer when all agents stop thinking', async () => {
      const { result, rerender } = renderHook(
        ({ thinking }) => useSessionTimer([], thinking, 'session-1'),
        { initialProps: { thinking: new Set(['agent-1']) } }
      );

      // Advance time while active
      await act(async () => {
        vi.advanceTimersByTime(2000);
      });

      const elapsedWhileActive = result.current.globalElapsedMs;
      expect(elapsedWhileActive).toBeGreaterThan(0);

      // Stop all agents
      act(() => {
        rerender({ thinking: new Set() });
      });

      // Advance more time while inactive
      await act(async () => {
        vi.advanceTimersByTime(3000);
      });

      // Timer should not have advanced significantly
      expect(result.current.globalElapsedMs).toBe(elapsedWhileActive);
    });

    it('resumes global timer when agents start thinking again', async () => {
      const { result, rerender } = renderHook(
        ({ thinking }) => useSessionTimer([], thinking, 'session-1'),
        { initialProps: { thinking: new Set(['agent-1']) } }
      );

      // First active period
      await act(async () => {
        vi.advanceTimersByTime(2000);
      });

      const firstElapsed = result.current.globalElapsedMs;

      // Pause
      act(() => {
        rerender({ thinking: new Set() });
      });

      // Wait while paused
      await act(async () => {
        vi.advanceTimersByTime(5000);
      });

      // Resume
      act(() => {
        rerender({ thinking: new Set(['agent-1']) });
      });

      // Advance time during second active period
      await act(async () => {
        vi.advanceTimersByTime(1000);
      });

      // Timer should have accumulated both active periods
      expect(result.current.globalElapsedMs).toBeGreaterThan(firstElapsed);
    });

    it('continues global timer when different agents are thinking', async () => {
      const { result, rerender } = renderHook(
        ({ thinking }) => useSessionTimer([], thinking, 'session-1'),
        { initialProps: { thinking: new Set(['agent-1']) } }
      );

      await act(async () => {
        vi.advanceTimersByTime(2000);
      });

      // Switch to different agent (global timer should keep running)
      act(() => {
        rerender({ thinking: new Set(['agent-2']) });
      });

      await act(async () => {
        vi.advanceTimersByTime(1000);
      });

      // Global timer should show accumulated time from both periods
      expect(result.current.globalElapsedMs).toBeGreaterThanOrEqual(2000);
    });
  });

  describe('per-agent timers', () => {
    it('tracks elapsed time for a single agent', async () => {
      const { result } = renderHook(() => 
        useSessionTimer([], new Set(['agent-1']), 'session-1')
      );

      await act(async () => {
        vi.advanceTimersByTime(3000);
      });

      const agentTime = result.current.agentElapsedMs.get('agent-1');
      expect(agentTime).toBeGreaterThanOrEqual(2000);
      expect(agentTime).toBeLessThanOrEqual(4000);
    });

    it('tracks multiple agents independently', async () => {
      const { result, rerender } = renderHook(
        ({ thinking }) => useSessionTimer([], thinking, 'session-1'),
        { initialProps: { thinking: new Set(['agent-1']) } }
      );

      // Agent 1 runs for 2 seconds
      await act(async () => {
        vi.advanceTimersByTime(2000);
      });

      // Add agent 2 (agent 1 continues)
      act(() => {
        rerender({ thinking: new Set(['agent-1', 'agent-2']) });
      });

      // Both run for 1 second
      await act(async () => {
        vi.advanceTimersByTime(1000);
      });

      const agent1Time = result.current.agentElapsedMs.get('agent-1');
      const agent2Time = result.current.agentElapsedMs.get('agent-2');

      // Agent 1 should have ~3 seconds total
      expect(agent1Time).toBeGreaterThanOrEqual(2000);
      expect(agent1Time).toBeLessThanOrEqual(4000);

      // Agent 2 should have ~1 second
      expect(agent2Time).toBeGreaterThanOrEqual(500);
      expect(agent2Time).toBeLessThanOrEqual(2000);
    });

    it('pauses agent timer when agent stops thinking', async () => {
      const { result, rerender } = renderHook(
        ({ thinking }) => useSessionTimer([], thinking, 'session-1'),
        { initialProps: { thinking: new Set(['agent-1']) } }
      );

      await act(async () => {
        vi.advanceTimersByTime(2000);
      });

      const timeBeforePause = result.current.agentElapsedMs.get('agent-1')!;
      expect(timeBeforePause).toBeGreaterThan(0);

      // Stop agent
      act(() => {
        rerender({ thinking: new Set() });
      });

      await act(async () => {
        vi.advanceTimersByTime(3000);
      });

      // Agent time should not have changed
      const timeAfterPause = result.current.agentElapsedMs.get('agent-1')!;
      expect(timeAfterPause).toBe(timeBeforePause);
    });

    it('resumes agent timer when agent starts thinking again', async () => {
      const { result, rerender } = renderHook(
        ({ thinking }) => useSessionTimer([], thinking, 'session-1'),
        { initialProps: { thinking: new Set(['agent-1']) } }
      );

      // First period
      await act(async () => {
        vi.advanceTimersByTime(2000);
      });

      const firstPeriod = result.current.agentElapsedMs.get('agent-1')!;

      // Pause
      act(() => {
        rerender({ thinking: new Set() });
      });

      await act(async () => {
        vi.advanceTimersByTime(5000);
      });

      // Resume
      act(() => {
        rerender({ thinking: new Set(['agent-1']) });
      });

      await act(async () => {
        vi.advanceTimersByTime(1000);
      });

      // Should have accumulated both periods
      const totalTime = result.current.agentElapsedMs.get('agent-1')!;
      expect(totalTime).toBeGreaterThan(firstPeriod);
    });

    it('handles agents starting at different times', async () => {
      const { result, rerender } = renderHook(
        ({ thinking }) => useSessionTimer([], thinking, 'session-1'),
        { initialProps: { thinking: new Set<string>() } }
      );

      // Start agent 1
      act(() => {
        rerender({ thinking: new Set(['agent-1']) });
      });

      await act(async () => {
        vi.advanceTimersByTime(2000);
      });

      // Start agent 2 (agent 1 continues)
      act(() => {
        rerender({ thinking: new Set(['agent-1', 'agent-2']) });
      });

      await act(async () => {
        vi.advanceTimersByTime(1000);
      });

      const agent1Time = result.current.agentElapsedMs.get('agent-1')!;
      const agent2Time = result.current.agentElapsedMs.get('agent-2')!;

      // Agent 1 should have more time than agent 2
      expect(agent1Time).toBeGreaterThan(agent2Time);
    });
  });

  describe('edge cases', () => {
    it('handles empty thinkingAgentIds set', () => {
      const { result } = renderHook(() => 
        useSessionTimer([], new Set(), 'session-1')
      );
      
      expect(result.current.globalElapsedMs).toBe(0);
      expect(result.current.agentElapsedMs.size).toBe(0);
      expect(result.current.isSessionActive).toBe(false);
    });

    it('handles rapid agent changes', async () => {
      const { result, rerender } = renderHook(
        ({ thinking }) => useSessionTimer([], thinking, 'session-1'),
        { initialProps: { thinking: new Set(['agent-1']) } }
      );

      // Rapid changes
      act(() => {
        rerender({ thinking: new Set(['agent-2']) });
      });

      act(() => {
        rerender({ thinking: new Set(['agent-1', 'agent-2']) });
      });

      act(() => {
        rerender({ thinking: new Set(['agent-1']) });
      });

      await act(async () => {
        vi.advanceTimersByTime(1000);
      });

      // Should not crash and should have some elapsed time
      expect(result.current.globalElapsedMs).toBeGreaterThanOrEqual(0);
      expect(result.current.agentElapsedMs.get('agent-1')).toBeGreaterThanOrEqual(0);
    });

    it('maintains timer state across re-renders with same thinkingAgentIds', async () => {
      const agentSet = new Set(['agent-1']);
      const { result, rerender } = renderHook(
        ({ thinking }) => useSessionTimer([], thinking, 'session-1'),
        { initialProps: { thinking: agentSet } }
      );

      await act(async () => {
        vi.advanceTimersByTime(2000);
      });

      const timeBefore = result.current.globalElapsedMs;

      // Re-render with same set
      rerender({ thinking: agentSet });

      // Time should not have reset
      expect(result.current.globalElapsedMs).toBeGreaterThanOrEqual(timeBefore);
    });

    it('ignores events parameter (backward compatibility)', async () => {
      const events: EventItem[] = [
        { id: '1', timestamp: 1000, type: 'user', content: 'test', agentId: 'agent-1' } as EventItem,
      ];

      const { result } = renderHook(() => 
        useSessionTimer(events, new Set(['agent-1']), 'session-1')
      );

      await act(async () => {
        vi.advanceTimersByTime(2000);
      });

      // Should work normally, ignoring events
      expect(result.current.globalElapsedMs).toBeGreaterThanOrEqual(0);
      expect(result.current.isSessionActive).toBe(true);
    });
  });

  describe('tick interval behavior', () => {
    it('updates agentElapsedMs map every second while active', async () => {
      const { result } = renderHook(() => 
        useSessionTimer([], new Set(['agent-1']), 'session-1')
      );

      const initialTime = result.current.agentElapsedMs.get('agent-1') || 0;

      // Advance by 1 second and trigger interval
      await act(async () => {
        vi.advanceTimersByTime(1000);
      });

      const timeAfter1s = result.current.agentElapsedMs.get('agent-1') || 0;
      expect(timeAfter1s).toBeGreaterThan(initialTime);

      // Advance another second
      await act(async () => {
        vi.advanceTimersByTime(1000);
      });

      const timeAfter2s = result.current.agentElapsedMs.get('agent-1') || 0;
      expect(timeAfter2s).toBeGreaterThan(timeAfter1s);
    });

    it('does not run tick interval when inactive', async () => {
      const { result } = renderHook(() => 
        useSessionTimer([], new Set(), 'session-1')
      );

      await act(async () => {
        vi.advanceTimersByTime(5000);
      });

      // Should remain at 0, no intervals running
      expect(result.current.globalElapsedMs).toBe(0);
    });
  });

  describe('per-session timer state', () => {
    it('resets timer when switching to a new session', async () => {
      const { result, rerender } = renderHook(
        ({ sessionId, thinking }) => useSessionTimer([], thinking, sessionId),
        { initialProps: { sessionId: 'session-1', thinking: new Set(['agent-1']) } }
      );

      // Let timer run for session-1
      await act(async () => {
        vi.advanceTimersByTime(3000);
      });

      const session1Time = result.current.globalElapsedMs;
      expect(session1Time).toBeGreaterThanOrEqual(2000);

      // Switch to session-2
      act(() => {
        rerender({ sessionId: 'session-2', thinking: new Set<string>() });
      });

      // Timer should be reset to 0 for new session
      expect(result.current.globalElapsedMs).toBe(0);
      expect(result.current.agentElapsedMs.size).toBe(0);
    });

    it('restores timer state when switching back to a previous session', async () => {
      const { result, rerender } = renderHook(
        ({ sessionId, thinking }) => useSessionTimer([], thinking, sessionId),
        { initialProps: { sessionId: 'session-1', thinking: new Set(['agent-1']) } }
      );

      // Let timer run for session-1
      await act(async () => {
        vi.advanceTimersByTime(3000);
      });

      const session1Time = result.current.globalElapsedMs;
      expect(session1Time).toBeGreaterThanOrEqual(2000);

      // Switch to session-2
      act(() => {
        rerender({ sessionId: 'session-2', thinking: new Set(['agent-2']) });
      });

      await act(async () => {
        vi.advanceTimersByTime(2000);
      });

      // Switch back to session-1
      act(() => {
        rerender({ sessionId: 'session-1', thinking: new Set<string>() });
      });

      // Should restore session-1's timer (approximately, accounting for save/restore)
      expect(result.current.globalElapsedMs).toBeGreaterThanOrEqual(session1Time - 100);
      expect(result.current.globalElapsedMs).toBeLessThanOrEqual(session1Time + 100);
    });

    it('preserves per-agent times across session switches', async () => {
      const { result, rerender } = renderHook(
        ({ sessionId, thinking }) => useSessionTimer([], thinking, sessionId),
        { initialProps: { sessionId: 'session-1', thinking: new Set(['agent-1']) } }
      );

      // Let agent-1 run in session-1
      await act(async () => {
        vi.advanceTimersByTime(2000);
      });

      const agent1Time = result.current.agentElapsedMs.get('agent-1')!;
      expect(agent1Time).toBeGreaterThan(0);

      // Switch to session-2
      act(() => {
        rerender({ sessionId: 'session-2', thinking: new Set(['agent-2']) });
      });

      await act(async () => {
        vi.advanceTimersByTime(1000);
      });

      // Switch back to session-1
      act(() => {
        rerender({ sessionId: 'session-1', thinking: new Set<string>() });
      });

      // Should restore agent-1's time
      const restoredAgent1Time = result.current.agentElapsedMs.get('agent-1');
      expect(restoredAgent1Time).toBeDefined();
      expect(restoredAgent1Time!).toBeGreaterThanOrEqual(agent1Time - 100);
      expect(restoredAgent1Time!).toBeLessThanOrEqual(agent1Time + 100);

      // Should not have agent-2 from session-2
      expect(result.current.agentElapsedMs.has('agent-2')).toBe(false);
    });

    it('handles switching to null sessionId (home page)', async () => {
      const { result, rerender } = renderHook(
        ({ sessionId, thinking }) => useSessionTimer([], thinking, sessionId),
        { initialProps: { sessionId: 'session-1', thinking: new Set(['agent-1']) } }
      );

      // Let timer run for session-1
      await act(async () => {
        vi.advanceTimersByTime(2000);
      });

      const session1Time = result.current.globalElapsedMs;
      expect(session1Time).toBeGreaterThan(0);

      // Navigate to home (null sessionId)
      act(() => {
        rerender({ sessionId: null, thinking: new Set<string>() });
      });

      // Timer should reset
      expect(result.current.globalElapsedMs).toBe(0);

      // Navigate back to session-1
      act(() => {
        rerender({ sessionId: 'session-1', thinking: new Set<string>() });
      });

      // Should restore session-1's time
      expect(result.current.globalElapsedMs).toBeGreaterThanOrEqual(session1Time - 100);
    });

    it('saves timer state for multiple sessions independently', async () => {
      const { result, rerender } = renderHook(
        ({ sessionId, thinking }) => useSessionTimer([], thinking, sessionId),
        { initialProps: { sessionId: 'session-1', thinking: new Set(['agent-1']) } }
      );

      // Run session-1 for 3 seconds
      await act(async () => {
        vi.advanceTimersByTime(3000);
      });
      const session1Time = result.current.globalElapsedMs;

      // Switch to session-2, run for 2 seconds
      act(() => {
        rerender({ sessionId: 'session-2', thinking: new Set(['agent-2']) });
      });
      await act(async () => {
        vi.advanceTimersByTime(2000);
      });
      const session2Time = result.current.globalElapsedMs;

      // Switch to session-3, run for 1 second
      act(() => {
        rerender({ sessionId: 'session-3', thinking: new Set(['agent-3']) });
      });
      await act(async () => {
        vi.advanceTimersByTime(1000);
      });
      const session3Time = result.current.globalElapsedMs;

      // Verify each session has independent times
      expect(session1Time).toBeGreaterThan(session2Time);
      expect(session2Time).toBeGreaterThan(session3Time);

      // Switch back to session-1
      act(() => {
        rerender({ sessionId: 'session-1', thinking: new Set<string>() });
      });
      expect(result.current.globalElapsedMs).toBeGreaterThanOrEqual(session1Time - 100);

      // Switch to session-2
      act(() => {
        rerender({ sessionId: 'session-2', thinking: new Set<string>() });
      });
      expect(result.current.globalElapsedMs).toBeGreaterThanOrEqual(session2Time - 100);
      expect(result.current.globalElapsedMs).toBeLessThan(session1Time);

      // Switch to session-3
      act(() => {
        rerender({ sessionId: 'session-3', thinking: new Set<string>() });
      });
      expect(result.current.globalElapsedMs).toBeGreaterThanOrEqual(session3Time - 100);
      expect(result.current.globalElapsedMs).toBeLessThan(session2Time);
    });
  });
});
