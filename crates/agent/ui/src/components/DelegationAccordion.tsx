/**
 * Collapsible accordion for delegation (sub-agent) streams
 * Shows summary when collapsed, full event list when expanded
 */

import { useState, useRef, useEffect } from 'react';
import { ChevronDown, ChevronRight, Clock, CheckCircle, XCircle, Loader } from 'lucide-react';
import { UiAgentInfo, EventRow } from '../types';
import { getAgentColor } from '../utils/agentColors';
import { getAgentShortName } from '../utils/agentNames';

export interface DelegationGroup {
  id: string;
  delegateToolCallId: string;
  delegateEvent: EventRow;
  agentId?: string;
  events: EventRow[];
  status: 'in_progress' | 'completed' | 'failed';
  startTime: number;
  endTime?: number;
}

export interface DelegationAccordionProps {
  group: DelegationGroup;
  agents: UiAgentInfo[];
  isExpanded: boolean;
  onToggle: () => void;
  onToolClick: (event: EventRow) => void;
  renderEvent: (event: EventRow) => React.ReactNode;
  isActive?: boolean; // Currently being executed
  highlightRef?: React.RefObject<HTMLDivElement | null>; // For scroll-into-view
}

export function DelegationAccordion({
  group,
  agents,
  isExpanded,
  onToggle,
  onToolClick: _onToolClick, // Prefixed with _ to indicate intentionally unused (events use renderEvent)
  renderEvent,
  isActive,
  highlightRef,
}: DelegationAccordionProps) {
  const [isHighlighted, setIsHighlighted] = useState(false);
  const accordionRef = useRef<HTMLDivElement>(null);
  
  // Calculate summary stats
  const toolCallCount = group.events.filter(e => e.type === 'tool_call').length;
  const agentColor = group.agentId ? getAgentColor(group.agentId) : '#b026ff';
  const agentName = group.agentId ? getAgentShortName(group.agentId, agents) : 'Sub-agent';
  
  // Duration
  const durationMs = group.endTime 
    ? group.endTime - group.startTime 
    : Date.now() - group.startTime;
  const durationStr = formatDuration(durationMs);

  // Highlight animation when scrolled to
  useEffect(() => {
    if (highlightRef?.current === accordionRef.current && accordionRef.current) {
      setIsHighlighted(true);
      const timeout = setTimeout(() => setIsHighlighted(false), 1500);
      return () => clearTimeout(timeout);
    }
  }, [highlightRef?.current]);

  return (
    <div
      ref={accordionRef}
      className={`
        relative rounded-lg border overflow-hidden transition-all duration-300
        ${isActive ? 'border-cyber-purple shadow-[0_0_20px_rgba(176,38,255,0.3)]' : 'border-cyber-border/50'}
        ${isHighlighted ? 'ring-2 ring-cyber-cyan ring-offset-2 ring-offset-cyber-bg' : ''}
      `}
      style={{
        borderLeftWidth: '3px',
        borderLeftColor: agentColor,
      }}
    >
      {/* Connector line anchor point - positioned at top left */}
      <div 
        className="delegation-anchor absolute -left-[3px] top-0 w-[3px] h-4"
        style={{ backgroundColor: agentColor }}
        data-delegation-id={group.id}
      />

      {/* Header - always visible */}
      <button
        onClick={onToggle}
        className={`
          w-full flex items-center gap-3 px-4 py-2.5 text-left
          transition-colors duration-200
          ${isExpanded ? 'bg-cyber-surface/80' : 'bg-cyber-surface/40 hover:bg-cyber-surface/60'}
        `}
      >
        {/* Expand/collapse icon */}
        <span className="flex-shrink-0 text-gray-400">
          {isExpanded ? (
            <ChevronDown className="w-4 h-4" />
          ) : (
            <ChevronRight className="w-4 h-4" />
          )}
        </span>

        {/* Agent indicator */}
        <span
          className="flex-shrink-0 text-xs font-semibold px-2 py-0.5 rounded"
          style={{
            backgroundColor: `${agentColor}20`,
            color: agentColor,
            border: `1px solid ${agentColor}40`,
          }}
        >
          {agentName}
        </span>

        {/* Status */}
        <span className="flex-shrink-0">
          {group.status === 'in_progress' && (
            <Loader className="w-4 h-4 text-cyber-purple animate-spin" />
          )}
          {group.status === 'completed' && (
            <CheckCircle className="w-4 h-4 text-cyber-lime" />
          )}
          {group.status === 'failed' && (
            <XCircle className="w-4 h-4 text-cyber-orange" />
          )}
        </span>

        {/* Summary when collapsed */}
        {!isExpanded && (
          <span className="flex-1 text-xs text-gray-400 truncate">
            {toolCallCount} tool call{toolCallCount !== 1 ? 's' : ''}
          </span>
        )}

        {/* Duration */}
        <span className="flex-shrink-0 flex items-center gap-1 text-xs text-gray-500">
          <Clock className="w-3 h-3" />
          {durationStr}
        </span>
      </button>

      {/* Expanded content */}
      {isExpanded && (
        <div className="border-t border-cyber-border/30 bg-cyber-bg/30">
          {/* Events list */}
          <div className="p-2 space-y-1 max-h-96 overflow-auto">
            {group.events.length === 0 ? (
              <div className="text-xs text-gray-500 text-center py-4">
                No events yet...
              </div>
            ) : (
              group.events.map((event) => (
                <div key={event.id} className="pl-2">
                  {renderEvent(event)}
                </div>
              ))
            )}
          </div>
        </div>
      )}
    </div>
  );
}

/**
 * Connector line between delegate tool call and its accordion
 * Renders as an SVG overlay
 */
export interface DelegationConnectorProps {
  startY: number; // Y position of delegate tool call
  endY: number;   // Y position of accordion
  startX: number; // X position (left edge)
  color: string;
  isActive: boolean;
}

export function DelegationConnector({ startY, endY, startX, color, isActive }: DelegationConnectorProps) {
  if (endY <= startY) return null;
  
  const height = endY - startY;
  const width = 20;
  
  return (
    <svg
      className="absolute pointer-events-none"
      style={{
        left: startX - 10,
        top: startY,
        width,
        height,
        overflow: 'visible',
      }}
    >
      {/* Vertical line */}
      <line
        x1={10}
        y1={0}
        x2={10}
        y2={height}
        stroke={color}
        strokeWidth={2}
        strokeDasharray={isActive ? '4 4' : 'none'}
        className={isActive ? 'delegation-connector-active' : ''}
      />
      {/* End dot */}
      <circle
        cx={10}
        cy={height}
        r={4}
        fill={color}
      />
    </svg>
  );
}

// Helper: format duration
function formatDuration(ms: number): string {
  if (ms < 1000) return `${ms}ms`;
  const seconds = Math.floor(ms / 1000);
  if (seconds < 60) return `${seconds}s`;
  const minutes = Math.floor(seconds / 60);
  const remainingSeconds = seconds % 60;
  return `${minutes}m ${remainingSeconds}s`;
}

// Note: Delegation group building is done in App.tsx via buildEventRowsWithDelegations()
// This ensures the groups are built with EventRow types that include depth and other metadata
