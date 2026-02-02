/**
 * Pinned user message component - appears when scrolling past the original message
 * Shows as sticky top bar
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
    <div className="pinned-user-bar-container">
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
    </div>
  );
}
