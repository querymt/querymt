/**
 * ChatInputBar - Unified input area with inline send/stop action.
 *
 * Layout: a single rounded container holding the textarea and a compact
 * circular action button on the right edge. The container itself carries
 * the focus border glow so the input and button feel like one element.
 *
 * When STT is available, a microphone button appears next to the send button.
 */

import { type RefObject, useCallback, useState } from 'react';
import { Send, Loader, Square, Mic, MicOff, CornerDownRight, Clock3, ChevronDown } from 'lucide-react';
import { MentionInput } from './MentionInput';
import type { RateLimitState, SessionRuntimeStatus } from '../types';
import type { PendingSessionInput } from '../hooks/useUiClient';
import type { FileIndexEntry } from '../generated/types';
import { useVoiceInput } from '../hooks/useVoiceInput';
import { useVoiceStore } from '../store/voiceStore';
import { useUiClientConfig } from '../context/UiClientContext';

interface ChatInputBarProps {
  mentionInputRef: RefObject<HTMLTextAreaElement | null>;
  prompt: string;
  setPrompt: (value: string) => void;
  handleSendPrompt: (delivery?: 'steer' | 'queue') => void;
  cancelSession: () => void;
  sessionId: string | null;
  connected: boolean;
  loading: boolean;
  isMobile: boolean;
  sessionThinkingAgentId: string | null;
  runtimeState?: SessionRuntimeStatus;
  pendingInputs: PendingSessionInput[];
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
  runtimeState,
  pendingInputs,
  rateLimitState,
  activeIndexStatus,
  allFiles,
  requestIndex,
  isLoadingFiles,
}: ChatInputBarProps) {
  const isThinking = sessionThinkingAgentId !== null || Boolean(runtimeState?.active_run_id);
  const defaultDelivery: 'steer' | 'queue' | undefined = runtimeState?.steerable
    ? 'steer'
    : runtimeState?.active_run_id
      ? 'queue'
      : undefined;
  const [deliveryOverride, setDeliveryOverride] = useState<'steer' | 'queue' | undefined>();
  const [menuOpen, setMenuOpen] = useState(false);
  const delivery = deliveryOverride ?? defaultDelivery;
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

  const actionButton = (
    <div className="relative flex items-center gap-1">
      <button
        onClick={() => handleSendPrompt(delivery)}
        disabled={!canSend}
        className="h-8 px-2.5 rounded-lg flex items-center gap-1.5 text-xs font-medium transition-all duration-150 hover:bg-accent-primary/10 disabled:opacity-20 disabled:cursor-not-allowed"
        style={{ color: 'var(--mode-color)' }}
        title={delivery === 'steer' ? 'Steer the active run' : delivery === 'queue' ? 'Queue as the next turn' : 'Send message'}
      >
        {loading ? <Loader className="w-3.5 h-3.5 animate-spin" /> : delivery === 'queue' ? <Clock3 className="w-3.5 h-3.5" /> : delivery === 'steer' ? <CornerDownRight className="w-3.5 h-3.5" /> : <Send className="w-3.5 h-3.5" />}
        {!isMobile && <span>{delivery === 'steer' ? 'Steer' : delivery === 'queue' ? 'Queue' : 'Send'}</span>}
      </button>
      {isThinking && (
        <>
          <button type="button" onClick={() => setMenuOpen((open) => !open)} className="w-7 h-8 rounded-lg flex items-center justify-center text-text-secondary hover:bg-accent-primary/10" aria-label="Choose message delivery">
            <ChevronDown className="w-3.5 h-3.5" />
          </button>
          {menuOpen && (
            <div className="absolute bottom-10 right-8 z-50 min-w-52 rounded-xl border border-surface-border bg-surface-elevated p-1 shadow-xl">
              {runtimeState?.steerable && (
                <button type="button" className="w-full rounded-lg px-3 py-2 text-left text-xs hover:bg-accent-primary/10" onClick={() => { setDeliveryOverride('steer'); setMenuOpen(false); }}>
                  <span className="font-medium text-text-primary">Steer current run</span><span className="block text-text-secondary mt-0.5">Apply at the next safe boundary</span>
                </button>
              )}
              <button type="button" className="w-full rounded-lg px-3 py-2 text-left text-xs hover:bg-accent-primary/10" onClick={() => { setDeliveryOverride('queue'); setMenuOpen(false); }}>
                <span className="font-medium text-text-primary">Queue next turn</span><span className="block text-text-secondary mt-0.5">Run after the current turn finishes</span>
              </button>
            </div>
          )}
          <button onClick={cancelSession} className="w-8 h-8 rounded-lg flex items-center justify-center bg-status-warning/15 text-status-warning hover:bg-status-warning/25" title="Stop current run"><Square className="w-3.5 h-3.5" /></button>
        </>
      )}
    </div>
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
      {runtimeState?.active_run_id && (
        <div className="mb-2 flex items-center justify-between gap-3 text-[11px] text-text-secondary">
          <span>{runtimeState.phase === 'tools' ? 'Running tools - steering applies after this batch' : runtimeState.phase === 'waiting' ? 'Waiting - steering will resume the run' : runtimeState.phase === 'closing' ? 'Finishing - new messages will be queued' : 'Working - send a correction or queue the next turn'}</span>
          <span className="shrink-0">{runtimeState.pending_steering_count} steering · {runtimeState.queued_input_count} queued</span>
        </div>
      )}
      {pendingInputs.some((item) => !['applied', 'started'].includes(item.state)) && (
        <div className="mb-2 flex flex-wrap gap-1.5" aria-label="Pending inputs">
          {pendingInputs.filter((item) => !['applied', 'started'].includes(item.state)).map((item) => (
            <span key={item.inputId} className="max-w-full truncate rounded-md border border-surface-border bg-surface-canvas/40 px-2 py-1 text-[11px] text-text-secondary" title={item.text}>
              {item.delivery === 'steer' ? 'Steering' : `Queued${item.position ? ` #${item.position}` : ''}`}: {item.text || '(attachment)'}
            </span>
          ))}
        </div>
      )}
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
