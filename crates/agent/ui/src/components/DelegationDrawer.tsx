import { useEffect, useRef, useState } from 'react';
import type { MouseEvent as ReactMouseEvent } from 'react';
import * as Dialog from '@radix-ui/react-dialog';
import { X, Copy, Check, Cpu, Wrench, DollarSign } from 'lucide-react';
import { DelegationGroupInfo, EventRow, UiAgentInfo, LlmConfigDetails } from '../types';
import { MessageContent } from './MessageContent';
import { ToolSummary } from './ToolSummary';
import { ModelConfigPopover } from './ModelConfigPopover';
import { getAgentColor } from '../utils/agentColors';
import { getAgentShortName } from '../utils/agentNames';
import { useCopyToClipboard } from '../hooks/useCopyToClipboard';
import { calculateDelegationStats } from '../utils/statsCalculator';
import { formatTokensAbbrev, formatCost, formatTimestamp } from '../utils/formatters';

interface DelegationDrawerProps {
  delegation?: DelegationGroupInfo;
  agents: UiAgentInfo[];
  onClose: () => void;
  onToolClick: (event: EventRow) => void;
  llmConfigCache?: Record<number, LlmConfigDetails>;
  requestLlmConfig?: (configId: number, callback: (config: LlmConfigDetails) => void) => void;
}



