import { useEffect, useState, useCallback, useRef } from 'react';
import {
  EventItem,
  RoutingMode,
  UiAgentInfo,
  UiClientMessage,
  UiServerMessage,
  SessionSummary,
  AuditView,
  AgentEvent,
} from '../types';

const DEFAULT_CWD = '/Users/wiking/querymt';

export function useUiClient() {
  const [events, setEvents] = useState<EventItem[]>([]);
  const [agents, setAgents] = useState<UiAgentInfo[]>([]);
  const [routingMode, setRoutingMode] = useState<RoutingMode>('single');
  const [activeAgentId, setActiveAgentId] = useState<string>('primary');
  const [sessionId, setSessionId] = useState<string | null>(null);
  const [connected, setConnected] = useState(false);
  const [sessionHistory, setSessionHistory] = useState<SessionSummary[]>([]);
  const [sessionAudit, setSessionAudit] = useState<AuditView | null>(null);
  const [isAgentThinking, setIsAgentThinking] = useState(false);
  const [thinkingAgentId, setThinkingAgentId] = useState<string | null>(null);
  const [isConversationComplete, setIsConversationComplete] = useState(false);
  const socketRef = useRef<WebSocket | null>(null);

  useEffect(() => {
    let mounted = true;
    const socket = new WebSocket('ws://127.0.0.1:3030/ui/ws');
    socketRef.current = socket;

    socket.onopen = () => {
      if (!mounted) return;
      setConnected(true);
      sendMessage({ type: 'init' });
    };

    socket.onclose = () => {
      if (!mounted) return;
      setConnected(false);
    };

    socket.onerror = () => {
      if (!mounted) return;
      setConnected(false);
    };

    socket.onmessage = (event) => {
      if (!mounted) return;
      try {
        const msg = JSON.parse(event.data) as UiServerMessage;
        handleServerMessage(msg);
      } catch (err) {
        console.error('Failed to parse UI message:', err);
      }
    };

    return () => {
      mounted = false;
      if (socketRef.current) {
        socketRef.current.close();
      }
    };
  }, []);

  const handleServerMessage = (msg: UiServerMessage) => {
    switch (msg.type) {
      case 'state':
        setAgents(msg.agents);
        setRoutingMode(msg.routing_mode);
        setActiveAgentId(msg.active_agent_id);
        setSessionId(msg.active_session_id ?? null);
        break;
      case 'session_created':
        if (msg.agent_id === activeAgentId) {
          setSessionId(msg.session_id);
          setEvents([]); // Clear events for fresh session
          setSessionAudit(null); // Clear audit data
        }
        break;
      case 'event': {
        const eventKind = msg.event?.kind?.type ?? msg.event?.kind?.type_name;
        // Track LLM thinking state and conversation completion
        if (eventKind === 'llm_request_start') {
          setIsAgentThinking(true);
          setThinkingAgentId(msg.agent_id);
          setIsConversationComplete(false);
        } else if (eventKind === 'llm_request_end') {
          // Check finish_reason to determine if turn is complete
          const finishReason = msg.event?.kind?.finish_reason;
          if (finishReason === 'stop' || finishReason === 'Stop') {
            setIsAgentThinking(false);
            setThinkingAgentId(null);
            setIsConversationComplete(true);
            // Auto-reset completion indicator after 2 seconds
            setTimeout(() => setIsConversationComplete(false), 2000);
          } else if (finishReason === 'tool_calls' || finishReason === 'ToolCalls') {
            // Tool calls requested, still thinking
            setIsAgentThinking(true);
          } else {
            // Other finish reasons (length, error, etc.) - stop thinking
            setIsAgentThinking(false);
            setThinkingAgentId(null);
          }
        } else if (eventKind === 'prompt_received') {
          // New prompt, reset completion state
          setIsConversationComplete(false);
        } else if (eventKind === 'assistant_message_stored') {
          // Fallback: if we somehow missed llm_request_end, stop thinking
          if (isAgentThinking) {
            setIsAgentThinking(false);
            setThinkingAgentId(null);
          }
        }
        setEvents((prev) => [...prev, translateAgentEvent(msg.agent_id, msg.event)]);
        break;
      }
      case 'error':
        console.error('UI server error:', msg.message);
        setEvents((prev) => [
          ...prev,
          {
            id: `ui-error-${Date.now()}-${Math.random()}`,
            agentId: 'system',
            type: 'agent',
            content: `UI server error: ${msg.message}`,
            timestamp: Date.now(),
          },
        ]);
        break;
      case 'session_list':
        setSessionHistory(msg.sessions);
        break;
      case 'session_loaded':
        setSessionId(msg.session_id);
        // Translate events from full audit view
        const loadedEvents = msg.audit.events.map(event => translateLoadedEvent(activeAgentId, event));
        setEvents(loadedEvents);
        // Store full audit for stats (tasks, artifacts, decisions, etc.)
        setSessionAudit(msg.audit);
        break;
      default:
        break;
    }
  };

  const sendMessage = (message: UiClientMessage) => {
    const socket = socketRef.current;
    if (!socket || socket.readyState !== WebSocket.OPEN) {
      return;
    }
    socket.send(JSON.stringify(message));
  };

  const newSession = useCallback(async () => {
    sendMessage({ type: 'new_session', cwd: DEFAULT_CWD });
  }, []);

  const sendPrompt = useCallback(async (promptText: string) => {
    sendMessage({ type: 'prompt', text: promptText });
  }, []);

  const selectAgent = useCallback((agentId: string) => {
    setActiveAgentId(agentId);
    sendMessage({ type: 'set_active_agent', agent_id: agentId });
  }, []);

  const selectRoutingMode = useCallback((mode: RoutingMode) => {
    setRoutingMode(mode);
    sendMessage({ type: 'set_routing_mode', mode });
  }, []);

  const loadSession = useCallback((sessionId: string) => {
    sendMessage({ type: 'load_session', session_id: sessionId });
  }, []);

  return {
    events,
    sessionId,
    connected,
    newSession,
    sendPrompt,
    agents,
    routingMode,
    activeAgentId,
    setActiveAgent: selectAgent,
    setRoutingMode: selectRoutingMode,
    sessionHistory,
    loadSession,
    sessionAudit,
    isAgentThinking,
    thinkingAgentId,
    isConversationComplete,
  };
}

