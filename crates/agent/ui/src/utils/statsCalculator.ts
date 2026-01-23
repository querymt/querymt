import { EventItem, AgentStats, CalculatedStats, SessionStats } from '../types';

// Track agent working state during reconstruction
interface AgentTimingState {
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

export function calculateStats(events: EventItem[]): CalculatedStats {
  const statsMap = new Map<string, AgentStats>();
  const timingMap = new Map<string, AgentTimingState>();
  const globalState: GlobalTimerState = {
    hasStarted: false,
    accumulatedMs: 0,
  };
  
  // Track session totals
  let totalCostUsd = 0;
  let totalMessages = 0;
  let totalToolCalls = 0;
  let latestCumulativeCost: number | undefined;
  let sessionStartTimestamp: number | undefined;
  let sessionEndTimestamp: number | undefined;
  
  for (const event of events) {
    if (event.type === 'system') {
      continue;
    }
    const agentId = event.agentId || 'unknown';
    const timestamp = event.timestamp;
    
    // Track session start/end timestamps
    if (!sessionEndTimestamp || timestamp > sessionEndTimestamp) {
      sessionEndTimestamp = timestamp;
    }
    
    if (!statsMap.has(agentId)) {
      statsMap.set(agentId, {
        agentId,
        messageCount: 0,
        toolCallCount: 0,
        toolResultCount: 0,
        toolBreakdown: {},
        costUsd: 0,
        activeTimeMs: 0,
        currentContextTokens: 0,
        maxContextTokens: undefined,
      });
      timingMap.set(agentId, {
        isWorking: false,
        accumulatedMs: 0,
        activeDelegationIds: new Set(),
      });
    }
    
    const stats = statsMap.get(agentId)!;
    const timing = timingMap.get(agentId)!;
    
    // Time tracking logic
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
      sessionStartTimestamp = timestamp;
    }
    
    // Start working when prompt received
    if (isPromptReceived && !timing.isWorking && timing.activeDelegationIds.size === 0) {
      timing.isWorking = true;
      timing.workStartedAt = timestamp;
    }
    
    // Pause when delegating to another agent
    if (isDelegationRequested && event.delegationId) {
      timing.activeDelegationIds.add(event.delegationId);
      if (timing.isWorking && timing.workStartedAt !== undefined) {
        const elapsed = timestamp - timing.workStartedAt;
        timing.accumulatedMs += elapsed;
        timing.isWorking = false;
        timing.workStartedAt = undefined;
      }
    }
    
    // Resume when delegation completes
    if (isDelegationCompleted && event.delegationId) {
      timing.activeDelegationIds.delete(event.delegationId);
      if (timing.activeDelegationIds.size === 0 && !timing.isWorking) {
        timing.isWorking = true;
        timing.workStartedAt = timestamp;
      }
    }
    
    // Pause when waiting for user (llm_request_end with finish_reason: stop)
    if (isLlmRequestEnd && timing.isWorking && timing.activeDelegationIds.size === 0) {
      if (finishReason === 'stop') {
        if (timing.workStartedAt !== undefined) {
          const elapsed = timestamp - timing.workStartedAt;
          timing.accumulatedMs += elapsed;
        }
        timing.isWorking = false;
        timing.workStartedAt = undefined;
      }
      // If finishReason === 'tool_calls', keep timer running
    }
    
    switch (event.type) {
      case 'user':
      case 'agent':
        // Only count actual user/assistant messages, not internal events
        if (event.isMessage) {
          stats.messageCount++;
          totalMessages++;
        }
        break;
      case 'tool_call':
        stats.toolCallCount++;
        totalToolCalls++;
        const toolName = event.toolCall?.kind || 'unknown';
        stats.toolBreakdown[toolName] = (stats.toolBreakdown[toolName] || 0) + 1;
        break;
      case 'tool_result':
        stats.toolResultCount++;
        break;
    }
    
    // Track current context size from backend (not accumulated, just latest value)
    if (event.contextTokens !== undefined) {
      stats.currentContextTokens = event.contextTokens;
    }
    
    if (event.costUsd !== undefined) {
      stats.costUsd += event.costUsd;
      totalCostUsd += event.costUsd;
    }
    
    // Keep track of latest cumulative cost (most accurate for session total)
    if (event.cumulativeCostUsd !== undefined) {
      latestCumulativeCost = event.cumulativeCostUsd;
    }
    
    // Track context limit from provider_changed events
    if (event.contextLimit !== undefined) {
      stats.maxContextTokens = event.contextLimit;
    }
  }
  
  // Finalize timing: if agent is still working at end of events, accumulate remaining time
  for (const [agentId, timing] of timingMap.entries()) {
    const stats = statsMap.get(agentId)!;
    if (timing.isWorking && timing.workStartedAt !== undefined && sessionEndTimestamp !== undefined) {
      const elapsed = sessionEndTimestamp - timing.workStartedAt;
      timing.accumulatedMs += elapsed;
    }
    stats.activeTimeMs = timing.accumulatedMs;
  }
  
  // GLOBAL TIMER: Finalize accumulated time
  const anyAgentWorking = Array.from(timingMap.values()).some(t => t.isWorking);
  if (!anyAgentWorking && globalState.lastActiveAt !== undefined && sessionEndTimestamp !== undefined) {
    const elapsed = sessionEndTimestamp - globalState.lastActiveAt;
    globalState.accumulatedMs += elapsed;
  } else if (anyAgentWorking && globalState.lastActiveAt !== undefined && sessionEndTimestamp !== undefined) {
    // Still working at end - add time from last active to end
    const elapsed = sessionEndTimestamp - globalState.lastActiveAt;
    globalState.accumulatedMs += elapsed;
  }
  
  // Use cumulative cost if available, otherwise use sum of individual costs
  const sessionTotalCost = latestCumulativeCost ?? totalCostUsd;
  
  const perAgent = Array.from(statsMap.values()).sort((a, b) => 
    a.agentId === 'primary' ? -1 : b.agentId === 'primary' ? 1 : a.agentId.localeCompare(b.agentId)
  );
  
  const session: SessionStats = {
    totalCostUsd: sessionTotalCost,
    totalMessages,
    totalToolCalls,
    totalElapsedMs: globalState.accumulatedMs,
    startTimestamp: sessionStartTimestamp,
  };
  
  return { session, perAgent };
}
