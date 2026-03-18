/**
 * Pinned user message component - appears when scrolling past the original message
 * Shows as sticky top bar
 */

import { MessageSquare, ArrowUp } from 'lucide-react';

interface PinnedUserMessageProps {
  message: string;
  timestamp: number;
  onJumpBack: () => void;
  visible: boolean;
}

export function PinnedUserMessage({ 
  message, 
  onJumpBack,
  visible,
}: PinnedUserMessageProps) {
  const cleanMessage = message.trim();

  const truncatedMessage = cleanMessage.length > 150
    ? cleanMessage.slice(0, 150) + '...'
    : cleanMessage;

  return (
    <div className={`pinned-user-bar-container ${visible ? 'pinned-visible' : ''}`}>
      <div className="pinned-user-bar">
        <MessageSquare className="w-4 h-4 text-accent-secondary flex-shrink-0" />
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
