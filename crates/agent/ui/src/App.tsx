import { useState, useRef, useEffect, useMemo } from 'react';
import { Virtuoso } from 'react-virtuoso';
import { Activity, Send, CheckCircle, XCircle, Loader, Menu, Plus } from 'lucide-react';
import { PatchDiff } from '@pierre/diffs/react';
import { useUiClient } from './hooks/useUiClient';
import { useSessionTimer } from './hooks/useSessionTimer';
import { EventItem, EventFilters, UiAgentInfo } from './types';
import { Sidebar } from './components/Sidebar';
import { MessageContent } from './components/MessageContent';
import { ThinkingIndicator } from './components/ThinkingIndicator';
import { EventFiltersBar } from './components/EventFilters';
import { FloatingStatsPanel } from './components/FloatingStatsPanel';
import { getAgentColor } from './utils/agentColors';
import { getAgentShortName } from './utils/agentNames';

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
    loadSession,
    isAgentThinking,
    thinkingAgentId,
    isConversationComplete,
  } = useUiClient();
  
  // Live timer hook
  const { globalElapsedMs, agentElapsedMs, isSessionActive } = useSessionTimer(
    events,
    isAgentThinking,
    isConversationComplete
  );
  const [prompt, setPrompt] = useState('');
  const [loading, setLoading] = useState(false);
  const [sidebarOpen, setSidebarOpen] = useState(false);
  const [sessionCopied, setSessionCopied] = useState(false);
  const copyTimeoutRef = useRef<number | null>(null);
  const [filters, setFilters] = useState<EventFilters>({
    types: new Set(['user', 'agent', 'tool_call', 'tool_result']),
    agents: new Set(),
    tools: new Set(),
    searchQuery: '',
  });
  const [expertMode, setExpertMode] = useState(false);

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
      // Cmd+N (Mac) or Ctrl+N (Windows/Linux) to create new session
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
    messageCount: events.length,
    createdAt: events.length > 0 ? events[0].timestamp : Date.now(),
  } : undefined;

  const derivedEvents = useMemo(() => buildEventRows(events), [events]);

  const filteredEvents = useMemo(() => {
    return derivedEvents.filter((event) => {
      // Filter out verbose internal events unless in expert mode
      if (!expertMode && event.type === 'agent') {
        const content = event.content?.toLowerCase() || '';
        const verbosePatterns = [
          'event: llm_request_start',
          'event: llm_request_end',
          'event: progress_recorded',
          'event: intent_captured',
          'event: decision_recorded',
          'event: task_created',
          'event: task_updated',
          'event: artifact_stored',
        ];
        if (verbosePatterns.some(pattern => content.includes(pattern))) {
          return false;
        }
      }

      if (!filters.types.has(event.type)) return false;
      
      if (filters.agents.size > 0 && event.agentId && !filters.agents.has(event.agentId)) {
        return false;
      }
      
      if (filters.tools.size > 0) {
        const toolName = event.toolCall?.kind;
        if (!toolName || !filters.tools.has(toolName)) return false;
      }
      
      if (filters.searchQuery) {
        const query = filters.searchQuery.toLowerCase();
        const content = event.content?.toLowerCase() || '';
        const toolName = event.toolCall?.kind?.toLowerCase() || '';
        if (!content.includes(query) && !toolName.includes(query)) return false;
      }
      
      return true;
    });
  }, [derivedEvents, filters, expertMode]);

  return (
    <div className="flex flex-col h-screen bg-cyber-bg text-gray-100 grid-background">
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
          <Activity className="w-6 h-6 text-cyber-cyan animate-glow-pulse" />
          <h1 className="text-xl font-semibold neon-text-cyan">QueryMT Agent</h1>
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
        </div>
      </header>

      {/* Event Timeline */}
      <div className="flex-1 overflow-hidden flex flex-col relative">
        {events.length > 0 && (
          <EventFiltersBar
            events={derivedEvents}
            filters={filters}
            onFiltersChange={setFilters}
            filteredCount={filteredEvents.length}
            totalCount={derivedEvents.length}
            expertMode={expertMode}
            onExpertModeChange={setExpertMode}
            agents={agents}
          />
        )}
        <div className="flex-1 overflow-hidden">
          {events.length === 0 ? (
            <div className="flex items-center justify-center h-full text-gray-500">
              <div className="text-center space-y-6 animate-fade-in">
                <Activity className="w-16 h-16 mx-auto opacity-30 text-cyber-cyan" />
                <div>
                  <p className="text-lg text-gray-400">No events yet</p>
                  {!sessionId && (
                    <div className="mt-4 space-y-3">
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
                            <span>Start New Session</span>
                          </>
                        )}
                      </button>
                      <p className="text-xs text-gray-500">
                        or press <kbd className="px-2 py-1 bg-cyber-bg border border-cyber-border rounded text-cyber-cyan font-mono text-[10px]">
                          {navigator.platform.includes('Mac') ? '⌘' : 'Ctrl'}+N
                        </kbd> to create a session
                      </p>
                    </div>
                  )}
                </div>
              </div>
            </div>
          ) : (
            <Virtuoso
              data={filteredEvents}
              itemContent={(_index, event) => <EventCard key={event.id} event={event} agents={agents} />}
              followOutput="smooth"
              className="h-full"
            />
          )}
        </div>
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

      {/* Thinking/Completion Indicator - shows above input when agent is processing or just completed */}
      {isAgentThinking && <ThinkingIndicator agentId={thinkingAgentId} agents={agents} />}
      {!isAgentThinking && isConversationComplete && (
        <ThinkingIndicator agentId={thinkingAgentId} agents={agents} isComplete={true} />
      )}

      {/* Input Area */}
      <div className="px-6 py-4 bg-cyber-surface border-t border-cyber-border shadow-[0_-4px_20px_rgba(0,255,249,0.05)]">
        <div className="flex gap-3">
          <input
            type="text"
            value={prompt}
            onChange={(e) => setPrompt(e.target.value)}
            onKeyDown={(e) => e.key === 'Enter' && handleSendPrompt()}
            placeholder={!sessionId ? "Create a session to start chatting..." : "Enter your prompt..."}
            className="
              flex-1 px-4 py-3 bg-cyber-bg border-2 border-cyber-border rounded-lg 
              focus:outline-none focus:border-cyber-cyan focus:shadow-neon-cyan
              text-white placeholder-gray-500 transition-all duration-200
            "
            disabled={loading || !connected || !sessionId}
          />
          <button
            onClick={handleSendPrompt}
            disabled={loading || !connected || !sessionId || !prompt.trim()}
            className="
              px-6 py-3 rounded-lg font-medium transition-all duration-200
              bg-cyber-cyan/10 border-2 border-cyber-cyan text-cyber-cyan
              hover:bg-cyber-cyan/20 hover:shadow-neon-cyan
              disabled:opacity-30 disabled:cursor-not-allowed
              flex items-center gap-2
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
                <span>Send</span>
              </>
            )}
          </button>
        </div>
      </div>
    </div>
  );
}

