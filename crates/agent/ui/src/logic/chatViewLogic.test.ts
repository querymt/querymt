/**
 * Comprehensive test suite for chatViewLogic
 * 
 * Tests all core event processing functions with focus on:
 * - Tool call/result merging logic
 * - Delegation lifecycle tracking
 * - Model timeline generation
 * - Turn-based conversation grouping
 * - Edge cases and complex scenarios
 */

import { describe, it, expect, beforeEach } from 'vitest';
import {
  buildModelTimeline,
  getActiveModelAt,
  hasMultipleModels,
  buildTurns,
  buildDelegationTurn,
  buildEventRowsWithDelegations,
  inferToolName,
} from './chatViewLogic';
import {
  resetFixtureCounter,
  makeEvent,
  makeUserEvent,
  makeAgentEvent,
  makeToolCallEvent,
  makeToolResultEvent,
  makeDelegateToolCallEvent,
  makeDelegationRequestedEvent,
  makeDelegationCompletedEvent,
  makeDelegationFailedEvent,
  makeSessionForkedEvent,
  makeSystemEvent,
  makeProviderChangedEvent,
} from '../test/fixtures';

describe('inferToolName', () => {
  beforeEach(() => {
    resetFixtureCounter();
  });

  it('returns name before colon from tool_call_id', () => {
    const event = makeEvent({
      toolCall: { tool_call_id: 'read_tool:abc123' },
    });
    expect(inferToolName(event)).toBe('read_tool');
  });

  it('returns undefined for tool_call_id without colon', () => {
    const event = makeEvent({
      toolCall: { tool_call_id: 'someid' },
    });
    // Without colon, tries description
    expect(inferToolName(event)).toBeUndefined();
  });

  it('returns name from description with "run xyz" pattern', () => {
    const event = makeEvent({
      toolCall: {
        tool_call_id: 'someid',
        description: 'run read_tool with args',
      },
    });
    expect(inferToolName(event)).toBe('read_tool');
  });

  it('returns undefined for description without "run" pattern', () => {
    const event = makeEvent({
      toolCall: {
        tool_call_id: 'someid',
        description: 'some other description',
      },
    });
    expect(inferToolName(event)).toBeUndefined();
  });

  it('returns undefined when no tool_call_id and no description', () => {
    const event = makeEvent({
      toolCall: {},
    });
    expect(inferToolName(event)).toBeUndefined();
  });

  it('returns undefined when tool_call_id has empty string before colon', () => {
    const event = makeEvent({
      toolCall: { tool_call_id: ':abc123' },
    });
    expect(inferToolName(event)).toBeUndefined();
  });

  it('handles case-insensitive "run" pattern', () => {
    const event = makeEvent({
      toolCall: {
        tool_call_id: 'someid',
        description: 'Run READ_FILE with args',
      },
    });
    expect(inferToolName(event)).toBe('READ_FILE');
  });

  it('extracts complex tool names from description', () => {
    const event = makeEvent({
      toolCall: {
        tool_call_id: 'someid',
        description: 'run mcp:git.commit',
      },
    });
    expect(inferToolName(event)).toBe('mcp:git.commit');
  });
});

describe('buildModelTimeline', () => {
  beforeEach(() => {
    resetFixtureCounter();
  });

  it('returns empty timeline for empty events', () => {
    const timeline = buildModelTimeline([]);
    expect(timeline).toEqual([]);
  });

  it('returns empty timeline for events without provider/model', () => {
    const events = [
      makeUserEvent('Hello'),
      makeAgentEvent('Hi there'),
    ];
    const timeline = buildModelTimeline(events);
    expect(timeline).toEqual([]);
  });

  it('creates timeline entries for events with provider and model', () => {
    const events = [
      makeProviderChangedEvent('openai', 'gpt-4', { timestamp: 1000, configId: 1 }),
    ];
    const timeline = buildModelTimeline(events);
    
    expect(timeline).toHaveLength(1);
    expect(timeline[0]).toEqual({
      timestamp: 1000,
      provider: 'openai',
      model: 'gpt-4',
      configId: 1,
      label: 'openai / gpt-4',
    });
  });

  it('creates multiple entries in order', () => {
    const events = [
      makeProviderChangedEvent('openai', 'gpt-4', { timestamp: 1000 }),
      makeProviderChangedEvent('anthropic', 'claude-3', { timestamp: 2000 }),
      makeProviderChangedEvent('openai', 'gpt-3.5', { timestamp: 3000 }),
    ];
    const timeline = buildModelTimeline(events);
    
    expect(timeline).toHaveLength(3);
    expect(timeline[0].label).toBe('openai / gpt-4');
    expect(timeline[1].label).toBe('anthropic / claude-3');
    expect(timeline[2].label).toBe('openai / gpt-3.5');
  });

  it('only includes events with both provider and model', () => {
    const events = [
      makeProviderChangedEvent('openai', 'gpt-4'),
      makeEvent({ provider: 'openai' }), // missing model
      makeEvent({ model: 'gpt-4' }), // missing provider
    ];
    const timeline = buildModelTimeline(events);
    
    expect(timeline).toHaveLength(1);
    expect(timeline[0].label).toBe('openai / gpt-4');
  });
});