export function DelegationDrawer({ delegation, agents, onClose, onToolClick, llmConfigCache = {}, requestLlmConfig }: DelegationDrawerProps) {
  const [drawerWidth, setDrawerWidth] = useState(() => {
    if (typeof window === 'undefined') return 420;
    const stored = window.localStorage.getItem('delegationDrawerWidth');
    const parsed = stored ? Number.parseInt(stored, 10) : 420;
    return Number.isNaN(parsed) ? 420 : parsed;
  });
  const [isMobile, setIsMobile] = useState(() => {
    if (typeof window === 'undefined') return false;
    return window.innerWidth < 768;
  });
  const dragStateRef = useRef<{ startX: number; startWidth: number } | null>(null);

  useEffect(() => {
    const handleResize = () => {
      setIsMobile(window.innerWidth < 768);
    };
    handleResize();
    window.addEventListener('resize', handleResize);
    return () => window.removeEventListener('resize', handleResize);
  }, []);

  useEffect(() => {
    if (typeof window === 'undefined') return;
    window.localStorage.setItem('delegationDrawerWidth', String(drawerWidth));
  }, [drawerWidth]);

  useEffect(() => {
    const handleMouseMove = (event: MouseEvent) => {
      if (!dragStateRef.current || isMobile) return;
      const delta = dragStateRef.current.startX - event.clientX;
      const maxWidth = Math.max(360, window.innerWidth - 80);
      const nextWidth = Math.min(
        maxWidth,
        Math.max(320, dragStateRef.current.startWidth + delta)
      );
      setDrawerWidth(nextWidth);
    };
    const handleMouseUp = () => {
      dragStateRef.current = null;
    };
    window.addEventListener('mousemove', handleMouseMove);
    window.addEventListener('mouseup', handleMouseUp);
    return () => {
      window.removeEventListener('mousemove', handleMouseMove);
      window.removeEventListener('mouseup', handleMouseUp);
    };
  }, [isMobile]);

  if (!delegation) return null;

  const agentId = delegation.targetAgentId ?? delegation.agentId;
  const agentName = agentId ? getAgentShortName(agentId, agents) : 'Sub-agent';
  const agentColor = agentId ? getAgentColor(agentId) : '#b026ff';
  const stats = calculateDelegationStats(delegation);
  const objective = delegation.objective ??
    (delegation.delegateEvent.toolCall?.raw_input as { objective?: string } | undefined)?.objective;

  // Extract model info from the first event with provider/model
  const modelEvent = delegation.events.find(e => e.provider && e.model);
  const modelLabel = modelEvent ? `${modelEvent.provider} / ${modelEvent.model}` : undefined;
  const modelConfigId = modelEvent?.configId;

  const visibleEvents = delegation.events
    .filter((event) =>
      event.type === 'tool_call' || (event.type === 'agent' && event.isMessage)
    )
    .sort((a, b) => a.timestamp - b.timestamp);

  const [showConfigPopover, setShowConfigPopover] = useState(false);
  const { copiedValue, copy: copyToClipboard } = useCopyToClipboard();

  const handleDragStart = (event: ReactMouseEvent) => {
    if (isMobile) return;
    dragStateRef.current = {
      startX: event.clientX,
      startWidth: drawerWidth,
    };
  };

  return (
    <Dialog.Root open onOpenChange={(open) => { if (!open) onClose(); }}>
      <Dialog.Portal>
        <Dialog.Overlay className="fixed inset-0 z-40 bg-black/50" />
        <Dialog.Content
          className="fixed top-0 right-0 z-50 h-full bg-cyber-surface border-l border-cyber-border shadow-[0_0_30px_rgba(0,255,249,0.12)] flex flex-col"
          style={{ width: isMobile ? '100%' : `${drawerWidth}px` }}
          aria-describedby={undefined}
          onOpenAutoFocus={(e) => e.preventDefault()}
        >
          {/* Resize handle */}
          {!isMobile && (
            <div
              onMouseDown={handleDragStart}
              className="absolute left-0 top-0 h-full w-1.5 cursor-col-resize bg-cyber-border/40 hover:bg-cyber-cyan/60 transition-colors"
              title="Drag to resize"
            />
          )}

          {/* Header */}
          <div className="group px-5 py-4 border-b border-cyber-border/50 flex items-start justify-between gap-3">
            <div className="flex-1">
              <div className="flex items-center gap-2 flex-wrap">
                <span
                  className="text-xs font-semibold uppercase tracking-wide px-2 py-0.5 rounded"
                  style={{
                    color: agentColor,
                    backgroundColor: `${agentColor}20`,
                    border: `1px solid ${agentColor}40`,
                  }}
                >
                  {agentName}
                </span>
                <span className="text-[10px] text-gray-500">
                  Delegation
                </span>
                {/* Model label in header */}
                {modelLabel && modelConfigId && requestLlmConfig ? (
                  <ModelConfigPopover
                    configId={modelConfigId}
                    open={showConfigPopover}
                    onOpenChange={setShowConfigPopover}
                    requestConfig={requestLlmConfig}
                    cachedConfig={llmConfigCache[modelConfigId]}
                  >
                    <button
                      type="button"
                      className="text-[10px] leading-none px-1.5 py-px rounded bg-cyber-surface/60 border border-cyber-border/40 text-gray-400 truncate max-w-[160px] hover:border-cyber-cyan/40 hover:text-gray-300 cursor-pointer transition-colors"
                      title={modelLabel}
                    >
                      {modelLabel}
                    </button>
                  </ModelConfigPopover>
                ) : modelLabel ? (
                  <span
                    className="text-[10px] leading-none px-1.5 py-px rounded bg-cyber-surface/60 border border-cyber-border/40 text-gray-400 truncate max-w-[160px] cursor-default"
                    title={modelLabel}
                  >
                    {modelLabel}
                  </span>
                ) : null}
              </div>
              <div className="flex items-center gap-2 mt-1">
                <Dialog.Title className="text-sm text-gray-300">
                  {objective ?? 'Delegated task'}
                </Dialog.Title>
                {objective && (
                  <button
                    onClick={() => copyToClipboard(objective, 'delegation-objective')}
                    className="opacity-0 group-hover:opacity-100 transition-opacity p-1 rounded hover:bg-cyber-bg/50"
                    title="Copy objective"
                  >
                    {copiedValue === 'delegation-objective' ? (
                      <Check className="w-3.5 h-3.5 text-cyber-lime" />
                    ) : (
                      <Copy className="w-3.5 h-3.5 text-gray-400 hover:text-cyber-cyan" />
                    )}
                  </button>
                )}
              </div>
              {/* Delegation stats */}
              <div className="flex items-center gap-3 mt-1.5 text-[10px]">
                <span className={`flex items-center gap-1 ${
                  (stats.contextPercent ?? 0) >= 80 ? 'text-cyber-orange' :
                  (stats.contextPercent ?? 0) >= 70 ? 'text-yellow-500' :
                  'text-gray-400'
                }`}>
                  <Cpu className="w-3 h-3" />
                  {stats.contextPercent !== undefined
                    ? `${stats.contextPercent}% (${formatTokensAbbrev(stats.contextTokens)}/${formatTokensAbbrev(stats.contextLimit!)})`
                    : stats.contextTokens > 0
                      ? formatTokensAbbrev(stats.contextTokens)
                      : '—'}
                </span>
                <span className="text-cyber-border/60">·</span>
                <span className="flex items-center gap-1 text-gray-400">
                  <Wrench className="w-3 h-3" />
                  {stats.toolCallCount} call{stats.toolCallCount === 1 ? '' : 's'}
                </span>
                {stats.costUsd > 0 && (
                  <>
                    <span className="text-cyber-border/60">·</span>
                    <span className="flex items-center gap-1 text-cyber-cyan">
                      <DollarSign className="w-3 h-3" />
                      {formatCost(stats.costUsd)}
                    </span>
                  </>
                )}
              </div>
            </div>
            <Dialog.Close
              className="p-2 rounded-md hover:bg-cyber-bg/70 transition-colors text-gray-400 hover:text-gray-200"
              aria-label="Close delegation details"
            >
              <X className="w-4 h-4" />
            </Dialog.Close>
          </div>

          {/* Events list */}
          <div className="flex-1 overflow-y-auto px-5 py-4 space-y-3">
            {visibleEvents.length === 0 ? (
              <div className="text-sm text-gray-500">No delegation events yet.</div>
            ) : (
              visibleEvents.map((event) => {
                if (event.type === 'tool_call') {
                  return (
                    <ToolSummary
                      key={event.id}
                      event={event}
                      onClick={() => onToolClick(event)}
                    />
                  );
                }
                if (event.type === 'agent' && event.isMessage) {
                  const eventAgentId = event.agentId ?? agentId;
                  const eventAgentName = eventAgentId
                    ? getAgentShortName(eventAgentId, agents)
                    : agentName;
                  const eventAgentColor = eventAgentId
                    ? getAgentColor(eventAgentId)
                    : agentColor;
                  return (
                    <div key={event.id} className="group/message rounded-md border border-cyber-border/40 bg-cyber-bg/40 px-3 py-2">
                      <div className="flex items-center gap-2 mb-1">
                        <span
                          className="text-[10px] font-semibold uppercase tracking-wide"
                          style={{ color: eventAgentColor }}
                        >
                          {eventAgentName}
                        </span>
                        <span className="text-[10px] text-gray-500">
                          {formatTimestamp(event.timestamp)}
                        </span>
                        <button
                          onClick={() => copyToClipboard(event.content, `delegation-message-${event.id}`)}
                          className="opacity-0 group-hover/message:opacity-100 transition-opacity p-1 rounded hover:bg-cyber-bg/50"
                          title="Copy message"
                        >
                          {copiedValue === `delegation-message-${event.id}` ? (
                            <Check className="w-3.5 h-3.5 text-cyber-lime" />
                          ) : (
                            <Copy className="w-3.5 h-3.5 text-gray-400 hover:text-cyber-cyan" />
                          )}
                        </button>
                      </div>
                      <MessageContent content={event.content} />
                    </div>
                  );
                }
                return null;
              })
            )}
          </div>
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  );
}
