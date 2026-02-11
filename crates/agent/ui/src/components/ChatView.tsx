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
import { Activity, Send, Loader, Plus, ChevronDown, Square } from 'lucide-react';
import { useUiClientContext } from '../context/UiClientContext';
import { useUiStore } from '../store/uiStore';
import { useSessionManager } from '../hooks/useSessionManager';
import { useFileMention } from '../hooks/useFileMention';
import { useTodoState } from '../hooks/useTodoState';
import { EventItem, EventRow } from '../types';
import { MentionInput } from './MentionInput';
import { ToolDetailModal } from './ToolDetailModal';
import { TurnCard } from './TurnCard';
import { DelegationsView } from './DelegationsView';
import { DelegationDrawer } from './DelegationDrawer';
import { TodoRail } from './TodoRail';
import { SessionPicker } from './SessionPicker';
import { ThinkingIndicator } from './ThinkingIndicator';
import { SystemLog } from './SystemLog';
import { GlitchText } from './GlitchText';
import { buildTurns, buildDelegationTurn, buildEventRowsWithDelegations, isRateLimitEvent, processRateLimitEvent } from '../logic/chatViewLogic';
import { RateLimitIndicator } from './RateLimitIndicator';

export function ChatView() {
  const {
    events,
    eventsBySession,
    mainSessionId,
    sessionId,
    connected,
    sendPrompt,
    cancelSession,
    agents,
    sessionGroups,
    thinkingBySession,
    sessionParentMap,
    isConversationComplete,
    setFileIndexCallback,
    setFileIndexErrorCallback,
    requestFileIndex,
    workspaceIndexStatus,
    llmConfigCache,
    requestLlmConfig,
    sendUndo,
    sendRedo,
    undoState,
  } = useUiClientContext();
  
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
    selectedToolEvent,
    setSelectedToolEvent,
    delegationDrawerOpen,
    setDelegationDrawerOpen,
    rateLimitBySession,
    setRateLimitState,
    clearRateLimitState,
    setMainInputRef,
  } = useUiStore();
  
  // Get rate limit state for current session
  const rateLimitState = sessionId ? rateLimitBySession.get(sessionId) : undefined;
  
  // Session-scoped thinking state (replaces global thinkingAgentId)
  const sessionThinkingAgentId = useMemo(() => {
    if (!sessionId || !thinkingBySession) return null;
    const agentSet = thinkingBySession.get(sessionId);
    if (!agentSet || agentSet.size === 0) return null;
    return Array.from(agentSet).pop()!;
  }, [sessionId, thinkingBySession]);
  
  // Session-scoped conversation complete state (only for main session)
  const sessionConversationComplete = sessionId === mainSessionId ? isConversationComplete : false;
  
  const virtuosoRef = useRef<VirtuosoHandle | null>(null);
  const mentionInputRef = useRef<HTMLTextAreaElement>(null);
  const activeIndexStatus = sessionId ? workspaceIndexStatus[sessionId]?.status : undefined;

  
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
  const { selectSession, createSession } = useSessionManager();

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

  // Keyboard shortcuts (Cmd+N, double Esc, etc. moved to AppShell)

  const handleSendPrompt = async () => {
    if (!prompt.trim() || loading || !sessionId) return;

    fileMention.clear();

    setLoading(true);
    try {
      await sendPrompt(prompt);
      setPrompt('');
    } catch (err) {
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
    for (const delegation of result.delegations) {
      // Use the childSessionId directly if available (set from session_forked event)
      if (delegation.childSessionId) {
        const sessionEvents = eventsBySession.get(delegation.childSessionId);
        if (sessionEvents) {
          const { rows } = buildEventRowsWithDelegations(sessionEvents);
          delegation.events = rows;
        }
      } else if (delegation.targetAgentId) {
        // Fallback: scan for child session by matching target agent ID
        for (const [sessionId, sessionEvents] of eventsBySession.entries()) {
          if (sessionId === mainSessionId) continue; // Skip main session
          const hasMatchingAgent = sessionEvents.some((e: EventItem) => e.agentId === delegation.targetAgentId);
          if (hasMatchingAgent) {
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
    () => events.filter((event: EventItem) => event.type === 'system'),
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

  // Calculate last user message turn index for pinned message
  const lastUserMessageTurnIndex = useMemo(() => {
    for (let i = filteredTurns.length - 1; i >= 0; i--) {
      if (filteredTurns[i].userMessage) {
        return i;
      }
    }
    return -1;
  }, [filteredTurns]);

  // Calculate last turn with tool calls (for undo button)
  const lastTurnWithToolCallsIndex = useMemo(() => {
    for (let i = filteredTurns.length - 1; i >= 0; i--) {
      if (filteredTurns[i].toolCalls.length > 0) {
        return i;
      }
    }
    return -1;
  }, [filteredTurns]);

  // Handle undo for a specific turn
  const handleUndo = useCallback((turnIndex: number) => {
    const turn = filteredTurns[turnIndex];
    const userMessage = turn.userMessage;
    if (!userMessage?.messageId) {
      console.error('[ChatView] Cannot undo: no message ID found for turn', turn.id);
      return;
    }
    console.log('[ChatView] Undoing turn', turn.id, 'with message ID', userMessage.messageId);
    sendUndo(userMessage.messageId, turn.id);
  }, [filteredTurns, sendUndo]);

  // Handle redo
  const handleRedo = useCallback(() => {
    console.log('[ChatView] Redoing changes');
    sendRedo();
  }, [sendRedo]);

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

  return (
    <div 
      className="flex flex-col flex-1 min-h-0 text-gray-100 relative"
      style={{ ['--todo-rail-width' as any]: showTodoRail ? (todoRailCollapsed ? '2rem' : '18rem') : '0px' }}
    >
      {/* Event Timeline with Todo Rail */}
      <div className="flex-1 overflow-hidden flex flex-row relative">
        <div className="flex-1 overflow-hidden flex flex-col min-w-0 relative">
        {sessionId && hasDelegations && (
          <div className="px-6 py-2 border-b border-cyber-border/60 bg-cyber-surface/40 flex items-center gap-2">
            <button
              type="button"
              onClick={() => {
                setActiveTimelineView('chat');
                setDelegationDrawerOpen(false);
              }}
              className={`text-xs uppercase tracking-wider px-3 py-1.5 rounded-full border transition-colors ${
                activeTimelineView === 'chat'
                  ? 'border-cyber-cyan text-cyber-cyan bg-cyber-cyan/10'
                  : 'border-cyber-border text-gray-400 hover:border-cyber-cyan/60 hover:text-gray-200'
              }`}
            >
              Chat
            </button>
            <button
              type="button"
              onClick={() => {
                setActiveTimelineView('delegations');
                if (delegations.length > 0) {
                  const currentValid = delegations.some(d => d.id === activeDelegationId);
                  if (!activeDelegationId || !currentValid) {
                    setActiveDelegationId(delegations[0].id);
                  }
                } else {
                  setActiveDelegationId(null);
                }
              }}
              className={`text-xs uppercase tracking-wider px-3 py-1.5 rounded-full border transition-colors ${
                activeTimelineView === 'delegations'
                  ? 'border-cyber-purple text-cyber-purple bg-cyber-purple/10'
                  : 'border-cyber-border text-gray-400 hover:border-cyber-purple/60 hover:text-gray-200'
              }`}
            >
              Delegations
              {hasDelegations && (
                <span className="ml-2 text-[10px] text-gray-500">{delegations.length}</span>
              )}
            </button>
          </div>
        )}
        <div className="flex-1 overflow-hidden relative">
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
                  <div className="text-center space-y-6 animate-fade-in">
                    <div>
                      <p className="text-lg text-gray-400 mb-6">Welcome to QueryMT</p>
                      <button
                        onClick={handleNewSession}
                        disabled={!connected || loading}
                        className="
                          px-8 py-4 rounded-lg font-medium text-base
                          bg-cyber-cyan/10 border-2 border-cyber-cyan
                          text-cyber-cyan
                          hover:bg-cyber-cyan/20 hover:shadow-neon-cyan
                          disabled:opacity-30 disabled:cursor-not-allowed
                          transition-all duration-200
                          flex items-center justify-center gap-3 mx-auto
                        "
                      >
                        {loading ? (
                          <>
                            <Loader className="w-6 h-6 animate-spin" />
                            <span>Creating Session...</span>
                          </>
                        ) : (
                          <>
                            <Plus className="w-6 h-6" />
                            <GlitchText text="Start New Session" variant="0" hoverOnly />
                          </>
                        )}
                      </button>
                      <p className="text-xs text-gray-500 mt-3">
                        or press <kbd className="px-2 py-1 bg-cyber-bg border border-cyber-border rounded text-cyber-cyan font-mono text-[10px]">
                          {navigator.platform.includes('Mac') ? '⌘' : 'Ctrl'}+N
                        </kbd> to create a session
                      </p>
                    </div>
                  </div>
                ) : (
                  // Sessions exist - show session picker
                  <SessionPicker
                    groups={sessionGroups}
                    onSelectSession={handleSelectSession}
                    onNewSession={handleNewSession}
                    disabled={!connected || loading}
                    activeSessionId={sessionId}
                    thinkingBySession={thinkingBySession}
                    sessionParentMap={sessionParentMap}
                  />
                )
              ) : (
                // Active session but no events yet - ready to chat
                <div className="text-center space-y-6 animate-fade-in text-gray-500">
                  <Activity className="w-16 h-16 mx-auto text-cyber-cyan opacity-30" />
                  <div>
                    <p className="text-lg text-gray-400">Session Ready</p>
                    <p className="text-sm text-gray-500 mt-2">Start chatting below to begin</p>
                  </div>
                </div>
              )}
            </div>
          ) : (
            <Virtuoso
              ref={virtuosoRef}
              data={filteredTurns}
              itemContent={(index, turn) => {
                const canUndo = index === lastTurnWithToolCallsIndex;
                const isUndone = undoState?.turnId === turn.id;
                const revertedFiles = isUndone ? undoState.revertedFiles : [];
                
                return (
                  <TurnCard
                    key={turn.id}
                    turn={turn}
                    agents={agents}
                    onToolClick={handleToolClick}
                    onDelegateClick={handleDelegateClick}
                    isLastUserMessage={index === lastUserMessageTurnIndex}
                    showModelLabel={sessionHasMultipleModels}
                    llmConfigCache={llmConfigCache}
                    requestLlmConfig={requestLlmConfig}
                    activeView={activeTimelineView}
                    canUndo={canUndo}
                    isUndone={isUndone}
                    revertedFiles={revertedFiles}
                    onUndo={() => handleUndo(index)}
                    onRedo={handleRedo}
                  />
                );
              }}
              atBottomStateChange={setIsAtBottom}
              className="h-full"
            />
          )}
        </div>
        {activeTimelineView === 'chat' && hasTurns && !isAtBottom && (
          <div className="absolute bottom-6 left-1/2 -translate-x-1/2">
            <button
              type="button"
              onClick={() => {
                if (filteredTurns.length === 0) return;
                virtuosoRef.current?.scrollToIndex({
                  index: filteredTurns.length - 1,
                  align: 'end',
                  behavior: 'smooth',
                });
              }}
              className="flex items-center gap-2 px-3 py-1.5 rounded-full text-xs text-gray-200 bg-black/70 border border-cyber-border/70 shadow-[0_0_18px_rgba(0,255,249,0.12)] hover:border-cyber-cyan/60 hover:text-cyber-cyan transition-all animate-fade-in-up"
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
      </div>

      {/* Thinking/Completion Indicator */}
      {sessionThinkingAgentId !== null && <ThinkingIndicator agentId={sessionThinkingAgentId} agents={agents} />}
      {sessionThinkingAgentId === null && sessionConversationComplete && (
        <ThinkingIndicator agentId={sessionThinkingAgentId} agents={agents} isComplete={true} />
      )}

      {visibleSystemEvents.length > 0 && (
        <SystemLog
          events={visibleSystemEvents}
          onClear={handleClearSystemEvents}
        />
      )}

      {/* Rate Limit Indicator */}
      {rateLimitState?.isRateLimited && sessionId && (
        <div className="px-6 py-2">
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

      {/* Input Area */}
      <div className="px-6 py-4 bg-cyber-surface border-t border-cyber-border shadow-[0_-4px_20px_rgba(0,255,249,0.05)]">
        <div 
          className="flex gap-3 relative items-end p-0.5 rounded-lg transition-colors duration-200"
          style={{ 
            background: `linear-gradient(90deg, rgba(var(--mode-rgb), 0.08) 0%, transparent 100%)` 
          }}
        >
          <div className="flex gap-3 relative items-end flex-1">
          <MentionInput
            ref={mentionInputRef}
            value={prompt}
            onChange={setPrompt}
            onSubmit={handleSendPrompt}
            placeholder={
              !sessionId 
                ? "Create a session to start chatting..." 
                : rateLimitState?.isRateLimited
                  ? "Waiting for rate limit..."
                  : "Enter your prompt... (use @ to mention files)"
            }
            disabled={loading || !connected || !sessionId || rateLimitState?.isRateLimited}
            files={fileMention.allFiles}
            onRequestFiles={fileMention.requestIndex}
            isLoadingFiles={fileMention.isLoading}
            showIndexBuilding={activeIndexStatus === 'building'}
          />
          {sessionThinkingAgentId !== null ? (
            <button
              onClick={cancelSession}
              className="
                px-6 py-3 rounded-lg font-medium transition-all duration-200
                bg-cyber-orange/10 border-2 border-cyber-orange text-cyber-orange
                hover:bg-cyber-orange/20 hover:shadow-[0_0_15px_rgba(255,165,0,0.3)]
                flex items-center gap-2 overflow-visible self-end min-h-[48px]
              "
              title="Stop generation (Esc Esc)"
            >
              <Square className="w-5 h-5" />
              <span>Stop</span>
            </button>
          ) : (
            <button
              onClick={handleSendPrompt}
              disabled={loading || !connected || !sessionId || !prompt.trim() || rateLimitState?.isRateLimited}
              className="
                px-6 py-3 rounded-lg font-medium transition-all duration-200
                bg-cyber-cyan/10 border-2 border-cyber-cyan text-cyber-cyan
                hover:bg-cyber-cyan/20 hover:shadow-neon-cyan
                disabled:opacity-30 disabled:cursor-not-allowed
                flex items-center gap-2 overflow-visible self-end min-h-[48px]
              "
            >
              {loading ? (
                <>
                  <Loader className="w-5 h-5 animate-spin" />
                  <span>Sending...</span>
                </>
              ) : (
                <>
                  <Send className="w-5 h-5" />
                  <GlitchText text="Send" variant="0" hoverOnly />
                </>
              )}
            </button>
          )}
          </div>
        </div>
      </div>

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