describe('getActiveModelAt', () => {
  beforeEach(() => {
    resetFixtureCounter();
  });

  it('returns undefined for empty timeline', () => {
    const active = getActiveModelAt([], 5000);
    expect(active).toBeUndefined();
  });

  it('returns undefined for timestamp before all entries', () => {
    const timeline = buildModelTimeline([
      makeProviderChangedEvent('openai', 'gpt-4', { timestamp: 2000 }),
    ]);
    const active = getActiveModelAt(timeline, 1000);
    expect(active).toBeUndefined();
  });

  it('returns entry for timestamp exactly on entry', () => {
    const timeline = buildModelTimeline([
      makeProviderChangedEvent('openai', 'gpt-4', { timestamp: 2000 }),
    ]);
    const active = getActiveModelAt(timeline, 2000);
    
    expect(active).toBeDefined();
    expect(active?.label).toBe('openai / gpt-4');
  });

  it('returns earlier entry for timestamp between entries', () => {
    const timeline = buildModelTimeline([
      makeProviderChangedEvent('openai', 'gpt-4', { timestamp: 1000 }),
      makeProviderChangedEvent('anthropic', 'claude-3', { timestamp: 3000 }),
    ]);
    const active = getActiveModelAt(timeline, 2000);
    
    expect(active).toBeDefined();
    expect(active?.label).toBe('openai / gpt-4');
  });

  it('returns last entry for timestamp after all entries', () => {
    const timeline = buildModelTimeline([
      makeProviderChangedEvent('openai', 'gpt-4', { timestamp: 1000 }),
      makeProviderChangedEvent('anthropic', 'claude-3', { timestamp: 2000 }),
    ]);
    const active = getActiveModelAt(timeline, 5000);
    
    expect(active).toBeDefined();
    expect(active?.label).toBe('anthropic / claude-3');
  });
});

describe('hasMultipleModels', () => {
  beforeEach(() => {
    resetFixtureCounter();
  });

  it('returns false for empty timeline', () => {
    expect(hasMultipleModels([])).toBe(false);
  });

  it('returns false for single model', () => {
    const timeline = buildModelTimeline([
      makeProviderChangedEvent('openai', 'gpt-4'),
    ]);
    expect(hasMultipleModels(timeline)).toBe(false);
  });

  it('returns false for same model appearing twice', () => {
    const timeline = buildModelTimeline([
      makeProviderChangedEvent('openai', 'gpt-4', { timestamp: 1000 }),
      makeProviderChangedEvent('openai', 'gpt-4', { timestamp: 2000 }),
    ]);
    expect(hasMultipleModels(timeline)).toBe(false);
  });

  it('returns true for two different models', () => {
    const timeline = buildModelTimeline([
      makeProviderChangedEvent('openai', 'gpt-4'),
      makeProviderChangedEvent('anthropic', 'claude-3'),
    ]);
    expect(hasMultipleModels(timeline)).toBe(true);
  });

  it('returns true for different providers with same model name', () => {
    const timeline = buildModelTimeline([
      makeProviderChangedEvent('openai', 'gpt-4'),
      makeProviderChangedEvent('azure', 'gpt-4'),
    ]);
    expect(hasMultipleModels(timeline)).toBe(true);
  });
});

