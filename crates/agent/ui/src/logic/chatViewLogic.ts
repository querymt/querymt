/**
 * chatViewLogic.ts - Pure business logic for ChatView
 * 
 * Contains all event processing functions extracted from ChatView.tsx.
 * These functions are pure, side-effect free, and fully testable.
 */

import { EventItem, EventRow, DelegationGroupInfo, Turn } from '../types';

// Model timeline entry
export interface ModelTimelineEntry {
  timestamp: number;
  provider: string;
  model: string;
  configId?: number;
  label: string; // "provider / model"
}

// Build model timeline from events
export function buildModelTimeline(events: EventItem[]): ModelTimelineEntry[] {
  const timeline: ModelTimelineEntry[] = [];
  for (const event of events) {
    if (event.provider && event.model) {
      timeline.push({
        timestamp: event.timestamp,
        provider: event.provider,
        model: event.model,
        configId: event.configId,
        label: `${event.provider} / ${event.model}`,
      });
    }
  }
  return timeline;
}

// Get active model at a given timestamp
export function getActiveModelAt(timeline: ModelTimelineEntry[], timestamp: number): ModelTimelineEntry | undefined {
  // Find the most recent model change before or at this timestamp
  let active: ModelTimelineEntry | undefined;
  for (const entry of timeline) {
    if (entry.timestamp <= timestamp) {
      active = entry;
    } else {
      break; // Timeline is sorted by timestamp
    }
  }
  return active;
}

// Check if session has multiple distinct models
export function hasMultipleModels(timeline: ModelTimelineEntry[]): boolean {
  const uniqueLabels = new Set(timeline.map(e => e.label));
  return uniqueLabels.size > 1;
}

// Build turns from event rows
export function buildTurns(events: EventItem[], sessionThinkingAgentId: string | null): {
  turns: Turn[];
  allEventRows: EventRow[];
  hasMultipleModels: boolean;
  delegations: DelegationGroupInfo[];
} {
  // First, build event rows with delegation grouping (from previous implementation)
  const { rows, delegationGroups } = buildEventRowsWithDelegations(events);
  
  // Build model timeline
  const modelTimeline = buildModelTimeline(events);
  const multipleModels = hasMultipleModels(modelTimeline);
  
  const turns: Turn[] = [];
  let currentTurn: Turn | null = null;
  let turnCounter = 0;

  for (const row of rows) {
    // Skip events that are part of a delegation (they'll be in the delegation group)
    if (row.delegationGroupId && !row.isDelegateToolCall) {
      continue;
    }

    // User message starts a new turn
    if (row.type === 'user') {
      // Close previous turn
      if (currentTurn) {
        currentTurn.endTime = currentTurn.endTime || row.timestamp;
        currentTurn.isActive = false;
        turns.push(currentTurn);
      }
      
      // Get active model at turn start
      const activeModel = getActiveModelAt(modelTimeline, row.timestamp);
      
      // Start new turn
      currentTurn = {
        id: `turn-${turnCounter++}`,
        userMessage: row,
        agentMessages: [],
        toolCalls: [],
        delegations: [],
        agentId: undefined,
        startTime: row.timestamp,
        endTime: undefined,
        isActive: true,
        modelLabel: activeModel?.label,
        modelConfigId: activeModel?.configId,
      };
    } else if (currentTurn) {
      // Add to current turn (only real messages, not internal events)
      if (row.type === 'agent' && row.isMessage) {
        currentTurn.agentMessages.push(row);
        if (!currentTurn.agentId && row.agentId) {
          currentTurn.agentId = row.agentId;
        }
        currentTurn.endTime = row.timestamp;
      } else if (row.type === 'tool_call' || row.type === 'tool_result') {
        // Only add tool_call events (results are merged)
        if (row.type === 'tool_call') {
          currentTurn.toolCalls.push(row);
          
          // Add delegation group if this is a delegate tool
          if (row.isDelegateToolCall && row.delegationGroupId) {
            const delGroup = delegationGroups.get(row.delegationGroupId);
            if (delGroup) {
              currentTurn.delegations.push(delGroup);
            }
          }
        }
        currentTurn.endTime = row.timestamp;
      }
      
      // Update model if changed during turn (from provider_changed event)
      if (row.provider && row.model) {
        currentTurn.modelLabel = `${row.provider} / ${row.model}`;
        currentTurn.modelConfigId = row.configId;
      }
    } else if (row.type === 'agent' && row.isMessage) {
      // No current turn (agent-initiated message)
      // Get active model at turn start
      const activeModel = getActiveModelAt(modelTimeline, row.timestamp);
      
      // Only create a turn if it's an actual message, not tool calls
      currentTurn = {
        id: `turn-${turnCounter++}`,
        userMessage: undefined,
        agentMessages: [row],
        toolCalls: [],
        delegations: [],
        agentId: row.agentId,
        startTime: row.timestamp,
        endTime: row.timestamp,
        isActive: true,
        modelLabel: activeModel?.label,
        modelConfigId: activeModel?.configId,
      };
    }
  }

  // Close final turn
  if (currentTurn) {
    currentTurn.isActive = sessionThinkingAgentId !== null;
    turns.push(currentTurn);
  }

  return {
    turns,
    allEventRows: rows,
    hasMultipleModels: multipleModels,
    delegations: Array.from(delegationGroups.values()).sort(
      (a, b) => a.startTime - b.startTime
    ),
  };
}

