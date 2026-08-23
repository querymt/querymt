import { fireEvent, render, screen } from '@testing-library/react';
import { createRef } from 'react';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { SessionRuntimePhase } from '../types';
import { ChatInputBar } from './ChatInputBar';

vi.mock('../hooks/useVoiceInput', () => ({
  useVoiceInput: () => ({ isRecording: false, isTranscribing: false, toggleRecording: vi.fn() }),
}));
vi.mock('../store/voiceStore', () => ({
  useVoiceStore: () => ({ sttProvider: '', sttModel: '' }),
}));
vi.mock('../context/UiClientContext', () => ({
  useUiClientConfig: () => ({ audioCapabilities: { stt_models: [], tts_models: [] } }),
}));

const baseProps = {
  mentionInputRef: createRef<HTMLTextAreaElement>(),
  prompt: 'Refocus on the failing test',
  setPrompt: vi.fn(),
  handleSendPrompt: vi.fn(),
  cancelSession: vi.fn(),
  sessionId: 'session-1',
  connected: true,
  loading: false,
  isMobile: false,
  sessionThinkingAgentId: 'primary',
  pendingInputs: [],
  rateLimitState: undefined,
  activeIndexStatus: undefined,
  allFiles: [],
  requestIndex: vi.fn(),
  isLoadingFiles: false,
};

const activeRuntime = {
  phase: SessionRuntimePhase.Model,
  active_run_id: 'run-1',
  steerable: true,
  pending_steering_count: 0,
  queued_input_count: 0,
  run_started_at_ms: 1,
};

describe('ChatInputBar steering', () => {
  beforeEach(() => vi.clearAllMocks());

  it('submits steering from Enter and the primary button', () => {
    render(<ChatInputBar {...baseProps} runtimeState={activeRuntime} />);

    const textbox = screen.getByRole('textbox');
    fireEvent.keyDown(textbox, { key: 'Enter', shiftKey: false });
    expect(baseProps.handleSendPrompt).toHaveBeenLastCalledWith('steer');

    fireEvent.click(screen.getByRole('button', { name: 'Steer' }));
    expect(baseProps.handleSendPrompt).toHaveBeenLastCalledWith('steer');
    expect(baseProps.handleSendPrompt).toHaveBeenCalledTimes(2);
  });

  it('resets an explicit steering selection when the run stops', () => {
    const { rerender } = render(<ChatInputBar {...baseProps} runtimeState={activeRuntime} />);

    fireEvent.click(screen.getByRole('button', { name: 'Choose message delivery' }));
    fireEvent.click(screen.getByRole('button', { name: /Steer current run/i }));
    expect(screen.getByRole('button', { name: 'Steer' })).toBeInTheDocument();

    rerender(
      <ChatInputBar
        {...baseProps}
        sessionThinkingAgentId={null}
        runtimeState={{
          phase: SessionRuntimePhase.Idle,
          steerable: false,
          pending_steering_count: 0,
          queued_input_count: 0,
        }}
      />,
    );

    expect(screen.getByRole('button', { name: 'Send' })).toBeInTheDocument();
    fireEvent.keyDown(screen.getByRole('textbox'), { key: 'Enter', shiftKey: false });
    expect(baseProps.handleSendPrompt).toHaveBeenLastCalledWith(undefined);
  });
});