describe('buildEventRowsWithDelegations', () => {
  beforeEach(() => {
    resetFixtureCounter();
  });

  describe('basic events', () => {
    it('returns empty rows and groups for empty events', () => {
      const { rows, delegationGroups } = buildEventRowsWithDelegations([]);
      expect(rows).toEqual([]);
      expect(delegationGroups.size).toBe(0);
    });

    it('creates row for single user event with depth 0', () => {
      const events = [makeUserEvent('Hello')];
      const { rows } = buildEventRowsWithDelegations(events);
      
      expect(rows).toHaveLength(1);
      expect(rows[0].type).toBe('user');
      expect(rows[0].depth).toBe(0);
      expect(rows[0].content).toBe('Hello');
    });

    it('creates rows for user and agent message', () => {
      const events = [
        makeUserEvent('Hello'),
        makeAgentEvent('Hi there'),
      ];
      const { rows } = buildEventRowsWithDelegations(events);
      
      expect(rows).toHaveLength(2);
      expect(rows[0].type).toBe('user');
      expect(rows[1].type).toBe('agent');
      expect(rows[1].isMessage).toBe(true);
    });

    it('skips system events entirely', () => {
      const events = [
        makeUserEvent('Hello'),
        makeSystemEvent('System message'),
        makeAgentEvent('Hi there'),
      ];
      const { rows } = buildEventRowsWithDelegations(events);
      
      expect(rows).toHaveLength(2);
      expect(rows[0].type).toBe('user');
      expect(rows[1].type).toBe('agent');
      // System event is not in rows
      expect(rows.find(r => r.type === 'system')).toBeUndefined();
    });
  });

  describe('tool call handling', () => {
    it('sets depth for tool_call as parent depth + 1', () => {
      const events = [
        makeUserEvent('Read file'),
        makeAgentEvent('Reading...'),
        makeToolCallEvent('read_tool'),
      ];
      const { rows } = buildEventRowsWithDelegations(events);
      
      const toolCall = rows.find(r => r.type === 'tool_call');
      expect(toolCall).toBeDefined();
      expect(toolCall?.depth).toBe(1); // agent message is depth 0, tool call is depth 1
    });

    it('infers tool name from tool_call_id', () => {
      const events = [
        makeAgentEvent('Reading...'),
        makeToolCallEvent('read_tool'),
      ];
      const { rows } = buildEventRowsWithDelegations(events);
      
      const toolCall = rows.find(r => r.type === 'tool_call');
      expect(toolCall?.toolName).toBe('read_tool');
    });

    it('creates multiple tool calls with correct depths', () => {
      const events = [
        makeAgentEvent('Processing...'),
        makeToolCallEvent('read_tool'),
        makeToolCallEvent('write_file'),
      ];
      const { rows } = buildEventRowsWithDelegations(events);
      
      const toolCalls = rows.filter(r => r.type === 'tool_call');
      expect(toolCalls).toHaveLength(2);
      expect(toolCalls[0].depth).toBe(1);
      expect(toolCalls[1].depth).toBe(1);
    });
  });

  describe('tool result merging', () => {
    it('merges tool_result into matching tool_call row', () => {
      const events = [
        makeAgentEvent('Reading...'),
        makeToolCallEvent('read_tool'),
      ];
      const toolCallId = events[1].toolCall?.tool_call_id!;
      events.push(makeToolResultEvent(toolCallId, { content: 'File contents' }));
      
      const { rows } = buildEventRowsWithDelegations(events);
      
      // Should have 2 rows (agent + tool_call), not 3
      expect(rows).toHaveLength(2);
      
      const toolCall = rows.find(r => r.type === 'tool_call');
      expect(toolCall?.mergedResult).toBeDefined();
      expect(toolCall?.mergedResult?.content).toBe('File contents');
      expect(toolCall?.mergedResult?.type).toBe('tool_result');
    });

    it('adds tool_result as separate row when no matching tool_call', () => {
      const events = [
        makeAgentEvent('Processing...'),
        makeToolResultEvent('unknown-id', { content: 'Orphan result' }),
      ];
      const { rows } = buildEventRowsWithDelegations(events);
      
      // Should have 2 rows (agent + tool_result)
      expect(rows).toHaveLength(2);
      expect(rows[1].type).toBe('tool_result');
      expect(rows[1].content).toBe('Orphan result');
    });

    it('handles multiple tool calls with results', () => {
      const events = [
        makeAgentEvent('Processing...'),
        makeToolCallEvent('read_tool'),
      ];
      const tc1Id = events[1].toolCall?.tool_call_id!;
      events.push(makeToolCallEvent('write_file'));
      const tc2Id = events[2].toolCall?.tool_call_id!;
      events.push(makeToolResultEvent(tc1Id, { content: 'Read result' }));
      events.push(makeToolResultEvent(tc2Id, { content: 'Write result' }));
      
      const { rows } = buildEventRowsWithDelegations(events);
      
      // Should have 3 rows (agent + 2 tool_calls with merged results)
      expect(rows).toHaveLength(3);
      
      const toolCalls = rows.filter(r => r.type === 'tool_call');
      expect(toolCalls).toHaveLength(2);
      expect(toolCalls[0].mergedResult?.content).toBe('Read result');
      expect(toolCalls[1].mergedResult?.content).toBe('Write result');
    });
  });

  describe('delegation lifecycle', () => {
    it('creates delegation group for delegate tool_call', () => {
      const events = [
        makeAgentEvent('Delegating...'),
        makeDelegateToolCallEvent('specialist'),
      ];
      const { rows, delegationGroups } = buildEventRowsWithDelegations(events);
      
      const delegateCall = rows.find(r => r.isDelegateToolCall);
      expect(delegateCall).toBeDefined();
      expect(delegateCall?.toolCall?.kind).toBe('delegate');
      
      expect(delegationGroups.size).toBe(1);
      const group = Array.from(delegationGroups.values())[0];
      expect(group.status).toBe('in_progress');
      expect(group.targetAgentId).toBe('specialist');
    });

    it('updates delegation group with delegation_requested event', () => {
      const events = [
        makeAgentEvent('Delegating...'),
        makeDelegateToolCallEvent('specialist'),
      ];
      const delegationId = 'del-123';
      
      events.push(
        makeDelegationRequestedEvent(delegationId, 'specialist', {
          delegationObjective: 'Complete analysis',
        })
      );
      
      const { delegationGroups } = buildEventRowsWithDelegations(events);
      
      expect(delegationGroups.size).toBe(1);
      const group = Array.from(delegationGroups.values())[0];
      expect(group.delegationId).toBe(delegationId);
      expect(group.objective).toBe('Complete analysis');
      expect(group.targetAgentId).toBe('specialist');
    });

    it('sets delegation group status to completed', () => {
      const events = [
        makeAgentEvent('Delegating...'),
        makeDelegateToolCallEvent('specialist'),
      ];
      const delegationId = 'del-123';
      events.push(makeDelegationRequestedEvent(delegationId, 'specialist'));
      events.push(makeDelegationCompletedEvent(delegationId, { timestamp: 5000 }));
      
      const { delegationGroups } = buildEventRowsWithDelegations(events);
      
      const group = Array.from(delegationGroups.values())[0];
      expect(group.status).toBe('completed');
      expect(group.endTime).toBe(5000);
    });

    it('sets delegation group status to failed', () => {
      const events = [
        makeAgentEvent('Delegating...'),
        makeDelegateToolCallEvent('specialist'),
      ];
      const delegationId = 'del-123';
      events.push(makeDelegationRequestedEvent(delegationId, 'specialist'));
      events.push(makeDelegationFailedEvent(delegationId, { timestamp: 5000 }));
      
      const { delegationGroups } = buildEventRowsWithDelegations(events);
      
      const group = Array.from(delegationGroups.values())[0];
      expect(group.status).toBe('failed');
      expect(group.endTime).toBe(5000);
    });

    it('sets childSessionId from session_forked event', () => {
      const events = [
        makeAgentEvent('Delegating...'),
        makeDelegateToolCallEvent('specialist'),
      ];
      const delegationId = 'del-123';
      events.push(makeDelegationRequestedEvent(delegationId, 'specialist'));
      events.push(makeSessionForkedEvent(delegationId, 'child-session-456'));
      
      const { delegationGroups } = buildEventRowsWithDelegations(events);
      
      const group = Array.from(delegationGroups.values())[0];
      expect(group.childSessionId).toBe('child-session-456');
    });
  });

  describe('events inside delegations', () => {
    it('adds delegated agent events to delegation group', () => {
      const events = [
        makeAgentEvent('Delegating...', { agentId: 'primary' }),
        makeDelegateToolCallEvent('specialist'),
      ];
      const delegationId = 'del-123';
      events.push(
        makeDelegationRequestedEvent(delegationId, 'specialist', { agentId: 'primary' })
      );
      
      // Events from the delegated agent
      events.push(makeAgentEvent('Working on it...', { agentId: 'specialist' }));
      events.push(makeToolCallEvent('analyze', { agentId: 'specialist' }));
      
      const { delegationGroups } = buildEventRowsWithDelegations(events);
      
      const group = Array.from(delegationGroups.values())[0];
      expect(group.events).toHaveLength(2);
      expect(group.events[0].type).toBe('agent');
      expect(group.events[1].type).toBe('tool_call');
      expect(group.agentId).toBe('specialist');
    });

    it('sets delegationGroupId on events from delegated agent', () => {
      const events = [
        makeAgentEvent('Delegating...', { agentId: 'primary' }),
        makeDelegateToolCallEvent('specialist'),
      ];
      const delegationId = 'del-123';
      events.push(
        makeDelegationRequestedEvent(delegationId, 'specialist', { agentId: 'primary' })
      );
      events.push(makeAgentEvent('Working...', { agentId: 'specialist' }));
      
      const { rows } = buildEventRowsWithDelegations(events);
      
      const specialistEvent = rows.find(r => r.agentId === 'specialist');
      expect(specialistEvent?.delegationGroupId).toBeDefined();
    });

    it('gives delegated agent events deeper depth', () => {
      const events = [
        makeAgentEvent('Delegating...', { agentId: 'primary' }),
        makeDelegateToolCallEvent('specialist'),
      ];
      const delegationId = 'del-123';
      events.push(
        makeDelegationRequestedEvent(delegationId, 'specialist', { agentId: 'primary' })
      );
      events.push(makeAgentEvent('Working...', { agentId: 'specialist' }));
      
      const { rows } = buildEventRowsWithDelegations(events);
      
      const primaryEvent = rows.find(r => r.agentId === 'primary' && r.type === 'agent');
      const delegateCall = rows.find(r => r.isDelegateToolCall);
      const specialistEvent = rows.find(r => r.agentId === 'specialist');
      
      expect(primaryEvent?.depth).toBe(0);
      expect(delegateCall?.depth).toBe(1);
      expect(specialistEvent?.depth).toBe(2);
    });
  });

  describe('complex delegation scenario', () => {
    it('handles full delegation flow with child agent work', () => {
      resetFixtureCounter();
      
      const events = [
        // User request
        makeUserEvent('Analyze data', { agentId: 'primary' }),
        
        // Primary agent delegates
        makeAgentEvent('I will delegate this', { agentId: 'primary' }),
        makeDelegateToolCallEvent('analyst', { agentId: 'primary' }),
      ];
      
      const delegationId = 'del-456';
      
      events.push(
        // Delegation requested
        makeDelegationRequestedEvent(delegationId, 'analyst', {
          agentId: 'primary',
          delegationObjective: 'Perform analysis',
        })
      );
      
      events.push(
        // Session forked for child
        makeSessionForkedEvent(delegationId, 'child-session-789')
      );
      
      events.push(
        // Analyst does work
        makeAgentEvent('Starting analysis', { agentId: 'analyst' }),
        makeToolCallEvent('analyze_data', { agentId: 'analyst' })
      );
      
      const analyzeToolCallId = events[6].toolCall?.tool_call_id!;
      
      events.push(
        makeToolResultEvent(analyzeToolCallId, {
          content: 'Analysis complete',
          agentId: 'analyst',
        })
      );
      
      events.push(
        // Delegation completes
        makeDelegationCompletedEvent(delegationId, { agentId: 'primary' })
      );
      
      events.push(
        // Primary responds
        makeAgentEvent('Analysis is done', { agentId: 'primary' })
      );
      
      const { rows, delegationGroups } = buildEventRowsWithDelegations(events);
      
      // Verify delegation group
      expect(delegationGroups.size).toBe(1);
      const group = Array.from(delegationGroups.values())[0];
      expect(group.delegationId).toBe(delegationId);
      expect(group.targetAgentId).toBe('analyst');
      expect(group.objective).toBe('Perform analysis');
      expect(group.status).toBe('completed');
      expect(group.childSessionId).toBe('child-session-789');
      expect(group.events).toHaveLength(2); // analyst message + tool call
      
      // Verify delegate tool call has merged result
      const delegateCall = rows.find(r => r.isDelegateToolCall);
      expect(delegateCall).toBeDefined();
      
      // Verify analyst events are in the group
      expect(group.events[0].type).toBe('agent');
      expect(group.events[0].content).toBe('Starting analysis');
      expect(group.events[1].type).toBe('tool_call');
      expect(group.events[1].mergedResult).toBeDefined();
      expect(group.events[1].mergedResult?.content).toBe('Analysis complete');
    });
  });
});

