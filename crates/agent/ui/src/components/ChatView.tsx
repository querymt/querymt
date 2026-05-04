/**
 * ChatView.tsx - Session chat view component
 *
 * Displays the chat timeline for an active session, including:
 * - Turn-based message view (Virtuoso)
 * - Chat/Delegations tabs
 * - Input area with mention support
 * - Todo rail (when todos exist)
 * - Tool detail modal
 * - Delegation drawer
 */

import { useState, useRef, useEffect, useMemo, useCallback } from 'react';
import { Virtuoso, type VirtuosoHandle } from 'react-virtuoso';
import { ChevronDown } from 'lucide-react';
import { useUiClientActions, useUiClientEvents, useUiClientSession, useUiClientConfig } from '../context/UiClientContext';
import { useUiStore } from '../store/uiStore';
import { useVoiceOutput } from '../hooks/useVoiceOutput';
import { useVoiceStore } from '../store/voiceStore';
import { useSessionManager } from '../hooks/useSessionManager';
import { useFileMention } from '../hooks/useFileMention';
import { useTodoState } from '../hooks/useTodoState';
import { EventItem, EventRow, Turn } from '../types';
import { ChatInputBar } from './ChatInputBar';
import { ChatTabBar } from './ChatTabBar';
import { ToolDetailModal } from './ToolDetailModal';
import { TurnCard } from './TurnCard';
import { DelegationsView } from './DelegationsView';
import { DelegationDrawer } from './DelegationDrawer';
import { TodoRail } from './TodoRail';
import { SchedulePanel } from './SchedulePanel';
import { SessionPicker } from './SessionPicker';
import { ThinkingIndicator } from './ThinkingIndicator';
import { SystemLog } from './SystemLog';
import { PinnedUserMessage } from './PinnedUserMessage';
import { WelcomeScreen } from './WelcomeScreen';
import { buildTurns, buildDelegationTurn, buildEventRowsWithDelegations, isRateLimitEvent, processRateLimitEvent, buildPromptBlocksFromInput } from '../logic/chatViewLogic';
import { RateLimitIndicator } from './RateLimitIndicator';
import { useIsMobile } from '../hooks/useIsMobile';

/** Stable empty array shared across TurnCard instances to avoid new allocations. */
const emptyRevertedFiles: string[] = [];

/** Pre-computed undo/overlay state for a single turn. */
interface TurnUndoProps {
  isUndone: boolean;
  isUndoPending: boolean;
  isStackedUndone: boolean;
  revertedFiles: string[];
}

