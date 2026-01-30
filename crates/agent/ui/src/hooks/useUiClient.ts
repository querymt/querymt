import { useEffect, useState, useCallback, useRef, useMemo } from 'react';
import {
  EventItem,
  RoutingMode,
  UiAgentInfo,
  UiClientMessage,
  UiServerMessage,
  SessionSummary,
  SessionGroup,
  AuditView,
  FileIndexEntry,
  ModelEntry,
  LlmConfigDetails,
  SessionLimits,
} from '../types';

// Callback type for file index updates
type FileIndexCallback = (files: FileIndexEntry[], generatedAt: number) => void;
type FileIndexErrorCallback = (message: string) => void;

export function useUiClient() {
  const [eventsBySession, setEventsBySession] = useState<Map<string, EventItem[]>>(new Map());
  const [mainSessionId, setMainSessionId] = useState<string | null>(null);
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
  const [thinkingAgentIds, setThinkingAgentIds] = useState<Set<string>>(new Set());
  const [isConversationComplete, setIsConversationComplete] = useState(false);
  const [workspaceIndexStatus, setWorkspaceIndexStatus] = useState<
    Record<string, { status: 'building' | 'ready' | 'error'; message?: string | null }>
  >({});
  const [llmConfigCache, setLlmConfigCache] = useState<Record<number, LlmConfigDetails>>({});
  const [sessionLimits, setSessionLimits] = useState<SessionLimits | null>(null);
  const [undoState, setUndoState] = useState<{ turnId: string; revertedFiles: string[] } | null>(null);
  const socketRef = useRef<WebSocket | null>(null);
  const fileIndexCallbackRef = useRef<FileIndexCallback | null>(null);
  const fileIndexErrorCallbackRef = useRef<FileIndexErrorCallback | null>(null);
  const llmConfigCallbacksRef = useRef<Map<number, (config: LlmConfigDetails) => void>>(new Map());

  // Derive main session events for backward compatibility
  const events = useMemo(
    () => (mainSessionId ? eventsBySession.get(mainSessionId) ?? [] : []),
    [eventsBySession, mainSessionId]
  );

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
          setMainSessionId(msg.session_id);
          setEventsBySession(new Map()); // Clear all event buckets for fresh session
          setSessionAudit(null); // Clear audit data
          setSessionLimits(null); // Clear session limits, will be set by session_configured event
        }
        break;
      case 'session_events': {
        // Replay batch - set all events for this session
        const translated = msg.events.map((e: any) => {
          const item = translateAgentEvent(msg.agent_id, e);
          item.sessionId = msg.session_id;
          item.seq = e.seq;
          return item;
        });
        setEventsBySession(prev => {
          const next = new Map(prev);
          next.set(msg.session_id, translated);
          return next;
        });

        // If this is the main session, update agentModels from last provider event
        if (msg.session_id === mainSessionId) {
          const lastProvider = [...translated].reverse()
            .find(e => e.provider || e.model);
          if (lastProvider) {
            setAgentModels(prev => ({
              ...prev,
              [msg.agent_id]: {
                provider: lastProvider.provider,
                model: lastProvider.model,
                contextLimit: lastProvider.contextLimit,
              },
            }));
          }
        }
        break;
      }
      case 'event': {
        const eventKind = msg.event?.kind?.type ?? msg.event?.kind?.type_name;
        
        // Track LLM thinking state and conversation completion (applies to all agents)
        if (eventKind === 'llm_request_start') {
          setThinkingAgentIds(prev => new Set(prev).add(msg.agent_id));
          setIsConversationComplete(false);
        } else if (eventKind === 'llm_request_end') {
          const finishReason = msg.event?.kind?.finish_reason;
          if (finishReason === 'stop' || finishReason === 'Stop') {
            setThinkingAgentIds(prev => {
              const next = new Set(prev);
              next.delete(msg.agent_id);
              if (next.size === 0) {
                setIsConversationComplete(true);
                setTimeout(() => setIsConversationComplete(false), 2000);
              }
              return next;
            });
          } else if (finishReason === 'tool_calls' || finishReason === 'ToolCalls') {
            // Tool calls requested, still thinking
          } else {
            setThinkingAgentIds(prev => {
              const next = new Set(prev);
              next.delete(msg.agent_id);
              return next;
            });
          }
        } else if (eventKind === 'prompt_received') {
          setIsConversationComplete(false);
        } else if (eventKind === 'assistant_message_stored') {
          setThinkingAgentIds(prev => {
            const next = new Set(prev);
            next.delete(msg.agent_id);
            return next;
          });
        } else if (eventKind === 'error') {
          setThinkingAgentIds(prev => {
            const next = new Set(prev);
            next.delete(msg.agent_id);
            return next;
          });
        } else if (eventKind === 'cancelled') {
          setThinkingAgentIds(prev => {
            const next = new Set(prev);
            next.delete(msg.agent_id);
            return next;
          });
        }

        // Auto-subscribe to delegation child sessions
        if (eventKind === 'session_forked' && msg.event?.kind?.origin === 'delegation') {
          sendMessage({
            type: 'subscribe_session',
            session_id: msg.event.kind.child_session_id,
            agent_id: msg.event.kind.target_agent_id,
          });
        }

        // Translate and route to correct session bucket with dedup
        const translated = translateAgentEvent(msg.agent_id, msg.event);
        translated.sessionId = msg.session_id;
        translated.seq = msg.event.seq;

        setEventsBySession(prev => {
          const next = new Map(prev);
          const existing = next.get(msg.session_id) ?? [];
          // Dedup: skip if we already have this seq
          if (existing.length > 0 && translated.seq != null) {
            const lastSeq = existing[existing.length - 1].seq ?? -1;
            if (translated.seq <= lastSeq) return prev;
          }
          next.set(msg.session_id, [...existing, translated]);
          return next;
        });

        // Provider/limits updates - only for main session
        if (msg.session_id === mainSessionId) {
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
          if (eventKind === 'session_configured' && msg.event?.kind?.limits) {
            setSessionLimits(msg.event.kind.limits);
          }
        }
        break;
      }
      case 'error': {
        console.error('UI server error:', msg.message);
        // Reset thinking state on error - clear all agents (connection-level error)
        setThinkingAgentIds(new Set());
        // Check if this is a file index related error and notify
        if (
          fileIndexErrorCallbackRef.current &&
          (msg.message.includes('workspace') ||
            msg.message.includes('File index') ||
            msg.message.includes('working directory'))
        ) {
          fileIndexErrorCallbackRef.current(msg.message);
        }
        // Add error to main session events if we have one
        if (mainSessionId) {
          setEventsBySession((prev) => {
            const next = new Map(prev);
            const existing = next.get(mainSessionId) ?? [];
            next.set(mainSessionId, [
              ...existing,
              {
                id: `ui-error-${Date.now()}-${Math.random()}`,
                agentId: 'system',
                type: 'system',
                content: msg.message,
                timestamp: Date.now(),
                isMessage: true,
              },
            ]);
            return next;
          });
        }
        break;
      }
      case 'session_list':
        setSessionGroups(msg.groups);
        // Flatten groups for backwards compatibility with Sidebar
        const flatSessions = msg.groups.flatMap(g => g.sessions);
        setSessionHistory(flatSessions);
        break;
      case 'session_loaded': {
        setSessionId(msg.session_id);
        setMainSessionId(msg.session_id);
        setSessionAudit(msg.audit);
        
        // Populate eventsBySession from the audit events (for old session history)
        const translated = msg.audit.events.map((e: any) => {
          const item = translateAgentEvent(msg.agent_id, e);
          item.sessionId = msg.session_id;
          item.seq = e.seq;
          return item;
        });
        
        // Initialize eventsBySession with the main session's events
        const eventsMap = new Map();
        eventsMap.set(msg.session_id, translated);
        setEventsBySession(eventsMap);
        
        // Update agentModels from the last provider event in the loaded session
        const lastProvider = [...translated].reverse()
          .find(e => e.provider || e.model);
        if (lastProvider) {
          setAgentModels(prev => ({
            ...prev,
            [msg.agent_id]: {
              provider: lastProvider.provider,
              model: lastProvider.model,
              contextLimit: lastProvider.contextLimit,
            },
          }));
        }
        
        // Subscribe to child delegation sessions
        for (const event of msg.audit.events) {
          if (
            (event.kind as any)?.type === 'session_forked' &&
            (event.kind as any)?.origin === 'delegation'
          ) {
            sendMessage({
              type: 'subscribe_session',
              session_id: (event.kind as any)?.child_session_id,
              agent_id: (event.kind as any)?.target_agent_id,
            });
          }
        }
        break;
      }
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
      case 'undo_result': {
        if (msg.success) {
          // Undo succeeded - update with the actual reverted files
          setUndoState(prev => prev ? { ...prev, revertedFiles: msg.reverted_files } : null);
          console.log('[useUiClient] Undo succeeded, reverted files:', msg.reverted_files);
        } else {
          console.error('[useUiClient] Undo failed:', msg.message);
          setUndoState(null);
        }
        break;
      }
      case 'redo_result': {
        if (msg.success) {
          // Redo succeeded - clear undo state
          setUndoState(null);
          console.log('[useUiClient] Redo succeeded');
        } else {
          console.error('[useUiClient] Redo failed:', msg.message);
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

  // Cancel the active session
  const cancelSession = useCallback(() => {
    sendMessage({ type: 'cancel_session' });
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

  // Derive thinkingAgentId for backward compatibility
  const thinkingAgentId = thinkingAgentIds.size > 0 ? Array.from(thinkingAgentIds).pop()! : null;

  const subscribeSession = useCallback((sessionId: string, agentId?: string) => {
    sendMessage({ type: 'subscribe_session', session_id: sessionId, agent_id: agentId });
  }, []);

  const unsubscribeSession = useCallback((sessionId: string) => {
    sendMessage({ type: 'unsubscribe_session', session_id: sessionId });
  }, []);

  const sendUndo = useCallback((messageId: string, turnId: string) => {
    sendMessage({ type: 'undo', message_id: messageId });
    // Temporarily set undo state with empty files - will be updated by undo_result
    setUndoState({ turnId, revertedFiles: [] });
  }, []);

  const sendRedo = useCallback(() => {
    sendMessage({ type: 'redo' });
  }, []);

  return {
    events,
    eventsBySession,
    mainSessionId,
    sessionId,
    connected,
    newSession,
    sendPrompt,
    cancelSession,
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
    thinkingAgentIds,
    isConversationComplete,
    setFileIndexCallback,
    setFileIndexErrorCallback,
    requestFileIndex,
    workspaceIndexStatus,
    llmConfigCache,
    requestLlmConfig,
    sessionLimits,
    subscribeSession,
    unsubscribeSession,
    sendUndo,
    sendRedo,
    undoState,
  };
}

function translateAgentEvent(agentId: string, event: any): EventItem {
  const kind = event?.kind?.type ?? event?.kind?.type_name ?? event?.kind?.type;
  const timestamp = typeof event.timestamp === 'number' ? event.timestamp * 1000 : Date.now();
  const id = event.seq ? String(event.seq) : `${Date.now()}-${Math.random()}`;
  const seq = event.seq;

  if (kind === 'tool_call_start') {
    return {
      id,
      agentId,
      seq: seq,
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
      seq: seq,
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
      seq: seq,
      type: 'user',
      content: event.kind?.content ?? '',
      timestamp,
      isMessage: true,
      messageId: event.kind?.message_id,
    };
  }

  if (kind === 'assistant_message_stored') {
    return {
      id,
      agentId,
      seq: seq,
      type: 'agent',
      content: event.kind?.content ?? '',
      timestamp,
      isMessage: true,
      messageId: event.kind?.message_id,
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
      metrics: event.kind?.metrics,
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
      delegationTargetAgentId: event.kind?.delegation?.target_agent_id,
      delegationObjective: event.kind?.delegation?.objective,
      delegationEventType: 'requested',
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
      delegationEventType: 'completed',
    };
  }

  if (kind === 'delegation_failed') {
    return {
      id,
      agentId,
      type: 'agent',
      content: `Event: delegation_failed`,
      timestamp,
      delegationId: event.kind?.delegation_id,
      delegationEventType: 'failed',
    };
  }

  if (kind === 'session_forked') {
    return {
      id,
      agentId,
      type: 'agent',
      content: `Event: session_forked`,
      timestamp,
      forkChildSessionId: event.kind?.child_session_id,
      forkDelegationId: event.kind?.fork_point_ref,
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

