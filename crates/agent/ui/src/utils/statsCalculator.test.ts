import { describe, it, expect, beforeEach } from 'vitest';
import { calculateStats, calculateDelegationStats } from './statsCalculator';
import { EventItem, DelegationGroupInfo } from '../types';
import { 
  resetFixtureCounter, 
  makeUserEvent, 
  makeAgentEvent, 
  makeToolCallEvent,
  makeToolResultEvent,
  makeSystemEvent,
} from '../test/fixtures';

describe('calculateStats', () => {
  beforeEach(() => {
    resetFixtureCounter();
  });

  describe('basic functionality', () => {
    it('returns zero stats for empty events array', () => {
      const result = calculateStats([]);
      expect(result.session.totalCostUsd).toBe(0);
      expect(result.session.totalMessages).toBe(0);
      expect(result.session.totalToolCalls).toBe(0);
      expect(result.perAgent).toHaveLength(0);
    });

    it('ignores system events completely', () => {
      const events = [
        makeSystemEvent('System initialized'),
        makeUserEvent('hello', { isMessage: true }),
      ];
      const result = calculateStats(events);
      expect(result.session.totalMessages).toBe(1); // Only user message
    });

    it('creates per-agent stats for each unique agentId', () => {
      const events = [
        makeUserEvent('hello', { agentId: 'agent-a' }),
        makeUserEvent('world', { agentId: 'agent-b' }),
      ];
      const result = calculateStats(events);
      expect(result.perAgent).toHaveLength(2);
      expect(result.perAgent.map(a => a.agentId).sort()).toEqual(['agent-a', 'agent-b']);
    });

    it('uses "unknown" for events without agentId', () => {
      const events = [
        { ...makeUserEvent('hello'), agentId: undefined } as EventItem,
      ];
      const result = calculateStats(events);
      expect(result.perAgent[0].agentId).toBe('unknown');
    });
  });

  describe('cost calculation', () => {
    it('accumulates costUsd for per-agent stats', () => {
      const events = [
        makeAgentEvent('response 1', { agentId: 'agent-1', costUsd: 0.10 }),
        makeAgentEvent('response 2', { agentId: 'agent-1', costUsd: 0.15 }),
      ];
      const result = calculateStats(events);
      expect(result.perAgent[0].costUsd).toBeCloseTo(0.25);
    });

    it('uses cumulativeCostUsd for session total when available', () => {
      const events = [
        makeAgentEvent('response 1', { costUsd: 0.10 }),
        makeAgentEvent('response 2', { costUsd: 0.15, cumulativeCostUsd: 0.30 }),
      ];
      const result = calculateStats(events);
      expect(result.session.totalCostUsd).toBeCloseTo(0.30); // Uses cumulative
    });

    it('falls back to sum of costUsd when cumulativeCostUsd not available', () => {
      const events = [
        makeAgentEvent('response 1', { costUsd: 0.10 }),
        makeAgentEvent('response 2', { costUsd: 0.15 }),
      ];
      const result = calculateStats(events);
      expect(result.session.totalCostUsd).toBeCloseTo(0.25); // Sum
    });

    it('BUG: per-agent costs do not match session total when cumulativeCostUsd differs', () => {
      // This test documents the bug where per-agent costs sum to a different
      // value than session total (which uses cumulativeCostUsd)
      const events = [
        makeAgentEvent('response 1', { agentId: 'agent-1', costUsd: 0.10 }),
        makeAgentEvent('response 2', { agentId: 'agent-1', costUsd: 0.15, cumulativeCostUsd: 0.50 }),
      ];
      const result = calculateStats(events);
      
      const perAgentTotal = result.perAgent.reduce((sum, a) => sum + a.costUsd, 0);
      // BUG: These don't match!
      expect(perAgentTotal).toBeCloseTo(0.25); // Per-agent sums individual costs
      expect(result.session.totalCostUsd).toBeCloseTo(0.50); // Session uses cumulative
      // This test documents the inconsistency
      expect(perAgentTotal).not.toBeCloseTo(result.session.totalCostUsd);
    });

    it('BUG: multi-agent scenario has inconsistent totals', () => {
      const events = [
        makeAgentEvent('r1', { agentId: 'agent-a', costUsd: 0.10 }),
        makeAgentEvent('r2', { agentId: 'agent-b', costUsd: 0.20, cumulativeCostUsd: 0.40 }),
      ];
      const result = calculateStats(events);
      
      const agentA = result.perAgent.find(a => a.agentId === 'agent-a')!;
      const agentB = result.perAgent.find(a => a.agentId === 'agent-b')!;
      
      expect(agentA.costUsd).toBeCloseTo(0.10);
      expect(agentB.costUsd).toBeCloseTo(0.20);
      expect(agentA.costUsd + agentB.costUsd).toBeCloseTo(0.30); // Sum of per-agent
      expect(result.session.totalCostUsd).toBeCloseTo(0.40); // Uses cumulative
      // Inconsistent!
    });
  });

  describe('message counting', () => {
    it('counts user messages with isMessage=true', () => {
      const events = [
        makeUserEvent('hello', { isMessage: true }),
        makeUserEvent('internal event', { isMessage: false }),
      ];
      const result = calculateStats(events);
      expect(result.session.totalMessages).toBe(1);
    });

    it('counts agent messages with isMessage=true', () => {
      const events = [
        makeAgentEvent('response', { isMessage: true }),
        makeAgentEvent('llm_request_end', { isMessage: false }),
      ];
      const result = calculateStats(events);
      expect(result.session.totalMessages).toBe(1);
    });

    it('tracks message count per agent', () => {
      const events = [
        makeUserEvent('q1', { agentId: 'agent-a', isMessage: true }),
        makeAgentEvent('a1', { agentId: 'agent-a', isMessage: true }),
        makeUserEvent('q2', { agentId: 'agent-b', isMessage: true }),
      ];
      const result = calculateStats(events);
      
      const agentA = result.perAgent.find(a => a.agentId === 'agent-a')!;
      const agentB = result.perAgent.find(a => a.agentId === 'agent-b')!;
      
      expect(agentA.messageCount).toBe(2);
      expect(agentB.messageCount).toBe(1);
    });

    it('does not count messages without isMessage flag', () => {
      const events = [
        makeUserEvent('hello', { isMessage: false }),
        makeAgentEvent('response', { isMessage: false }),
      ];
      const result = calculateStats(events);
      // Without isMessage=true, messages shouldn't be counted
      expect(result.session.totalMessages).toBe(0);
    });
  });

  describe('tool call tracking', () => {
    it('counts tool calls correctly', () => {
      const events = [
        makeToolCallEvent('read_tool'),
        makeToolCallEvent('write_file'),
        makeToolCallEvent('read_tool'),
      ];
      const result = calculateStats(events);
      expect(result.session.totalToolCalls).toBe(3);
    });

    it('builds tool breakdown by kind', () => {
      const events = [
        makeToolCallEvent('read_tool'),
        makeToolCallEvent('write_file'),
        makeToolCallEvent('read_tool'),
      ];
      const result = calculateStats(events);
      expect(result.perAgent[0].toolBreakdown).toEqual({
        'read_tool': 2,
        'write_file': 1,
      });
    });

    it('uses "unknown" for tool calls without kind', () => {
      const events = [
        makeToolCallEvent('read_tool'),
        { ...makeToolCallEvent('x'), toolCall: { status: 'completed' } } as EventItem,
      ];
      const result = calculateStats(events);
      expect(result.perAgent[0].toolBreakdown['unknown']).toBe(1);
    });

    it('counts tool results separately', () => {
      const events = [
        makeToolCallEvent('read_tool'),
        makeToolResultEvent('tc-1'),
      ];
      const result = calculateStats(events);
      expect(result.perAgent[0].toolCallCount).toBe(1);
      expect(result.perAgent[0].toolResultCount).toBe(1);
    });

    it('tracks tools per agent independently', () => {
      const events = [
        makeToolCallEvent('read_tool', { agentId: 'agent-a' }),
        makeToolCallEvent('write_file', { agentId: 'agent-a' }),
        makeToolCallEvent('shell', { agentId: 'agent-b' }),
      ];
      const result = calculateStats(events);
      
      const agentA = result.perAgent.find(a => a.agentId === 'agent-a')!;
      const agentB = result.perAgent.find(a => a.agentId === 'agent-b')!;
      
      expect(agentA.toolCallCount).toBe(2);
      expect(agentB.toolCallCount).toBe(1);
    });
  });

  describe('context tracking', () => {
    it('uses latest contextTokens value (not accumulated)', () => {
      const events = [
        makeAgentEvent('r1', { contextTokens: 1000 }),
        makeAgentEvent('r2', { contextTokens: 2500 }),
        makeAgentEvent('r3', { contextTokens: 3000 }),
      ];
      const result = calculateStats(events);
      expect(result.perAgent[0].currentContextTokens).toBe(3000);
    });

    it('captures contextLimit from events', () => {
      const events = [
        makeAgentEvent('r1', { contextLimit: 8000 }),
      ];
      const result = calculateStats(events);
      expect(result.perAgent[0].maxContextTokens).toBe(8000);
    });

    it('tracks context per agent independently', () => {
      const events = [
        makeAgentEvent('r1', { agentId: 'agent-a', contextTokens: 1000 }),
        makeAgentEvent('r2', { agentId: 'agent-b', contextTokens: 2000 }),
      ];
      const result = calculateStats(events);
      
      const agentA = result.perAgent.find(a => a.agentId === 'agent-a')!;
      const agentB = result.perAgent.find(a => a.agentId === 'agent-b')!;
      
      expect(agentA.currentContextTokens).toBe(1000);
      expect(agentB.currentContextTokens).toBe(2000);
    });

    it('handles missing contextTokens gracefully', () => {
      const events = [
        makeAgentEvent('r1'), // No contextTokens
      ];
      const result = calculateStats(events);
      expect(result.perAgent[0].currentContextTokens).toBe(0); // Default value
    });
  });

  describe('steps and turns from metrics', () => {
    it('captures steps from event metrics', () => {
      const events = [
        makeAgentEvent('r1', { metrics: { steps: 3, turns: 1 } }),
        makeAgentEvent('r2', { metrics: { steps: 5, turns: 2 } }),
      ];
      const result = calculateStats(events);
      expect(result.perAgent[0].steps).toBe(5); // Latest
    });

    it('captures turns from event metrics', () => {
      const events = [
        makeAgentEvent('r1', { metrics: { steps: 1, turns: 1 } }),
        makeAgentEvent('r2', { metrics: { steps: 2, turns: 3 } }),
      ];
      const result = calculateStats(events);
      expect(result.perAgent[0].turns).toBe(3); // Latest
    });

    it('uses primary agent for session totals', () => {
      const events = [
        makeAgentEvent('r1', { agentId: 'primary', metrics: { steps: 10, turns: 5 } }),
        makeAgentEvent('r2', { agentId: 'delegate', metrics: { steps: 3, turns: 2 } }),
      ];
      const result = calculateStats(events);
      expect(result.session.totalSteps).toBe(10);
      expect(result.session.totalTurns).toBe(5);
    });

    it('falls back to first agent when no primary agent', () => {
      const events = [
        makeAgentEvent('r1', { agentId: 'agent-a', metrics: { steps: 7, turns: 4 } }),
      ];
      const result = calculateStats(events);
      expect(result.session.totalSteps).toBe(7);
      expect(result.session.totalTurns).toBe(4);
    });

    it('returns zero steps/turns when no metrics available', () => {
      const events = [
        makeAgentEvent('r1'), // No metrics
      ];
      const result = calculateStats(events);
      expect(result.session.totalSteps).toBe(0);
      expect(result.session.totalTurns).toBe(0);
    });
  });

  describe('session limits', () => {
    it('includes session limits when provided', () => {
      const limits = { max_steps: 100, max_turns: 10, max_cost_usd: 5.0 };
      const result = calculateStats([], limits);
      expect(result.session.limits).toEqual(limits);
    });

    it('handles null session limits', () => {
      const result = calculateStats([], null);
      expect(result.session.limits).toBeUndefined();
    });

    it('handles undefined session limits', () => {
      const result = calculateStats([]);
      expect(result.session.limits).toBeUndefined();
    });
  });

  describe('agent sorting', () => {
    it('sorts primary agent first', () => {
      const events = [
        makeAgentEvent('r1', { agentId: 'zebra' }),
        makeAgentEvent('r2', { agentId: 'primary' }),
        makeAgentEvent('r3', { agentId: 'alpha' }),
      ];
      const result = calculateStats(events);
      expect(result.perAgent[0].agentId).toBe('primary');
    });

    it('sorts remaining agents alphabetically', () => {
      const events = [
        makeAgentEvent('r1', { agentId: 'zebra' }),
        makeAgentEvent('r2', { agentId: 'alpha' }),
        makeAgentEvent('r3', { agentId: 'middle' }),
      ];
      const result = calculateStats(events);
      expect(result.perAgent.map(a => a.agentId)).toEqual(['alpha', 'middle', 'zebra']);
    });
  });

  describe('session start timestamp', () => {
    it('captures session start from first non-system event', () => {
      const events = [
        makeSystemEvent('init', { timestamp: 1000 }),
        makeUserEvent('hello', { timestamp: 2000 }),
        makeAgentEvent('response', { timestamp: 3000 }),
      ];
      const result = calculateStats(events);
      // System events are skipped, so start should be from first user event
      expect(result.session.startTimestamp).toBe(2000);
    });

    it('returns undefined startTimestamp for empty events', () => {
      const result = calculateStats([]);
      expect(result.session.startTimestamp).toBeUndefined();
    });
  });
});