function EventCard({ event, agents }: { event: EventRow; agents: UiAgentInfo[] }) {
  const depth = event.depth ?? 0;
  const toolName = event.toolName ?? inferToolName(event);
  const toolKind = inferToolKind(event) ?? event.toolCall?.kind;
  const agentColor = event.agentId ? getAgentColor(event.agentId) : undefined;

  // For merged tool calls, determine status from result
  const isToolCall = event.type === 'tool_call';
  const hasMergedResult = isToolCall && event.mergedResult;
  const toolStatus = hasMergedResult 
    ? event.mergedResult?.toolCall?.status 
    : event.toolCall?.status;
  const isInProgress = isToolCall && !hasMergedResult;

  const bgColor = {
    user: 'bg-cyber-surface/80 border-cyber-magenta/30 shadow-[0_0_15px_rgba(255,0,255,0.1)]',
    agent: 'bg-cyber-surface/50 border-cyber-border shadow-[0_0_10px_rgba(0,255,249,0.05)]',
    tool_call: 'bg-cyber-surface/80 border-cyber-purple/30 shadow-[0_0_15px_rgba(176,38,255,0.1)]',
    tool_result: 'bg-cyber-surface/80 border-cyber-lime/30 shadow-[0_0_15px_rgba(57,255,20,0.1)]',
  }[event.type];

  const labelColor = {
    user: 'neon-text-magenta',
    agent: 'neon-text-cyan',
    tool_call: 'text-cyber-purple',
    tool_result: 'neon-text-lime',
  }[event.type];

  const depthOffset = depth * 18;
  const hasHierarchy = depth > 0;

  return (
    <div className="mx-4 my-1" style={{ marginLeft: depthOffset }}>
      <div
        className={`relative rounded-md border px-3 py-2 ${bgColor} animate-fade-in-up ${
          hasHierarchy ? 'pl-4' : ''
        }`}
        style={{
          borderLeftWidth: agentColor ? '3px' : undefined,
          borderLeftColor: agentColor,
          borderLeftStyle: agentColor ? 'solid' : undefined,
        }}
      >
        {hasHierarchy && (
          <>
            <div className="absolute left-0 top-0 bottom-0 w-px bg-cyber-border/50" />
            <div className="absolute -left-1.5 top-3 h-2 w-2 rounded-full bg-cyber-border/80" />
          </>
        )}
        <div className="flex items-start gap-2">
          <div className="flex-1 min-w-0">
            <div className="flex flex-wrap items-center gap-2 text-[11px] tracking-wide">
              {event.agentId && (
                <span className={`font-semibold ${labelColor} normal-case`}>
                  {getAgentShortName(event.agentId, agents)}
                </span>
              )}
              <span className="text-gray-500 normal-case">
                {new Date(event.timestamp).toLocaleTimeString()}
              </span>
              {toolName && (
                <span className="text-[10px] font-mono bg-cyber-bg/80 px-2 py-0.5 rounded border border-cyber-border text-cyber-cyan">
                  {toolName}
                </span>
              )}
              {!toolName && toolKind && (
                <span className="text-[10px] font-mono bg-cyber-bg/80 px-2 py-0.5 rounded border border-cyber-border text-cyber-cyan">
                  {toolKind}
                </span>
              )}
              {isInProgress && (
                <span className="flex items-center gap-1 text-[10px] px-2 py-0.5 rounded border bg-cyber-purple/10 border-cyber-purple/30 text-cyber-purple normal-case">
                  <Loader className="w-3 h-3 animate-spin" />
                  running...
                </span>
              )}
              {toolStatus && !isInProgress && (
                <span
                  className={`text-[10px] px-2 py-0.5 rounded border normal-case ${
                    toolStatus === 'completed'
                      ? 'bg-cyber-lime/10 border-cyber-lime/30 text-cyber-lime'
                      : toolStatus === 'failed'
                      ? 'bg-cyber-orange/10 border-cyber-orange/30 text-cyber-orange'
                      : 'bg-cyber-purple/10 border-cyber-purple/30 text-cyber-purple'
                  }`}
                >
                  {toolStatus}
                </span>
              )}
            </div>
            {event.type !== 'tool_result' && event.type !== 'tool_call' && event.content && (
              <div className="mt-1 text-sm text-gray-200">
                <MessageContent content={event.content} />
              </div>
            )}
            {event.type === 'tool_call' && event.content && (
              <div className="mt-1 text-sm text-gray-300">
                <MessageContent content={event.content} />
              </div>
            )}
            {event.type === 'tool_result' && (
              <div className="mt-2 text-sm text-gray-200">
                <ToolResultContent event={event} />
              </div>
            )}
            {/* For merged tool calls, show input and result together */}
            {event.type === 'tool_call' && (
              <>
                <ToolInputContent event={event} />
                {hasMergedResult && event.mergedResult && (
                  <div className="mt-2">
                    <div className="text-[10px] text-gray-500 uppercase mb-1">Result</div>
                    <ToolResultContent event={event.mergedResult as EventRow} />
                  </div>
                )}
              </>
            )}
          </div>
        </div>
      </div>
    </div>
  );
}