function translateAgentEvent(agentId: string, event: any): EventItem {
  const kind = event?.kind?.type ?? event?.kind?.type_name ?? event?.kind?.type;
  const timestamp = typeof event.timestamp === 'number' ? event.timestamp * 1000 : Date.now();
  const id = event.seq ? String(event.seq) : `${Date.now()}-${Math.random()}`;

  if (kind === 'tool_call_start') {
    return {
      id,
      agentId,
      type: 'tool_call',
      content: event.kind?.tool_name ?? 'tool_call',
      timestamp,
      toolCall: {
        tool_call_id: event.kind?.tool_call_id,
        kind: event.kind?.tool_name,
        status: 'in_progress',
        raw_input: parseJsonMaybe(event.kind?.arguments),
      },
    };
  }

  if (kind === 'tool_call_end') {
    const status = event.kind?.is_error ? 'failed' : 'completed';
    return {
      id,
      agentId,
      type: 'tool_result',
      content: event.kind?.result ?? '',
      timestamp,
      toolCall: {
        tool_call_id: event.kind?.tool_call_id,
        kind: event.kind?.tool_name,
        status,
        raw_output: parseJsonMaybe(event.kind?.result),
      },
    };
  }

  if (kind === 'prompt_received') {
    return {
      id,
      agentId,
      type: 'user',
      content: event.kind?.content ?? '',
      timestamp,
    };
  }

  if (kind === 'assistant_message_stored') {
    return {
      id,
      agentId,
      type: 'agent',
      content: event.kind?.content ?? '',
      timestamp,
    };
  }

  if (kind === 'llm_request_end') {
    return {
      id,
      agentId,
      type: 'agent',
      content: `Event: llm_request_end`,
      timestamp,
      usage: event.kind?.usage,
      costUsd: event.kind?.cost_usd,
      cumulativeCostUsd: event.kind?.cumulative_cost_usd,
      finishReason: event.kind?.finish_reason,
    };
  }

  if (kind === 'delegation_requested') {
    return {
      id,
      agentId,
      type: 'agent',
      content: `Event: delegation_requested`,
      timestamp,
      delegationId: event.kind?.delegation?.public_id,
    };
  }

  if (kind === 'delegation_completed') {
    return {
      id,
      agentId,
      type: 'agent',
      content: `Event: delegation_completed`,
      timestamp,
      delegationId: event.kind?.delegation_id,
    };
  }

  if (kind === 'provider_changed') {
    return {
      id,
      agentId,
      type: 'agent',
      content: `Event: provider_changed`,
      timestamp,
      contextLimit: event.kind?.context_limit,
    };
  }

  if (kind === 'error') {
    return {
      id,
      agentId,
      type: 'agent',
      content: event.kind?.message ?? 'Error',
      timestamp,
    };
  }

  return {
    id,
    agentId,
    type: 'agent',
    content: summarizeUnknownEvent(event),
    timestamp,
  };
}