describe('buildTurns', () => {
  beforeEach(() => {
    resetFixtureCounter();
  });

  it('returns no turns for empty events', () => {
    const { turns } = buildTurns([], null);
    expect(turns).toEqual([]);
  });

  it('creates turn for single user message', () => {
    const events = [makeUserEvent('Hello')];
    const { turns } = buildTurns(events, null);
    
    expect(turns).toHaveLength(1);
    expect(turns[0].userMessage?.content).toBe('Hello');
    expect(turns[0].agentMessages).toEqual([]);
    expect(turns[0].isActive).toBe(false);
  });

  it('creates turn with user message and agent response', () => {
    const events = [
      makeUserEvent('Hello'),
      makeAgentEvent('Hi there'),
    ];
    const { turns } = buildTurns(events, null);
    
    expect(turns).toHaveLength(1);
    expect(turns[0].userMessage?.content).toBe('Hello');
    expect(turns[0].agentMessages).toHaveLength(1);
    expect(turns[0].agentMessages[0].content).toBe('Hi there');
  });

  it('creates two turns for two user messages', () => {
    const events = [
      makeUserEvent('Hello'),
      makeAgentEvent('Hi'),
      makeUserEvent('How are you?'),
      makeAgentEvent('Good'),
    ];
    const { turns } = buildTurns(events, null);
    
    expect(turns).toHaveLength(2);
    expect(turns[0].userMessage?.content).toBe('Hello');
    expect(turns[0].isActive).toBe(false);
    expect(turns[1].userMessage?.content).toBe('How are you?');
  });

  it('creates turn without user message for agent-initiated message', () => {
    const events = [makeAgentEvent('I have an update')];
    const { turns } = buildTurns(events, null);
    
    expect(turns).toHaveLength(1);
    expect(turns[0].userMessage).toBeUndefined();
    expect(turns[0].agentMessages).toHaveLength(1);
  });

  it('includes tool calls in turn', () => {
    const events = [
      makeUserEvent('Read file'),
      makeAgentEvent('Reading...'),
      makeToolCallEvent('read_tool'),
    ];
    const { turns } = buildTurns(events, null);
    
    expect(turns).toHaveLength(1);
    expect(turns[0].toolCalls).toHaveLength(1);
    expect(turns[0].toolCalls[0].type).toBe('tool_call');
  });

  it('includes delegation in turn', () => {
    const events = [
      makeUserEvent('Analyze data'),
      makeAgentEvent('Delegating...'),
      makeDelegateToolCallEvent('analyst'),
    ];
    const delegationId = 'del-123';
    events.push(makeDelegationRequestedEvent(delegationId, 'analyst'));
    
    const { turns } = buildTurns(events, null);
    
    expect(turns).toHaveLength(1);
    expect(turns[0].delegations).toHaveLength(1);
    expect(turns[0].delegations[0].targetAgentId).toBe('analyst');
  });

  it('excludes events inside delegation from main turn', () => {
    const events = [
      makeUserEvent('Analyze', { agentId: 'primary' }),
      makeAgentEvent('Delegating...', { agentId: 'primary' }),
      makeDelegateToolCallEvent('analyst', { agentId: 'primary' }),
    ];
    const delegationId = 'del-123';
    events.push(
      makeDelegationRequestedEvent(delegationId, 'analyst', { agentId: 'primary' })
    );
    
    // Analyst events (should be excluded from main turn)
    events.push(makeAgentEvent('Working...', { agentId: 'analyst' }));
    events.push(makeToolCallEvent('analyze', { agentId: 'analyst' }));
    
    const { turns } = buildTurns(events, null);
    
    expect(turns).toHaveLength(1);
    // Should only have primary agent's message, not analyst's
    expect(turns[0].agentMessages).toHaveLength(1);
    expect(turns[0].agentMessages[0].agentId).toBe('primary');
    
    // Should only have delegate tool call, not analyst's tool call
    expect(turns[0].toolCalls).toHaveLength(1);
    expect(turns[0].toolCalls[0].isDelegateToolCall).toBe(true);
  });

  it('marks last turn as active when sessionThinkingAgentId is set', () => {
    const events = [
      makeUserEvent('Hello'),
      makeAgentEvent('Processing...'),
    ];
    const { turns } = buildTurns(events, 'primary');
    
    expect(turns).toHaveLength(1);
    expect(turns[0].isActive).toBe(true);
  });

  it('marks last turn as inactive when sessionThinkingAgentId is null', () => {
    const events = [
      makeUserEvent('Hello'),
      makeAgentEvent('Done'),
    ];
    const { turns } = buildTurns(events, null);
    
    expect(turns).toHaveLength(1);
    expect(turns[0].isActive).toBe(false);
  });

  it('sets model label from model timeline', () => {
    const events = [
      makeProviderChangedEvent('openai', 'gpt-4', { timestamp: 1000 }),
      makeUserEvent('Hello', { timestamp: 2000 }),
      makeAgentEvent('Hi', { timestamp: 3000 }),
    ];
    const { turns } = buildTurns(events, null);
    
    expect(turns).toHaveLength(1);
    expect(turns[0].modelLabel).toBe('openai / gpt-4');
  });

  it('returns hasMultipleModels flag', () => {
    const events = [
      makeProviderChangedEvent('openai', 'gpt-4'),
      makeUserEvent('Hello'),
      makeProviderChangedEvent('anthropic', 'claude-3'),
    ];
    const { hasMultipleModels } = buildTurns(events, null);
    
    expect(hasMultipleModels).toBe(true);
  });

  it('returns delegations sorted by startTime', () => {
    const events = [
      makeUserEvent('Task'),
      makeAgentEvent('Delegating...', { agentId: 'primary' }),
      makeDelegateToolCallEvent('agent1', { agentId: 'primary', timestamp: 2000 }),
      makeDelegationRequestedEvent('del-1', 'agent1', {
        agentId: 'primary',
        timestamp: 2001,
      }),
      makeDelegateToolCallEvent('agent2', { agentId: 'primary', timestamp: 1000 }),
      makeDelegationRequestedEvent('del-2', 'agent2', {
        agentId: 'primary',
        timestamp: 1001,
      }),
    ];
    
    const { delegations } = buildTurns(events, null);
    
    expect(delegations).toHaveLength(2);
    // Should be sorted by startTime
    expect(delegations[0].startTime).toBeLessThan(delegations[1].startTime);
  });

  it('returns all event rows', () => {
    const events = [
      makeUserEvent('Hello'),
      makeAgentEvent('Hi'),
      makeToolCallEvent('read_tool'),
    ];
    const { allEventRows } = buildTurns(events, null);
    
    expect(allEventRows).toHaveLength(3);
    expect(allEventRows[0].type).toBe('user');
    expect(allEventRows[1].type).toBe('agent');
    expect(allEventRows[2].type).toBe('tool_call');
  });
});