export default App;

type EventRow = EventItem & {
  depth: number;
  parentId?: string;
  toolName?: string;
  mergedResult?: EventItem; // For merged tool_call + tool_result
};

function buildEventRows(events: EventItem[]): EventRow[] {
  const rows: EventRow[] = [];
  const depthMap = new Map<string, number>();
  const toolCallMap = new Map<
    string,
    { eventId: string; depth: number; kind?: string; name?: string; rowIndex?: number }
  >();
  const openDelegations: string[] = [];
  let currentAgentId: string | null = null;

  for (const event of events) {
    let depth = 0;
    let parentId: string | undefined;
    let toolName: string | undefined;

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
      if (event.toolCall?.kind === 'delegate') {
        openDelegations.push(toolCallKey);
      }
      
      depthMap.set(event.id, depth);
      rows.push({ ...event, depth, parentId, toolName });
    } else if (event.type === 'tool_result') {
      const toolCallKey = event.toolCall?.tool_call_id;
      const toolParent = toolCallKey ? toolCallMap.get(toolCallKey) : undefined;
      
      if (toolParent && toolParent.rowIndex !== undefined) {
        // Merge result into the tool_call row instead of creating a new row
        const toolCallRow = rows[toolParent.rowIndex];
        if (toolCallRow) {
          toolCallRow.mergedResult = event;
        }
      } else {
        // No matching tool_call, render as separate event (shouldn't happen normally)
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
        depthMap.set(event.id, depth);
        rows.push({ ...event, depth, parentId, toolName });
      }
      
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
      if (openDelegations.length > 0) {
        const delegationId = openDelegations[openDelegations.length - 1];
        const delegationDepth = toolCallMap.get(delegationId)?.depth ?? 1;
        depth = delegationDepth + 1;
        parentId = toolCallMap.get(delegationId)?.eventId;
      }
      if (event.type === 'agent') {
        currentAgentId = event.id;
      }
      
      depthMap.set(event.id, depth);
      rows.push({ ...event, depth, parentId, toolName });
    }
  }

  return rows;
}

