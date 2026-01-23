import { useState, useRef, useEffect, useMemo, useCallback } from 'react';
import { Virtuoso, type VirtuosoHandle } from 'react-virtuoso';
import { Activity, Send, CheckCircle, XCircle, Loader, Menu, Plus, Code, ChevronDown } from 'lucide-react';
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
import { CircuitBackground } from './components/CircuitBackground';
import { GlitchText } from './components/GlitchText';
import { SessionPicker } from './components/SessionPicker';
import { SystemLog } from './components/SystemLog';

function App() {
  const {
    events,
    sessionId,
    connected,
    newSession,
    sendPrompt,
    agents,
    routingMode,
    activeAgentId,
    setActiveAgent,
    setRoutingMode,
    sessionHistory,
    sessionGroups,
    loadSession,
    thinkingAgentId,
    isConversationComplete,
    setFileIndexCallback,
    setFileIndexErrorCallback,
    requestFileIndex,
    workspaceIndexStatus,
  } = useUiClient();
  
  // Live timer hook
  const { globalElapsedMs, agentElapsedMs, isSessionActive } = useSessionTimer(
    events,
    thinkingAgentId,
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
  
  // Modal state for tool details
  const [selectedToolEvent, setSelectedToolEvent] = useState<EventRow | null>(null);
  
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
    messageCount: events.filter((event) => event.type !== 'system').length,
    createdAt: events.length > 0 ? events[0].timestamp : Date.now(),
  } : undefined;

  // Build turns from events
  const { turns } = useMemo(
    () => buildTurns(events, thinkingAgentId),
    [events, thinkingAgentId]
  );
  const systemEvents = useMemo(
    () => events.filter((event) => event.type === 'system'),
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

  // Calculate last user message turn index for pinned message
  const lastUserMessageTurnIndex = useMemo(() => {
    for (let i = filteredTurns.length - 1; i >= 0; i--) {
      if (filteredTurns[i].userMessage) {
        return i;
      }
    }
    return -1;
  }, [filteredTurns]);

  // Handle tool click to open modal
  const handleToolClick = useCallback((event: EventRow) => {
    setSelectedToolEvent(event);
  }, []);

  // Handle delegation click - scroll to accordion
  const handleDelegateClick = useCallback((delegationId: string) => {
    setTimeout(() => {
      const accordion = document.querySelector(`[data-delegation-id="${delegationId}"]`);
      if (accordion) {
        accordion.scrollIntoView({ behavior: 'smooth', block: 'center' });
      }
    }, 100);
  }, []);

  // Render event item (used for delegations)
  const renderEventItem = useCallback((_event: EventRow) => {
    // For delegation child events, we don't need special rendering
    // They're just shown in a simple list
    return null;
  }, []);

  return (
    <div className="flex flex-col h-screen bg-cyber-bg text-gray-100 relative">
      {/* Circuit Board Background */}
      <CircuitBackground className="opacity-20" />
      
      {/* Sidebar */}
      <Sidebar
        isOpen={sidebarOpen}
        onClose={() => setSidebarOpen(false)}
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
            <GlitchText text="QueryMT" variant="3" />
          </h1>
        </div>
        <div className="flex items-center gap-4 flex-wrap justify-end">
          {activeAgentId && (
            <span className="text-xs font-mono bg-cyber-bg px-3 py-1 rounded-lg border border-cyber-border text-cyber-cyan">
              Active: {activeAgentId}
            </span>
          )}
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
        <div className="flex-1 overflow-hidden relative">
          {!hasTurns ? (
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
                  />
                )
              ) : (
                // Active session but no events yet - ready to chat
                <div className="text-center space-y-6 animate-fade-in text-gray-500">
                  <Activity className="w-16 h-16 mx-auto opacity-30 text-cyber-cyan animate-glow-pulse" />
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
              itemContent={(index, turn) => (
                <TurnCard
                  key={turn.id}
                  turn={turn}
                  agents={agents}
                  onToolClick={handleToolClick}
                  onDelegateClick={handleDelegateClick}
                  renderEvent={renderEventItem}
                  isLastUserMessage={index === lastUserMessageTurnIndex}
                />
              )}
              followOutput="smooth"
              atBottomStateChange={setIsAtBottom}
              className="h-full"
            />
          )}
        </div>
        {hasTurns && !isAtBottom && (
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
        <FloatingStatsPanel 
          events={events} 
          agents={agents} 
          expertMode={expertMode}
          globalElapsedMs={globalElapsedMs}
          agentElapsedMs={agentElapsedMs}
          isSessionActive={isSessionActive}
        />
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
          <button
            onClick={handleSendPrompt}
            disabled={loading || !connected || !sessionId || !prompt.trim()}
            className="
              px-6 py-3 rounded-lg font-medium transition-all duration-200
              bg-cyber-cyan/10 border-2 border-cyber-cyan text-cyber-cyan
              hover:bg-cyber-cyan/20 hover:shadow-neon-cyan
              disabled:opacity-30 disabled:cursor-not-allowed
              flex items-center gap-2 overflow-visible
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
        </div>
      </div>

      {/* Tool Detail Modal */}
      {selectedToolEvent && (
        <ToolDetailModal
          event={selectedToolEvent}
          onClose={() => setSelectedToolEvent(null)}
        />
      )}

    </div>
  );
}

export default App;

// Build turns from event rows
function buildTurns(events: EventItem[], thinkingAgentId: string | null): {
  turns: Turn[];
  allEventRows: EventRow[];
} {
  // First, build event rows with delegation grouping (from previous implementation)
  const { rows, delegationGroups } = buildEventRowsWithDelegations(events);
  
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
    } else if (row.type === 'agent' && row.isMessage) {
      // No current turn (agent-initiated message)
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
      };
    }
  }

  // Close final turn
  if (currentTurn) {
    currentTurn.isActive = thinkingAgentId !== null;
    turns.push(currentTurn);
  }

  return { turns, allEventRows: rows };
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
  const openDelegations: string[] = [];
  let currentAgentId: string | null = null;

  for (const event of events) {
    if (event.type === 'system') {
      continue;
    }
    let depth = 0;
    let parentId: string | undefined;
    let toolName: string | undefined;
    let isDelegateToolCall = false;
    let delegationGroupId: string | undefined;

    if (event.type === 'tool_call') {
      const toolCallKey = event.toolCall?.tool_call_id ?? event.id;
      const delegationParent = openDelegations.length
        ? toolCallMap.get(openDelegations[openDelegations.length - 1])?.eventId
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
      if (event.toolCall?.kind === 'delegate' || event.toolCall?.kind === 'mcp_task') {
        isDelegateToolCall = true;
        delegationGroupId = toolCallKey;
        openDelegations.push(toolCallKey);
        
        // Create delegation group
        delegationGroups.set(toolCallKey, {
          id: toolCallKey,
          delegateToolCallId: toolCallKey,
          delegateEvent: { ...event, depth, parentId, toolName, isDelegateToolCall: true, delegationGroupId: toolCallKey },
          events: [],
          status: 'in_progress',
          startTime: event.timestamp,
        });
      }
      
      // If we're inside a delegation, mark this event
      if (openDelegations.length > 0 && !isDelegateToolCall) {
        delegationGroupId = openDelegations[openDelegations.length - 1];
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
          group.endTime = event.timestamp;
          group.status = event.toolCall?.status === 'failed' ? 'failed' : 'completed';
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
        if (openDelegations.length > 0) {
          delegationGroupId = openDelegations[openDelegations.length - 1];
          const group = delegationGroups.get(delegationGroupId);
          if (group) {
            group.events.push({ ...event, depth, parentId, toolName, delegationGroupId });
          }
        }
        
        depthMap.set(event.id, depth);
        rows.push({ ...event, depth, parentId, toolName, delegationGroupId });
      }
      
      // Close delegation if this result completes it
      if (
        toolCallKey &&
        openDelegations[openDelegations.length - 1] === toolCallKey &&
        (!event.toolCall?.status ||
          event.toolCall?.status === 'completed' ||
          event.toolCall?.status === 'failed')
      ) {
        openDelegations.pop();
      }
    } else {
      // user or agent event
      if (openDelegations.length > 0) {
        const delegationId = openDelegations[openDelegations.length - 1];
        const delegationDepth = toolCallMap.get(delegationId)?.depth ?? 1;
        depth = delegationDepth + 1;
        parentId = toolCallMap.get(delegationId)?.eventId;
        delegationGroupId = delegationId;
        
        // Add to delegation group
        const group = delegationGroups.get(delegationId);
        if (group) {
          group.events.push({ ...event, depth, parentId, toolName, delegationGroupId });
          if (event.agentId && !group.agentId) {
            group.agentId = event.agentId;
          }
        }
      }
      if (event.type === 'agent') {
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