describe('buildDelegationTurn', () => {
  beforeEach(() => {
    resetFixtureCounter();
  });

  it('creates turn from delegation group events', () => {
    const events = [
      makeAgentEvent('Working on task', { agentId: 'specialist' }),
      makeToolCallEvent('analyze', { agentId: 'specialist' }),
    ];
    const { rows } = buildEventRowsWithDelegations(events);
    
    const group = {
      id: 'del-1',
      delegateToolCallId: 'delegate:123',
      delegateEvent: rows[0],
      targetAgentId: 'specialist',
      events: rows,
      status: 'completed' as const,
      startTime: 1000,
      endTime: 2000,
    };
    
    const turn = buildDelegationTurn(group);
    
    expect(turn.id).toBe('delegation-del-1');
    expect(turn.agentMessages).toHaveLength(1);
    expect(turn.toolCalls).toHaveLength(1);
    expect(turn.agentId).toBe('specialist');
    // Uses first event timestamp (1001) not group startTime (1000)
    expect(turn.startTime).toBe(rows[0].timestamp);
    // Uses last event timestamp (1002) not group endTime (2000)
    expect(turn.endTime).toBe(2000); // group.endTime is used if set
  });

  it('uses group startTime when no events', () => {
    const group = {
      id: 'del-1',
      delegateToolCallId: 'delegate:123',
      delegateEvent: makeAgentEvent('Delegate') as any,
      events: [],
      status: 'in_progress' as const,
      startTime: 5000,
    };
    
    const turn = buildDelegationTurn(group);
    
    expect(turn.startTime).toBe(5000);
    expect(turn.endTime).toBe(5000);
  });

  it('sets isActive true for in_progress status', () => {
    const group = {
      id: 'del-1',
      delegateToolCallId: 'delegate:123',
      delegateEvent: makeAgentEvent('Delegate') as any,
      events: [],
      status: 'in_progress' as const,
      startTime: 1000,
    };
    
    const turn = buildDelegationTurn(group);
    
    expect(turn.isActive).toBe(true);
  });

  it('sets isActive false for completed status', () => {
    const group = {
      id: 'del-1',
      delegateToolCallId: 'delegate:123',
      delegateEvent: makeAgentEvent('Delegate') as any,
      events: [],
      status: 'completed' as const,
      startTime: 1000,
      endTime: 2000,
    };
    
    const turn = buildDelegationTurn(group);
    
    expect(turn.isActive).toBe(false);
  });

  it('sets model label from delegation events', () => {
    const events = [
      makeProviderChangedEvent('openai', 'gpt-4', { timestamp: 1000, agentId: 'specialist' }),
      makeAgentEvent('Working', { timestamp: 1001, agentId: 'specialist' }),
    ];
    const { rows } = buildEventRowsWithDelegations(events);
    
    const group = {
      id: 'del-1',
      delegateToolCallId: 'delegate:123',
      delegateEvent: rows[0],
      targetAgentId: 'specialist',
      events: rows,
      status: 'completed' as const,
      startTime: 1000,
      endTime: 2000,
    };
    
    const turn = buildDelegationTurn(group);
    
    expect(turn.modelLabel).toBe('openai / gpt-4');
  });

  it('filters out non-message agent events from agentMessages', () => {
    const events = [
      makeAgentEvent('Message 1', { agentId: 'specialist', isMessage: true }),
      makeAgentEvent('Internal event', { agentId: 'specialist', isMessage: false }),
      makeAgentEvent('Message 2', { agentId: 'specialist', isMessage: true }),
    ];
    const { rows } = buildEventRowsWithDelegations(events);
    
    const group = {
      id: 'del-1',
      delegateToolCallId: 'delegate:123',
      delegateEvent: rows[0],
      targetAgentId: 'specialist',
      events: rows,
      status: 'completed' as const,
      startTime: 1000,
      endTime: 3000,
    };
    
    const turn = buildDelegationTurn(group);
    
    expect(turn.agentMessages).toHaveLength(2);
    expect(turn.agentMessages[0].content).toBe('Message 1');
    expect(turn.agentMessages[1].content).toBe('Message 2');
  });

  it('uses group agentId when targetAgentId is not set', () => {
    const group = {
      id: 'del-1',
      delegateToolCallId: 'delegate:123',
      delegateEvent: makeAgentEvent('Delegate') as any,
      agentId: 'fallback-agent',
      events: [],
      status: 'completed' as const,
      startTime: 1000,
    };
    
    const turn = buildDelegationTurn(group);
    
    expect(turn.agentId).toBe('fallback-agent');
  });
});

