/**
 * ChatInputBar - Unified input area with inline send/stop action.
 *
 * Layout: a single rounded container holding the textarea and a compact
 * circular action button on the right edge. The container itself carries
 * the focus border glow so the input and button feel like one element.
 */

import { type RefObject } from 'react';
import { Send, Loader, Square } from 'lucide-react';
import { MentionInput } from './MentionInput';
import type { RateLimitState } from '../types';
import type { FileIndexEntry } from '../generated/types';

interface ChatInputBarProps {
  mentionInputRef: RefObject<HTMLTextAreaElement | null>;
  prompt: string;
  setPrompt: (value: string) => void;
  handleSendPrompt: () => void;
  cancelSession: () => void;
  sessionId: string | null;
  connected: boolean;
  loading: boolean;
  isMobile: boolean;
  sessionThinkingAgentId: string | null;
  rateLimitState: RateLimitState | undefined;
  activeIndexStatus: string | undefined;
  // File mention
  allFiles: FileIndexEntry[];
  requestIndex: () => void;
  isLoadingFiles: boolean;
}

export function ChatInputBar({
  mentionInputRef,
  prompt,
  setPrompt,
  handleSendPrompt,
  cancelSession,
  sessionId,
  connected,
  loading,
  isMobile,
  sessionThinkingAgentId,
  rateLimitState,
  activeIndexStatus,
  allFiles,
  requestIndex,
  isLoadingFiles,
}: ChatInputBarProps) {
  const isThinking = sessionThinkingAgentId !== null;
  const canSend = !loading && connected && !!sessionId && !!prompt.trim() && !rateLimitState?.isRateLimited;

  const actionButton = isThinking ? (
    <button
      onClick={cancelSession}
      className="w-8 h-8 rounded-lg flex items-center justify-center transition-all duration-150 bg-status-warning/15 text-status-warning hover:bg-status-warning/25"
      title="Stop generation (Esc Esc)"
    >
      <Square className="w-3.5 h-3.5" />
    </button>
  ) : (
    <button
      onClick={handleSendPrompt}
      disabled={!canSend}
      className="w-8 h-8 rounded-lg flex items-center justify-center transition-all duration-150 hover:bg-accent-primary/10 disabled:opacity-20 disabled:cursor-not-allowed"
      style={{ color: 'var(--mode-color)' }}
      title="Send message"
    >
      {loading ? (
        <Loader className="w-3.5 h-3.5 animate-spin" />
      ) : (
        <Send className="w-3.5 h-3.5" />
      )}
    </button>
  );

  return (
    <div
      className="px-3 md:px-6 py-3 bg-surface-elevated border-t border-surface-border"
      style={{ paddingBottom: `max(12px, env(safe-area-inset-bottom, 12px))` }}
    >
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
              : isMobile ? "Enter your prompt..." : "Enter your prompt... (@ to mention files)"
        }
        disabled={loading || !connected || !sessionId || rateLimitState?.isRateLimited}
        files={allFiles}
        onRequestFiles={requestIndex}
        isLoadingFiles={isLoadingFiles}
        showIndexBuilding={activeIndexStatus === 'building'}
        actionButton={actionButton}
      />
    </div>
  );
}
