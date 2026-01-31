import { useState, useRef, useEffect, useMemo, useCallback } from 'react';
import { Virtuoso, type VirtuosoHandle } from 'react-virtuoso';
import { Activity, Send, CheckCircle, XCircle, Loader, Menu, Plus, Code, ChevronDown, Square } from 'lucide-react';
import { useUiClient } from './hooks/useUiClient';
import { useSessionTimer } from './hooks/useSessionTimer';
import { useFileMention } from './hooks/useFileMention';
import { EventItem, EventRow, DelegationGroupInfo, Turn } from './types';
import { Sidebar } from './components/Sidebar';
import { ThinkingIndicator } from './components/ThinkingIndicator';
import { FloatingStatsPanel } from './components/FloatingStatsPanel';
import { MentionInput } from './components/MentionInput';
import { ToolDetailModal } from './components/ToolDetailModal';
import { TurnCard } from './components/TurnCard';
import { DelegationsView } from './components/DelegationsView';
import { DelegationDrawer } from './components/DelegationDrawer';

import { GlitchText } from './components/GlitchText';
import { SessionPicker } from './components/SessionPicker';
import { SystemLog } from './components/SystemLog';
import { ModelPickerPopover } from './components/ModelPickerPopover';

function App() {
  const {
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
    setActiveAgent,
    setRoutingMode,
    sessionHistory,
    sessionGroups,
    loadSession,
    thinkingAgentId,
    thinkingAgentIds,
    thinkingBySession,
    isConversationComplete,
    setFileIndexCallback,
    setFileIndexErrorCallback,
    requestFileIndex,
    workspaceIndexStatus,
    allModels,
    sessionsByAgent,
    agentModels,
    refreshAllModels,
    setSessionModel,
    llmConfigCache,
    requestLlmConfig,
    sessionLimits,
    sendUndo,
    sendRedo,
    undoState,
  } = useUiClient();
  
  // Live timer hook
  const { globalElapsedMs, agentElapsedMs, isSessionActive } = useSessionTimer(
    events,
    thinkingAgentIds,
    isConversationComplete
  );
  const [prompt, setPrompt] = useState('');
  const [loading, setLoading] = useState(false);
  const [sidebarOpen, setSidebarOpen] = useState(false);
  const [sessionCopied, setSessionCopied] = useState(false);
  const copyTimeoutRef = useRef<number | null>(null);
  const [expertMode, setExpertMode] = useState(false);
  const [isAtBottom, setIsAtBottom] = useState(true);
  const virtuosoRef = useRef<VirtuosoHandle | null>(null);
  const activeIndexStatus = sessionId ? workspaceIndexStatus[sessionId]?.status : undefined;
  const [activeTimelineView, setActiveTimelineView] = useState<'chat' | 'delegations'>('chat');
  const [activeDelegationId, setActiveDelegationId] = useState<string | null>(null);
  
  // Modal state for tool details
  const [selectedToolEvent, setSelectedToolEvent] = useState<EventRow | null>(null);
  
  // Model picker popover state
  const [modelPickerOpen, setModelPickerOpen] = useState(false);

  
  // File mention hook
  const fileMention = useFileMention(requestFileIndex);
  
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

  useEffect(() => {
    return () => {
      if (copyTimeoutRef.current) {
        window.clearTimeout(copyTimeoutRef.current);
      }
    };
  }, []);

  useEffect(() => {
    setActiveDelegationId(null);
    setActiveTimelineView('chat');
  }, [sessionId]);

  // Keyboard shortcut: Cmd+N / Ctrl+N to create new session
  useEffect(() => {
    const handleKeyDown = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && e.key === 'n') {
        e.preventDefault();
        if (connected && !loading) {
          handleNewSession();
        }
      }
    };
    
    window.addEventListener('keydown', handleKeyDown);
    return () => window.removeEventListener('keydown', handleKeyDown);
  }, [connected, loading]);

  // Keyboard shortcut: Double Escape (within 500ms) to cancel active session
  useEffect(() => {
    let lastEscapeTime = 0;
    
    const handleKeyDown = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        const now = Date.now();
        const timeSinceLastEsc = now - lastEscapeTime;
        
        if (timeSinceLastEsc < 500 && thinkingAgentId !== null) {
          e.preventDefault();
          e.stopPropagation();
          cancelSession();
          lastEscapeTime = 0;
        } else {
          lastEscapeTime = now;
        }
      }
    };
    
    window.addEventListener('keydown', handleKeyDown, { capture: true });
    return () => window.removeEventListener('keydown', handleKeyDown, { capture: true });
  }, [thinkingAgentId, cancelSession]);

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
      await newSession();
    } catch (err) {
      console.error('Failed to create session:', err);
      throw err;
    }
  };

  const handleCopySessionId = async () => {
    if (!sessionId) return;
    const text = String(sessionId);
    try {
      if (navigator.clipboard?.writeText) {
        await navigator.clipboard.writeText(text);
      } else {
        const textarea = document.createElement('textarea');
        textarea.value = text;
        textarea.setAttribute('readonly', 'true');
        textarea.style.position = 'absolute';
        textarea.style.left = '-9999px';
        document.body.appendChild(textarea);
        textarea.select();
        document.execCommand('copy');
        document.body.removeChild(textarea);
      }
      setSessionCopied(true);
      if (copyTimeoutRef.current) {
        window.clearTimeout(copyTimeoutRef.current);
      }
      copyTimeoutRef.current = window.setTimeout(() => {
        setSessionCopied(false);
      }, 1500);
    } catch (err) {
      console.error('Failed to copy session id:', err);
    }
  };

  // Calculate session info
  const sessionInfo = sessionId ? {
    messageCount: events.filter((event: EventItem) => event.type !== 'system').length,
    createdAt: events.length > 0 ? events[0].timestamp : Date.now(),
  } : undefined;

  // Build turns from events and enrich delegations with child session events
  const {
    turns,
    delegations,
    hasMultipleModels: sessionHasMultipleModels,
  } = useMemo(() => {
    const result = buildTurns(events, thinkingAgentId);
    
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
          const hasMatchingAgent = sessionEvents.some(e => e.agentId === delegation.targetAgentId);
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
  }, [events, eventsBySession, mainSessionId, thinkingAgentId]);
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
      console.error('[App] Cannot undo: no message ID found for turn', turn.id);
      return;
    }
    console.log('[App] Undoing turn', turn.id, 'with message ID', userMessage.messageId);
    sendUndo(userMessage.messageId, turn.id);
  }, [filteredTurns, sendUndo]);

  // Handle redo
  const handleRedo = useCallback(() => {
    console.log('[App] Redoing changes');
    sendRedo();
  }, [sendRedo]);

  // Handle tool click to open modal
  const handleToolClick = useCallback((event: EventRow) => {
    setSelectedToolEvent(event);
  }, []);

  // Handle delegation click - open drawer
  const handleDelegateClick = useCallback((delegationId: string) => {
    console.log('[handleDelegateClick] Setting delegation ID:', delegationId);
    console.log('[handleDelegateClick] Current state:', {
      activeTimelineView,
      hasTurns,
      sessionId,
      delegationsCount: delegations.length,
      eventsCount: events.length
    });
    setActiveDelegationId(delegationId);
  }, [activeTimelineView, hasTurns, sessionId, delegations.length, events.length]);

  const activeDelegation = useMemo(
    () => delegations.find((delegation) => delegation.id === activeDelegationId),
    [delegations, activeDelegationId]
  );
  const activeDelegationTurn = useMemo(
    () => (activeDelegation ? buildDelegationTurn(activeDelegation) : null),
    [activeDelegation]
  );

  useEffect(() => {
    if (activeDelegationId && !activeDelegation) {
      setActiveDelegationId(null);
    }
  }, [activeDelegation, activeDelegationId]);

  useEffect(() => {
    if (activeTimelineView === 'delegations' && delegations.length === 0) {
      setActiveTimelineView('chat');
    }
  }, [activeTimelineView, delegations.length]);

  // Handle view switch - only select first if no valid selection exists
  useEffect(() => {
    if (activeTimelineView === 'delegations') {
      // Auto-close drawer when switching to delegations tab
      setActiveDelegationId(null);
      
      // Only auto-select first if:
      // 1. We have delegations AND
      // 2. No current selection OR current selection is invalid
      if (delegations.length > 0) {
        const currentSelectionExists = delegations.some(d => d.id === activeDelegationId);
        if (!activeDelegationId || !currentSelectionExists) {
          setActiveDelegationId(delegations[0].id);
        }
      }
    }
  }, [activeTimelineView]); // Only depends on view switch

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
    <div className="flex flex-col h-screen bg-cyber-bg text-gray-100 relative">
      
      {/* Sidebar */}
      <Sidebar
        open={sidebarOpen}
        onOpenChange={setSidebarOpen}
        sessionId={sessionId}
        connected={connected}
        onNewSession={handleNewSession}
        sessionInfo={sessionInfo}
        agents={agents}
        routingMode={routingMode}
        activeAgentId={activeAgentId}
        onSetActiveAgent={setActiveAgent}
        onSetRoutingMode={setRoutingMode}
        sessionHistory={sessionHistory}
        onLoadSession={loadSession}
      />

      {/* Header */}
      <header className="flex flex-wrap items-center justify-between gap-4 px-6 py-4 bg-cyber-surface border-b border-cyber-border shadow-[0_0_20px_rgba(0,255,249,0.05)]">
        <div className="flex items-center gap-3">
          <button
            onClick={() => setSidebarOpen(true)}
            className="p-2 hover:bg-cyber-bg rounded-lg transition-colors"
          >
            <Menu className="w-6 h-6 text-cyber-cyan" />
          </button>
          <h1 className="text-xl font-semibold neon-text-cyan">
            <GlitchText text="QueryMT" variant="3" hoverOnly />
          </h1>
        </div>
        <div className="flex items-center gap-4 flex-wrap justify-end">
          {activeAgentId && (
            <span className="text-xs font-mono bg-cyber-bg px-3 py-1 rounded-lg border border-cyber-border text-cyber-cyan">
              Active: {activeAgentId}
            </span>
          )}
          {/* Model badge + popover */}
          <ModelPickerPopover
            open={modelPickerOpen}
            onOpenChange={setModelPickerOpen}
            connected={connected}
            routingMode={routingMode}
            activeAgentId={activeAgentId}
            sessionId={sessionId}
            sessionsByAgent={sessionsByAgent}
            agents={agents}
            allModels={allModels}
            currentProvider={agentModels[activeAgentId]?.provider}
            currentModel={agentModels[activeAgentId]?.model}
            onRefresh={refreshAllModels}
            onSetSessionModel={setSessionModel}
          />
          <div className="flex items-center gap-2">
            {connected ? (
              <>
                <CheckCircle className="w-5 h-5 text-cyber-lime" />
                <span className="text-sm text-gray-400">Connected</span>
              </>
            ) : (
              <>
                <XCircle className="w-5 h-5 text-cyber-orange" />
                <span className="text-sm text-gray-400">Disconnected</span>
              </>
            )}
          </div>
          {sessionId && (
            <button
              type="button"
              onClick={handleCopySessionId}
              title="Click to copy full session id"
              className="text-xs text-gray-500 font-mono bg-cyber-bg px-3 py-1 rounded-lg border border-cyber-border hover:border-cyber-cyan/60 hover:text-gray-300 transition-colors max-w-[70vw] break-all text-left"
            >
              <span className="text-gray-400">Session:</span> {String(sessionId)}
              {sessionCopied && <span className="ml-2 text-cyber-lime">Copied</span>}
            </button>
          )}
          <button
            onClick={() => setExpertMode(!expertMode)}
            className={`flex items-center gap-1 px-3 py-1.5 rounded border text-sm transition-colors ${
              expertMode
                ? 'border-cyber-purple text-cyber-purple bg-cyber-purple/10'
                : 'border-cyber-border text-gray-400 hover:border-cyber-purple/50'
            }`}
            title="Toggle expert mode (show all internal events)"
          >
            <Code className="w-4 h-4" />
            <span>Expert</span>
          </button>
        </div>
      </header>

      {/* Event Timeline */}
      <div className="flex-1 overflow-hidden flex flex-col relative">
        {sessionId && (hasTurns || hasDelegations) && (
          <div className="px-6 py-2 border-b border-cyber-border/60 bg-cyber-surface/40 flex items-center gap-2">
            <button
              type="button"
              onClick={() => setActiveTimelineView('chat')}
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
              onClick={() => setActiveTimelineView('delegations')}
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
                    onSelectSession={loadSession}
                    onNewSession={handleNewSession}
                    disabled={!connected || loading}
                    activeSessionId={sessionId}
                    thinkingBySession={thinkingBySession}
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
              followOutput="smooth"
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
        {/* Floating Stats Panel */}
        {activeTimelineView === 'chat' && (
          <FloatingStatsPanel 
            events={events} 
            agents={agents} 
            expertMode={expertMode}
            globalElapsedMs={globalElapsedMs}
            agentElapsedMs={agentElapsedMs}
            isSessionActive={isSessionActive}
            agentModels={agentModels}
            sessionLimits={sessionLimits}
          />
        )}
      </div>

      {/* Thinking/Completion Indicator */}
      {thinkingAgentId !== null && <ThinkingIndicator agentId={thinkingAgentId} agents={agents} />}
      {thinkingAgentId === null && isConversationComplete && (
        <ThinkingIndicator agentId={thinkingAgentId} agents={agents} isComplete={true} />
      )}

      {visibleSystemEvents.length > 0 && (
        <SystemLog
          events={visibleSystemEvents}
          onClear={handleClearSystemEvents}
        />
      )}

      {/* Input Area */}
      <div className="px-6 py-4 bg-cyber-surface border-t border-cyber-border shadow-[0_-4px_20px_rgba(0,255,249,0.05)]">
        <div className="flex gap-3 relative items-end">
          <MentionInput
            value={prompt}
            onChange={setPrompt}
            onSubmit={handleSendPrompt}
            placeholder={!sessionId ? "Create a session to start chatting..." : "Enter your prompt... (use @ to mention files)"}
            disabled={loading || !connected || !sessionId}
            files={fileMention.allFiles}
            onRequestFiles={fileMention.requestIndex}
            isLoadingFiles={fileMention.isLoading}
            showIndexBuilding={activeIndexStatus === 'building'}
          />
          {thinkingAgentId !== null ? (
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
              disabled={loading || !connected || !sessionId || !prompt.trim()}
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

      {/* Tool Detail Modal */}
      {selectedToolEvent && (
        <ToolDetailModal
          event={selectedToolEvent}
          onClose={() => setSelectedToolEvent(null)}
        />
      )}

      {activeTimelineView === 'chat' && activeDelegation && (
        <DelegationDrawer
          delegation={activeDelegation}
          agents={agents}
          onClose={() => {
          console.log('[DelegationDrawer] Closing drawer');
          setActiveDelegationId(null);
        }}
          onToolClick={handleToolClick}
          llmConfigCache={llmConfigCache}
          requestLlmConfig={requestLlmConfig}
        />
      )}

    </div>
  );
}

export default App;

// Model timeline entry
interface ModelTimelineEntry {
  timestamp: number;
  provider: string;
  model: string;
  configId?: number;
  label: string; // "provider / model"
}

// Build model timeline from events
function buildModelTimeline(events: EventItem[]): ModelTimelineEntry[] {
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
function getActiveModelAt(timeline: ModelTimelineEntry[], timestamp: number): ModelTimelineEntry | undefined {
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
function hasMultipleModels(timeline: ModelTimelineEntry[]): boolean {
  const uniqueLabels = new Set(timeline.map(e => e.label));
  return uniqueLabels.size > 1;
}

// Build turns from event rows
function buildTurns(events: EventItem[], thinkingAgentId: string | null): {
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
    currentTurn.isActive = thinkingAgentId !== null;
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

function buildDelegationTurn(group: DelegationGroupInfo): Turn {
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
function buildEventRowsWithDelegations(events: EventItem[]): {
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

function inferToolName(event: EventItem): string | undefined {
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