export function buildDelegationTurn(group: DelegationGroupInfo): Turn {
  const messageEvents = group.events.filter(
    (event) => event.type === 'agent' && event.isMessage
  );
  const toolCalls = group.events.filter((event) => event.type === 'tool_call');
  const firstTimestamp = group.events[0]?.timestamp ?? group.startTime;
  const lastTimestamp = group.events[group.events.length - 1]?.timestamp ?? group.endTime ?? group.startTime;

  // Build model timeline from the delegation's own child session events
  const modelTimeline = buildModelTimeline(group.events);
  const activeModel = getActiveModelAt(modelTimeline, firstTimestamp);

  return {
    id: `delegation-${group.id}`,
    userMessage: undefined,
    agentMessages: messageEvents,
    toolCalls,
    delegations: [],
    agentId: group.targetAgentId ?? group.agentId,
    startTime: firstTimestamp,
    endTime: group.endTime ?? lastTimestamp,
    isActive: group.status === 'in_progress',
    modelLabel: activeModel?.label,
    modelConfigId: activeModel?.configId,
  };
}

// Build event rows with delegation grouping (from previous implementation)
export function buildEventRowsWithDelegations(events: EventItem[]): {
  rows: EventRow[];
  delegationGroups: Map<string, DelegationGroupInfo>;
} {
  const rows: EventRow[] = [];
  const delegationGroups = new Map<string, DelegationGroupInfo>();
  const depthMap = new Map<string, number>();
  const toolCallMap = new Map<
    string,
    { eventId: string; depth: number; kind?: string; name?: string; rowIndex?: number }
  >();
  let currentAgentId: string | null = null;
  const pendingDelegationsByAgent = new Map<string, string[]>();
  const delegationIdToToolCall = new Map<string, string>();
  const activeDelegationByAgent = new Map<string, string>();

  const getDelegateTargetAgentId = (event: EventItem): string | undefined => {
    if (event.toolCall?.raw_input && typeof event.toolCall.raw_input === 'object') {
      const rawInput = event.toolCall.raw_input as {
        target_agent_id?: string;
        targetAgentId?: string;
      };
      return rawInput.target_agent_id ?? rawInput.targetAgentId;
    }
    return undefined;
  };

  const addPendingDelegation = (agentId: string, toolCallId: string) => {
    const pending = pendingDelegationsByAgent.get(agentId) ?? [];
    pending.push(toolCallId);
    pendingDelegationsByAgent.set(agentId, pending);
  };

  const takePendingDelegation = (agentId: string) => {
    const pending = pendingDelegationsByAgent.get(agentId);
    if (!pending || pending.length === 0) return undefined;
    const next = pending.shift();
    if (pending.length === 0) {
      pendingDelegationsByAgent.delete(agentId);
    }
    return next;
  };

  const ensureDelegationGroup = (toolCallId: string, fallbackEvent: EventItem) => {
    if (delegationGroups.has(toolCallId)) {
      return delegationGroups.get(toolCallId)!;
    }
    const delegateEvent: EventRow = {
      ...fallbackEvent,
      type: 'tool_call',
      content: fallbackEvent.content || 'delegate',
      depth: 1,
      toolCall: fallbackEvent.toolCall ?? { kind: 'delegate', status: 'in_progress' },
      isDelegateToolCall: true,
      delegationGroupId: toolCallId,
    };
    const group: DelegationGroupInfo = {
      id: toolCallId,
      delegateToolCallId: toolCallId,
      delegateEvent,
      events: [],
      status: 'in_progress',
      startTime: fallbackEvent.timestamp,
    };
    delegationGroups.set(toolCallId, group);
    return group;
  };

  for (const event of events) {
    if (event.type === 'system') {
      continue;
    }
    let depth = 0;
    let parentId: string | undefined;
    let toolName: string | undefined;
    let isDelegateToolCall = false;
    let delegationGroupId: string | undefined;

     if (event.delegationEventType === 'requested' && event.delegationId) {
       const targetAgentId = event.delegationTargetAgentId;
       const toolCallId = targetAgentId ? takePendingDelegation(targetAgentId) : undefined;
       const delegationKey = toolCallId ?? event.delegationId;
       delegationIdToToolCall.set(event.delegationId, delegationKey);
       const group = ensureDelegationGroup(delegationKey, event);
       group.delegationId = event.delegationId;
       group.targetAgentId = targetAgentId ?? group.targetAgentId;
       group.objective = event.delegationObjective ?? group.objective;
       group.startTime = event.timestamp;
       if (targetAgentId) {
         activeDelegationByAgent.set(targetAgentId, delegationKey);
       }
     }

     // Handle session_forked events to capture child session ID
     if (event.forkChildSessionId && event.forkDelegationId) {
       const delegationKey = delegationIdToToolCall.get(event.forkDelegationId) ?? event.forkDelegationId;
       const group = delegationGroups.get(delegationKey);
       if (group) {
         group.childSessionId = event.forkChildSessionId;
       }
     }

     if (event.delegationEventType === 'completed' && event.delegationId) {
       const delegationKey = delegationIdToToolCall.get(event.delegationId) ?? event.delegationId;
       const group = delegationGroups.get(delegationKey);
       if (group) {
         group.endTime = event.timestamp;
         if (group.status === 'in_progress') {
           group.status = 'completed';
         }
         if (group.targetAgentId) {
           activeDelegationByAgent.delete(group.targetAgentId);
         }
       }
     }

     if (event.delegationEventType === 'failed' && event.delegationId) {
       const delegationKey = delegationIdToToolCall.get(event.delegationId) ?? event.delegationId;
       const group = delegationGroups.get(delegationKey);
       if (group) {
         group.endTime = event.timestamp;
         group.status = 'failed';
         if (group.targetAgentId) {
           activeDelegationByAgent.delete(group.targetAgentId);
         }
       }
     }

     const activeDelegationKey = event.agentId
       ? activeDelegationByAgent.get(event.agentId)
       : undefined;
     if (activeDelegationKey) {
       delegationGroupId = activeDelegationKey;
     }

    if (event.type === 'tool_call') {
      const toolCallKey = event.toolCall?.tool_call_id ?? event.id;
      const delegationParent = delegationGroupId
        ? toolCallMap.get(delegationGroupId)?.eventId
        : null;
      const parentCandidate = delegationParent ?? currentAgentId;
      const parentDepth = parentCandidate ? depthMap.get(parentCandidate) ?? 0 : 0;
      depth = parentDepth + 1;
      parentId = parentCandidate ?? undefined;
      toolName = inferToolName(event);
      
      const rowIndex = rows.length;
      toolCallMap.set(toolCallKey, {
        eventId: event.id,
        depth,
        kind: event.toolCall?.kind,
        name: toolName,
        rowIndex,
      });

      // Check if this is a delegate tool call
      if (event.toolCall?.kind === 'delegate') {
        isDelegateToolCall = true;
        delegationGroupId = toolCallKey;
        const targetAgentId = getDelegateTargetAgentId(event);
        if (targetAgentId) {
          addPendingDelegation(targetAgentId, toolCallKey);
        }

        const group = ensureDelegationGroup(toolCallKey, event);
        group.targetAgentId = targetAgentId ?? group.targetAgentId;
        group.objective =
          group.objective ??
          ((event.toolCall?.raw_input as { objective?: string } | undefined)?.objective ??
            event.delegationObjective);
        group.delegateEvent = {
          ...event,
          depth,
          parentId,
          toolName,
          isDelegateToolCall: true,
          delegationGroupId: toolCallKey,
        };
      }
      
      // If we're inside a delegation, mark this event
      if (delegationGroupId && !isDelegateToolCall) {
        const group = delegationGroups.get(delegationGroupId);
        if (group) {
          const childRow: EventRow = { ...event, depth, parentId, toolName, delegationGroupId };
          group.events.push(childRow);
          if (event.agentId && !group.agentId) {
            group.agentId = event.agentId;
          }
        }
      }
      
      depthMap.set(event.id, depth);
      rows.push({ ...event, depth, parentId, toolName, isDelegateToolCall, delegationGroupId });
    } else if (event.type === 'tool_result') {
      const toolCallKey = event.toolCall?.tool_call_id;
      const toolParent = toolCallKey ? toolCallMap.get(toolCallKey) : undefined;
      
      if (toolParent && toolParent.rowIndex !== undefined) {
        // Merge result into the tool_call row
        const toolCallRow = rows[toolParent.rowIndex];
        if (toolCallRow) {
          toolCallRow.mergedResult = event;
        }
        
        // Also update delegation group's delegate event
        if (toolCallKey && delegationGroups.has(toolCallKey)) {
          const group = delegationGroups.get(toolCallKey)!;
          group.delegateEvent.mergedResult = event;
          if (event.toolCall?.status === 'failed') {
            group.status = 'failed';
          }
        }
        
        // Also update delegation group's events array copy (for tools within the delegation)
        if (toolCallRow?.delegationGroupId) {
          const group = delegationGroups.get(toolCallRow.delegationGroupId);
          if (group) {
            const groupEvent = group.events.find(
              e => e.toolCall?.tool_call_id === toolCallKey
            );
            if (groupEvent) {
              groupEvent.mergedResult = event;
            }
          }
        }
      } else {
        // No matching tool_call
        if (toolParent) {
          parentId = toolParent.eventId;
          depth = toolParent.depth + 1;
          toolName = toolParent.name;
        } else if (currentAgentId) {
          parentId = currentAgentId;
          depth = (depthMap.get(currentAgentId) ?? 0) + 1;
        } else {
          depth = 1;
        }
        
        // Check if inside a delegation
        if (delegationGroupId) {
          const group = delegationGroups.get(delegationGroupId);
          if (group) {
            group.events.push({ ...event, depth, parentId, toolName, delegationGroupId });
          }
        }
        
        depthMap.set(event.id, depth);
        rows.push({ ...event, depth, parentId, toolName, delegationGroupId });
      }
    } else {
      // user or agent event
      if (delegationGroupId) {
        const delegationDepth = toolCallMap.get(delegationGroupId)?.depth ?? 1;
        depth = delegationDepth + 1;
        parentId = toolCallMap.get(delegationGroupId)?.eventId;
        
        // Add to delegation group
        const group = delegationGroups.get(delegationGroupId);
        if (group) {
          group.events.push({ ...event, depth, parentId, toolName, delegationGroupId });
          if (event.agentId && !group.agentId) {
            group.agentId = event.agentId;
          }
        }
      }
      if (event.type === 'agent' && event.isMessage) {
        currentAgentId = event.id;
      }
      
      depthMap.set(event.id, depth);
      rows.push({ ...event, depth, parentId, toolName, delegationGroupId });
    }
  }

  return { rows, delegationGroups };
}

export function inferToolName(event: EventItem): string | undefined {
  const toolCallId = event.toolCall?.tool_call_id;
  if (typeof toolCallId === 'string' && toolCallId.includes(':')) {
    const name = toolCallId.split(':')[0];
    if (name) return name;
  }
  const desc = event.toolCall?.description;
  if (typeof desc === 'string') {
    const match = desc.match(/run\s+([a-z0-9_.:-]+)/i);
    if (match?.[1]) return match[1];
  }
  return undefined;
}
