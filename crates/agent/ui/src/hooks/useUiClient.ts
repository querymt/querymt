import { useEffect, useState, useCallback, useRef, useMemo } from 'react';
import { v7 as uuidv7 } from 'uuid';
import {
  EventItem,
  RoutingMode,
  UiAgentInfo,
  UiClientMessage,
  UiServerMessage,
  SessionGroup,
  PromptBlock,
  AuditView,
  FileIndexEntry,
  ModelEntry,
  RecentModelEntry,
  LlmConfigDetails,
  SessionLimits,
  AuthProviderEntry,
  ModelDownloadStatus,
  OAuthFlowState,
  ProviderCapabilityEntry,
  OAuthResultState,
  UndoStackFrame,
  RemoteNodeInfo,
  PluginUpdateStatus,
  PluginUpdateResult,
} from '../types';
import { debugLog, debugTrace } from '../utils/debugLog';

// Callback type for file index updates
type FileIndexCallback = (files: FileIndexEntry[], generatedAt: number) => void;
type FileIndexErrorCallback = (message: string) => void;

type UndoFrame = {
  turnId: string;
  messageId: string;
  status: 'pending' | 'confirmed';
  revertedFiles: string[];
};

type UndoState = {
  stack: UndoFrame[];
  frontierMessageId?: string;
} | null;

function findLiveAccumulatorIndex(
  events: EventItem[],
  messageId: string | undefined,
  agentId: string
): number {
  if (messageId) {
    const matchByMessageId = [...events].reverse().findIndex(
      e => e.streamMessageId === messageId && e.isStreamDelta
    );
    if (matchByMessageId >= 0) {
      return events.length - 1 - matchByMessageId;
    }
  }

  const matchByAgent = [...events].reverse().findIndex(
    e => e.isStreamDelta && e.agentId === agentId
  );
  return matchByAgent >= 0 ? events.length - 1 - matchByAgent : -1;
}

function nonEmptyString(value: string | undefined): string | undefined {
  if (!value) {
    return undefined;
  }
  const trimmed = value.trim();
  return trimmed.length > 0 ? value : undefined;
}

function buildUndoStateFromServerStack(
  undoStack: UndoStackFrame[],
  previousState: UndoState,
  revertedFilesByMessageId?: Map<string, string[]>,
  preferredFrontierMessageId?: string
) {
  if (!undoStack || undoStack.length === 0) {
    return null;
  }

  const previousStack = previousState?.stack ?? null;
  const previousByMessageId = new Map<string, UndoFrame>();
  for (const frame of previousStack ?? []) {
    previousByMessageId.set(frame.messageId, frame);
  }

  const stack: UndoFrame[] = undoStack.map((frame) => {
    const previous = previousByMessageId.get(frame.message_id);
    const overrideFiles = revertedFilesByMessageId?.get(frame.message_id);
    return {
      turnId: previous?.turnId ?? frame.message_id,
      messageId: frame.message_id,
      status: 'confirmed',
      revertedFiles: overrideFiles ?? previous?.revertedFiles ?? [],
    };
  });

  const hasMessage = (messageId?: string | null) =>
    !!messageId && stack.some((frame) => frame.messageId === messageId);

  let frontierMessageId: string | undefined;
  if (hasMessage(preferredFrontierMessageId)) {
    frontierMessageId = preferredFrontierMessageId ?? undefined;
  } else if (hasMessage(previousState?.frontierMessageId)) {
    frontierMessageId = previousState?.frontierMessageId;
  } else if (previousStack && previousStack.length > 0) {
    for (let i = previousStack.length - 1; i >= 0; i--) {
      const candidate = previousStack[i]?.messageId;
      if (hasMessage(candidate)) {
        frontierMessageId = candidate;
        break;
      }
    }
  }

  const frontierFrame = stack.find((frame) => frame.messageId === frontierMessageId) ?? stack[stack.length - 1];

  return {
    stack,
    frontierMessageId: frontierFrame.messageId,
  };
}

