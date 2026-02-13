import React, { useState } from 'react';
import type { Turn, EventRow } from '../types';

interface SessionTimelineProps {
  turns: Turn[];
  events: EventRow[];
  expertMode: boolean;
  onJumpToTurn: (turnIndex: number) => void;
  activeTurnIndex?: number;
}

interface TimelineDot {
  id: string;
  type: 'user' | 'agent' | 'tool' | 'error' | 'delegation' | 'internal';
  turnIndex: number;
  eventIndex: number;
  label: string;
  size: 'large' | 'medium' | 'small' | 'tiny';
  opacity: number;
  color: string;
}

export function SessionTimeline({ 
  turns, 
  events, 
  expertMode, 
  onJumpToTurn, 
  activeTurnIndex 
}: SessionTimelineProps) {
  const [hoveredTurn, setHoveredTurn] = useState<number | null>(null);

  // Build timeline dots from turns and events
  const dots: TimelineDot[] = [];
  
  turns.forEach((turn, turnIndex) => {
    // Add user message dot
    if (turn.userMessage) {
      dots.push({
        id: `turn-${turnIndex}-user`,
        type: 'user',
        turnIndex,
        eventIndex: events.findIndex(e => e.id === turn.userMessage?.id),
        label: turn.userMessage.content.slice(0, 50),
        size: 'large',
        opacity: 1,
        color: 'accent-secondary'
      });
    }

    // Add tool call dots
    turn.toolCalls.forEach((toolCall, toolIndex) => {
      // Skip if not visible based on expert mode
      if (!expertMode && !toolCall.isMessage) {
        return;
      }

      let dotType: TimelineDot['type'] = 'tool';
      let dotSize: TimelineDot['size'] = 'small';
      let dotOpacity = 0.5;
      let dotColor = 'accent-tertiary';

      // Check if it's a delegation
      if (toolCall.isDelegateToolCall) {
        dotType = 'delegation';
        dotSize = 'medium';
        dotOpacity = 0.7;
        dotColor = 'accent-tertiary';
      } else if (toolCall.toolCall?.status === 'failed' || toolCall.mergedResult?.toolCall?.status === 'failed') {
        dotType = 'error';
        dotOpacity = 1;
        dotColor = 'status-warning';
      } else if (!toolCall.isMessage) {
        // Internal events
        dotType = 'internal';
        dotSize = 'tiny';
        dotOpacity = 0.25;
        dotColor = 'accent-primary';
      }

      dots.push({
        id: `turn-${turnIndex}-tool-${toolIndex}`,
        type: dotType,
        turnIndex,
        eventIndex: events.findIndex(e => e.id === toolCall.id),
        label: toolCall.toolName || toolCall.type,
        size: dotSize,
        opacity: dotOpacity,
        color: dotColor
      });
    });

    // Add agent message dots
    turn.agentMessages.forEach((message, msgIndex) => {
      dots.push({
        id: `turn-${turnIndex}-agent-${msgIndex}`,
        type: 'agent',
        turnIndex,
        eventIndex: events.findIndex(e => e.id === message.id),
        label: message.content.slice(0, 50),
        size: 'large',
        opacity: 0.8,
        color: 'accent-primary'
      });
    });
  });

  const handleDotClick = (turnIndex: number) => {
    onJumpToTurn(turnIndex);
  };

  const getSizeClass = (size: TimelineDot['size']) => {
    switch (size) {
      case 'large': return 'timeline-dot-large';
      case 'medium': return 'timeline-dot-medium';
      case 'small': return 'timeline-dot-small';
      case 'tiny': return 'timeline-dot-tiny';
    }
  };

  const getColorClass = (color: string) => {
    return `timeline-dot-${color}`;
  };

  return (
    <div className="session-timeline">
      <div className="timeline-track">
        {dots.map((dot, index) => {
          const isHovered = hoveredTurn === dot.turnIndex;
          const isActive = activeTurnIndex === dot.turnIndex;
          const isFirstInTurn = index === 0 || dots[index - 1].turnIndex !== dot.turnIndex;
          
          return (
            <React.Fragment key={dot.id}>
              {/* Add spacing before first dot of new turn */}
              {isFirstInTurn && index > 0 && (
                <div className="timeline-turn-spacer" />
              )}
              
              <button
                className={`timeline-dot ${getSizeClass(dot.size)} ${getColorClass(dot.color)} ${isHovered ? 'hovered' : ''} ${isActive ? 'active' : ''}`}
                style={{ opacity: dot.opacity }}
                onClick={() => handleDotClick(dot.turnIndex)}
                onMouseEnter={() => setHoveredTurn(dot.turnIndex)}
                onMouseLeave={() => setHoveredTurn(null)}
                title={`Turn ${dot.turnIndex + 1}: ${dot.label}`}
                aria-label={`Jump to turn ${dot.turnIndex + 1}: ${dot.type}`}
              />
            </React.Fragment>
          );
        })}
      </div>
    </div>
  );
}
