import { useEffect, useState, useCallback, useRef, useMemo } from 'react';
import { v7 as uuidv7 } from 'uuid';
import {
  EventItem,
  RoutingMode,
  UiAgentInfo,
  UiClientMessage,
  UiServerMessage,
  SessionGroup,
  UiPromptBlock,
  AuditView,
  FileIndexEntry,
  ModelEntry,
  RecentModelEntry,
  LlmConfigDetails,
  SessionLimits,
  AuthProviderEntry,
  AuthMethod,
  ModelDownloadStatus,
  OAuthFlowState,
  OAuthResultState,
  ProviderCapabilityEntry,
  OAuthFlowKind,
  UndoStackFrame,
  RemoteNodeInfo,
  PluginUpdateStatus,
  PluginUpdateResult,
  ScheduleInfo,
  KnowledgeEntryInfo,
  ConsolidationInfo,
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
  const [routingMode, setRoutingMode] = useState<RoutingMode>(RoutingMode.Single);
  const [activeAgentId, setActiveAgentId] = useState<string>('primary');
  const [sessionId, setSessionId] = useState<string | null>(null);
  const [connected, setConnected] = useState(false);
  const [agentMode, setAgentModeState] = useState<string>('build');
  const agentModeRef = useRef(agentMode);
  agentModeRef.current = agentMode;
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
  const [apiTokenResult, setApiTokenResult] = useState<{ provider: string; success: boolean; message: string } | null>(null);
  const [sessionsByAgent, setSessionsByAgent] = useState<Record<string, string>>({});
  const [agentModels, setAgentModels] = useState<
    Record<string, { provider?: string; model?: string; contextLimit?: number; node?: string }>
  >({});
  const [sessionAudit, setSessionAudit] = useState<AuditView | null>(null);
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
  const [sessionActionNotices, setSessionActionNotices] = useState<
    { id: number; kind: 'success' | 'error'; message: string }[]
  >([]);
  // Tracks the most recent session ID that failed to load so consumers
  // (e.g. useSessionManager) can navigate away rather than retrying.
  const [lastLoadErrorSessionId, setLastLoadErrorSessionId] = useState<string | null>(null);
  const [pluginUpdateStatus, setPluginUpdateStatus] = useState<Record<string, PluginUpdateStatus>>({});
  const [pluginUpdateResults, setPluginUpdateResults] = useState<PluginUpdateResult[] | null>(null);
  const [isUpdatingPlugins, setIsUpdatingPlugins] = useState(false);
  const [schedules, setSchedules] = useState<ScheduleInfo[]>([]);
  const [knowledgeEntries, setKnowledgeEntries] = useState<KnowledgeEntryInfo[]>([]);
  const [knowledgeConsolidations, setKnowledgeConsolidations] = useState<ConsolidationInfo[]>([]);
  const [knowledgeStats, setKnowledgeStats] = useState<{
    totalEntries: number;
    unconsolidatedEntries: number;
    totalConsolidations: number;
    latestEntryAt: string | null;
    latestConsolidationAt: string | null;
  } | null>(null);
  const [defaultCwd, setDefaultCwd] = useState<string | null>(null);
  const [workspacePathDialogOpen, setWorkspacePathDialogOpen] = useState(false);
  const [workspacePathDialogDefaultValue, setWorkspacePathDialogDefaultValue] = useState('');
  const socketRef = useRef<WebSocket | null>(null);
  const fileIndexCallbackRef = useRef<FileIndexCallback | null>(null);
  const fileIndexErrorCallbackRef = useRef<FileIndexErrorCallback | null>(null);
  const llmConfigCallbacksRef = useRef<Map<number, (config: LlmConfigDetails) => void>>(new Map());
  const pendingRequestsRef = useRef<Map<string, (sessionId: string) => void>>(new Map());
  const pendingDeleteLabelsRef = useRef<Map<string, string>>(new Map());
  const pendingLoadLabelsRef = useRef<Map<string, string>>(new Map());
  const pendingForkResolverRef = useRef<{
    resolve: (sessionId: string) => void;
    reject: (reason?: unknown) => void;
  } | null>(null);
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
      sendMessage({ type: 'list_all_models', data: { refresh: false } });
      sendMessage({ type: 'get_recent_models', data: { limit_per_workspace: 10 } });
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
      const pendingFork = pendingForkResolverRef.current;
      pendingForkResolverRef.current = null;
      if (pendingFork) {
        sessionCreatingRef.current = false;
        pendingFork.reject(new Error('Socket closed before fork completed'));
      }
      if (socketRef.current) {
        socketRef.current.close();
      }
    };
  }, []);

  const pushSessionActionNotice = useCallback((kind: 'success' | 'error', message: string) => {
    const id = Date.now() + Math.floor(Math.random() * 1000);
    setSessionActionNotices((prev) => [...prev, { id, kind, message }]);
    setTimeout(() => {
      setSessionActionNotices((prev) => prev.filter((n) => n.id !== id));
    }, 5000);
  }, []);

  const handleServerMessage = (msg: UiServerMessage) => {
    debugLog('[useUiClient] Received message:', () => ({ type: msg.type, msg }));
    switch (msg.type) {
      case 'state': {
        const d = msg.data;
        setAgents(d.agents);
        setRoutingMode(d.routing_mode);
        setActiveAgentId(d.active_agent_id);
        setSessionId(d.active_session_id ?? null);
        setDefaultCwd(d.default_cwd ?? null);
        setSessionsByAgent(d.sessions_by_agent ?? {});
        if (d.agent_mode) {
          setAgentModeState(d.agent_mode);
        }
        break;
      }
      case 'session_created': {
        const d = msg.data;
        if (d.agent_id === activeAgentId) {
          setSessionId(d.session_id);
          setMainSessionId(d.session_id);
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
        if (d.request_id && pendingRequestsRef.current.has(d.request_id)) {
          pendingRequestsRef.current.get(d.request_id)!(d.session_id);
          pendingRequestsRef.current.delete(d.request_id);
        }
        break;
      }
      case 'session_events': {
        const d = msg.data;
        // Replay batch - set all events for this session
        // Events arrive as EventEnvelope[] (adjacently tagged); unwrap before translating.
        const translated = d.events.map((e: any) => {
          const unwrapped = unwrapEnvelope(e);
          const item = translateAgentEvent(d.agent_id, unwrapped);
          item.sessionId = d.session_id;
          item.seq = unwrapped.seq;
          return item;
        });
        setEventsBySession(prev => {
          const next = new Map(prev);
          next.set(d.session_id, translated);
          return next;
        });

        // If this is the main session, update agentModels from last provider event
        if (d.session_id === mainSessionId) {
          const lastProvider = [...translated].reverse()
            .find(e => e.provider || e.model);
          if (lastProvider) {
            debugLog('[useUiClient] session_events: Setting agentModels from replay', () => ({
              session_id: d.session_id,
              agent_id: d.agent_id,
              provider: lastProvider.provider,
              model: lastProvider.model,
              mainSessionId,
              eventCount: translated.length,
            }));
            setAgentModels(prev => ({
              ...prev,
              [d.agent_id]: {
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
        const d = msg.data;
        // AgentEventKind uses adjacently tagged serde: kind.data holds the payload
        // EventEnvelope is also adjacently tagged: unwrap .data to get the inner event.
        const eventEnvelope = unwrapEnvelope(d.event);
        const eventKind = eventEnvelope?.kind?.type;
        const kindData = eventEnvelope?.kind?.data ?? {};

        if (
          eventKind === 'llm_request_start' ||
          eventKind === 'llm_request_end' ||
          eventKind === 'assistant_thinking_delta' ||
          eventKind === 'assistant_content_delta' ||
          eventKind === 'assistant_message_stored'
        ) {
          const rawContent = kindData.content;
          const contentLen = typeof rawContent === 'string' ? rawContent.length : 0;
          debugTrace('[useUiClient] stream event received', () => ({
            session_id: d.session_id,
            agent_id: d.agent_id,
            seq: eventEnvelope?.seq,
            event_kind: eventKind,
            message_id: kindData.message_id,
            content_len: contentLen,
            has_thinking: typeof kindData.thinking === 'string' && kindData.thinking.length > 0,
            finish_reason: kindData.finish_reason,
          }));
        }
        
        // Track LLM thinking state per session
        // turn_started fires right after user_message_stored, before history
        // loading / middleware / tool collection, so the UI can show the
        // "thinking..." indicator immediately instead of an empty card.
        if (eventKind === 'turn_started' || eventKind === 'llm_request_start') {
          setThinkingBySession(prev => {
            const next = new Map(prev);
            const sessionAgents = new Set(next.get(d.session_id) ?? []);
            sessionAgents.add(d.agent_id);
            next.set(d.session_id, sessionAgents);
            return next;
          });
          // Clear conversation complete flag for the main session
          if (d.session_id === mainSessionId) {
            setIsConversationComplete(false);
          }
          // turn_started is a side-effect-only event (sets thinking state).
          // Don't append it to the event list — it has no display purpose.
          if (eventKind === 'turn_started') {
            break;
          }
        } else if (eventKind === 'llm_request_end') {
          const finishReason = kindData.finish_reason;
          if (finishReason === 'stop' || finishReason === 'Stop') {
            setThinkingBySession(prev => {
              const next = new Map(prev);
              const sessionAgents = new Set(next.get(d.session_id) ?? []);
              sessionAgents.delete(d.agent_id);
              if (sessionAgents.size === 0) {
                next.delete(d.session_id);
                // Set conversation complete flag only for main session
                if (d.session_id === mainSessionId) {
                  setIsConversationComplete(true);
                  setTimeout(() => setIsConversationComplete(false), 2000);
                }
              } else {
                next.set(d.session_id, sessionAgents);
              }
              return next;
            });
          } else if (finishReason === 'tool_calls' || finishReason === 'ToolCalls') {
            // Tool calls requested, still thinking
          } else {
            setThinkingBySession(prev => {
              const next = new Map(prev);
              const sessionAgents = new Set(next.get(d.session_id) ?? []);
              sessionAgents.delete(d.agent_id);
              if (sessionAgents.size === 0) {
                next.delete(d.session_id);
              } else {
                next.set(d.session_id, sessionAgents);
              }
              return next;
            });
          }
        } else if (eventKind === 'prompt_received') {
          const isCurrentMainOrActiveSession =
            d.session_id === mainSessionId || d.session_id === sessionId;
          if (isCurrentMainOrActiveSession) {
            setIsConversationComplete(false);
            // A new prompt commits the current timeline branch; stacked redo history is no longer valid.
            setUndoState(null);
            undoStateRef.current = null;
          }
        } else if (eventKind === 'error') {
          setThinkingBySession(prev => {
            const next = new Map(prev);
            const sessionAgents = new Set(next.get(d.session_id) ?? []);
            sessionAgents.delete(d.agent_id);
            if (sessionAgents.size === 0) {
              next.delete(d.session_id);
            } else {
              next.set(d.session_id, sessionAgents);
            }
            return next;
          });
        } else if (eventKind === 'cancelled') {
          setThinkingBySession(prev => {
            const next = new Map(prev);
            const sessionAgents = new Set(next.get(d.session_id) ?? []);
            sessionAgents.delete(d.agent_id);
            if (sessionAgents.size === 0) {
              next.delete(d.session_id);
            } else {
              next.set(d.session_id, sessionAgents);
            }
            return next;
          });
        } else if (eventKind === 'delegation_cancelled') {
          // When a delegation is cancelled, the delegate agent should be removed from thinking state
          // The d.agent_id here is the delegator (parent), but we need to clear thinking state
          // for the delegate agent. Since we track thinking per agent, and cancellation of the
          // delegation will also trigger 'cancelled' on the child session, we rely on that.
          // However, we can defensively clear all thinking state to ensure UI responsiveness.
          // For now, just trust the child session's 'cancelled' event will arrive.
        }

        // Auto-subscribe to delegation child sessions
        // kindData.origin is the session_forked data payload (string field)
        if (eventKind === 'session_forked' && kindData.origin === 'delegation') {
          const childSessionId = kindData.child_session_id;
          sendMessage({
            type: 'subscribe_session',
            data: {
              session_id: childSessionId,
              agent_id: kindData.target_agent_id,
            },
          } as any);
          // Track parent-child relationship for thinking state propagation
          setSessionParentMap(prev => {
            const next = new Map(prev);
            next.set(childSessionId, d.session_id);
            return next;
          });
        }

        // Translate and route to correct session bucket with dedup
        const translated = translateAgentEvent(d.agent_id, eventEnvelope);
        translated.sessionId = d.session_id;
        translated.seq = eventEnvelope?.seq;

        // === STREAMING DELTA MERGE LOGIC ===
        // Delta events are merged in-place into a single live accumulator rather
        // than appended as separate list items. This keeps the event list clean
        // and avoids per-token React re-renders of the full list.
        if (
          eventKind === 'assistant_content_delta' ||
          eventKind === 'assistant_thinking_delta'
        ) {
          const messageId = kindData.message_id;
          setEventsBySession(prev => {
            const next = new Map(prev);
            const existing = next.get(d.session_id) ?? [];
            const realLiveIdx = findLiveAccumulatorIndex(existing, messageId, d.agent_id);

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
              next.set(d.session_id, updated);
              debugTrace('[useUiClient] stream delta merged', () => ({
                session_id: d.session_id,
                event_kind: eventKind,
                message_id: messageId,
                live_index: realLiveIdx,
                existing_len: existing.length,
                new_content_len: updated[realLiveIdx].content.length,
                new_thinking_len: (updated[realLiveIdx].thinking ?? '').length,
              }));
            } else {
              // First delta for this message — create the live accumulator entry
              next.set(d.session_id, [...existing, translated]);
              debugTrace('[useUiClient] stream delta created live accumulator', () => ({
                session_id: d.session_id,
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
          const messageId = kindData.message_id;
          setEventsBySession(prev => {
            const next = new Map(prev);
            const existing = next.get(d.session_id) ?? [];
            const realLiveIdx = findLiveAccumulatorIndex(existing, messageId, d.agent_id);

            if (realLiveIdx >= 0) {
              // Swap live accumulator → final message. Preserve streamed thinking if final event omitted it.
              const updated = [...existing];
              const live = updated[realLiveIdx];
              updated[realLiveIdx] = {
                ...translated,
                thinking: nonEmptyString(translated.thinking) ?? nonEmptyString(live.thinking),
              };
              next.set(d.session_id, updated);
              debugTrace('[useUiClient] final assistant message replaced live accumulator', () => ({
                session_id: d.session_id,
                message_id: messageId,
                live_index: realLiveIdx,
                final_content_len: updated[realLiveIdx].content.length,
                final_thinking_len: (updated[realLiveIdx].thinking ?? '').length,
              }));
            } else {
              // Non-streaming provider or out-of-order final message: append if newer.
              const lastSeq = existing.length > 0 ? (existing[existing.length - 1].seq ?? -1) : -1;
              if (translated.seq == null || translated.seq > lastSeq) {
                next.set(d.session_id, [...existing, translated]);
              }
              debugTrace('[useUiClient] final assistant message appended without live accumulator', () => ({
                session_id: d.session_id,
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
            const existing = next.get(d.session_id) ?? [];
            // Dedup: skip if we already have this seq
            if (existing.length > 0 && translated.seq != null) {
              const lastSeq = existing[existing.length - 1].seq ?? -1;
              if (translated.seq <= lastSeq) return prev;
            }
            next.set(d.session_id, [...existing, translated]);
            return next;
          });
        }

        // Provider/limits updates - only for main session
        if (d.session_id === mainSessionId) {
          if (eventKind === 'provider_changed') {
            debugLog('[useUiClient] provider_changed event: Setting agentModels', () => ({
              session_id: d.session_id,
              agent_id: d.agent_id,
              provider: kindData.provider,
              model: kindData.model,
              mainSessionId,
              seq: eventEnvelope?.seq,
            }));
            setAgentModels((prev) => ({
              ...prev,
              [d.agent_id]: {
                provider: kindData.provider,
                model: kindData.model,
                contextLimit: kindData.context_limit,
                node: kindData.provider_node_id ?? kindData.provider_node ?? undefined,
              },
            }));
          }
          if (eventKind === 'session_configured' && kindData.limits) {
            setSessionLimits(kindData.limits);
          }
        }
        break;
      }
      case 'error': {
        const d = msg.data;
        console.error('UI server error:', d.message);
        const isDeleteError = d.message.includes('Failed to delete session');
        const isLoadError = d.message.includes('Failed to load session');

        if (isDeleteError) {
          pendingDeleteLabelsRef.current.clear();
          pushSessionActionNotice('error', d.message);
          sendMessage({ type: 'list_sessions' } as UiClientMessage);
        }

        if (isLoadError) {
          const pendingEntries = Array.from(pendingLoadLabelsRef.current.entries());
          const [failedSessionId, pendingLabel] = pendingEntries[pendingEntries.length - 1] ?? [null, undefined];
          pendingLoadLabelsRef.current.clear();
          pushSessionActionNotice(
            'error',
            pendingLabel
              ? `Failed to open session: ${pendingLabel}`
              : d.message
          );
          if (failedSessionId) {
            setLastLoadErrorSessionId(failedSessionId);
          }
          sendMessage({ type: 'list_sessions' } as UiClientMessage);
        }

        // Connection-level errors have no session_id. Do not inject them into the
        // active session timeline, otherwise provider errors can bleed across sessions.
        setThinkingBySession(new Map());
        // Check if this is a file index related error and notify
        if (
          fileIndexErrorCallbackRef.current &&
          (d.message.includes('workspace') ||
            d.message.includes('File index') ||
            d.message.includes('working directory'))
        ) {
          fileIndexErrorCallbackRef.current(d.message);
        }
        // Surface non-session-action errors to the generic connection error toast queue
        if (!isDeleteError && !isLoadError) {
          const errorId = Date.now();
          setConnectionErrors((prev) => [...prev, { id: errorId, message: d.message }]);
          // Auto-dismiss after 8 seconds
          setTimeout(() => {
            setConnectionErrors((prev) => prev.filter((e) => e.id !== errorId));
          }, 8000);
        }
        break;
      }
      case 'session_list': {
        const d = msg.data;
        setSessionGroups(d.groups);

        if (pendingDeleteLabelsRef.current.size > 0) {
          const remainingSessionIds = new Set(
            d.groups.flatMap((group: any) => group.sessions.map((session: any) => session.session_id))
          );

          for (const [pendingId, label] of pendingDeleteLabelsRef.current.entries()) {
            if (!remainingSessionIds.has(pendingId)) {
              pendingDeleteLabelsRef.current.delete(pendingId);
              pushSessionActionNotice('success', `Deleted session: ${label}`);
            }
          }
        }
        break;
      }
      case 'session_loaded': {
        const d = msg.data;
        pendingLoadLabelsRef.current.delete(d.session_id);
        setSessionId(d.session_id);
        setMainSessionId(d.session_id);
        setSessionAudit(d.audit);
        // Hydrate undo stack from backend so refresh/load reflects persisted state.
        setUndoState((prev) => {
          const next = buildUndoStateFromServerStack(d.undo_stack, prev);
          undoStateRef.current = next;
          return next;
        });
        
        // Populate eventsBySession from the audit events (for old session history)
        const translated = d.audit.events.map((e: any) => {
          const item = translateAgentEvent(d.agent_id, e);
          item.sessionId = d.session_id;
          item.seq = e.seq;
          return item;
        });
        
        // Initialize eventsBySession with the main session's events
        const eventsMap = new Map();
        eventsMap.set(d.session_id, translated);
        setEventsBySession(eventsMap);
        
        // Update agentModels from the last provider event in the loaded session.
        // Clear first so stale model info from a previous session doesn't persist.
        const lastProvider = [...translated].reverse()
          .find(e => e.provider || e.model);
        if (lastProvider) {
          debugLog('[useUiClient] session_loaded: Setting agentModels from loaded session', () => ({
            session_id: d.session_id,
            agent_id: d.agent_id,
            provider: lastProvider.provider,
            model: lastProvider.model,
            eventCount: translated.length,
          }));
          setAgentModels({
            [d.agent_id]: {
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
        for (const event of d.audit.events) {
          if (
            (event.kind as any)?.type === 'session_forked' &&
            (event.kind as any)?.data?.origin === 'delegation'
          ) {
            const childSessionId = (event.kind as any)?.data?.child_session_id;
            sendMessage({
              type: 'subscribe_session',
              data: {
                session_id: childSessionId,
                agent_id: (event.kind as any)?.data?.target_agent_id,
              },
            } as any);
            // Track parent-child relationship
            setSessionParentMap(prev => {
              const next = new Map(prev);
              next.set(childSessionId, d.session_id);
              return next;
            });
          }
        }
        break;
      }
      case 'workspace_index_status': {
        const d = msg.data;
        setWorkspaceIndexStatus(prev => ({
          ...prev,
          [d.session_id]: { status: d.status as 'building' | 'ready' | 'error', message: d.message ?? null },
        }));
        break;
      }
      case 'file_index': {
        const d = msg.data;
        if (fileIndexCallbackRef.current) {
          fileIndexCallbackRef.current(d.files, d.generated_at);
        }
        break;
      }
      case 'all_models_list': {
        const d = msg.data;
        setAllModels(d.models);
        break;
      }
      case 'provider_capabilities': {
        const d = msg.data;
        const next: Record<string, ProviderCapabilityEntry> = {};
        for (const entry of d.providers) {
          next[entry.provider] = entry;
        }
        setProviderCapabilities(next);
        break;
      }
      case 'recent_models': {
        const d = msg.data;
        // Convert null keys to empty string for consistent lookup
        const normalized: Record<string, RecentModelEntry[]> = {};
        for (const [key, value] of Object.entries(d.by_workspace)) {
          normalized[key === 'null' ? '' : key] = value as RecentModelEntry[];
        }
        setRecentModelsByWorkspace(normalized);
        break;
      }
      case 'auth_providers': {
        const d = msg.data;
        setAuthProviders(d.providers);
        break;
      }
      case 'oauth_flow_started': {
        const d = msg.data;
        setOauthFlow({
          flow_id: d.flow_id,
          provider: d.provider,
          authorization_url: d.authorization_url,
          flow_kind: d.flow_kind as OAuthFlowKind,
        });
        setOauthResult(null);
        break;
      }
      case 'oauth_result': {
        const d = msg.data;
        setOauthResult({
          provider: d.provider,
          success: d.success,
          message: d.message,
        });
        if (d.success) {
          setOauthFlow(null);
        }
        break;
      }
      case 'api_token_result': {
        const d = msg.data;
        setApiTokenResult({
          provider: d.provider,
          success: d.success,
          message: d.message,
        });
        break;
      }
      case 'llm_config': {
        const d = msg.data;
        const config: LlmConfigDetails = {
          configId: d.config_id,
          provider: d.provider,
          model: d.model,
          params: d.params,
        };
        // Cache the config
        setLlmConfigCache((prev) => ({ ...prev, [d.config_id]: config }));
        // Notify any pending callbacks
        const callback = llmConfigCallbacksRef.current.get(d.config_id);
        if (callback) {
          callback(config);
          llmConfigCallbacksRef.current.delete(d.config_id);
        }
        break;
      }
      case 'undo_result': {
        const d = msg.data;
        const filesByMessageId = new Map<string, string[]>();
        const messageIdForFiles = d.message_id
          ?? d.undo_stack[d.undo_stack.length - 1]?.message_id;
        if (d.success && messageIdForFiles) {
          filesByMessageId.set(messageIdForFiles, d.reverted_files);
        }

        setUndoState((prev) => {
          const preferredFrontier = d.success ? messageIdForFiles : undefined;
          const next = buildUndoStateFromServerStack(
            d.undo_stack,
            prev,
            filesByMessageId,
            preferredFrontier
          );
          undoStateRef.current = next;
          return next;
        });

        if (d.success) {
          debugLog('[useUiClient] Undo succeeded', () => ({ reverted_files: d.reverted_files }));
        } else {
          console.error('[useUiClient] Undo failed:', d.message);
        }
        break;
      }
      case 'redo_result': {
        const d = msg.data;
        setUndoState((prev) => {
          const next = buildUndoStateFromServerStack(d.undo_stack, prev);
          undoStateRef.current = next;
          return next;
        });

        if (d.success) {
          debugLog('[useUiClient] Redo succeeded');
        } else {
          console.error('[useUiClient] Redo failed:', d.message);
        }
        break;
      }
      case 'fork_result': {
        const d = msg.data;
        const pendingFork = pendingForkResolverRef.current;
        pendingForkResolverRef.current = null;

        if (d.success && d.forked_session_id) {
          if (pendingFork) {
            pendingFork.resolve(d.forked_session_id);
          }
        } else {
          const errorMessage = d.message ?? 'Failed to fork session';
          sessionCreatingRef.current = false;
          if (pendingFork) {
            pendingFork.reject(new Error(errorMessage));
          }
          pushSessionActionNotice('error', errorMessage);
        }
        break;
      }
      case 'agent_mode': {
        const d = msg.data;
        setAgentModeState(d.mode);
        break;
      }
      case 'remote_nodes': {
        const d = msg.data;
        setRemoteNodes(d.nodes);
        break;
      }
      case 'remote_sessions': {
        const d = msg.data;
        // Currently used for on-demand node session listing;
        // data is handled by the caller via callback if needed.
        debugLog('[useUiClient] remote_sessions for node:', () => ({ node_id: d.node_id, sessions: d.sessions }));
        break;
      }
      case 'model_download_status': {
        const d = msg.data;
        const key = `${d.provider}:${d.model_id}`;
        setModelDownloads((prev) => ({
          ...prev,
          [key]: {
            provider: d.provider,
            model_id: d.model_id,
            status: d.status,
            bytes_downloaded: d.bytes_downloaded,
            bytes_total: d.bytes_total,
            percent: d.percent,
            speed_bps: d.speed_bps,
            eta_seconds: d.eta_seconds,
            message: d.message,
          },
        }));

        if (d.status === 'completed') {
          setTimeout(() => {
            sendMessage({ type: 'list_all_models', data: { refresh: true } });
          }, 250);
        }
        break;
      }
      case 'plugin_update_status': {
        const d = msg.data;
        setIsUpdatingPlugins(true);
        setPluginUpdateStatus(prev => ({
          ...prev,
          [d.plugin_name]: {
            plugin_name: d.plugin_name,
            image_reference: d.image_reference,
            phase: d.phase,
            bytes_downloaded: d.bytes_downloaded,
            bytes_total: d.bytes_total,
            percent: d.percent,
            message: d.message,
          },
        }));
        break;
      }
      case 'plugin_update_complete': {
        const d = msg.data;
        setIsUpdatingPlugins(false);
        setPluginUpdateResults(d.results);
        setTimeout(() => setPluginUpdateResults(null), 8000);
        break;
      }
      case 'schedule_list': {
        const d = msg.data;
        setSchedules(d.schedules);
        break;
      }
      case 'schedule_created_result': {
        const d = msg.data;
        if (!d.success) {
          pushSessionActionNotice('error', d.message ?? 'Failed to create schedule');
        }
        break;
      }
      case 'schedule_action_result': {
        const d = msg.data;
        if (!d.success) {
          pushSessionActionNotice('error', d.message ?? `Failed to ${d.action} schedule`);
        }
        break;
      }
      case 'knowledge_query_result': {
        const d = msg.data;
        setKnowledgeEntries(d.entries);
        setKnowledgeConsolidations(d.consolidations);
        break;
      }
      case 'knowledge_list_result': {
        const d = msg.data;
        setKnowledgeEntries(d.entries);
        break;
      }
      case 'knowledge_stats_result': {
        const d = msg.data;
        setKnowledgeStats({
          totalEntries: d.total_entries,
          unconsolidatedEntries: d.unconsolidated_entries,
          totalConsolidations: d.total_consolidations,
          latestEntryAt: d.latest_entry_at ?? null,
          latestConsolidationAt: d.latest_consolidation_at ?? null,
        });
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
          data: {
            node_id: node,
            cwd: cwd.length > 0 ? cwd : undefined,
            request_id: requestId,
          },
        });
      });
    }

    return new Promise((resolve) => {
      pendingRequestsRef.current.set(requestId, resolve);
      sendMessage({
        type: 'new_session',
        data: {
          cwd: cwd.length > 0 ? cwd : undefined,
          request_id: requestId,
        },
      });
    });
  }, [requestWorkspacePath, sessionGroups, sessionId, defaultCwd]);

  const sendPrompt = useCallback(async (prompt: UiPromptBlock[]) => {
    sendMessage({ type: 'prompt', data: { prompt } });
  }, []);

  const selectAgent = useCallback((agentId: string) => {
    setActiveAgentId(agentId);
    sendMessage({ type: 'set_active_agent', data: { agent_id: agentId } });
  }, []);

  const selectRoutingMode = useCallback((mode: RoutingMode) => {
    setRoutingMode(mode);
    sendMessage({ type: 'set_routing_mode', data: { mode } });
  }, []);

  const loadSession = useCallback((sessionId: string, sessionLabel?: string) => {
    const label = sessionLabel && sessionLabel.trim().length > 0 ? sessionLabel : sessionId;
    pendingLoadLabelsRef.current.set(sessionId, label);
    sendMessage({ type: 'load_session', data: { session_id: sessionId } });
  }, []);

  const attachRemoteSession = useCallback((nodeId: string, sessionId: string, sessionLabel?: string) => {
    const label = sessionLabel && sessionLabel.trim().length > 0 ? sessionLabel : sessionId;
    pendingLoadLabelsRef.current.set(sessionId, label);
    sendMessage({ type: 'attach_remote_session', data: { node_id: nodeId, session_id: sessionId } });
  }, []);

  const refreshAllModels = useCallback(() => {
    sendMessage({ type: 'list_all_models', data: { refresh: true } });
  }, []);

  const setSessionModel = useCallback((sessionId: string, modelId: string, node?: string) => {
    sendMessage({ type: 'set_session_model', data: { session_id: sessionId, model_id: modelId, node_id: node } });
    // Refresh recent models after a short delay (only for local providers)
    if (!node) {
      setTimeout(() => {
        sendMessage({ type: 'get_recent_models', data: { limit_per_workspace: 10 } });
      }, 500);
    }
  }, []);

  const addCustomModelFromHf = useCallback(
    (provider: string, repo: string, filename: string, displayName?: string) => {
      sendMessage({
        type: 'add_custom_model_from_hf',
        data: { provider, repo, filename, display_name: displayName },
      });
    },
    []
  );

  const addCustomModelFromFile = useCallback(
    (provider: string, filePath: string, displayName?: string) => {
      sendMessage({
        type: 'add_custom_model_from_file',
        data: { provider, file_path: filePath, display_name: displayName },
      });
    },
    []
  );

  const deleteCustomModel = useCallback((provider: string, modelId: string) => {
    sendMessage({ type: 'delete_custom_model', data: { provider, model_id: modelId } });
  }, []);

  const fetchRecentModels = useCallback(() => {
    sendMessage({ type: 'get_recent_models', data: { limit_per_workspace: 10 } });
  }, []);

  const requestAuthProviders = useCallback(() => {
    sendMessage({ type: 'list_auth_providers' });
  }, []);

  const startOAuthLogin = useCallback((provider: string) => {
    setOauthResult(null);
    sendMessage({ type: 'start_oauth_login', data: { provider } });
  }, []);

  const completeOAuthLogin = useCallback((flowId: string, response: string) => {
    sendMessage({ type: 'complete_oauth_login', data: { flow_id: flowId, response } });
  }, []);

  const disconnectOAuth = useCallback((provider: string) => {
    setOauthFlow(null);
    setOauthResult(null);
    sendMessage({ type: 'disconnect_oauth', data: { provider } });
  }, []);

  const clearOAuthState = useCallback(() => {
    setOauthFlow(null);
    setOauthResult(null);
  }, []);

  const setApiToken = useCallback((provider: string, apiKey: string) => {
    setApiTokenResult(null);
    sendMessage({ type: 'set_api_token', data: { provider, api_key: apiKey } });
  }, []);

  const clearApiToken = useCallback((provider: string) => {
    setApiTokenResult(null);
    sendMessage({ type: 'clear_api_token', data: { provider } });
  }, []);

  const setAuthMethodPref = useCallback((provider: string, method: AuthMethod) => {
    sendMessage({ type: 'set_auth_method', data: { provider, method } });
  }, []);

  const clearApiTokenResult = useCallback(() => {
    setApiTokenResult(null);
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

  const deleteSession = useCallback((targetSessionId: string, sessionLabel?: string) => {
    const label = sessionLabel && sessionLabel.trim().length > 0 ? sessionLabel : targetSessionId;
    pendingDeleteLabelsRef.current.set(targetSessionId, label);
    sendMessage({ type: 'delete_session', data: { session_id: targetSessionId } });
  }, []);

  // Refresh the list of remote nodes from the mesh
  const listRemoteNodes = useCallback(() => {
    sendMessage({ type: 'list_remote_nodes' });
  }, []);

  // Dismiss a connection error by id
  const dismissConnectionError = useCallback((errorId: number) => {
    setConnectionErrors((prev) => prev.filter((e) => e.id !== errorId));
  }, []);

  const dismissSessionActionNotice = useCallback((noticeId: number) => {
    setSessionActionNotices((prev) => prev.filter((notice) => notice.id !== noticeId));
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
    sendMessage({ type: 'get_llm_config', data: { config_id: configId } });
  }, [llmConfigCache]);

  // Derive the flat thinkingAgentIds set from the per-session map so that
  // consumers who need a global view (e.g. AppShell's isSessionActive indicator
  // and the double-ESC cancel shortcut) still work correctly without maintaining
  // a separate piece of state that can drift out of sync.
  const thinkingAgentIds = useMemo(() => {
    const all = new Set<string>();
    for (const agents of thinkingBySession.values()) {
      for (const a of agents) all.add(a);
    }
    return all;
  }, [thinkingBySession]);

  // Derive thinkingAgentId for backward compatibility
  const thinkingAgentId = thinkingAgentIds.size > 0 ? Array.from(thinkingAgentIds).pop()! : null;

  const subscribeSession = useCallback((sessionId: string, agentId?: string) => {
    sendMessage({ type: 'subscribe_session', data: { session_id: sessionId, agent_id: agentId } });
  }, []);

  const unsubscribeSession = useCallback((sessionId: string) => {
    sendMessage({ type: 'unsubscribe_session', data: { session_id: sessionId } });
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
    sendMessage({ type: 'undo', data: { message_id: messageId } });
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

  const forkSessionAtMessage = useCallback((messageId: string): Promise<string> => {
    if (pendingForkResolverRef.current) {
      return Promise.reject(new Error('Another fork request is already in progress'));
    }

    return new Promise((resolve, reject) => {
      pendingForkResolverRef.current = { resolve, reject };
      sendMessage({ type: 'fork_session', data: { message_id: messageId } });
    });
  }, []);

  const sendElicitationResponse = useCallback((
    elicitationId: string,
    action: 'accept' | 'decline' | 'cancel',
    content?: Record<string, unknown>
  ) => {
    sendMessage({ type: 'elicitation_response', data: { elicitation_id: elicitationId, action, content } });
  }, []);

  const setAgentMode = useCallback((mode: string) => {
    setAgentModeState(mode);  // Optimistic update
    sendMessage({ type: 'set_agent_mode', data: { mode } });
  }, []);

  const cycleAgentMode = useCallback(() => {
    const currentIndex = availableModes.indexOf(agentModeRef.current);
    const nextMode = availableModes[(currentIndex + 1) % availableModes.length];
    setAgentMode(nextMode);
}, [setAgentMode, availableModes]);

  const updatePlugins = useCallback(() => {
    setIsUpdatingPlugins(true);
    setPluginUpdateStatus({});
    setPluginUpdateResults(null);
    sendMessage({ type: 'update_plugins' });
  }, []);

  // ── Schedule management ──────────────────────────────────────────────────

  const listSchedules = useCallback((sessionId?: string) => {
    sendMessage({ type: 'list_schedules', data: { session_id: sessionId } });
  }, []);

  const createSchedule = useCallback((
    sessionId: string,
    prompt: string,
    trigger: any,
    opts?: { maxSteps?: number; maxCostUsd?: number; maxRuns?: number },
  ) => {
    sendMessage({
      type: 'create_schedule',
      data: {
        session_id: sessionId,
        prompt,
        trigger,
        max_steps: opts?.maxSteps,
        max_cost_usd: opts?.maxCostUsd,
        max_runs: opts?.maxRuns,
      },
    });
  }, []);

  const pauseSchedule = useCallback((schedulePublicId: string) => {
    sendMessage({ type: 'pause_schedule', data: { schedule_public_id: schedulePublicId } });
  }, []);

  const resumeSchedule = useCallback((schedulePublicId: string) => {
    sendMessage({ type: 'resume_schedule', data: { schedule_public_id: schedulePublicId } });
  }, []);

  const triggerScheduleNow = useCallback((schedulePublicId: string) => {
    sendMessage({ type: 'trigger_schedule', data: { schedule_public_id: schedulePublicId } });
  }, []);

  const deleteSchedule = useCallback((schedulePublicId: string) => {
    sendMessage({ type: 'delete_schedule', data: { schedule_public_id: schedulePublicId } });
  }, []);

  const queryKnowledge = useCallback((scope: string, question: string, limit?: number) => {
    sendMessage({ type: 'query_knowledge', data: { scope, question, limit } });
  }, []);

  const listKnowledge = useCallback((scope: string, filter?: Record<string, unknown>) => {
    sendMessage({ type: 'list_knowledge', data: { scope, filter } });
  }, []);

  const getKnowledgeStats = useCallback((scope: string) => {
    sendMessage({ type: 'knowledge_stats', data: { scope } });
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
    attachRemoteSession,
    refreshAllModels,
    fetchRecentModels,
    requestAuthProviders,
    startOAuthLogin,
    completeOAuthLogin,
    disconnectOAuth,
    clearOAuthState,
    apiTokenResult,
    setApiToken,
    clearApiToken,
    setAuthMethodPref,
    clearApiTokenResult,
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
    forkSessionAtMessage,
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
    sessionActionNotices,
    dismissSessionActionNotice,
    lastLoadErrorSessionId,
    pluginUpdateStatus,
    pluginUpdateResults,
    isUpdatingPlugins,
    updatePlugins,
    schedules,
    listSchedules,
    createSchedule,
    pauseSchedule,
    resumeSchedule,
    triggerScheduleNow,
    deleteSchedule,
    knowledgeEntries,
    knowledgeConsolidations,
    knowledgeStats,
    queryKnowledge,
    listKnowledge,
    getKnowledgeStats,
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

/** Unwrap an EventEnvelope (adjacently tagged) into a flat AgentEvent-like object. */
function unwrapEnvelope(envelope: any): any {
  const inner = envelope?.data ?? envelope;
  return {
    ...inner,
    seq: inner.stream_seq ?? inner.seq,
  };
}

function translateAgentEvent(agentId: string, event: any): EventItem {
  const kind = event?.kind?.type ?? event?.kind?.type_name ?? event?.kind?.type;
  // AgentEventKind uses adjacently tagged serde: kind.data holds the variant payload
  const kindData = event?.kind?.data ?? {};
  const timestamp = typeof event.timestamp === 'number' ? event.timestamp * 1000 : Date.now();
  const id = event.seq ? String(event.seq) : `${Date.now()}-${Math.random()}`;
  const seq = event.seq;

  if (kind === 'tool_call_start') {
    return {
      id,
      agentId,
      seq: seq,
      type: 'tool_call',
      content: kindData.tool_name ?? 'tool_call',
      timestamp,
      toolCall: {
        tool_call_id: kindData.tool_call_id,
        kind: kindData.tool_name,
        status: 'in_progress',
        raw_input: parseJsonMaybe(kindData.arguments),
      },
    };
  }

  if (kind === 'tool_call_end') {
    const status = kindData.is_error ? 'failed' : 'completed';
    return {
      id,
      agentId,
      seq: seq,
      type: 'tool_result',
      content: kindData.result ?? '',
      timestamp,
      toolCall: {
        tool_call_id: kindData.tool_call_id,
        kind: kindData.tool_name,
        status,
        raw_output: parseJsonMaybe(kindData.result),
      },
    };
  }

  if (kind === 'prompt_received') {
    return {
      id,
      agentId,
      seq: seq,
      type: 'user',
      content: kindData.content ?? '',
      timestamp,
      isMessage: true,
      messageId: kindData.message_id,
    };
  }

  if (kind === 'assistant_message_stored') {
    return {
      id,
      agentId,
      seq: seq,
      type: 'agent',
      content: kindData.content ?? '',
      thinking: kindData.thinking,
      timestamp,
      isMessage: true,
      messageId: kindData.message_id,
      // streamMessageId matches message_id so UI can find and replace the live accumulator
      streamMessageId: kindData.message_id,
    };
  }

  if (kind === 'assistant_content_delta') {
    return {
      id,
      agentId,
      seq,
      type: 'agent',
      content: kindData.content ?? '',
      timestamp,
      isStreamDelta: true,
      isThinkingDelta: false,
      streamMessageId: kindData.message_id,
    };
  }

  if (kind === 'assistant_thinking_delta') {
    return {
      id,
      agentId,
      seq,
      type: 'agent',
      content: '',
      thinking: kindData.content ?? '',
      timestamp,
      isStreamDelta: true,
      isThinkingDelta: true,
      streamMessageId: kindData.message_id,
    };
  }

  if (kind === 'llm_request_end') {
    return {
      id,
      agentId,
      type: 'agent',
      content: `Event: llm_request_end`,
      timestamp,
      usage: kindData.usage,
      costUsd: kindData.cost_usd,
      cumulativeCostUsd: kindData.cumulative_cost_usd,
      contextTokens: kindData.context_tokens,
      finishReason: kindData.finish_reason,
      metrics: kindData.metrics,
    };
  }

  if (kind === 'delegation_requested') {
    return {
      id,
      agentId,
      type: 'agent',
      content: `Event: delegation_requested`,
      timestamp,
      delegationId: kindData.delegation?.public_id,
      delegationTargetAgentId: kindData.delegation?.target_agent_id,
      delegationObjective: kindData.delegation?.objective,
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
      delegationId: kindData.delegation_id,
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
      delegationId: kindData.delegation_id,
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
      forkChildSessionId: kindData.child_session_id,
      forkDelegationId: kindData.fork_point_ref,
    };
  }

  if (kind === 'provider_changed') {
    return {
      id,
      agentId,
      type: 'agent',
      content: `Event: provider_changed`,
      timestamp,
      provider: kindData.provider,
      model: kindData.model,
      contextLimit: kindData.context_limit,
      configId: kindData.config_id,
      providerNode: kindData.provider_node_id ?? kindData.provider_node ?? undefined,
    };
  }

  if (kind === 'elicitation_requested') {
    return {
      id,
      agentId,
      seq,
      type: 'tool_call',  // Render it as a tool interaction
      content: kindData.message ?? 'Elicitation',
      timestamp,
      elicitationData: {
        elicitationId: kindData.elicitation_id,
        sessionId: kindData.session_id,
        message: kindData.message,
        requestedSchema: kindData.requested_schema,
        source: kindData.source ?? 'unknown',
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
      compactionTokenEstimate: kindData.token_estimate,
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
      compactionSummary: kindData.summary,
      compactionSummaryLen: kindData.summary_len,
    };
  }

  // Note: 'error' event kinds are handled in the switch statement above
  // by resetting thinking state. This translation just converts to EventItem.
  if (kind === 'error') {
    return {
      id,
      agentId: 'system',
      type: 'system',
      content: kindData.message ?? 'Error',
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