function ToolInputContent({ event }: { event: EventItem }) {
  const rawInput = parseJsonMaybe(event.toolCall?.raw_input) ?? event.toolCall?.raw_input;
  if (!rawInput) return null;

  const toolKind = event.toolCall?.kind;
  const patchValue = extractPatchValue(rawInput);
  const editInput = extractEditInput(rawInput);
  const isApplyPatch = toolKind === 'apply_patch' || typeof patchValue === 'string';
  const isEdit = toolKind === 'edit' || (editInput?.oldString && editInput?.newString);

  if (isApplyPatch) {
    const { cleaned, patch } = stripPatchFromInput(rawInput, patchValue ?? '');
    const hasExtra = cleaned && Object.keys(cleaned).length > 0;
    return (
      <details className="mt-2">
        <summary className="text-[11px] text-cyber-cyan cursor-pointer hover:text-cyber-magenta transition-colors">
          Patch Input
        </summary>
        <div className="mt-2 event-diff-container">
          <PatchDiff
            patch={patch ?? ''}
            options={{
              theme: 'github-dark',
              themeType: 'dark',
              diffStyle: 'unified',
              diffIndicators: 'bars',
              overflow: 'wrap',
              useCSSClasses: true,
              disableBackground: true,
            }}
          />
        </div>
        {hasExtra && (
          <pre className="mt-2 p-2 bg-cyber-bg/80 border border-cyber-border rounded-md text-xs overflow-x-auto">
            {JSON.stringify(cleaned, null, 2)}
          </pre>
        )}
      </details>
    );
  }

  if (isEdit && editInput) {
    const { cleaned, patch } = stripEditFromInput(rawInput, editInput);
    const hasExtra = cleaned && Object.keys(cleaned).length > 0;
    return (
      <details open className="mt-2">
        <summary className="text-[11px] text-cyber-cyan cursor-pointer hover:text-cyber-magenta transition-colors">
          Edit Input
        </summary>
        <div className="mt-2 event-diff-container">
          <PatchDiff
            patch={patch}
            options={{
              theme: 'pierre-dark',
              themeType: 'dark',
              diffStyle: 'split',
              diffIndicators: 'bars',
              lineDiffType: 'word-alt',
              overflow: 'wrap',
              disableLineNumbers: false,
              useCSSClasses: true,
              disableBackground: true,
            }}
          />
        </div>
        {hasExtra && (
          <pre className="mt-2 p-2 bg-cyber-bg/80 border border-cyber-border rounded-md text-xs overflow-x-auto">
            {JSON.stringify(cleaned, null, 2)}
          </pre>
        )}
      </details>
    );
  }

  return (
    <details className="mt-2">
      <summary className="text-[11px] text-cyber-cyan cursor-pointer hover:text-cyber-magenta transition-colors">
        View Input
      </summary>
      <pre className="mt-2 p-2 bg-cyber-bg/80 border border-cyber-border rounded-md text-xs overflow-x-auto">
        {JSON.stringify(rawInput, null, 2)}
      </pre>
    </details>
  );
}

