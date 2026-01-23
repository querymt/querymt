/**
 * Pinned user message component - appears when scrolling past the original message
 * Shows as left side card on wide screens, top bar on narrow screens
 */

import { MessageSquare, ArrowUp } from 'lucide-react';

interface PinnedUserMessageProps {
  message: string;
  timestamp: number;
  onJumpBack: () => void;
}

export function PinnedUserMessage({ 
  message, 
  onJumpBack 
}: PinnedUserMessageProps) {
  // Strip the "Attachments:" section from the message
  // This section is appended by the system and contains [file: ...] or [dir: ...] entries
  const cleanMessage = message.split(/\n\s*Attachments:/)[0].trim();
  
  const truncatedMessage = cleanMessage.length > 150 
    ? cleanMessage.slice(0, 150) + '...' 
    : cleanMessage;

  return (
    <>
      {/* Left side card (wide screens >= 1100px) */}
      <div className="pinned-user-card">
        <div className="pinned-user-card-header">
          <MessageSquare className="w-3 h-3" />
          <span>User</span>
        </div>
        <div className="pinned-user-card-text">
          "{truncatedMessage}"
        </div>
        <button 
          className="pinned-user-card-jump"
          onClick={onJumpBack}
          type="button"
        >
          <ArrowUp className="w-3 h-3" />
          <span>Back</span>
        </button>
      </div>

      {/* Top bar (narrow screens < 1100px) */}
      <div className="pinned-user-bar">
        <MessageSquare className="w-4 h-4 text-cyber-magenta flex-shrink-0" />
        <span className="pinned-user-bar-text">
          "{truncatedMessage}"
        </span>
        <button 
          className="pinned-user-bar-jump"
          onClick={onJumpBack}
          type="button"
        >
          <ArrowUp className="w-4 h-4" />
        </button>
      </div>
    </>
  );
}