export function useUiClient() {
  const [eventsBySession, setEventsBySession] = useState<Map<string, EventItem[]>>(new Map());
  const [mainSessionId, setMainSessionId] = useState<string | null>(null);
  const [agents, setAgents] = useState<UiAgentInfo[]>([]);
  const [routingMode, setRoutingMode] = useState<RoutingMode>('single');
  const [activeAgentId, setActiveAgentId] = useState<string>('primary');
  const [sessionId, setSessionId] = useState<string | null>(null);
  const [connected, setConnected] = useState(false);
  const [agentMode, setAgentModeState] = useState<string>('build');
  // @ts-expect-error - setAvailableModes reserved for future backend integration
  const [availableModes, setAvailableModes] = useState<string[]>(['build', 'plan']);
  const [sessionGroups, setSessionGroups] = useState<SessionGroup[]>([]);
  const [allModels, setAllModels] = useState<ModelEntry[]>([]);
  const [providerCapabilities, setProviderCapabilities] = useState<Record<string, ProviderCapabilityEntry>>({});
  const [recentModelsByWorkspace, setRecentModelsByWorkspace] = useState<Record<string, RecentModelEntry[]>>({});
  const [authProviders, setAuthProviders] = useState<AuthProviderEntry[]>([]);
  const [modelDownloads, setModelDownloads] = useState<Record<string, ModelDownloadStatus>>({});
  const [oauthFlow, setOauthFlow] = useState<OAuthFlowState | null>(null);
  const [oauthResult, setOauthResult] = useState<OAuthResultState | null>(null);
  const [sessionsByAgent, setSessionsByAgent] = useState<Record<string, string>>({});
  const [agentModels, setAgentModels] = useState<
    Record<string, { provider?: string; model?: string; contextLimit?: number; node?: string }>
  >({});
  const [sessionAudit, setSessionAudit] = useState<AuditView | null>(null);
  const [thinkingAgentIds, setThinkingAgentIds] = useState<Set<string>>(new Set());
  const [isConversationComplete, setIsConversationComplete] = useState(false);
  // Track thinking state per session: Map<sessionId, Set<agentId>>
  const [thinkingBySession, setThinkingBySession] = useState<Map<string, Set<string>>>(new Map());
  // Track parent-child session relationships from session_forked events
  const [sessionParentMap, setSessionParentMap] = useState<Map<string, string>>(new Map());
  const [workspaceIndexStatus, setWorkspaceIndexStatus] = useState<
    Record<string, { status: 'building' | 'ready' | 'error'; message?: string | null }>
  >({});
  const [llmConfigCache, setLlmConfigCache] = useState<Record<number, LlmConfigDetails>>({});
  const [sessionLimits, setSessionLimits] = useState<SessionLimits | null>(null);
  const [undoState, setUndoState] = useState<UndoState>(null);
  const undoStateRef = useRef<UndoState>(null);
  const [remoteNodes, setRemoteNodes] = useState<RemoteNodeInfo[]>([]);
  const [connectionErrors, setConnectionErrors] = useState<{ id: number; message: string }[]>([]);
  const [pluginUpdateStatus, setPluginUpdateStatus] = useState<Record<string, PluginUpdateStatus>>({});
  const [pluginUpdateResults, setPluginUpdateResults] = useState<PluginUpdateResult[] | null>(null);
  const [isUpdatingPlugins, setIsUpdatingPlugins] = useState(false);
  const [defaultCwd, setDefaultCwd] = useState<string | null>(null);
  const [workspacePathDialogOpen, setWorkspacePathDialogOpen] = useState(false);
  const [workspacePathDialogDefaultValue, setWorkspacePathDialogDefaultValue] = useState('');
  const socketRef = useRef<WebSocket | null>(null);
  const fileIndexCallbackRef = useRef<FileIndexCallback | null>(null);
  const fileIndexErrorCallbackRef = useRef<FileIndexErrorCallback | null>(null);
  const llmConfigCallbacksRef = useRef<Map<number, (config: LlmConfigDetails) => void>>(new Map());
  const pendingRequestsRef = useRef<Map<string, (sessionId: string) => void>>(new Map());
  const workspacePathDialogResolverRef = useRef<((value: { cwd: string; node: string | null } | null) => void) | null>(null);
  const sessionCreatingRef = useRef(false);

  // Derive main session events for backward compatibility
  const events = useMemo(
    () => (mainSessionId ? eventsBySession.get(mainSessionId) ?? [] : []),
    [eventsBySession, mainSessionId]
  );

  // Use a ref to always access the latest handleServerMessage from the socket callback.
  // Without this, the onmessage handler captures a stale closure from the initial render,
  // causing all state reads (mainSessionId, activeAgentId, etc.) to be permanently stale.
  const handleServerMessageRef = useRef<(msg: UiServerMessage) => void>(() => {});

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
      sendMessage({ type: 'get_recent_models', limit_per_workspace: 10 });
      sendMessage({ type: 'list_remote_nodes' });
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
        handleServerMessageRef.current(msg);
      } catch (err) {
        console.error('Failed to parse UI message:', err);
      }
    };

    return () => {
      mounted = false;
      const resolver = workspacePathDialogResolverRef.current;
      workspacePathDialogResolverRef.current = null;
      if (resolver) {
        resolver(null);
      }
      if (socketRef.current) {
        socketRef.current.close();
      }
    };
  }, []);

  const handleServerMessage = (msg: UiServerMessage) => {
    debugLog('[useUiClient] Received message:', () => ({ type: msg.type, msg }));
    switch (msg.type) {
      case 'state':
        setAgents(msg.agents);
        setRoutingMode(msg.routing_mode);
        setActiveAgentId(msg.active_agent_id);
        setSessionId(msg.active_session_id ?? null);
        setDefaultCwd(msg.default_cwd ?? null);
        setSessionsByAgent(msg.sessions_by_agent ?? {});
        if (msg.agent_mode) {
          setAgentModeState(msg.agent_mode);
        }
        break;
      case 'session_created':
        if (msg.agent_id === activeAgentId) {
          setSessionId(msg.session_id);
          setMainSessionId(msg.session_id);
          setEventsBySession(new Map()); // Clear all event buckets for fresh session
          setSessionAudit(null); // Clear audit data
          setSessionLimits(null); // Clear session limits, will be set by session_configured event
          setIsConversationComplete(false); // Reset conversation complete state
          setThinkingBySession(new Map()); // Clear all session thinking state
          setUndoState(null); // Clear undo state from previous session
          undoStateRef.current = null;
          // NOTE: We intentionally do NOT clear agentModels here.
          // The model badge should continue to show the last known model
          // until we receive a provider_changed event for the new session.
          // This provides better UX than showing an empty badge.
        }
        // Resolve pending promise if there's a request_id match
        if (msg.request_id && pendingRequestsRef.current.has(msg.request_id)) {
          pendingRequestsRef.current.get(msg.request_id)!(msg.session_id);
          pendingRequestsRef.current.delete(msg.request_id);
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
            debugLog('[useUiClient] session_events: Setting agentModels from replay', () => ({
              session_id: msg.session_id,
              agent_id: msg.agent_id,
              provider: lastProvider.provider,
              model: lastProvider.model,
              mainSessionId,
              eventCount: translated.length,
            }));
            setAgentModels(prev => ({
              ...prev,
              [msg.agent_id]: {
                provider: lastProvider.provider,
                model: lastProvider.model,
                contextLimit: lastProvider.contextLimit,
                node: lastProvider.providerNode,
              },
            }));
          }
        }
        break;
      }
      case 'event': {
        const eventKind = msg.event?.kind?.type ?? msg.event?.kind?.type_name;

        if (
          eventKind === 'llm_request_start' ||
          eventKind === 'llm_request_end' ||
          eventKind === 'assistant_thinking_delta' ||
          eventKind === 'assistant_content_delta' ||
          eventKind === 'assistant_message_stored'
        ) {
          const rawContent = msg.event?.kind?.content;
          const contentLen = typeof rawContent === 'string' ? rawContent.length : 0;
          debugTrace('[useUiClient] stream event received', () => ({
            session_id: msg.session_id,
            agent_id: msg.agent_id,
            seq: msg.event?.seq,
            event_kind: eventKind,
            message_id: msg.event?.kind?.message_id,
            content_len: contentLen,
            has_thinking: typeof msg.event?.kind?.thinking === 'string' && msg.event.kind.thinking.length > 0,
            finish_reason: msg.event?.kind?.finish_reason,
          }));
        }
        
        // Track LLM thinking state per session
        if (eventKind === 'llm_request_start') {
          setThinkingBySession(prev => {
            const next = new Map(prev);
            const sessionAgents = new Set(next.get(msg.session_id) ?? []);
            sessionAgents.add(msg.agent_id);
            next.set(msg.session_id, sessionAgents);
            return next;
          });
          // Also update global thinking state for backward compatibility
          setThinkingAgentIds(prev => new Set(prev).add(msg.agent_id));
          // Clear conversation complete flag for the main session
          if (msg.session_id === mainSessionId) {
            setIsConversationComplete(false);
          }
        } else if (eventKind === 'llm_request_end') {
          const finishReason = msg.event?.kind?.finish_reason;
          if (finishReason === 'stop' || finishReason === 'Stop') {
            setThinkingBySession(prev => {
              const next = new Map(prev);
              const sessionAgents = new Set(next.get(msg.session_id) ?? []);
              sessionAgents.delete(msg.agent_id);
              if (sessionAgents.size === 0) {
                next.delete(msg.session_id);
                // Set conversation complete flag only for main session
                if (msg.session_id === mainSessionId) {
                  setIsConversationComplete(true);
                  setTimeout(() => setIsConversationComplete(false), 2000);
                }
              } else {
                next.set(msg.session_id, sessionAgents);
              }
              return next;
            });
            // Also update global thinking state
            setThinkingAgentIds(prev => {
              const next = new Set(prev);
              next.delete(msg.agent_id);
              return next;
            });
          } else if (finishReason === 'tool_calls' || finishReason === 'ToolCalls') {
            // Tool calls requested, still thinking
          } else {
            setThinkingBySession(prev => {
              const next = new Map(prev);
              const sessionAgents = new Set(next.get(msg.session_id) ?? []);
              sessionAgents.delete(msg.agent_id);
              if (sessionAgents.size === 0) {
                next.delete(msg.session_id);
              } else {
                next.set(msg.session_id, sessionAgents);
              }
              return next;
            });
            setThinkingAgentIds(prev => {
              const next = new Set(prev);
              next.delete(msg.agent_id);
              return next;
            });
          }
        } else if (eventKind === 'prompt_received') {
          const isCurrentMainOrActiveSession =
            msg.session_id === mainSessionId || msg.session_id === sessionId;
          if (isCurrentMainOrActiveSession) {
            setIsConversationComplete(false);
            // A new prompt commits the current timeline branch; stacked redo history is no longer valid.
            setUndoState(null);
            undoStateRef.current = null;
          }
        } else if (eventKind === 'error') {
          setThinkingBySession(prev => {
            const next = new Map(prev);
            const sessionAgents = new Set(next.get(msg.session_id) ?? []);
            sessionAgents.delete(msg.agent_id);
            if (sessionAgents.size === 0) {
              next.delete(msg.session_id);
            } else {
              next.set(msg.session_id, sessionAgents);
            }
            return next;
          });
          setThinkingAgentIds(prev => {
            const next = new Set(prev);
            next.delete(msg.agent_id);
            return next;
          });
        } else if (eventKind === 'cancelled') {
          setThinkingBySession(prev => {
            const next = new Map(prev);
            const sessionAgents = new Set(next.get(msg.session_id) ?? []);
            sessionAgents.delete(msg.agent_id);
            if (sessionAgents.size === 0) {
              next.delete(msg.session_id);
            } else {
              next.set(msg.session_id, sessionAgents);
            }
            return next;
          });
          setThinkingAgentIds(prev => {
            const next = new Set(prev);
            next.delete(msg.agent_id);
            return next;
          });
        } else if (eventKind === 'delegation_cancelled') {
          // When a delegation is cancelled, the delegate agent should be removed from thinking state
          // The msg.agent_id here is the delegator (parent), but we need to clear thinking state
          // for the delegate agent. Since we track thinking per agent, and cancellation of the
          // delegation will also trigger 'cancelled' on the child session, we rely on that.
          // However, we can defensively clear all thinking state to ensure UI responsiveness.
          // For now, just trust the child session's 'cancelled' event will arrive.
        }

        // Auto-subscribe to delegation child sessions
        if (eventKind === 'session_forked' && msg.event?.kind?.origin === 'delegation') {
          const childSessionId = msg.event.kind.child_session_id;
          sendMessage({
            type: 'subscribe_session',
            session_id: childSessionId,
            agent_id: msg.event.kind.target_agent_id,
          });
          // Track parent-child relationship for thinking state propagation
          setSessionParentMap(prev => {
            const next = new Map(prev);
            next.set(childSessionId, msg.session_id);
            return next;
          });
        }

        // Translate and route to correct session bucket with dedup
        const translated = translateAgentEvent(msg.agent_id, msg.event);
        translated.sessionId = msg.session_id;
        translated.seq = msg.event.seq;

        // === STREAMING DELTA MERGE LOGIC ===
        // Delta events are merged in-place into a single live accumulator rather
        // than appended as separate list items. This keeps the event list clean
        // and avoids per-token React re-renders of the full list.
        if (
          eventKind === 'assistant_content_delta' ||
          eventKind === 'assistant_thinking_delta'
        ) {
          const messageId = msg.event?.kind?.message_id;
          setEventsBySession(prev => {
            const next = new Map(prev);
            const existing = next.get(msg.session_id) ?? [];
            const realLiveIdx = findLiveAccumulatorIndex(existing, messageId, msg.agent_id);

            if (realLiveIdx >= 0) {
              const updated = [...existing];
              const live = updated[realLiveIdx];
              if (eventKind === 'assistant_thinking_delta') {
                updated[realLiveIdx] = {
                  ...live,
                  thinking: (live.thinking ?? '') + (translated.thinking ?? ''),
                };
              } else {
                updated[realLiveIdx] = {
                  ...live,
                  content: live.content + translated.content,
                };
              }
              next.set(msg.session_id, updated);
              debugTrace('[useUiClient] stream delta merged', () => ({
                session_id: msg.session_id,
                event_kind: eventKind,
                message_id: messageId,
                live_index: realLiveIdx,
                existing_len: existing.length,
                new_content_len: updated[realLiveIdx].content.length,
                new_thinking_len: (updated[realLiveIdx].thinking ?? '').length,
              }));
            } else {
              // First delta for this message — create the live accumulator entry
              next.set(msg.session_id, [...existing, translated]);
              debugTrace('[useUiClient] stream delta created live accumulator', () => ({
                session_id: msg.session_id,
                event_kind: eventKind,
                message_id: messageId,
                existing_len: existing.length,
              }));
            }
            return next;
          });
          // Don't fall through to the normal append path
          break;
        }

        // === ASSISTANT MESSAGE STORED — replace live accumulator with final message ===
        if (eventKind === 'assistant_message_stored') {
          const messageId = msg.event?.kind?.message_id;
          setEventsBySession(prev => {
            const next = new Map(prev);
            const existing = next.get(msg.session_id) ?? [];
            const realLiveIdx = findLiveAccumulatorIndex(existing, messageId, msg.agent_id);

            if (realLiveIdx >= 0) {
              // Swap live accumulator → final message. Preserve streamed thinking if final event omitted it.
              const updated = [...existing];
              const live = updated[realLiveIdx];
              updated[realLiveIdx] = {
                ...translated,
                thinking: nonEmptyString(translated.thinking) ?? nonEmptyString(live.thinking),
              };
              next.set(msg.session_id, updated);
              debugTrace('[useUiClient] final assistant message replaced live accumulator', () => ({
                session_id: msg.session_id,
                message_id: messageId,
                live_index: realLiveIdx,
                final_content_len: updated[realLiveIdx].content.length,
                final_thinking_len: (updated[realLiveIdx].thinking ?? '').length,
              }));
            } else {
              // Non-streaming provider or out-of-order final message: append if newer.
              const lastSeq = existing.length > 0 ? (existing[existing.length - 1].seq ?? -1) : -1;
              if (translated.seq == null || translated.seq > lastSeq) {
                next.set(msg.session_id, [...existing, translated]);
              }
              debugTrace('[useUiClient] final assistant message appended without live accumulator', () => ({
                session_id: msg.session_id,
                message_id: messageId,
                existing_len: existing.length,
                translated_seq: translated.seq,
                last_seq: lastSeq,
              }));
            }
            return next;
          });
          // Still fall through so thinking-state logic below fires
        } else {
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
        }

        // Provider/limits updates - only for main session
        if (msg.session_id === mainSessionId) {
          if (eventKind === 'provider_changed') {
            debugLog('[useUiClient] provider_changed event: Setting agentModels', () => ({
              session_id: msg.session_id,
              agent_id: msg.agent_id,
              provider: msg.event?.kind?.provider,
              model: msg.event?.kind?.model,
              mainSessionId,
              seq: msg.event?.seq,
            }));
            setAgentModels((prev) => ({
              ...prev,
              [msg.agent_id]: {
                provider: msg.event?.kind?.provider,
                model: msg.event?.kind?.model,
                contextLimit: msg.event?.kind?.context_limit,
                node: msg.event?.kind?.provider_node ?? undefined,
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
        // Connection-level errors have no session_id. Do not inject them into the
        // active session timeline, otherwise provider errors can bleed across sessions.
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
        // Surface error to the UI via connectionErrors state
        {
          const errorId = Date.now();
          setConnectionErrors((prev) => [...prev, { id: errorId, message: msg.message }]);
          // Auto-dismiss after 8 seconds
          setTimeout(() => {
            setConnectionErrors((prev) => prev.filter((e) => e.id !== errorId));
          }, 8000);
        }
        break;
      }
      case 'session_list':
        setSessionGroups(msg.groups);
        break;
      case 'session_loaded': {
        setSessionId(msg.session_id);
        setMainSessionId(msg.session_id);
        setSessionAudit(msg.audit);
        // Hydrate undo stack from backend so refresh/load reflects persisted state.
        setUndoState((prev) => {
          const next = buildUndoStateFromServerStack(msg.undo_stack, prev);
          undoStateRef.current = next;
          return next;
        });
        
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
        
        // Update agentModels from the last provider event in the loaded session.
        // Clear first so stale model info from a previous session doesn't persist.
        const lastProvider = [...translated].reverse()
          .find(e => e.provider || e.model);
        if (lastProvider) {
          debugLog('[useUiClient] session_loaded: Setting agentModels from loaded session', () => ({
            session_id: msg.session_id,
            agent_id: msg.agent_id,
            provider: lastProvider.provider,
            model: lastProvider.model,
            eventCount: translated.length,
          }));
          setAgentModels({
            [msg.agent_id]: {
              provider: lastProvider.provider,
              model: lastProvider.model,
              contextLimit: lastProvider.contextLimit,
              node: lastProvider.providerNode,
            },
          });
        } else {
          debugLog('[useUiClient] session_loaded: Clearing agentModels (no provider info)');
          // No provider info in loaded session - clear stale model badge
          setAgentModels({});
        }
        
        // Subscribe to child delegation sessions
        for (const event of msg.audit.events) {
          if (
            (event.kind as any)?.type === 'session_forked' &&
            (event.kind as any)?.origin === 'delegation'
          ) {
            const childSessionId = (event.kind as any)?.child_session_id;
            sendMessage({
              type: 'subscribe_session',
              session_id: childSessionId,
              agent_id: (event.kind as any)?.target_agent_id,
            });
            // Track parent-child relationship
            setSessionParentMap(prev => {
              const next = new Map(prev);
              next.set(childSessionId, msg.session_id);
              return next;
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
      case 'provider_capabilities': {
        const next: Record<string, ProviderCapabilityEntry> = {};
        for (const entry of msg.providers) {
          next[entry.provider] = entry;
        }
        setProviderCapabilities(next);
        break;
      }
      case 'recent_models': {
        // Convert null keys to empty string for consistent lookup
        const normalized: Record<string, RecentModelEntry[]> = {};
        for (const [key, value] of Object.entries(msg.by_workspace)) {
          normalized[key === 'null' ? '' : key] = value;
        }
        setRecentModelsByWorkspace(normalized);
        break;
      }
      case 'auth_providers':
        setAuthProviders(msg.providers);
        break;
      case 'oauth_flow_started':
        setOauthFlow({
          flow_id: msg.flow_id,
          provider: msg.provider,
          authorization_url: msg.authorization_url,
          flow_kind: msg.flow_kind,
        });
        setOauthResult(null);
        break;
      case 'oauth_result':
        setOauthResult({
          provider: msg.provider,
          success: msg.success,
          message: msg.message,
        });
        if (msg.success) {
          setOauthFlow(null);
        }
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
        const filesByMessageId = new Map<string, string[]>();
        const messageIdForFiles = msg.message_id
          ?? msg.undo_stack[msg.undo_stack.length - 1]?.message_id;
        if (msg.success && messageIdForFiles) {
          filesByMessageId.set(messageIdForFiles, msg.reverted_files);
        }

        setUndoState((prev) => {
          const preferredFrontier = msg.success ? messageIdForFiles : undefined;
          const next = buildUndoStateFromServerStack(
            msg.undo_stack,
            prev,
            filesByMessageId,
            preferredFrontier
          );
          undoStateRef.current = next;
          return next;
        });

        if (msg.success) {
          debugLog('[useUiClient] Undo succeeded', () => ({ reverted_files: msg.reverted_files }));
        } else {
          console.error('[useUiClient] Undo failed:', msg.message);
        }
        break;
      }
      case 'redo_result': {
        setUndoState((prev) => {
          const next = buildUndoStateFromServerStack(msg.undo_stack, prev);
          undoStateRef.current = next;
          return next;
        });

        if (msg.success) {
          debugLog('[useUiClient] Redo succeeded');
        } else {
          console.error('[useUiClient] Redo failed:', msg.message);
        }
        break;
      }
      case 'agent_mode':
        setAgentModeState(msg.mode);
        break;
      case 'remote_nodes':
        setRemoteNodes(msg.nodes);
        break;
      case 'remote_sessions':
        // Currently used for on-demand node session listing;
        // data is handled by the caller via callback if needed.
        debugLog('[useUiClient] remote_sessions for node:', () => ({ node_id: msg.node_id, sessions: msg.sessions }));
        break;
      case 'model_download_status': {
        const key = `${msg.provider}:${msg.model_id}`;
        setModelDownloads((prev) => ({
          ...prev,
          [key]: {
            provider: msg.provider,
            model_id: msg.model_id,
            status: msg.status,
            bytes_downloaded: msg.bytes_downloaded,
            bytes_total: msg.bytes_total,
            percent: msg.percent,
            speed_bps: msg.speed_bps,
            eta_seconds: msg.eta_seconds,
            message: msg.message,
          },
        }));

        if (msg.status === 'completed') {
          setTimeout(() => {
            sendMessage({ type: 'list_all_models', refresh: true });
          }, 250);
        }
        break;
      }
      case 'plugin_update_status': {
        setIsUpdatingPlugins(true);
        setPluginUpdateStatus(prev => ({
          ...prev,
          [msg.plugin_name]: {
            plugin_name: msg.plugin_name,
            image_reference: msg.image_reference,
            phase: msg.phase,
            bytes_downloaded: msg.bytes_downloaded,
            bytes_total: msg.bytes_total,
            percent: msg.percent,
            message: msg.message,
          },
        }));
        break;
      }
      case 'plugin_update_complete': {
        setIsUpdatingPlugins(false);
        setPluginUpdateResults(msg.results);
        setTimeout(() => setPluginUpdateResults(null), 8000);
        break;
      }
      default:
        break;
    }
  };

  // Keep the ref always pointing to the latest version of handleServerMessage
  // so the WebSocket onmessage handler never uses a stale closure.
  handleServerMessageRef.current = handleServerMessage;

  useEffect(() => {
    undoStateRef.current = undoState;
  }, [undoState]);

  const sendMessage = (message: UiClientMessage) => {
    const socket = socketRef.current;
    if (!socket || socket.readyState !== WebSocket.OPEN) {
      return;
    }
    socket.send(JSON.stringify(message));
  };

  const requestWorkspacePath = useCallback((defaultValue: string) => {
    setWorkspacePathDialogDefaultValue(defaultValue);
    setWorkspacePathDialogOpen(true);
    return new Promise<{ cwd: string; node: string | null } | null>((resolve) => {
      workspacePathDialogResolverRef.current = resolve;
    });
  }, []);

  const submitWorkspacePathDialog = useCallback((value: string, node: string | null = null) => {
    const resolver = workspacePathDialogResolverRef.current;
    workspacePathDialogResolverRef.current = null;
    setWorkspacePathDialogOpen(false);
    if (resolver) {
      resolver({ cwd: value, node });
    }
  }, []);

  const cancelWorkspacePathDialog = useCallback(() => {
    const resolver = workspacePathDialogResolverRef.current;
    workspacePathDialogResolverRef.current = null;
    setWorkspacePathDialogOpen(false);
    if (resolver) {
      resolver(null);
    }
  }, []);

  const newSession = useCallback(async (): Promise<string> => {
    const currentWorkspace = findCurrentWorkspace(sessionGroups, sessionId);
    const initialWorkspace = currentWorkspace ?? (sessionId ? '' : defaultCwd ?? '');
    const result = await requestWorkspacePath(initialWorkspace);
    if (result === null) {
      throw new Error('Session creation cancelled');
    }
    const cwd = result.cwd.trim() || initialWorkspace.trim();
    const node = result.node;
    const requestId = uuidv7();
    
    // Signal that session creation is in progress to prevent route sync interference.
    // Cleared by useSessionRoute once URL and state are in sync.
    sessionCreatingRef.current = true;
    
    if (node) {
      // Remote session: send create_remote_session with request_id.
      // The backend responds with SessionCreated (same as local).
      return new Promise((resolve) => {
        pendingRequestsRef.current.set(requestId, resolve);
        sendMessage({
          type: 'create_remote_session',
          node_id: node,
          cwd: cwd.length > 0 ? cwd : null,
          request_id: requestId,
        });
      });
    }

    return new Promise((resolve) => {
      pendingRequestsRef.current.set(requestId, resolve);
      sendMessage({ 
        type: 'new_session', 
        cwd: cwd.length > 0 ? cwd : null,
        request_id: requestId 
      });
    });
  }, [requestWorkspacePath, sessionGroups, sessionId, defaultCwd]);

  const sendPrompt = useCallback(async (prompt: PromptBlock[]) => {
    sendMessage({ type: 'prompt', prompt });
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

  const setSessionModel = useCallback((sessionId: string, modelId: string, node?: string) => {
    sendMessage({ type: 'set_session_model', session_id: sessionId, model_id: modelId, node_id: node });
    // Refresh recent models after a short delay (only for local providers)
    if (!node) {
      setTimeout(() => {
        sendMessage({ type: 'get_recent_models', limit_per_workspace: 10 });
      }, 500);
    }
  }, []);

  const addCustomModelFromHf = useCallback(
    (provider: string, repo: string, filename: string, displayName?: string) => {
      sendMessage({
        type: 'add_custom_model_from_hf',
        provider,
        repo,
        filename,
        display_name: displayName,
      });
    },
    []
  );

  const addCustomModelFromFile = useCallback(
    (provider: string, filePath: string, displayName?: string) => {
      sendMessage({
        type: 'add_custom_model_from_file',
        provider,
        file_path: filePath,
        display_name: displayName,
      });
    },
    []
  );

  const deleteCustomModel = useCallback((provider: string, modelId: string) => {
    sendMessage({ type: 'delete_custom_model', provider, model_id: modelId });
  }, []);

  const fetchRecentModels = useCallback(() => {
    sendMessage({ type: 'get_recent_models', limit_per_workspace: 10 });
  }, []);

  const requestAuthProviders = useCallback(() => {
    sendMessage({ type: 'list_auth_providers' });
  }, []);

  const startOAuthLogin = useCallback((provider: string) => {
    setOauthResult(null);
    sendMessage({ type: 'start_oauth_login', provider });
  }, []);

  const completeOAuthLogin = useCallback((flowId: string, response: string) => {
    sendMessage({ type: 'complete_oauth_login', flow_id: flowId, response });
  }, []);

  const disconnectOAuth = useCallback((provider: string) => {
    setOauthFlow(null);
    setOauthResult(null);
    sendMessage({ type: 'disconnect_oauth', provider });
  }, []);

  const clearOAuthState = useCallback(() => {
    setOauthFlow(null);
    setOauthResult(null);
  }, []);

  // Register a callback for file index updates
  const setFileIndexCallback = useCallback((callback: FileIndexCallback | null) => {
    debugLog('[useUiClient] Registering file index callback:', () => ({ hasCallback: !!callback }));
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

  const deleteSession = useCallback((targetSessionId: string) => {
    sendMessage({ type: 'delete_session', session_id: targetSessionId });
  }, []);

  // Refresh the list of remote nodes from the mesh
  const listRemoteNodes = useCallback(() => {
    sendMessage({ type: 'list_remote_nodes' });
  }, []);

  // Dismiss a connection error by id
  const dismissConnectionError = useCallback((errorId: number) => {
    setConnectionErrors((prev) => prev.filter((e) => e.id !== errorId));
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
    if (undoStateRef.current?.stack.some((frame) => frame.status === 'pending')) {
      console.warn('[useUiClient] Undo ignored: undo confirmation pending');
      return;
    }

    const currentUndoState = undoStateRef.current;
    const nextUndoState = {
      stack: [
        ...(currentUndoState?.stack ?? []),
        { turnId, messageId, status: 'pending' as const, revertedFiles: [] },
      ],
      frontierMessageId: messageId,
    };

    undoStateRef.current = nextUndoState;
    sendMessage({ type: 'undo', message_id: messageId });
    // Optimistically push pending frame; confirmation arrives via undo_result.
    setUndoState(nextUndoState);
  }, []);

  const sendRedo = useCallback(() => {
    const currentUndoState = undoStateRef.current;
    if (!currentUndoState || currentUndoState.stack.length === 0) {
      console.warn('[useUiClient] Redo ignored: undo stack is empty');
      return;
    }
    if (currentUndoState.stack.some((frame) => frame.status === 'pending')) {
      console.warn('[useUiClient] Redo ignored: undo confirmation pending');
      return;
    }
    sendMessage({ type: 'redo' });
  }, []);

  const sendElicitationResponse = useCallback((
    elicitationId: string,
    action: 'accept' | 'decline' | 'cancel',
    content?: Record<string, unknown>
  ) => {
    sendMessage({ type: 'elicitation_response', elicitation_id: elicitationId, action, content });
  }, []);

  const setAgentMode = useCallback((mode: string) => {
    setAgentModeState(mode);  // Optimistic update
    sendMessage({ type: 'set_agent_mode', mode });
  }, []);

  const cycleAgentMode = useCallback(() => {
    const currentIndex = availableModes.indexOf(agentMode);
    const nextMode = availableModes[(currentIndex + 1) % availableModes.length];
    setAgentMode(nextMode);
  }, [agentMode, setAgentMode, availableModes]);

  const updatePlugins = useCallback(() => {
    setIsUpdatingPlugins(true);
    setPluginUpdateStatus({});
    setPluginUpdateResults(null);
    sendMessage({ type: 'update_plugins' });
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
    deleteSession,
    agents,
    routingMode,
    activeAgentId,
    setActiveAgent: selectAgent,
    setRoutingMode: selectRoutingMode,
    sessionGroups,
    allModels,
    providerCapabilities,
    modelDownloads,
    recentModelsByWorkspace,
    authProviders,
    oauthFlow,
    oauthResult,
    sessionsByAgent,
    agentModels,
    loadSession,
    refreshAllModels,
    fetchRecentModels,
    requestAuthProviders,
    startOAuthLogin,
    completeOAuthLogin,
    disconnectOAuth,
    clearOAuthState,
    setSessionModel,
    addCustomModelFromHf,
    addCustomModelFromFile,
    deleteCustomModel,
    sessionAudit,
    thinkingAgentId,
    thinkingAgentIds,
    thinkingBySession,
    sessionParentMap,
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
    sessionCreatingRef,
    workspacePathDialogOpen,
    workspacePathDialogDefaultValue,
    submitWorkspacePathDialog,
    cancelWorkspacePathDialog,
    sendElicitationResponse,
    agentMode,
    availableModes,
    setAgentMode,
    cycleAgentMode,
    remoteNodes,
    listRemoteNodes,
    connectionErrors,
    dismissConnectionError,
    pluginUpdateStatus,
    pluginUpdateResults,
    isUpdatingPlugins,
    updatePlugins,
  };
}

function findCurrentWorkspace(groups: SessionGroup[], activeSessionId: string | null): string | null {
  if (!activeSessionId) {
    return null;
  }
  for (const group of groups) {
    if (group.sessions.some((session) => session.session_id === activeSessionId)) {
      return group.cwd ?? null;
    }
  }
  return null;
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
      thinking: event.kind?.thinking,
      timestamp,
      isMessage: true,
      messageId: event.kind?.message_id,
      // streamMessageId matches message_id so UI can find and replace the live accumulator
      streamMessageId: event.kind?.message_id,
    };
  }

  if (kind === 'assistant_content_delta') {
    return {
      id,
      agentId,
      seq,
      type: 'agent',
      content: event.kind?.content ?? '',
      timestamp,
      isStreamDelta: true,
      isThinkingDelta: false,
      streamMessageId: event.kind?.message_id,
    };
  }

  if (kind === 'assistant_thinking_delta') {
    return {
      id,
      agentId,
      seq,
      type: 'agent',
      content: '',
      thinking: event.kind?.content ?? '',
      timestamp,
      isStreamDelta: true,
      isThinkingDelta: true,
      streamMessageId: event.kind?.message_id,
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
      providerNode: event.kind?.provider_node ?? undefined,
    };
  }

  if (kind === 'elicitation_requested') {
    return {
      id,
      agentId,
      seq,
      type: 'tool_call',  // Render it as a tool interaction
      content: event.kind?.message ?? 'Elicitation',
      timestamp,
      elicitationData: {
        elicitationId: event.kind?.elicitation_id,
        sessionId: event.kind?.session_id,
        message: event.kind?.message,
        requestedSchema: event.kind?.requested_schema,
        source: event.kind?.source ?? 'unknown',
      },
    };
  }

  if (kind === 'compaction_start') {
    return {
      id,
      agentId,
      seq,
      type: 'system',
      content: 'Context compaction started',
      timestamp,
      compactionTokenEstimate: event.kind?.token_estimate,
    };
  }

  if (kind === 'compaction_end') {
    return {
      id,
      agentId,
      seq,
      type: 'system',
      content: 'Context compacted',
      timestamp,
      compactionSummary: event.kind?.summary,
      compactionSummaryLen: event.kind?.summary_len,
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