function parseJsonMaybe(value: any) {
  if (typeof value !== 'string') return value;
  try {
    return JSON.parse(value);
  } catch {
    return value;
  }
}

function summarizeUnknownEvent(event: any): string {
  const kind = event?.kind?.type ?? event?.kind?.type_name;
  if (kind) {
    return `Event: ${kind}`;
  }
  return 'Event';
}

function translateLoadedEvent(agentId: string, event: AgentEvent): EventItem {
  // Similar to translateAgentEvent but for loaded history
  const kind = event.kind;
  const timestamp = event.timestamp * 1000;
  const id = String(event.seq);

  if (kind.type === 'tool_call_start') {
    const toolCallStart = kind as { type: 'tool_call_start'; tool_call_id: string; tool_name: string; arguments?: string };
    return {
      id,
      agentId,
      type: 'tool_call',
      content: toolCallStart.tool_name ?? 'tool_call',
      timestamp,
      toolCall: {
        tool_call_id: toolCallStart.tool_call_id,
        kind: toolCallStart.tool_name,
        status: 'in_progress',
        raw_input: parseJsonMaybe(toolCallStart.arguments),
      },
    };
  }

  if (kind.type === 'tool_call_end') {
    const toolCallEnd = kind as { type: 'tool_call_end'; tool_call_id: string; tool_name: string; result?: string; is_error?: boolean };
    return {
      id,
      agentId,
      type: 'tool_result',
      content: toolCallEnd.result ?? '',
      timestamp,
      toolCall: {
        tool_call_id: toolCallEnd.tool_call_id,
        kind: toolCallEnd.tool_name,
        status: toolCallEnd.is_error ? 'failed' : 'completed',
        raw_output: parseJsonMaybe(toolCallEnd.result),
      },
    };
  }

  if (kind.type === 'prompt_received') {
    const promptReceived = kind as { type: 'prompt_received'; content: string };
    return {
      id,
      agentId,
      type: 'user',
      content: promptReceived.content ?? '',
      timestamp,
    };
  }

  if (kind.type === 'assistant_message_stored') {
    const assistantMessage = kind as { type: 'assistant_message_stored'; content: string };
    return {
      id,
      agentId,
      type: 'agent',
      content: assistantMessage.content ?? '',
      timestamp,
    };
  }

  if (kind.type === 'llm_request_end') {
    const llmRequestEnd = kind as { 
      type: 'llm_request_end'; 
      usage?: { input_tokens: number; output_tokens: number };
      cost_usd?: number;
      cumulative_cost_usd?: number;
      finish_reason?: string;
    };
    return {
      id,
      agentId,
      type: 'agent',
      content: `Event: llm_request_end`,
      timestamp,
      usage: llmRequestEnd.usage,
      costUsd: llmRequestEnd.cost_usd,
      cumulativeCostUsd: llmRequestEnd.cumulative_cost_usd,
      finishReason: llmRequestEnd.finish_reason,
    };
  }

  if (kind.type === 'delegation_requested') {
    const delegationRequested = kind as {
      type: 'delegation_requested';
      delegation?: { public_id?: string };
    };
    return {
      id,
      agentId,
      type: 'agent',
      content: `Event: delegation_requested`,
      timestamp,
      delegationId: delegationRequested.delegation?.public_id,
    };
  }

  if (kind.type === 'delegation_completed') {
    const delegationCompleted = kind as {
      type: 'delegation_completed';
      delegation_id?: string;
    };
    return {
      id,
      agentId,
      type: 'agent',
      content: `Event: delegation_completed`,
      timestamp,
      delegationId: delegationCompleted.delegation_id,
    };
  }

  if (kind.type === 'provider_changed') {
    const providerChanged = kind as {
      type: 'provider_changed';
      provider?: string;
      model?: string;
      config_id?: number;
      context_limit?: number;
    };
    return {
      id,
      agentId,
      type: 'agent',
      content: `Event: provider_changed`,
      timestamp,
      contextLimit: providerChanged.context_limit,
    };
  }

  return {
    id,
    agentId,
    type: 'agent',
    content: `Event: ${kind.type}`,
    timestamp,
  };
}