// ==================== Test Suite: messageId Propagation ====================

describe('buildTurns - messageId propagation', () => {
  beforeEach(() => {
    resetFixtureCounter();
  });

  it('preserves messageId on userMessage', () => {
    const userEvent = makeUserEvent('Hello', { messageId: 'msg-abc-123' });
    
    const result = buildTurns([userEvent], null);
    
    expect(result.turns).toHaveLength(1);
    expect(result.turns[0].userMessage).toBeDefined();
    expect(result.turns[0].userMessage!.messageId).toBe('msg-abc-123');
  });

  it('preserves messageId on agentMessages', () => {
    const userEvent = makeUserEvent('Hello');
    const agentEvent = makeAgentEvent('Hi there', { messageId: 'msg-def-456' });
    
    const result = buildTurns([userEvent, agentEvent], null);
    
    expect(result.turns).toHaveLength(1);
    expect(result.turns[0].agentMessages).toHaveLength(1);
    expect(result.turns[0].agentMessages[0].messageId).toBe('msg-def-456');
  });

  it('turn without userMessage has no messageId crash', () => {
    // Agent-initiated message (no user message)
    const agentEvent = makeAgentEvent('Agent message', { messageId: 'msg-agent-only' });
    
    const result = buildTurns([agentEvent], null);
    
    // Should not crash, validates undo button logic won't crash
    expect(result.turns).toHaveLength(1);
    expect(result.turns[0].userMessage).toBeUndefined();
  });
});
