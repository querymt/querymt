/**
 * Test fixtures factory for generating EventItem test data
 * 
 * Provides factory functions to create valid EventItem objects with sensible defaults
 * and support for partial overrides. Auto-incrementing IDs and timestamps ensure
 * deterministic test ordering.
 */

import { EventItem } from '../types';

// Auto-incrementing counter for unique IDs and timestamps
let counter = 0;

/**
 * Reset the fixture counter to 0.
 * Call this in beforeEach() to ensure deterministic test data.
 */
export function resetFixtureCounter(): void {
  counter = 0;
}

/**
 * Generate the next ID in the sequence
 */
function nextId(): string {
  return `evt-${++counter}`;
}

/**
 * Generate the next timestamp in the sequence
 */
function nextTimestamp(): number {
  return 1000 + counter;
}

/**
 * Create a base EventItem with sensible defaults
 */
export function makeEvent(overrides: Partial<EventItem> = {}): EventItem {
  const id = overrides.id ?? nextId();
  const timestamp = overrides.timestamp ?? nextTimestamp();
  
  return {
    id,
    agentId: 'primary',
    sessionId: 'session-1',
    type: 'agent',
    content: '',
    timestamp,
    ...overrides,
  };
}

/**
 * Create a user message event
 */
export function makeUserEvent(content: string, overrides: Partial<EventItem> = {}): EventItem {
  return makeEvent({
    type: 'user',
    content,
    isMessage: true,
    ...overrides,
  });
}

/**
 * Create an agent message event
 */
export function makeAgentEvent(content: string, overrides: Partial<EventItem> = {}): EventItem {
  return makeEvent({
    type: 'agent',
    content,
    isMessage: true,
    ...overrides,
  });
}

/**
 * Create a tool_call event
 */
export function makeToolCallEvent(toolKind: string, overrides: Partial<EventItem> = {}): EventItem {
  const toolCallId = overrides.toolCall?.tool_call_id ?? `${toolKind}:${nextId()}`;
  
  return makeEvent({
    type: 'tool_call',
    content: `Calling ${toolKind}`,
    toolCall: {
      tool_call_id: toolCallId,
      kind: toolKind,
      status: 'in_progress',
      description: `run ${toolKind}`,
      ...overrides.toolCall,
    },
    ...overrides,
  });
}

/**
 * Create a tool_result event
 */
export function makeToolResultEvent(toolCallId: string, overrides: Partial<EventItem> = {}): EventItem {
  return makeEvent({
    type: 'tool_result',
    content: 'Tool result',
    toolCall: {
      tool_call_id: toolCallId,
      status: 'completed',
      ...overrides.toolCall,
    },
    ...overrides,
  });
}

/**
 * Create a delegate tool_call event (kind='delegate')
 */
export function makeDelegateToolCallEvent(
  targetAgentId: string,
  overrides: Partial<EventItem> = {}
): EventItem {
  const toolCallId = overrides.toolCall?.tool_call_id ?? `delegate:${nextId()}`;
  
  return makeEvent({
    type: 'tool_call',
    content: `Delegating to ${targetAgentId}`,
    toolCall: {
      tool_call_id: toolCallId,
      kind: 'delegate',
      status: 'in_progress',
      description: `run delegate`,
      raw_input: {
        target_agent_id: targetAgentId,
        objective: 'Complete task',
        ...((overrides.toolCall?.raw_input as any) || {}),
      },
      ...overrides.toolCall,
    },
    ...overrides,
  });
}

/**
 * Create a delegation_requested event
 */
export function makeDelegationRequestedEvent(
  delegationId: string,
  targetAgentId: string,
  overrides: Partial<EventItem> = {}
): EventItem {
  return makeEvent({
    type: 'agent',
    content: `Delegation requested to ${targetAgentId}`,
    delegationId,
    delegationTargetAgentId: targetAgentId,
    delegationObjective: 'Complete task',
    delegationEventType: 'requested',
    ...overrides,
  });
}

/**
 * Create a delegation_completed event
 */
export function makeDelegationCompletedEvent(
  delegationId: string,
  overrides: Partial<EventItem> = {}
): EventItem {
  return makeEvent({
    type: 'agent',
    content: 'Delegation completed',
    delegationId,
    delegationEventType: 'completed',
    ...overrides,
  });
}

/**
 * Create a delegation_failed event
 */
export function makeDelegationFailedEvent(
  delegationId: string,
  overrides: Partial<EventItem> = {}
): EventItem {
  return makeEvent({
    type: 'agent',
    content: 'Delegation failed',
    delegationId,
    delegationEventType: 'failed',
    ...overrides,
  });
}

/**
 * Create a session_forked event
 * Note: These are agent-type events, not system events, so they get processed
 */
export function makeSessionForkedEvent(
  delegationId: string,
  childSessionId: string,
  overrides: Partial<EventItem> = {}
): EventItem {
  return makeEvent({
    type: 'agent',
    content: 'Session forked',
    forkDelegationId: delegationId,
    forkChildSessionId: childSessionId,
    ...overrides,
  });
}

/**
 * Create a system event
 */
export function makeSystemEvent(content: string, overrides: Partial<EventItem> = {}): EventItem {
  return makeEvent({
    type: 'system',
    content,
    ...overrides,
  });
}

/**
 * Create a provider_changed event
 */
export function makeProviderChangedEvent(
  provider: string,
  model: string,
  overrides: Partial<EventItem> = {}
): EventItem {
  return makeEvent({
    type: 'agent',
    content: `Provider changed to ${provider} / ${model}`,
    provider,
    model,
    configId: 1,
    contextLimit: 8000,
    ...overrides,
  });
}