function ToolResultContent({ event }: { event: EventRow }) {
  const toolName = event.toolName ?? inferToolName(event);
  const toolKind = inferToolKind(event);
  if (toolName === 'shell' || toolKind === 'shell') {
    return (
      <ShellOutput
        rawOutput={event.toolCall?.raw_output ?? event.content}
        fallback={event.content}
      />
    );
  }
  if (toolName === 'read_file' || toolKind === 'read_file') {
    return (
      <ReadFileOutput
        rawOutput={event.toolCall?.raw_output ?? event.content}
        fallback={event.content}
      />
    );
  }
  if (!event.content) return null;
  return (
    <pre className="text-xs font-mono bg-cyber-bg/70 border border-cyber-border rounded-md p-2 overflow-x-auto">
      {event.content}
    </pre>
  );
}

function ShellOutput({
  rawOutput,
  fallback,
}: {
  rawOutput?: unknown;
  fallback?: string;
}) {
  const parsed = parseJsonMaybe(rawOutput) ?? parseJsonMaybe(fallback);
  const stdout =
    typeof parsed?.stdout === 'string'
      ? parsed.stdout
      : typeof rawOutput === 'string'
      ? rawOutput
      : fallback ?? '';
  const stderr = typeof parsed?.stderr === 'string' ? parsed.stderr : '';
  const exitCode = typeof parsed?.exit_code === 'number' ? parsed.exit_code : undefined;

  return (
    <div className="rounded-md border border-cyber-border/70 bg-black/60 font-mono text-xs text-gray-200">
      <div className="flex items-center justify-between px-2 py-1 border-b border-cyber-border/60 text-[10px] uppercase tracking-wide text-gray-400">
        <span>terminal</span>
        {exitCode !== undefined && <span>exit {exitCode}</span>}
      </div>
      <div className="max-h-64 overflow-auto p-2 space-y-2">
        {stdout && (
          <div>
            <div className="text-[10px] uppercase tracking-wide text-gray-400">stdout</div>
            <pre className="whitespace-pre-wrap break-words">{stdout}</pre>
          </div>
        )}
        {stderr && (
          <div>
            <div className="text-[10px] uppercase tracking-wide text-gray-400">stderr</div>
            <pre className="whitespace-pre-wrap break-words text-cyber-orange/90">
              {stderr}
            </pre>
          </div>
        )}
        {!stdout && !stderr && (
          <div className="text-gray-500 text-[11px]">No output</div>
        )}
      </div>
    </div>
  );
}