describe('calculateDelegationStats', () => {
  beforeEach(() => {
    resetFixtureCounter();
  });

  function makeDelegationGroup(events: EventItem[]): DelegationGroupInfo {
    return {
      id: 'del-group-1',
      delegateToolCallId: 'tc-1',
      delegateEvent: events[0] as any,
      events: events as any[],
      status: 'completed',
      startTime: events[0]?.timestamp ?? 0,
    };
  }

  it('counts tool calls in delegation group', () => {
    const events = [
      makeToolCallEvent('read_tool'),
      makeToolCallEvent('write_file'),
    ];
    const result = calculateDelegationStats(makeDelegationGroup(events));
    expect(result.toolCallCount).toBe(2);
  });

  it('counts only agent messages with isMessage=true', () => {
    const events = [
      makeAgentEvent('response', { isMessage: true }),
      makeAgentEvent('llm_request_end', { isMessage: false }),
      makeUserEvent('prompt', { isMessage: true }), // Not counted (type !== 'agent')
    ];
    const result = calculateDelegationStats(makeDelegationGroup(events));
    expect(result.messageCount).toBe(1);
  });

  it('sums costUsd from all events', () => {
    const events = [
      makeAgentEvent('r1', { costUsd: 0.05 }),
      makeAgentEvent('r2', { costUsd: 0.10 }),
    ];
    const result = calculateDelegationStats(makeDelegationGroup(events));
    expect(result.costUsd).toBeCloseTo(0.15);
  });

  it('uses latest contextTokens value', () => {
    const events = [
      makeAgentEvent('r1', { contextTokens: 500 }),
      makeAgentEvent('r2', { contextTokens: 1500 }),
    ];
    const result = calculateDelegationStats(makeDelegationGroup(events));
    expect(result.contextTokens).toBe(1500);
  });

  it('captures contextLimit', () => {
    const events = [
      makeAgentEvent('r1', { contextLimit: 4000 }),
    ];
    const result = calculateDelegationStats(makeDelegationGroup(events));
    expect(result.contextLimit).toBe(4000);
  });

  it('calculates context percentage correctly', () => {
    const events = [
      makeAgentEvent('r1', { contextTokens: 2000, contextLimit: 4000 }),
    ];
    const result = calculateDelegationStats(makeDelegationGroup(events));
    expect(result.contextPercent).toBe(50);
  });

  it('caps context percentage at 100%', () => {
    const events = [
      makeAgentEvent('r1', { contextTokens: 5000, contextLimit: 4000 }),
    ];
    const result = calculateDelegationStats(makeDelegationGroup(events));
    expect(result.contextPercent).toBe(100);
  });

  it('returns undefined contextPercent when no limit', () => {
    const events = [
      makeAgentEvent('r1', { contextTokens: 2000 }),
    ];
    const result = calculateDelegationStats(makeDelegationGroup(events));
    expect(result.contextPercent).toBeUndefined();
  });

  it('returns undefined contextPercent when limit is zero', () => {
    const events = [
      makeAgentEvent('r1', { contextTokens: 2000, contextLimit: 0 }),
    ];
    const result = calculateDelegationStats(makeDelegationGroup(events));
    expect(result.contextPercent).toBeUndefined();
  });

  it('accumulates input/output tokens from usage', () => {
    const events = [
      makeAgentEvent('r1', { usage: { input_tokens: 100, output_tokens: 50 } }),
      makeAgentEvent('r2', { usage: { input_tokens: 200, output_tokens: 75 } }),
    ];
    const result = calculateDelegationStats(makeDelegationGroup(events));
    expect(result.inputTokens).toBe(300);
    expect(result.outputTokens).toBe(125);
  });

  it('captures latest steps and turns from metrics', () => {
    const events = [
      makeAgentEvent('r1', { metrics: { steps: 2, turns: 1 } }),
      makeAgentEvent('r2', { metrics: { steps: 5, turns: 3 } }),
    ];
    const result = calculateDelegationStats(makeDelegationGroup(events));
    expect(result.steps).toBe(5);
    expect(result.turns).toBe(3);
  });

  it('handles empty events array', () => {
    const result = calculateDelegationStats(makeDelegationGroup([]));
    expect(result.toolCallCount).toBe(0);
    expect(result.messageCount).toBe(0);
    expect(result.costUsd).toBe(0);
    expect(result.contextTokens).toBe(0);
    expect(result.inputTokens).toBe(0);
    expect(result.outputTokens).toBe(0);
    expect(result.steps).toBe(0);
    expect(result.turns).toBe(0);
  });
});
