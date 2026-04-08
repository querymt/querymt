/**
 * ChatInputBar - Unified input area with inline send/stop action.
 *
 * Layout: a single rounded container holding the textarea and a compact
 * circular action button on the right edge. The container itself carries
 * the focus border glow so the input and button feel like one element.
 *
 * When STT is available, a microphone button appears next to the send button.
 */

import { type RefObject, useCallback } from 'react';
import { Send, Loader, Square, Mic, MicOff } from 'lucide-react';
import { MentionInput } from './MentionInput';
import type { RateLimitState } from '../types';
import type { FileIndexEntry } from '../generated/types';
import { useVoiceInput } from '../hooks/useVoiceInput';
import { useVoiceStore } from '../store/voiceStore';
import { useUiClientConfig } from '../context/UiClientContext';

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

  const { audioCapabilities } = useUiClientConfig();
  const { sttProvider, sttModel } = useVoiceStore();

  const onTranscribed = useCallback((text: string) => {
    const separator = prompt.length > 0 && !prompt.endsWith(' ') ? ' ' : '';
    setPrompt(prompt + separator + text);
  }, [prompt, setPrompt]);

  const { isRecording, isTranscribing, toggleRecording } = useVoiceInput({
    provider: sttProvider,
    model: sttModel,
    onTranscribed,
  });

  const showMic = audioCapabilities.stt_models.length > 0 && connected && !!sessionId;

  const micButton = showMic ? (
    <button
      onClick={toggleRecording}
      disabled={isTranscribing}
      className={`w-8 h-8 rounded-lg flex items-center justify-center transition-all duration-150 ${
        isRecording
          ? 'bg-status-error/15 text-status-error hover:bg-status-error/25'
          : isTranscribing
            ? 'text-text-secondary opacity-50 cursor-wait'
            : 'text-text-secondary hover:bg-accent-primary/10 hover:text-text-primary'
      }`}
      title={isRecording ? 'Stop recording' : isTranscribing ? 'Transcribing...' : 'Voice input'}
    >
      {isTranscribing ? (
        <Loader className="w-3.5 h-3.5 animate-spin" />
      ) : isRecording ? (
        <MicOff className="w-3.5 h-3.5" />
      ) : (
        <Mic className="w-3.5 h-3.5" />
      )}
    </button>
  ) : null;

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

  const buttons = (
    <div className="flex items-center gap-0.5">
      {micButton}
      {actionButton}
    </div>
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
        actionButton={buttons}
      />
    </div>
  );
}
