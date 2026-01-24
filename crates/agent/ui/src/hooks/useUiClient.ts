import { useEffect, useState, useCallback, useRef } from 'react';
import {
  EventItem,
  RoutingMode,
  UiAgentInfo,
  UiClientMessage,
  UiServerMessage,
  SessionSummary,
  SessionGroup,
  AuditView,
  AgentEvent,
  FileIndexEntry,
  ModelEntry,
  LlmConfigDetails,
} from '../types';

// Callback type for file index updates
type FileIndexCallback = (files: FileIndexEntry[], generatedAt: number) => void;
type FileIndexErrorCallback = (message: string) => void;

export function useUiClient() {
  const [events, setEvents] = useState<EventItem[]>([]);
  const [agents, setAgents] = useState<UiAgentInfo[]>([]);
  const [routingMode, setRoutingMode] = useState<RoutingMode>('single');
  const [activeAgentId, setActiveAgentId] = useState<string>('primary');
  const [sessionId, setSessionId] = useState<string | null>(null);
  const [connected, setConnected] = useState(false);
  const [sessionHistory, setSessionHistory] = useState<SessionSummary[]>([]);
  const [sessionGroups, setSessionGroups] = useState<SessionGroup[]>([]);
  const [allModels, setAllModels] = useState<ModelEntry[]>([]);
  const [sessionsByAgent, setSessionsByAgent] = useState<Record<string, string>>({});
  const [agentModels, setAgentModels] = useState<
    Record<string, { provider?: string; model?: string; contextLimit?: number }>
  >({});
  const [sessionAudit, setSessionAudit] = useState<AuditView | null>(null);
  const [thinkingAgentId, setThinkingAgentId] = useState<string | null>(null);
  const [isConversationComplete, setIsConversationComplete] = useState(false);
  const [workspaceIndexStatus, setWorkspaceIndexStatus] = useState<
    Record<string, { status: 'building' | 'ready' | 'error'; message?: string | null }>
  >({});
  const [llmConfigCache, setLlmConfigCache] = useState<Record<number, LlmConfigDetails>>({});
  const socketRef = useRef<WebSocket | null>(null);
  const fileIndexCallbackRef = useRef<FileIndexCallback | null>(null);
  const fileIndexErrorCallbackRef = useRef<FileIndexErrorCallback | null>(null);
  const llmConfigCallbacksRef = useRef<Map<number, (config: LlmConfigDetails) => void>>(new Map());

  useEffect(() => {
    let mounted = true;
    // Dynamically construct WebSocket URL from current page location
    const wsProtocol = window.location.protocol === 'https:' ? 'wss:' : 'ws:';
    const socket = new WebSocket(`${wsProtocol}//${window.location.host}/ui/ws`);
    socketRef.current = socket;

    socket.onopen = () => {
      if (!mounted) return;
      setConnected(true);
      sendMessage({ type: 'init' });
      sendMessage({ type: 'list_all_models', refresh: false });
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
    console.log('[useUiClient] Received message:', msg.type, msg);
    switch (msg.type) {
      case 'state':
        setAgents(msg.agents);
        setRoutingMode(msg.routing_mode);
        setActiveAgentId(msg.active_agent_id);
        setSessionId(msg.active_session_id ?? null);
        setSessionsByAgent(msg.sessions_by_agent ?? {});
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
          setThinkingAgentId(msg.agent_id);
          setIsConversationComplete(false);
        } else if (eventKind === 'llm_request_end') {
          // Check finish_reason to determine if turn is complete
          const finishReason = msg.event?.kind?.finish_reason;
          if (finishReason === 'stop' || finishReason === 'Stop') {
            setThinkingAgentId(null);
            setIsConversationComplete(true);
            // Auto-reset completion indicator after 2 seconds
            setTimeout(() => setIsConversationComplete(false), 2000);
          } else if (finishReason === 'tool_calls' || finishReason === 'ToolCalls') {
            // Tool calls requested, still thinking - keep thinkingAgentId set
          } else {
            // Other finish reasons (length, error, etc.) - stop thinking
            setThinkingAgentId(null);
          }
        } else if (eventKind === 'prompt_received') {
          // New prompt, reset completion state
          setIsConversationComplete(false);
        } else if (eventKind === 'assistant_message_stored') {
          // Fallback: if we somehow missed llm_request_end, stop thinking
          if (thinkingAgentId !== null) {
            setThinkingAgentId(null);
          }
        } else if (eventKind === 'error') {
          // Reset thinking state on error - the agent has stopped processing
          setThinkingAgentId(null);
        }
        const translated = translateAgentEvent(msg.agent_id, msg.event);
        if (eventKind === 'provider_changed') {
          setAgentModels((prev) => ({
            ...prev,
            [msg.agent_id]: {
              provider: msg.event?.kind?.provider,
              model: msg.event?.kind?.model,
              contextLimit: msg.event?.kind?.context_limit,
            },
          }));
        }
        setEvents((prev) => [...prev, translated]);
        break;
      }
      case 'error': {
        console.error('UI server error:', msg.message);
        // Reset thinking state on error - the agent has stopped processing
        setThinkingAgentId(null);
        // Check if this is a file index related error and notify
        if (
          fileIndexErrorCallbackRef.current &&
          (msg.message.includes('workspace') ||
            msg.message.includes('File index') ||
            msg.message.includes('working directory'))
        ) {
          fileIndexErrorCallbackRef.current(msg.message);
        }
        setEvents((prev) => [
          ...prev,
          {
            id: `ui-error-${Date.now()}-${Math.random()}`,
            agentId: 'system',
            type: 'system',
            content: msg.message,
            timestamp: Date.now(),
            isMessage: true,
          },
        ]);
        break;
      }
      case 'session_list':
        setSessionGroups(msg.groups);
        // Flatten groups for backwards compatibility with Sidebar
        const flatSessions = msg.groups.flatMap(g => g.sessions);
        setSessionHistory(flatSessions);
        break;
      case 'session_loaded':
        setSessionId(msg.session_id);
        // Translate events from full audit view
        const loadedEvents = msg.audit.events.map(event => translateLoadedEvent(activeAgentId, event));
        setEvents(loadedEvents);
        const lastProviderEvent = [...loadedEvents]
          .reverse()
          .find((event) => event.provider || event.model || event.contextLimit !== undefined);
        if (lastProviderEvent) {
          setAgentModels((prev) => ({
            ...prev,
            [activeAgentId]: {
              provider: lastProviderEvent.provider,
              model: lastProviderEvent.model,
              contextLimit: lastProviderEvent.contextLimit,
            },
          }));
        }
        // Store full audit for stats (tasks, artifacts, decisions, etc.)
        setSessionAudit(msg.audit);
        break;
      case 'workspace_index_status':
        setWorkspaceIndexStatus(prev => ({
          ...prev,
          [msg.session_id]: { status: msg.status, message: msg.message ?? null },
        }));
        break;
      case 'file_index':
        if (fileIndexCallbackRef.current) {
          fileIndexCallbackRef.current(msg.files, msg.generated_at);
        }
        break;
      case 'all_models_list':
        setAllModels(msg.models);
        break;
      case 'llm_config': {
        const config: LlmConfigDetails = {
          configId: msg.config_id,
          provider: msg.provider,
          model: msg.model,
          params: msg.params,
        };
        // Cache the config
        setLlmConfigCache((prev) => ({ ...prev, [msg.config_id]: config }));
        // Notify any pending callbacks
        const callback = llmConfigCallbacksRef.current.get(msg.config_id);
        if (callback) {
          callback(config);
          llmConfigCallbacksRef.current.delete(msg.config_id);
        }
        break;
      }
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
    const input = window.prompt('Workspace path (blank for none):', '');
    if (input === null) {
      return;
    }
    const cwd = input.trim();
    sendMessage({ type: 'new_session', cwd: cwd.length > 0 ? cwd : null });
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

  const refreshAllModels = useCallback(() => {
    sendMessage({ type: 'list_all_models', refresh: true });
  }, []);

  const setSessionModel = useCallback((sessionId: string, modelId: string) => {
    sendMessage({ type: 'set_session_model', session_id: sessionId, model_id: modelId });
  }, []);

  // Register a callback for file index updates
  const setFileIndexCallback = useCallback((callback: FileIndexCallback | null) => {
    console.log('[useUiClient] Registering file index callback:', !!callback);
    fileIndexCallbackRef.current = callback;
  }, []);

  // Register a callback for file index errors
  const setFileIndexErrorCallback = useCallback((callback: FileIndexErrorCallback | null) => {
    fileIndexErrorCallbackRef.current = callback;
  }, []);

  // Request file index from server
  const requestFileIndex = useCallback(() => {
    sendMessage({ type: 'get_file_index' });
  }, []);

  // Request LLM config by ID (returns cached if available, otherwise fetches)
  const requestLlmConfig = useCallback((configId: number, callback: (config: LlmConfigDetails) => void) => {
    // Check cache first
    const cached = llmConfigCache[configId];
    if (cached) {
      callback(cached);
      return;
    }
    // Register callback and request
    llmConfigCallbacksRef.current.set(configId, callback);
    sendMessage({ type: 'get_llm_config', config_id: configId });
  }, [llmConfigCache]);

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
    sessionGroups,
    allModels,
    sessionsByAgent,
    agentModels,
    loadSession,
    refreshAllModels,
    setSessionModel,
    sessionAudit,
    thinkingAgentId,
    isConversationComplete,
    setFileIndexCallback,
    setFileIndexErrorCallback,
    requestFileIndex,
    workspaceIndexStatus,
    llmConfigCache,
    requestLlmConfig,
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
      isMessage: true,
    };
  }

  if (kind === 'assistant_message_stored') {
    return {
      id,
      agentId,
      type: 'agent',
      content: event.kind?.content ?? '',
      timestamp,
      isMessage: true,
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
      contextTokens: event.kind?.context_tokens,
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
      provider: event.kind?.provider,
      model: event.kind?.model,
      contextLimit: event.kind?.context_limit,
      configId: event.kind?.config_id,
    };
  }

  // Note: 'error' event kinds are handled in the switch statement above
  // by resetting thinking state. This translation just converts to EventItem.
  if (kind === 'error') {
    return {
      id,
      agentId: 'system',
      type: 'system',
      content: event.kind?.message ?? 'Error',
      timestamp,
      isMessage: true,
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
      isMessage: true,
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
      isMessage: true,
    };
  }

  if (kind.type === 'llm_request_end') {
    const llmRequestEnd = kind as { 
      type: 'llm_request_end'; 
      usage?: { input_tokens: number; output_tokens: number };
      cost_usd?: number;
      cumulative_cost_usd?: number;
      context_tokens?: number;
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
      contextTokens: llmRequestEnd.context_tokens,
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
      provider: providerChanged.provider,
      model: providerChanged.model,
      contextLimit: providerChanged.context_limit,
      configId: providerChanged.config_id,
    };
  }

  if (kind.type === 'error') {
    const errorEvent = kind as { type: 'error'; message?: string };
    return {
      id,
      agentId: 'system',
      type: 'system',
      content: errorEvent.message ?? 'Error',
      timestamp,
      isMessage: true,
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
