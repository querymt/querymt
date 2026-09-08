import { act, renderHook } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { SessionRuntimePhase, RemoteSessionConnectionState } from '../types';
import { useUiClient } from './useUiClient';

class MockWebSocket {
  static instance: MockWebSocket | null = null;
  static readonly OPEN = 1;
  static readonly CLOSED = 3;

  onopen: ((event: Event) => void) | null = null;
  onclose: ((event: CloseEvent) => void) | null = null;
  onmessage: ((event: MessageEvent) => void) | null = null;
  readyState = MockWebSocket.OPEN;
  sent: string[] = [];

  constructor(public url: string) {
    MockWebSocket.instance = this;
    Promise.resolve().then(() => this.onopen?.(new Event('open')));
  }

  send(data: string) {
    this.sent.push(data);
  }

  close() {
    this.readyState = MockWebSocket.CLOSED;
  }

  simulateMessage(data: unknown) {
    this.onmessage?.(new MessageEvent('message', { data: JSON.stringify(data) }));
  }
}

const OriginalWebSocket = globalThis.WebSocket;

describe('useUiClient steering', () => {
  beforeEach(() => {
    MockWebSocket.instance = null;
    (globalThis as unknown as { WebSocket: typeof MockWebSocket }).WebSocket = MockWebSocket;
  });

  afterEach(() => {
    MockWebSocket.instance?.close();
    globalThis.WebSocket = OriginalWebSocket;
    vi.useRealTimers();
  });

  function eventMessage(sessionId: string, kind: { type: string; data: Record<string, unknown> }) {
    return {
      type: 'event',
      data: {
        session_id: sessionId,
        agent_id: 'primary',
        event: {
          type: 'durable',
          data: {
            seq: 1,
            timestamp: 1,
            session_id: sessionId,
            origin: 'local',
            kind,
          },
        },
      },
    };
  }

  it('uses fresh runtime state from a callback captured before the run started', async () => {
    const { result } = renderHook(() => useUiClient());
    const submitInput = result.current.submitInput;

    await act(async () => {
      await Promise.resolve();
      await Promise.resolve();
    });

    await act(async () => {
      MockWebSocket.instance?.simulateMessage({
        type: 'runtime_state',
        data: {
          session_id: 'session-1',
          state: {
            phase: SessionRuntimePhase.Model,
            active_run_id: 'run-1',
            steerable: true,
            pending_steering_count: 0,
            queued_input_count: 0,
            run_started_at_ms: 42,
          },
        },
      });
    });

    let dispatchResult: ReturnType<typeof submitInput> | undefined;
    await act(async () => {
      dispatchResult = submitInput(
        'steer',
        [{ type: 'text', data: { text: 'Focus on the failing test.' } }],
        'session-1',
      );
    });

    expect(dispatchResult?.accepted).toBe(true);
    const messages = MockWebSocket.instance?.sent.map((value) => JSON.parse(value)) ?? [];
    expect(messages).toContainEqual(expect.objectContaining({
      type: 'submit_input',
      data: expect.objectContaining({
        session_id: 'session-1',
        delivery: 'steer',
        expected_run_id: 'run-1',
      }),
    }));
  });

  it('reconciles replayed accepted and queued lifecycle events by client input id', async () => {
    const { result } = renderHook(() => useUiClient());
    await act(async () => {
      await Promise.resolve();
      await Promise.resolve();
    });

    let queuedInputId = '';
    await act(async () => {
      const dispatch = result.current.submitInput(
        'queue',
        [{ type: 'text', data: { text: 'Run this next.' } }],
        'session-1',
      );
      expect(dispatch.accepted).toBe(true);
      if (dispatch.accepted) queuedInputId = dispatch.inputId;
    });
    expect(result.current.pendingInputsBySession.get('session-1')?.[0]?.state).toBe('sending');

    await act(async () => {
      MockWebSocket.instance?.simulateMessage(eventMessage('session-1', {
        type: 'input_queued',
        data: { input_id: queuedInputId, position: 2 },
      }));
    });
    expect(result.current.pendingInputsBySession.get('session-1')?.[0]).toMatchObject({
      inputId: queuedInputId,
      state: 'queued',
      position: 2,
    });

    // Replay input_queued event to verify idempotent reconciliation
    await act(async () => {
      MockWebSocket.instance?.simulateMessage(eventMessage('session-1', {
        type: 'input_queued',
        data: { input_id: queuedInputId, position: 2 },
      }));
    });
    expect(result.current.pendingInputsBySession.get('session-1')).toHaveLength(1);
    expect(result.current.pendingInputsBySession.get('session-1')?.[0]).toMatchObject({
      inputId: queuedInputId,
      state: 'queued',
      position: 2,
    });

    await act(async () => {
      MockWebSocket.instance?.simulateMessage({
        type: 'runtime_state',
        data: {
          session_id: 'session-1',
          state: {
            phase: SessionRuntimePhase.Model,
            active_run_id: 'run-1',
            steerable: true,
            pending_steering_count: 0,
            queued_input_count: 1,
            run_started_at_ms: 42,
          },
        },
      });
    });

    let steeringInputId = '';
    await act(async () => {
      const dispatch = result.current.submitInput(
        'steer',
        [{ type: 'text', data: { text: 'Focus here.' } }],
        'session-1',
      );
      expect(dispatch.accepted).toBe(true);
      if (dispatch.accepted) steeringInputId = dispatch.inputId;
    });
    await act(async () => {
      MockWebSocket.instance?.simulateMessage(eventMessage('session-1', {
        type: 'steering_accepted',
        data: { run_id: 'run-1', input_id: steeringInputId, position: 1 },
      }));
    });
    expect(result.current.pendingInputsBySession.get('session-1')?.find(
      (item) => item.inputId === steeringInputId,
    )).toMatchObject({ state: 'accepted', position: 1 });

    // Replay steering_accepted event to verify idempotent reconciliation
    await act(async () => {
      MockWebSocket.instance?.simulateMessage(eventMessage('session-1', {
        type: 'steering_accepted',
        data: { run_id: 'run-1', input_id: steeringInputId, position: 1 },
      }));
    });
    expect(result.current.pendingInputsBySession.get('session-1')).toHaveLength(2);
    expect(result.current.pendingInputsBySession.get('session-1')?.find(
      (item) => item.inputId === steeringInputId,
    )).toMatchObject({ state: 'accepted', position: 1 });
  });

  it('refreshes runtime state when a queued input starts', async () => {
    const { result } = renderHook(() => useUiClient());
    await act(async () => {
      await Promise.resolve();
      await Promise.resolve();
    });

    let inputId = '';
    await act(async () => {
      const dispatch = result.current.submitInput(
        'queue',
        [{ type: 'text', data: { text: 'Run this next.' } }],
        'session-1',
      );
      expect(dispatch.accepted).toBe(true);
      if (dispatch.accepted) inputId = dispatch.inputId;
    });
    MockWebSocket.instance!.sent = [];

    await act(async () => {
      MockWebSocket.instance?.simulateMessage(eventMessage('session-1', {
        type: 'queued_input_started',
        data: { input_id: inputId, run_id: 'run-2' },
      }));
    });

    expect(result.current.pendingInputsBySession.get('session-1')).toBeUndefined();
    const messages = MockWebSocket.instance?.sent.map((value) => JSON.parse(value)) ?? [];
    expect(messages).toContainEqual({
      type: 'get_runtime_state',
      data: { session_id: 'session-1' },
    });
  });
  it('waits for a correlated backend receipt and blocks duplicate dispatch', async () => {
    const { result } = renderHook(() => useUiClient());
    await act(async () => { await Promise.resolve(); });
    const prompt = [{ type: 'text' as const, data: { text: 'Keep this draft' } }];
    let dispatch!: ReturnType<typeof result.current.submitInput>;
    act(() => { dispatch = result.current.submitInput('queue', prompt, 's1'); });
    if (!dispatch.accepted) throw new Error('dispatch rejected');
    const acknowledged = vi.fn();
    dispatch.acknowledgement.then(acknowledged);
    await act(async () => { await Promise.resolve(); });
    expect(acknowledged).not.toHaveBeenCalled();
    expect(result.current.submitInput('queue', prompt, 's1')).toEqual({ accepted: false, reason: 'pending_input' });
    await act(async () => {
      MockWebSocket.instance!.simulateMessage({ type: 'input_submitted', data: {
        session_id: 's1', result: { status: 'started', data: { input_id: dispatch.accepted && dispatch.inputId, run_id: 'run' } },
      } });
    });
    expect(acknowledged).toHaveBeenCalledWith(true);
  });

  it('retains failed and ambiguous input past five seconds and reconciles cached events', async () => {
    const { result } = renderHook(() => useUiClient());
    await act(async () => { await Promise.resolve(); });
    vi.useFakeTimers();
    const prompt = [{ type: 'text' as const, data: { text: 'Do not lose me' } }];
    let dispatch!: ReturnType<typeof result.current.submitInput>;
    act(() => { dispatch = result.current.submitInput('queue', prompt, 's1'); });
    if (!dispatch.accepted) throw new Error('dispatch rejected');
    const id = dispatch.inputId;
    await act(async () => {
      MockWebSocket.instance!.simulateMessage({ type: 'input_submission_failed', data: {
        session_id: 's1', client_input_id: id, code: 'submission_outcome_unknown', message: 'Reply lost',
      } });
      vi.advanceTimersByTime(6000);
    });
    expect(await dispatch.acknowledgement).toBe(false);
    expect(result.current.pendingInputsBySession.get('s1')?.[0]).toMatchObject({ state: 'unknown', text: 'Do not lose me', prompt });
    expect(result.current.submitInput('queue', prompt, 's1')).toEqual({ accepted: false, reason: 'pending_input' });
    await act(async () => {
      MockWebSocket.instance!.simulateMessage({ type: 'session_events', data: {
        session_id: 's1', agent_id: 'primary', events: [eventMessage('s1', {
          type: 'queued_input_started', data: { input_id: id, run_id: 'run' },
        }).data.event], cursor: { local_seq: 1, remote_seq_by_source: {} },
      } });
    });
    expect(result.current.pendingInputsBySession.get('s1')).toBeUndefined();
    let failed!: ReturnType<typeof result.current.submitInput>;
    act(() => { failed = result.current.submitInput('queue', prompt, 's1'); });
    if (!failed.accepted) throw new Error('dispatch rejected');
    await act(async () => {
      MockWebSocket.instance!.simulateMessage({ type: 'input_submission_failed', data: {
        session_id: 's1', client_input_id: failed.accepted && failed.inputId, code: 'submission_not_delivered', message: 'Offline',
      } });
      vi.advanceTimersByTime(6000);
    });
    expect(await failed.acknowledgement).toBe(false);
    expect(result.current.pendingInputsBySession.get('s1')?.[0]).toMatchObject({ state: 'failed', text: 'Do not lose me' });
  });

  it('marks a lost acknowledgement unknown on socket close without resending', async () => {
    const { result } = renderHook(() => useUiClient());
    await act(async () => { await Promise.resolve(); });
    let dispatch!: ReturnType<typeof result.current.submitInput>;
    act(() => { dispatch = result.current.submitInput('queue', [{ type: 'text', data: { text: 'Once' } }], 's1'); });
    if (!dispatch.accepted) throw new Error('dispatch rejected');
    act(() => { MockWebSocket.instance!.onclose?.(new CloseEvent('close')); });
    expect(await dispatch.acknowledgement).toBe(false);
    expect(result.current.pendingInputsBySession.get('s1')?.[0].state).toBe('unknown');
    expect(MockWebSocket.instance!.sent.filter(value => JSON.parse(value).type === 'submit_input')).toHaveLength(1);
  });

  it('does not overwrite durable acceptance with a late request failure', async () => {
    const { result } = renderHook(() => useUiClient());
    await act(async () => { await Promise.resolve(); });
    let dispatch!: ReturnType<typeof result.current.submitInput>;
    act(() => { dispatch = result.current.submitInput('queue', [{ type: 'text', data: { text: 'Once' } }], 's1'); });
    if (!dispatch.accepted) throw new Error('dispatch rejected');
    const inputId = dispatch.inputId;
    await act(async () => {
      MockWebSocket.instance!.simulateMessage(eventMessage('s1', {
        type: 'input_queued', data: { input_id: inputId, position: 1 },
      }));
      MockWebSocket.instance!.simulateMessage({ type: 'input_submission_failed', data: {
        session_id: 's1', client_input_id: inputId, code: 'submission_outcome_unknown', message: 'Late failure',
      } });
    });
    expect(await dispatch.acknowledgement).toBe(true);
    expect(result.current.pendingInputsBySession.get('s1')?.[0]).toMatchObject({ state: 'queued', position: 1 });
    expect(result.current.sessionActionNotices).toHaveLength(0);
  });

  it('settles a synchronous send failure without losing or resending the draft', async () => {
    const { result } = renderHook(() => useUiClient());
    await act(async () => { await Promise.resolve(); });
    vi.spyOn(MockWebSocket.instance!, 'send').mockImplementationOnce(() => { throw new Error('Socket failed'); });
    const prompt = [{ type: 'text' as const, data: { text: 'Keep me' } }];
    let dispatch!: ReturnType<typeof result.current.submitInput>;
    act(() => { dispatch = result.current.submitInput('queue', prompt, 's1'); });
    if (!dispatch.accepted) throw new Error('dispatch rejected');
    expect(await dispatch.acknowledgement).toBe(false);
    expect(result.current.pendingInputsBySession.get('s1')?.[0]).toMatchObject({ state: 'unknown', prompt });
    expect(result.current.submitInput('queue', prompt, 's1')).toEqual({ accepted: false, reason: 'pending_input' });
  });

  it('sets Connecting immediately and restores Disconnected on scoped reconnect failure', async () => {
    const { result } = renderHook(() => useUiClient());
    await act(async () => { await Promise.resolve(); });
    act(() => { result.current.attachRemoteSession('peer', 's1'); });
    expect(result.current.sessionConnectionStates.s1).toBe(RemoteSessionConnectionState.Connecting);
    act(() => { MockWebSocket.instance!.simulateMessage({ type: 'error', data: {
      code: 'remote_recovery_failed', session_id: 's1', message: 'Peer unavailable',
    } }); });
    expect(result.current.sessionConnectionStates.s1).toBe(RemoteSessionConnectionState.Disconnected);
    expect(result.current.lastLoadErrorSessionId).toBeNull();
  });

});