function ReadFileOutput({
  rawOutput,
  fallback,
}: {
  rawOutput?: unknown;
  fallback?: string;
}) {
  const parsed = parseJsonMaybe(rawOutput) ?? parseJsonMaybe(fallback);
  const filePath = typeof parsed?.path === 'string' ? parsed.path : undefined;
  const content = typeof parsed?.content === 'string' ? parsed.content : fallback ?? '';
  const startLine = typeof parsed?.start_line === 'number' ? parsed.start_line : undefined;
  const endLine = typeof parsed?.end_line === 'number' ? parsed.end_line : undefined;

  return (
    <details className="rounded-md border border-cyber-border/70 bg-cyber-bg/70 p-2">
      <summary className="cursor-pointer text-[11px] text-cyber-cyan hover:text-cyber-magenta transition-colors">
        Read file{filePath ? `: ${filePath}` : ''}
        {startLine !== undefined && endLine !== undefined ? ` (${startLine}-${endLine})` : ''}
      </summary>
      {typeof parsed?.content === 'string' ? (
        <pre className="mt-2 max-h-64 overflow-auto whitespace-pre-wrap break-words text-xs font-mono text-gray-200">
          {content || 'No content'}
        </pre>
      ) : (
        <pre className="mt-2 max-h-64 overflow-auto whitespace-pre-wrap break-words text-xs font-mono text-gray-200">
          {parsed ? JSON.stringify(parsed, null, 2) : content || 'No content'}
        </pre>
      )}
    </details>
  );
}

function parseJsonMaybe(value: unknown): any | undefined {
  if (typeof value === 'string') {
    try {
      const parsed = JSON.parse(value);
      if (typeof parsed === 'string') {
        const trimmed = parsed.trim();
        if (
          (trimmed.startsWith('{') && trimmed.endsWith('}')) ||
          (trimmed.startsWith('[') && trimmed.endsWith(']'))
        ) {
          try {
            return JSON.parse(trimmed);
          } catch {
            return parsed;
          }
        }
      }
      return parsed;
    } catch {
      return undefined;
    }
  }
  if (typeof value === 'object' && value !== null) {
    return value;
  }
  return undefined;
}

function inferToolKind(event: EventItem): string | undefined {
  if (event.toolCall?.kind) return event.toolCall.kind;
  const parsed = parseJsonMaybe(event.toolCall?.raw_output ?? event.content);
  if (!parsed || typeof parsed !== 'object') return undefined;
  if (typeof parsed.stdout === 'string' || typeof parsed.stderr === 'string') {
    return 'shell';
  }
  if (typeof parsed.path === 'string' && typeof parsed.content === 'string') {
    return 'read_file';
  }
  return undefined;
}

