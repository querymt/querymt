import { act, renderHook } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { SessionRuntimePhase } from '../types';
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
});