export function ChatView() {
  // Split context subscriptions — ChatView subscribes to Events + Session + Actions
  // (no Config context), so auth/model-list/plugin changes won't trigger re-renders.
  const {
    sendPrompt,
    cancelSession,
    deleteSession,
    loadMoreSessions,
    loadMoreGroupSessions,
    searchSessions,
    loadSessionChildren,
    setFileIndexCallback,
    setFileIndexErrorCallback,
    requestFileIndex,
    requestLlmConfig,
    sendUndo,
    sendRedo,
    forkSessionAtMessage,
    listSchedules,
    pauseSchedule,
    resumeSchedule,
    triggerScheduleNow,
    deleteSchedule,
  } = useUiClientActions();

  const {
    events,
    eventsBySession,
    mainSessionId,
  } = useUiClientEvents();

  const {
    sessionId,
    connected,
    reconnecting,
    agents,
    sessionGroups,
    thinkingBySession,
    sessionParentMap,
    sessionNextCursor,
    sessionTotalCount,
    sessionPageLoading,
    sessionChildrenLoading,
    workspaceIndexStatus,
    llmConfigCache,
    undoState,
    schedules,
  } = useUiClientSession();

  // UI state from Zustand store
  const {
    prompt,
    setPrompt,
    loading,
    setLoading,
    todoRailCollapsed,
    setTodoRailCollapsed,
    isAtBottom,
    setIsAtBottom,
    activeTimelineView,
    setActiveTimelineView,
    activeDelegationId,
    setActiveDelegationId,
    followNewMessages,
    selectedToolEvent,
    setSelectedToolEvent,
    delegationDrawerOpen,
    setDelegationDrawerOpen,
    rateLimitBySession,
    setRateLimitState,
    clearRateLimitState,
    compactingBySession,
    setCompactingState,
    setMainInputRef,
    schedulePanelCollapsed,
    setSchedulePanelCollapsed,
    setCreateScheduleDialogOpen,
  } = useUiStore();

  const isMobile = useIsMobile();
  const { audioCapabilities } = useUiClientConfig();
  const { ttsProvider, ttsModel, ttsVoice } = useVoiceStore();

  const { speak: speakTts } = useVoiceOutput({
    provider: ttsProvider,
    model: ttsModel,
    voice: ttsVoice || undefined,
  });

  const handleSpeakTurn = useCallback((text: string) => {
    speakTts(text);
  }, [speakTts]);

  // Only pass onSpeakTurn if TTS is available
  const onSpeakTurn = audioCapabilities.tts_models.length > 0 ? handleSpeakTurn : undefined;

  // Fetch schedules when session changes
  useEffect(() => {
    if (sessionId) {
      listSchedules(sessionId);
    }
  }, [sessionId, listSchedules]);

  // Whether to show the schedule panel (has schedules or panel was explicitly opened)
  const showSchedulePanel = schedules.length > 0;

  // Get rate limit state for current session
  const rateLimitState = sessionId ? rateLimitBySession.get(sessionId) : undefined;

  // Get live compaction state for current session
  const compactingState = sessionId ? compactingBySession.get(sessionId) : undefined;

  // Session-scoped thinking state (replaces global thinkingAgentId)
  const sessionThinkingAgentId = useMemo(() => {
    if (!sessionId || !thinkingBySession) return null;
    const agentSet = thinkingBySession.get(sessionId);
    if (!agentSet || agentSet.size === 0) return null;
    return Array.from(agentSet).pop()!;
  }, [sessionId, thinkingBySession]);

  const virtuosoRef = useRef<VirtuosoHandle | null>(null);
  const chatTimelineRef = useRef<HTMLDivElement | null>(null);
  const followArmedRef = useRef(false);
  const previousEventCountRef = useRef(0);
  const trailingFollowTimeoutRef = useRef<number | null>(null);
  const previousThinkingAgentIdRef = useRef<string | null>(null);
  const mentionInputRef = useRef<HTMLTextAreaElement>(null);
  const promptRef = useRef(prompt);
  const activeIndexStatus = sessionId ? workspaceIndexStatus[sessionId]?.status : undefined;

  promptRef.current = prompt;

  // File mention hook
  const fileMention = useFileMention(requestFileIndex);

  // Register main input ref for focus management
  useEffect(() => {
    setMainInputRef(mentionInputRef);
    return () => setMainInputRef(null);
  }, [setMainInputRef]);

  // Register file index callback
  useEffect(() => {
    setFileIndexCallback(fileMention.handleFileIndex);
    return () => {
      setFileIndexCallback(null);
    };
  }, [setFileIndexCallback, fileMention.handleFileIndex]);

  // Register file index error callback
  useEffect(() => {
    setFileIndexErrorCallback(fileMention.handleFileIndexError);
    return () => setFileIndexErrorCallback(null);
  }, [setFileIndexErrorCallback, fileMention.handleFileIndexError]);

  // Clear file index when session changes (different session = different CWD = different files)
  useEffect(() => {
    fileMention.resetIndex();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [sessionId]);

  // useSessionManager for session navigation
  const { selectSession, createSession, goHome } = useSessionManager();

  // Process rate limit events
  useEffect(() => {
    if (!sessionId) return;

    const sessionEvents = events;
    const latestEvent = sessionEvents[sessionEvents.length - 1];
    if (latestEvent && isRateLimitEvent(latestEvent)) {
      processRateLimitEvent(latestEvent, sessionId, setRateLimitState);
    }
  }, [events, sessionId, setRateLimitState]);

  // Clear rate limit state when switching sessions
  useEffect(() => {
    return () => {
      if (sessionId) {
        clearRateLimitState(sessionId);
      }
    };
  }, [sessionId, clearRateLimitState]);

  // Process compaction events to drive the live compacting indicator
  useEffect(() => {
    if (!sessionId) return;
    const latestEvent = events[events.length - 1];
    if (!latestEvent) return;
    if (latestEvent.compactionTokenEstimate !== undefined && latestEvent.content === 'Context compaction started') {
      // compaction_start: show live indicator
      setCompactingState(sessionId, {
        tokenEstimate: latestEvent.compactionTokenEstimate,
        startedAt: latestEvent.timestamp,
      });
    } else if (latestEvent.compactionSummary !== undefined) {
      // compaction_end: clear live indicator (compaction card will appear via turn data)
      setCompactingState(sessionId, null);
    }
  }, [events, sessionId, setCompactingState]);

  // Clear compaction state when switching sessions
  useEffect(() => {
    return () => {
      if (sessionId) {
        setCompactingState(sessionId, null);
      }
    };
  }, [sessionId, setCompactingState]);

  // Keyboard shortcuts (Ctrl+X chords, double Esc, etc. moved to AppShell)

  const handleSendPrompt = async () => {
    if (!prompt.trim() || loading || !sessionId) return;

    fileMention.clear();
    followArmedRef.current = followNewMessages;

    setLoading(true);
    try {
      await sendPrompt(buildPromptBlocksFromInput(prompt));
      setPrompt('');
    } catch (err) {
      followArmedRef.current = false;
      console.error('Failed to send prompt:', err);
    } finally {
      setLoading(false);
    }
  };

  const handleNewSession = async () => {
    try {
      await createSession();
    } catch (err) {
      console.error('Failed to create session:', err);
    }
  };

  const handleSelectSession = useCallback((sessionId: string) => {
    selectSession(sessionId);
  }, [selectSession]);

  const handleDeleteSession = useCallback((targetSessionId: string, sessionLabel?: string) => {
    deleteSession(targetSessionId, sessionLabel);
    if (targetSessionId === sessionId) {
      goHome();
    }
  }, [deleteSession, sessionId, goHome]);

  // Handle cancel during rate limit wait
  const handleCancelRateLimit = useCallback(() => {
    if (sessionId) {
      cancelSession();
      // State will be cleared when cancel event is received
    }
  }, [sessionId, cancelSession]);



  // Build turns from events and enrich delegations with child session events
  const {
    turns,
    delegations,
    hasMultipleModels: sessionHasMultipleModels,
  } = useMemo(() => {
    const result = buildTurns(events, sessionThinkingAgentId);

    // Enrich delegations with child session events from eventsBySession
    const claimedSessionIds = new Set<string>();
    for (const delegation of result.delegations) {
      // Use the childSessionId directly if available (set from session_forked event)
      if (delegation.childSessionId) {
        claimedSessionIds.add(delegation.childSessionId);
        const sessionEvents = eventsBySession.get(delegation.childSessionId);
        if (sessionEvents) {
          const { rows } = buildEventRowsWithDelegations(sessionEvents);
          delegation.events = rows;
        }
      } else if (delegation.targetAgentId) {
        // Fallback: scan for child session by matching target agent ID
        for (const [sessionId, sessionEvents] of eventsBySession.entries()) {
          if (sessionId === mainSessionId) continue; // Skip main session
          if (claimedSessionIds.has(sessionId)) continue; // Skip already claimed sessions
          const hasMatchingAgent = sessionEvents.some((e: EventItem) => e.agentId === delegation.targetAgentId);
          if (hasMatchingAgent) {
            claimedSessionIds.add(sessionId);
            delegation.childSessionId = sessionId;
            const { rows } = buildEventRowsWithDelegations(sessionEvents);
            delegation.events = rows;
            break;
          }
        }
      }
    }

    return result;
  }, [events, eventsBySession, mainSessionId, sessionThinkingAgentId]);
  const systemEvents = useMemo(
    () => events.filter((event: EventItem) =>
      event.type === 'system' &&
      !event.compactionTokenEstimate &&  // exclude compaction_start
      !event.compactionSummary           // exclude compaction_end
    ),
    [events]
  );
  const [systemClearIndex, setSystemClearIndex] = useState(0);
  const visibleSystemEvents = useMemo(
    () => systemEvents.slice(systemClearIndex),
    [systemEvents, systemClearIndex]
  );

  useEffect(() => {
    if (systemEvents.length === 0) {
      setSystemClearIndex(0);
      return;
    }
    if (systemClearIndex > systemEvents.length) {
      setSystemClearIndex(0);
    }
  }, [systemEvents, systemClearIndex]);

  const handleClearSystemEvents = useCallback(() => {
    setSystemClearIndex(systemEvents.length);
  }, [systemEvents.length]);

  const filteredTurns = useMemo(() => {
    // For now, show all turns. We can add turn-level filtering later if needed
    return turns;
  }, [turns]);
  const hasTurns = filteredTurns.length > 0;
  const hasDelegations = delegations.length > 0;

  // --- Pinned user-message bar (DOM-based scroll detection) ---
  const scrollerNodeRef = useRef<HTMLElement | null>(null);
  const pinnedRafRef = useRef<number | null>(null);
  const [pinnedMessage, setPinnedMessage] = useState<{ content: string; timestamp: number; turnIndex: number } | null>(null);

  const scrollerRefCallback = useCallback((ref: HTMLElement | Window | null) => {
    scrollerNodeRef.current = ref instanceof HTMLElement ? ref : null;
  }, []);

  const updatePinnedMessage = useCallback(() => {
    const scroller = scrollerNodeRef.current;
    if (!scroller || filteredTurns.length === 0) {
      setPinnedMessage(null);
      return;
    }

    if (scroller.scrollHeight - scroller.scrollTop - scroller.clientHeight <= 10) {
      setPinnedMessage(null);
      return;
    }

    const scrollerRect = scroller.getBoundingClientRect();
    const nodes = scroller.querySelectorAll<HTMLElement>('.user-message[data-turn-index]');

    let candidate: { content: string; timestamp: number; turnIndex: number } | null = null;
    for (const node of nodes) {
      if (node.getBoundingClientRect().bottom <= scrollerRect.top) {
        const idx = Number(node.dataset.turnIndex);
        const msg = filteredTurns[idx]?.userMessage;
        if (msg && (!candidate || idx > candidate.turnIndex)) {
          candidate = { content: msg.content, timestamp: msg.timestamp, turnIndex: idx };
        }
      }
    }
    setPinnedMessage(candidate);
  }, [filteredTurns]);

  useEffect(() => {
    const scroller = scrollerNodeRef.current;
    if (!scroller) return;

    const onScroll = () => {
      if (pinnedRafRef.current !== null) return;
      pinnedRafRef.current = requestAnimationFrame(() => {
        pinnedRafRef.current = null;
        updatePinnedMessage();
      });
    };

    scroller.addEventListener('scroll', onScroll, { passive: true });
    updatePinnedMessage();

    return () => {
      scroller.removeEventListener('scroll', onScroll);
      if (pinnedRafRef.current !== null) {
        cancelAnimationFrame(pinnedRafRef.current);
        pinnedRafRef.current = null;
      }
    };
  }, [updatePinnedMessage]);

  useEffect(() => {
    updatePinnedMessage();
  }, [updatePinnedMessage]);

  const handleJumpBackToPinnedMessage = useCallback(() => {
    if (pinnedMessage) {
      virtuosoRef.current?.scrollToIndex({ index: pinnedMessage.turnIndex, behavior: 'smooth', align: 'start' });
    }
  }, [pinnedMessage]);

  // Calculate current undo candidate. If we already undid a turn, move left from that message frontier.
  const undoTurnIndex = useMemo(() => {
    const frontierMessageId = undoState?.frontierMessageId;
    let startIndex = filteredTurns.length - 1;

    if (frontierMessageId) {
      const frontierIndex = filteredTurns.findIndex(
        turn => turn.userMessage?.messageId === frontierMessageId
      );
      if (frontierIndex >= 0) {
        startIndex = frontierIndex - 1;
      }
    }

    for (let i = startIndex; i >= 0; i--) {
      const turn = filteredTurns[i];
      // Only user-initiated turns are undo-eligible.
      if (!!turn.userMessage?.messageId) {
        return i;
      }
    }
    return -1;
  }, [filteredTurns, undoState?.frontierMessageId]);

  // Pre-compute per-turn undo/overlay props so that itemContent doesn't
  // need to recalculate on every render. This map is cheap to rebuild
  // (only iterates filteredTurns when undoState changes) and keeps the
  // Virtuoso itemContent closure free of undoState references.
  const turnUndoPropsMap = useMemo(() => {
    const map = new Map<number, TurnUndoProps>();
    if (!undoState || undoState.stack.length === 0) return map;

    const frontierFrame = undoState.frontierMessageId
      ? undoState.stack.find((frame) => frame.messageId === undoState.frontierMessageId)
      : undefined;
    const effectiveFrontierFrame = frontierFrame
      ?? undoState.stack[undoState.stack.length - 1];

    for (let i = 0; i < filteredTurns.length; i++) {
      const turnMessageId = filteredTurns[i].userMessage?.messageId;
      if (!turnMessageId) continue;

      const frameForTurn = undoState.stack.find(frame => frame.messageId === turnMessageId);
      if (!frameForTurn) continue;

      const isFrontierFrame = frameForTurn.messageId === effectiveFrontierFrame?.messageId;
      map.set(i, {
        isUndoPending: isFrontierFrame && effectiveFrontierFrame?.status === 'pending',
        isUndone: isFrontierFrame && effectiveFrontierFrame?.status === 'confirmed',
        isStackedUndone: frameForTurn.status === 'confirmed' && !isFrontierFrame,
        revertedFiles: isFrontierFrame && effectiveFrontierFrame?.status === 'confirmed'
          ? (effectiveFrontierFrame?.revertedFiles ?? [])
          : [],
      });
    }
    return map;
  }, [filteredTurns, undoState]);

  // Handle undo for a specific turn (stable: depends on filteredTurns ref, not per-item closure)
  const handleUndoTurn = useCallback((turnIndex: number) => {
    const turn = filteredTurns[turnIndex];
    const userMessage = turn.userMessage;
    if (!userMessage?.messageId) {
      console.error('[ChatView] Cannot undo: no message ID found for turn', turn.id);
      return;
    }

    // Restore the undone user text into the composer only when the user has
    // not already started typing a new draft.  We can only restore the stored
    // prompt text representation here; file mentions come back as plain text.
    if (userMessage.content && !promptRef.current.trim()) {
      setPrompt(userMessage.content);
    }

    console.log('[ChatView] Undoing turn', turn.id, 'with message ID', userMessage.messageId);
    sendUndo(userMessage.messageId, turn.id);
  }, [filteredTurns, sendUndo, setPrompt]);

  // Handle redo
  const handleRedo = useCallback(() => {
    console.log('[ChatView] Redoing changes');
    sendRedo();
  }, [sendRedo]);

  const handleForkTurn = useCallback(async (turnIndex: number) => {
    const turn = filteredTurns[turnIndex];
    const lastAssistantMessageId = [...turn.agentMessages]
      .reverse()
      .find((message) => !!message.messageId)?.messageId;
    const messageId = lastAssistantMessageId ?? turn.userMessage?.messageId;
    if (!messageId) {
      console.error('[ChatView] Cannot fork: no message ID found for turn', turn.id);
      return;
    }

    try {
      const forkedSessionId = await forkSessionAtMessage(messageId);
      selectSession(forkedSessionId);
    } catch (err) {
      console.error('[ChatView] Failed to fork session:', err);
    }
  }, [filteredTurns, forkSessionAtMessage, selectSession]);

  // Handle tool click to open modal
  const handleToolClick = useCallback((event: EventRow) => {
    setSelectedToolEvent(event);
  }, []);

  // Handle delegation click - open drawer
  const handleDelegateClick = useCallback((delegationId: string) => {
    setActiveDelegationId(delegationId);
    setDelegationDrawerOpen(true);
  }, [setActiveDelegationId, setDelegationDrawerOpen]);

  const activeDelegation = useMemo(
    () => delegations.find((delegation) => delegation.id === activeDelegationId),
    [delegations, activeDelegationId]
  );
  const activeDelegationTurn = useMemo(
    () => (activeDelegation ? buildDelegationTurn(activeDelegation) : null),
    [activeDelegation]
  );

  // Delegation-aware todo event selection
  const todoEvents = useMemo(() => {
    if (activeTimelineView === 'delegations' && activeDelegation?.events) {
      return activeDelegation.events;
    }
    return events; // Default to main session events
  }, [activeTimelineView, activeDelegation, events]);

  // Todo state hook with delegation-aware events
  const { todos, hasTodos, stats: todoStats, recentlyChangedIds } = useTodoState(todoEvents);

  // Compute showTodoRail with delegation-aware logic
  const showTodoRail = useMemo(() => {
    if (activeTimelineView === 'delegations') {
      // In delegations view, only show if a delegation is selected and has todos
      return !!activeDelegation && hasTodos;
    }
    // In chat view, show if main session has todos
    return hasTodos;
  }, [activeTimelineView, activeDelegation, hasTodos]);

  // Keyboard shortcut: Cmd+Shift+T / Ctrl+Shift+T to toggle todo rail
  useEffect(() => {
    const handleKeyDown = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && e.shiftKey && e.key === 'T' && showTodoRail) {
        e.preventDefault();
        setTodoRailCollapsed(!todoRailCollapsed);
      }
    };

    window.addEventListener('keydown', handleKeyDown);
    return () => window.removeEventListener('keydown', handleKeyDown);
  }, [showTodoRail, todoRailCollapsed, setTodoRailCollapsed]);

  useEffect(() => {
    if (events.length > 0 && activeDelegationId && !activeDelegation) {
      setActiveDelegationId(null);
    }
  }, [activeDelegation, activeDelegationId, events.length]);

  useEffect(() => {
    if (events.length > 0 && activeTimelineView === 'delegations' && delegations.length === 0) {
      setActiveTimelineView('chat');
    }
  }, [activeTimelineView, delegations.length, events.length]);

  // Fallback to first when selected delegation disappears (due to updates/changes)
  useEffect(() => {
    if (activeTimelineView === 'delegations' && activeDelegationId) {
      const stillExists = delegations.some(d => d.id === activeDelegationId);
      if (!stillExists && delegations.length > 0) {
        setActiveDelegationId(delegations[0].id);
      }
    }
  }, [delegations, activeTimelineView, activeDelegationId]);

  const scrollToLatest = useCallback((behavior: 'auto' | 'smooth' = 'auto') => {
    if (filteredTurns.length === 0) {
      return;
    }
    const index = filteredTurns.length - 1;
    const scroll = () => {
      virtuosoRef.current?.scrollToIndex({
        index,
        align: 'end',
        behavior,
      });
    };

    scroll();
    const rafOne = window.requestAnimationFrame(scroll);
    const rafTwo = window.requestAnimationFrame(() => {
      window.requestAnimationFrame(scroll);
    });

    return () => {
      window.cancelAnimationFrame(rafOne);
      window.cancelAnimationFrame(rafTwo);
    };
  }, [filteredTurns.length]);

  const clearTrailingFollowTimeout = useCallback(() => {
    if (trailingFollowTimeoutRef.current !== null) {
      window.clearTimeout(trailingFollowTimeoutRef.current);
      trailingFollowTimeoutRef.current = null;
    }
  }, []);

  const scheduleTrailingFollowScroll = useCallback(() => {
    clearTrailingFollowTimeout();
    trailingFollowTimeoutRef.current = window.setTimeout(() => {
      trailingFollowTimeoutRef.current = null;
      if (
        !followNewMessages ||
        !followArmedRef.current ||
        activeTimelineView !== 'chat' ||
        filteredTurns.length === 0
      ) {
        return;
      }
      scrollToLatest('auto');
    }, 120);
  }, [clearTrailingFollowTimeout, followNewMessages, activeTimelineView, filteredTurns.length, scrollToLatest]);

  const handleAtBottomStateChange = useCallback((atBottom: boolean) => {
    setIsAtBottom(atBottom);
    if (atBottom && followNewMessages) {
      followArmedRef.current = true;
    }
  }, [setIsAtBottom, followNewMessages]);

  useEffect(() => {
    const previousEventCount = previousEventCountRef.current;
    const hasNewEvents = events.length > previousEventCount;
    previousEventCountRef.current = events.length;

    if (
      !hasNewEvents ||
      !followNewMessages ||
      !followArmedRef.current ||
      activeTimelineView !== 'chat' ||
      filteredTurns.length === 0
    ) {
      return;
    }

    const cancelImmediateScroll = scrollToLatest('auto');
    scheduleTrailingFollowScroll();
    return () => {
      if (cancelImmediateScroll) {
        cancelImmediateScroll();
      }
      clearTrailingFollowTimeout();
    };
  }, [
    events.length,
    followNewMessages,
    activeTimelineView,
    filteredTurns.length,
    scrollToLatest,
    scheduleTrailingFollowScroll,
    clearTrailingFollowTimeout,
  ]);

  useEffect(() => {
    const wasThinking = previousThinkingAgentIdRef.current !== null;
    const isThinking = sessionThinkingAgentId !== null;
    previousThinkingAgentIdRef.current = sessionThinkingAgentId;

    if (!wasThinking || isThinking) {
      return;
    }
    if (
      !followNewMessages ||
      !followArmedRef.current ||
      activeTimelineView !== 'chat' ||
      filteredTurns.length === 0
    ) {
      return;
    }

    scheduleTrailingFollowScroll();
    return clearTrailingFollowTimeout;
  }, [
    sessionThinkingAgentId,
    followNewMessages,
    activeTimelineView,
    filteredTurns.length,
    scheduleTrailingFollowScroll,
    clearTrailingFollowTimeout,
  ]);

  useEffect(() => {
    if (!followNewMessages) {
      followArmedRef.current = false;
    }
  }, [followNewMessages]);

  useEffect(() => {
    previousEventCountRef.current = 0;
    previousThinkingAgentIdRef.current = null;
    followArmedRef.current = false;
    clearTrailingFollowTimeout();
  }, [sessionId, clearTrailingFollowTimeout]);

  useEffect(() => {
    return clearTrailingFollowTimeout;
  }, [clearTrailingFollowTimeout]);

  useEffect(() => {
    const timelineNode = chatTimelineRef.current;
    if (!timelineNode) {
      return;
    }

    const disarmFollow = () => {
      if (followArmedRef.current) {
        followArmedRef.current = false;
      }
    };

    timelineNode.addEventListener('wheel', disarmFollow, { passive: true, capture: true });
    timelineNode.addEventListener('touchmove', disarmFollow, { passive: true, capture: true });

    return () => {
      timelineNode.removeEventListener('wheel', disarmFollow, true);
      timelineNode.removeEventListener('touchmove', disarmFollow, true);
    };
  }, []);

  // Stable Virtuoso itemContent callback.  All per-item state is looked up
  // from pre-computed maps / scalars so that the closure reference itself
  // only changes when the maps or scalar dependencies change — NOT on every
  // ChatView render.  This lets Virtuoso's internal memo wrapper bail out
  // for items whose data hasn't changed.
  const renderTurnItem = useCallback((index: number, turn: Turn) => {
    const undoProps = turnUndoPropsMap.get(index);
    const isLastTurn = index === filteredTurns.length - 1;

    return (
      <TurnCard
        key={turn.id}
        turn={turn}
        turnIndex={index}
        agents={agents}
        onToolClick={handleToolClick}
        onDelegateClick={handleDelegateClick}
        showModelLabel={sessionHasMultipleModels}
        llmConfigCache={llmConfigCache}
        requestLlmConfig={requestLlmConfig}
        canUndo={index === undoTurnIndex}
        isUndone={undoProps?.isUndone ?? false}
        isUndoPending={undoProps?.isUndoPending ?? false}
        isStackedUndone={undoProps?.isStackedUndone ?? false}
        revertedFiles={undoProps?.revertedFiles ?? emptyRevertedFiles}
        onUndoTurn={handleUndoTurn}
        onForkTurn={handleForkTurn}
        onRedo={handleRedo}
        isCompacting={isLastTurn && !!compactingState}
        compactingTokenEstimate={isLastTurn ? compactingState?.tokenEstimate : undefined}
        onSpeakTurn={onSpeakTurn}
      />
    );
  }, [
    filteredTurns.length,
    turnUndoPropsMap,
    agents,
    handleToolClick,
    handleDelegateClick,
    sessionHasMultipleModels,
    llmConfigCache,
    requestLlmConfig,
    undoTurnIndex,
    handleUndoTurn,
    handleForkTurn,
    handleRedo,
    compactingState,
    onSpeakTurn,
  ]);

  return (
    <div
      className="flex flex-col flex-1 min-h-0 text-ui-primary relative"
      style={{ ['--todo-rail-width' as any]: showTodoRail ? (todoRailCollapsed ? '2rem' : '18rem') : '0px' }}
    >
      {/* Event Timeline with Todo Rail */}
      <div className="flex-1 overflow-hidden flex flex-row relative">
        <div className="flex-1 overflow-hidden flex flex-col min-w-0 relative">
        {sessionId && hasDelegations && (
          <ChatTabBar
            activeTimelineView={activeTimelineView}
            setActiveTimelineView={setActiveTimelineView}
            setDelegationDrawerOpen={setDelegationDrawerOpen}
            delegations={delegations}
            activeDelegationId={activeDelegationId}
            setActiveDelegationId={setActiveDelegationId}
            hasDelegations={hasDelegations}
          />
        )}
        <div ref={chatTimelineRef} className="flex-1 overflow-hidden relative">
          {activeTimelineView === 'delegations' ? (
            <DelegationsView
              delegations={delegations}
              agents={agents}
              activeDelegationId={activeDelegationId}
              activeTurn={activeDelegationTurn}
              onSelectDelegation={handleDelegateClick}
              onToolClick={handleToolClick}
              llmConfigCache={llmConfigCache}
              requestLlmConfig={requestLlmConfig}
            />
          ) : !hasTurns ? (
            <div className="flex items-center justify-center h-full">
               {!sessionId ? (
                // No active session
                sessionGroups.length === 0 ? (
                  // No sessions exist - show welcome + new session button
                  <WelcomeScreen
                    onNewSession={handleNewSession}
                    disabled={!connected || loading}
                    loading={loading}
                  />
                ) : (
                  // Sessions exist - show session picker
                  <SessionPicker
                    groups={sessionGroups}
                    onSelectSession={handleSelectSession}
                    onDeleteSession={handleDeleteSession}
                    onNewSession={handleNewSession}
                    onLoadMoreSessions={() => loadMoreSessions(20)}
                    onLoadMoreGroupSessions={(cwd) => loadMoreGroupSessions(cwd, 20)}
                    onSearchSessions={(q) => searchSessions(q, 30)}
                    onLoadSessionChildren={loadSessionChildren}
                    sessionChildrenLoading={sessionChildrenLoading}
                    disabled={!connected || loading}
                    activeSessionId={sessionId}
                    thinkingBySession={thinkingBySession}
                    sessionParentMap={sessionParentMap}
                    hasMoreSessions={!!sessionNextCursor || sessionGroups.reduce((n, g) => n + g.sessions.length, 0) < sessionTotalCount}
                    sessionPageLoading={sessionPageLoading}
                  />
                )
              ) : (
                // Active session but no events yet - ready to chat
                <div className="text-center animate-fade-in">
                  <p className="text-sm text-ui-muted">Start chatting below</p>
                </div>
              )}
            </div>
          ) : (
            <Virtuoso
              ref={virtuosoRef}
              data={filteredTurns}
              itemContent={renderTurnItem}
              atBottomStateChange={handleAtBottomStateChange}
              scrollerRef={scrollerRefCallback}
              className="h-full"
            />
          )}
          {activeTimelineView === 'chat' && (
            <PinnedUserMessage
              message={pinnedMessage?.content ?? ''}
              timestamp={pinnedMessage?.timestamp ?? 0}
              onJumpBack={handleJumpBackToPinnedMessage}
              visible={!!pinnedMessage}
            />
          )}
        </div>
        {activeTimelineView === 'chat' && hasTurns && !isAtBottom && (
          <div className="absolute bottom-6 left-1/2 -translate-x-1/2 z-10">
            <button
              type="button"
              onClick={() => {
                if (followNewMessages) {
                  followArmedRef.current = true;
                }
                scrollToLatest('smooth');
              }}
              className="flex items-center gap-2 px-3 py-1.5 rounded-full text-xs text-ui-primary bg-surface-canvas/80 border border-surface-border/70 shadow-[0_0_18px_rgba(var(--accent-primary-rgb),0.12)] hover:border-accent-primary/60 hover:text-accent-primary transition-all animate-fade-in-up"
            >
              <span>Scroll to latest</span>
              <ChevronDown className="w-3.5 h-3.5" />
            </button>
          </div>
        )}
        </div>

        {/* Todo Rail */}
        {showTodoRail && (
          <TodoRail
            todos={todos}
            stats={todoStats}
            collapsed={todoRailCollapsed}
            onToggleCollapse={() => setTodoRailCollapsed(!todoRailCollapsed)}
            recentlyChangedIds={recentlyChangedIds}
          />
        )}

        {/* System Log - positioned as overlay so it doesn't clip the chat scroll area */}
        {visibleSystemEvents.length > 0 && (
          <div className="absolute bottom-0 left-0 right-0 z-20 pointer-events-none">
            <SystemLog
              events={visibleSystemEvents}
              onClear={handleClearSystemEvents}
            />
          </div>
        )}

        {/* Schedule Panel */}
        {showSchedulePanel && (
          <SchedulePanel
            schedules={schedules}
            collapsed={schedulePanelCollapsed}
            onToggleCollapse={() => setSchedulePanelCollapsed(!schedulePanelCollapsed)}
            onPause={pauseSchedule}
            onResume={resumeSchedule}
            onTriggerNow={triggerScheduleNow}
            onDelete={deleteSchedule}
            onCreateNew={() => setCreateScheduleDialogOpen(true)}
          />
        )}
      </div>

      {/* Thinking Indicator */}
      {sessionThinkingAgentId !== null && <ThinkingIndicator agentId={sessionThinkingAgentId} agents={agents} />}

      {/* Rate Limit Indicator */}
      {rateLimitState?.isRateLimited && sessionId && (
        <div className="px-3 md:px-6 py-2">
          <RateLimitIndicator
            sessionId={sessionId}
            message={rateLimitState.message}
            waitSecs={rateLimitState.waitSecs}
            startedAt={rateLimitState.startedAt}
            attempt={rateLimitState.attempt}
            maxAttempts={rateLimitState.maxAttempts}
            remainingSecs={rateLimitState.remainingSecs}
            onCancel={handleCancelRateLimit}
          />
        </div>
      )}

      {/* Reconnection banner */}
      {!connected && reconnecting && (
        <div className="mx-4 mb-2 px-4 py-2 rounded-lg border border-status-warning/40 bg-surface-elevated text-xs text-status-warning flex items-center gap-2 animate-fade-in">
          <span className="w-1.5 h-1.5 rounded-full bg-status-warning animate-pulse flex-shrink-0" />
          Connection lost. Reconnecting...
        </div>
      )}

      {/* Input Area */}
      <ChatInputBar
        mentionInputRef={mentionInputRef}
        prompt={prompt}
        setPrompt={setPrompt}
        handleSendPrompt={handleSendPrompt}
        cancelSession={cancelSession}
        sessionId={sessionId}
        connected={connected}
        loading={loading}
        isMobile={isMobile}
        sessionThinkingAgentId={sessionThinkingAgentId}
        rateLimitState={rateLimitState}
        activeIndexStatus={activeIndexStatus}
        allFiles={fileMention.allFiles}
        requestIndex={fileMention.requestIndex}
        isLoadingFiles={fileMention.isLoading}
      />

      {/* Tool Detail Modal */}
      {selectedToolEvent && (
        <ToolDetailModal
          event={selectedToolEvent}
          onClose={() => setSelectedToolEvent(null)}
        />
      )}

      {activeTimelineView === 'chat' && delegationDrawerOpen && activeDelegation && (
        <DelegationDrawer
          delegation={activeDelegation}
          agents={agents}
          onClose={() => {
            setDelegationDrawerOpen(false);
          }}
          onToolClick={handleToolClick}
          llmConfigCache={llmConfigCache}
          requestLlmConfig={requestLlmConfig}
        />
      )}

    </div>
  );
}