function inferToolName(event: EventItem): string | undefined {
  const named = (event as EventRow).toolName;
  if (typeof named === 'string' && named.length > 0) return named;
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

function extractPatchValue(rawInput: unknown): string | undefined {
  if (!rawInput) return undefined;
  if (typeof rawInput === 'object' && rawInput !== null) {
    const direct = (rawInput as { patch?: unknown }).patch;
    if (typeof direct === 'string') return direct;
    const args = (rawInput as { arguments?: unknown }).arguments;
    if (typeof args === 'string') {
      const parsed = parseJsonMaybe(args);
      if (typeof parsed?.patch === 'string') return parsed.patch;
    }
    if (typeof args === 'object' && args !== null) {
      const argPatch = (args as { patch?: unknown }).patch;
      if (typeof argPatch === 'string') return argPatch;
    }
  }
  if (typeof rawInput === 'string') {
    const parsed = parseJsonMaybe(rawInput);
    if (typeof parsed?.patch === 'string') return parsed.patch;
  }
  return undefined;
}

type EditInput = {
  filePath?: string;
  oldString?: string;
  newString?: string;
};

function extractEditInput(rawInput: unknown): EditInput | undefined {
  if (!rawInput) return undefined;
  if (typeof rawInput === 'object' && rawInput !== null) {
    const direct = rawInput as EditInput & { arguments?: unknown };
    if (direct.oldString || direct.newString || direct.filePath) {
      return {
        filePath: direct.filePath,
        oldString: direct.oldString,
        newString: direct.newString,
      };
    }
    const args = direct.arguments;
    if (typeof args === 'string') {
      const parsed = parseJsonMaybe(args);
      if (parsed && typeof parsed === 'object') {
        const parsedEdit = parsed as EditInput;
        return {
          filePath: parsedEdit.filePath,
          oldString: parsedEdit.oldString,
          newString: parsedEdit.newString,
        };
      }
    }
    if (typeof args === 'object' && args !== null) {
      const parsedEdit = args as EditInput;
      return {
        filePath: parsedEdit.filePath,
        oldString: parsedEdit.oldString,
        newString: parsedEdit.newString,
      };
    }
  }
  if (typeof rawInput === 'string') {
    const parsed = parseJsonMaybe(rawInput);
    if (parsed && typeof parsed === 'object') {
      const parsedEdit = parsed as EditInput;
      return {
        filePath: parsedEdit.filePath,
        oldString: parsedEdit.oldString,
        newString: parsedEdit.newString,
      };
    }
  }
  return undefined;
}

function buildEditPatch(editInput: EditInput): string {
  const rawPath = editInput.filePath ?? 'file';
  const normalizedPath = rawPath.replace(/^\/+/, '') || 'file';
  const oldText = editInput.oldString ?? '';
  const newText = editInput.newString ?? '';
  const oldLines = oldText.split('\n').length;
  const newLines = newText.split('\n').length;
  const oldBlock = oldText
    .split('\n')
    .map((line) => `-${line}`)
    .join('\n');
  const newBlock = newText
    .split('\n')
    .map((line) => `+${line}`)
    .join('\n');
  return [
    `diff --git a/${normalizedPath} b/${normalizedPath}`,
    `--- a/${normalizedPath}`,
    `+++ b/${normalizedPath}`,
    `@@ -1,${oldLines} +1,${newLines} @@`,
    oldBlock,
    newBlock,
  ].join('\n');
}

function stripEditFromInput(rawInput: unknown, editInput: EditInput) {
  const patch = buildEditPatch(editInput);
  if (typeof rawInput !== 'object' || rawInput === null) {
    return { cleaned: undefined as Record<string, unknown> | undefined, patch };
  }
  const input = { ...(rawInput as Record<string, unknown>) };
  if ('oldString' in input) delete input.oldString;
  if ('newString' in input) delete input.newString;
  if ('filePath' in input) delete input.filePath;
  if (typeof input.arguments === 'object' && input.arguments !== null) {
    const args = { ...(input.arguments as Record<string, unknown>) };
    if ('oldString' in args) delete args.oldString;
    if ('newString' in args) delete args.newString;
    if ('filePath' in args) delete args.filePath;
    input.arguments = args;
  }
  return { cleaned: input, patch };
}

function stripPatchFromInput(rawInput: unknown, patchValue: string) {
  if (typeof rawInput !== 'object' || rawInput === null) {
    return { cleaned: undefined as Record<string, unknown> | undefined, patch: patchValue };
  }
  const input = { ...(rawInput as Record<string, unknown>) };
  if (input.patch === patchValue) {
    delete input.patch;
    return { cleaned: input, patch: patchValue };
  }
  if (typeof input.arguments === 'object' && input.arguments !== null) {
    const args = { ...(input.arguments as Record<string, unknown>) };
    if (args.patch === patchValue) {
      delete args.patch;
      input.arguments = args;
      return { cleaned: input, patch: patchValue };
    }
  }
  return { cleaned: input, patch: patchValue };
}
