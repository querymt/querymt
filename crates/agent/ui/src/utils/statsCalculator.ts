import { EventItem, AgentStats, CalculatedStats, SessionStats, SessionLimits, DelegationGroupInfo } from '../types';

export function calculateStats(events: EventItem[], sessionLimits?: SessionLimits | null): CalculatedStats {
  const statsMap = new Map<string, AgentStats>();
  
  // Track session totals
  let totalCostUsd = 0;
  let totalMessages = 0;
  let totalToolCalls = 0;
  let latestCumulativeCost: number | undefined;
  let sessionStartTimestamp: number | undefined;
  
  for (const event of events) {
    if (event.type === 'system') {
      continue;
    }
    const agentId = event.agentId || 'unknown';
    const timestamp = event.timestamp;
    
    // Track session start timestamp from first non-system event
    if (sessionStartTimestamp === undefined) {
      sessionStartTimestamp = timestamp;
    }
    
    if (!statsMap.has(agentId)) {
      statsMap.set(agentId, {
        agentId,
        messageCount: 0,
        toolCallCount: 0,
        toolResultCount: 0,
        toolBreakdown: {},
        costUsd: 0,
        currentContextTokens: 0,
        maxContextTokens: undefined,
        steps: 0,
        turns: 0,
      });
    }
    
    const stats = statsMap.get(agentId)!;
    
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
    
    // Track steps and turns from backend metrics
    if (event.metrics) {
      stats.steps = event.metrics.steps;
      stats.turns = event.metrics.turns;
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
  
  // Use cumulative cost if available, otherwise use sum of individual costs
  const sessionTotalCost = latestCumulativeCost ?? totalCostUsd;
  
  const perAgent = Array.from(statsMap.values()).sort((a, b) => 
    a.agentId === 'primary' ? -1 : b.agentId === 'primary' ? 1 : a.agentId.localeCompare(b.agentId)
  );
  
  // Calculate total steps and turns from the primary agent (or first agent with data)
  const primaryStats = perAgent.find(s => s.agentId === 'primary') ?? perAgent[0];
  const totalSteps = primaryStats?.steps ?? 0;
  const totalTurns = primaryStats?.turns ?? 0;
  
  const session: SessionStats = {
    totalCostUsd: sessionTotalCost,
    totalMessages,
    totalToolCalls,
    startTimestamp: sessionStartTimestamp,
    totalSteps,
    totalTurns,
    limits: sessionLimits ?? undefined,
  };
  
  return { session, perAgent };
}

/** Stats computed from a delegation group's events */
export interface DelegationStats {
  contextTokens: number;
  contextLimit: number | undefined;
  contextPercent: number | undefined;
  toolCallCount: number;
  messageCount: number;
  costUsd: number;
  inputTokens: number;
  outputTokens: number;
  steps: number;
  turns: number;
}

export function calculateDelegationStats(group: DelegationGroupInfo): DelegationStats {
  let contextTokens = 0;
  let contextLimit: number | undefined;
  let costUsd = 0;
  let inputTokens = 0;
  let outputTokens = 0;
  let steps = 0;
  let turns = 0;

  const toolCallCount = group.events.filter(e => e.type === 'tool_call').length;
  const messageCount = group.events.filter(e => e.type === 'agent' && e.isMessage).length;

  for (const event of group.events) {
    if (event.contextTokens !== undefined) {
      contextTokens = event.contextTokens; // latest value wins
    }
    if (event.contextLimit !== undefined) {
      contextLimit = event.contextLimit;
    }
    if (event.costUsd !== undefined) {
      costUsd += event.costUsd;
    }
    if (event.usage) {
      inputTokens += event.usage.input_tokens;
      outputTokens += event.usage.output_tokens;
    }
    if (event.metrics) {
      steps = event.metrics.steps;
      turns = event.metrics.turns;
    }
  }

  const contextPercent = contextLimit && contextLimit > 0
    ? Math.min(100, Math.round((contextTokens / contextLimit) * 100))
    : undefined;

  return {
    contextTokens,
    contextLimit,
    contextPercent,
    toolCallCount,
    messageCount,
    costUsd,
    inputTokens,
    outputTokens,
    steps,
    turns,
  };
}
