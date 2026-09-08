import { act, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { useUiStore } from '../store/uiStore';
import { ChatView } from './ChatView';

const mocks = vi.hoisted(() => {
  const noop = vi.fn();
  return {
    submitInput: vi.fn(),
    noop,
    session: {
      sessionId: 's1', connected: true, reconnecting: false, agents: [], sessionGroups: [],
      thinkingBySession: new Map(), runtimeBySession: new Map(), pendingInputsBySession: new Map(),
      sessionParentMap: new Map(), sessionChildrenLoading: new Set(), workspaceIndexStatus: {},
      llmConfigCache: {}, schedules: [], loadedSessionNodeIds: {}, sessionConnectionStates: {},
      sessionTotalCount: 0, sessionPageLoading: false, undoState: null,
    },
    events: { events: [], eventsBySession: new Map(), mainSessionId: 's1' },
  };
});
vi.mock('../context/UiClientContext', () => ({
  useUiClientActions: () => new Proxy({ submitInput: mocks.submitInput }, {
    get: (target, key) => key === 'submitInput' ? target.submitInput : mocks.noop,
  }),
  useUiClientEvents: () => mocks.events,
  useUiClientSession: () => mocks.session,
  useUiClientConfig: () => ({ audioCapabilities: { stt_models: [], tts_models: [] } }),
}));
vi.mock('../hooks/useSessionManager', () => ({
  useSessionManager: () => ({ selectSession: mocks.noop, createSession: mocks.noop, goHome: mocks.noop }),
}));
vi.mock('../hooks/useVoiceOutput', () => ({ useVoiceOutput: () => ({ speak: mocks.noop }) }));
vi.mock('../hooks/useVoiceInput', () => ({
  useVoiceInput: () => ({ isRecording: false, isTranscribing: false, toggleRecording: mocks.noop }),
}));
vi.mock('../hooks/useFileMention', () => ({
  useFileMention: () => ({
    allFiles: [], isLoading: false, requestIndex: mocks.noop, clear: mocks.noop,
    resetIndex: mocks.noop, handleFileIndex: mocks.noop, handleFileIndexError: mocks.noop,
  }),
}));

describe('ChatView acknowledged submission', () => {
  beforeEach(() => {
    vi.clearAllMocks();
    mocks.session.sessionId = 's1';
    useUiStore.setState({ prompt: 'Keep my draft', loading: false, activeTimelineView: 'chat' });
  });

  it.each([true, false])('clears the draft only for an accepted backend result (%s)', async (accepted) => {
    let acknowledge!: (accepted: boolean) => void;
    const acknowledgement = new Promise<boolean>(resolve => { acknowledge = resolve; });
    mocks.submitInput.mockReturnValue({ accepted: true, inputId: 'input', acknowledgement });
    render(<ChatView />);
    fireEvent.click(screen.getByRole('button', { name: 'Send' }));
    expect(mocks.submitInput).toHaveBeenCalledTimes(1);
    expect(useUiStore.getState().prompt).toBe('Keep my draft');
    expect(useUiStore.getState().loading).toBe(true);
    await act(async () => { acknowledge(accepted); });
    await waitFor(() => expect(useUiStore.getState().loading).toBe(false));
    expect(useUiStore.getState().prompt).toBe(accepted ? '' : 'Keep my draft');
  });

  it('does not clear edits made while acknowledgement is pending', async () => {
    let acknowledge!: (accepted: boolean) => void;
    mocks.submitInput.mockReturnValue({ accepted: true, inputId: 'input',
      acknowledgement: new Promise<boolean>(resolve => { acknowledge = resolve; }) });
    render(<ChatView />);
    fireEvent.click(screen.getByRole('button', { name: 'Send' }));
    act(() => { useUiStore.getState().setPrompt('A different draft'); });
    await act(async () => { acknowledge(true); });
    expect(useUiStore.getState().prompt).toBe('A different draft');
  });

  it('does not clear another session draft after navigation', async () => {
    let acknowledge!: (accepted: boolean) => void;
    mocks.submitInput.mockReturnValue({ accepted: true, inputId: 'input',
      acknowledgement: new Promise<boolean>(resolve => { acknowledge = resolve; }) });
    const { rerender } = render(<ChatView />);
    fireEvent.click(screen.getByRole('button', { name: 'Send' }));
    mocks.session.sessionId = 's2';
    rerender(<ChatView />);
    await act(async () => { acknowledge(true); });
    expect(useUiStore.getState().prompt).toBe('Keep my draft');
  });
});
