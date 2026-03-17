/**
 * Tests for turn_started event handling in useUiClient.
 *
 * Verifies that receiving a turn_started event:
 *   1. Immediately sets thinkingBySession (so the UI shows "thinking..." / "Working...")
 *   2. Does NOT append the event to eventsBySession (so it never appears in the SystemLog)
 */

import { describe, it, expect, beforeEach, afterEach } from 'vitest';
import { renderHook, act } from '@testing-library/react';
import { useUiClient } from './useUiClient';

// Mock WebSocket
class MockWebSocket {
  static instance: MockWebSocket | null = null;

  onopen: ((event: Event) => void) | null = null;
  onclose: ((event: CloseEvent) => void) | null = null;
  onmessage: ((event: MessageEvent) => void) | null = null;

  readyState = WebSocket.OPEN;

  constructor(public url: string) {
    MockWebSocket.instance = this;
    Promise.resolve().then(() => {
      this.onopen?.(new Event('open'));
    });
  }

  send(_data: string) {}

  close() {
    this.readyState = WebSocket.CLOSED;
    this.onclose?.(new CloseEvent('close'));
  }

  simulateMessage(data: any) {
    this.onmessage?.(new MessageEvent('message', { data: JSON.stringify(data) }));
  }
}

const OriginalWebSocket = globalThis.WebSocket;

/** Helper: simulate a durable agent event envelope */
function makeEventMessage(sessionId: string, agentId: string, kind: any, seq = 1) {
  return {
    type: 'event',
    data: {
      session_id: sessionId,
      agent_id: agentId,
      event: {
        type: 'durable',
        data: {
          seq,
          timestamp: 1000,
          session_id: sessionId,
          origin: 'local',
          kind,
        },
      },
    },
  };
}

describe('useUiClient - turn_started event', () => {
  beforeEach(() => {
    MockWebSocket.instance = null;
    (globalThis as any).WebSocket = MockWebSocket;
    Object.defineProperty(window, 'location', {
      value: { protocol: 'http:', host: 'localhost:3000' },
      writable: true,
    });
  });

  afterEach(() => {
    MockWebSocket.instance?.close();
    (globalThis as any).WebSocket = OriginalWebSocket;
  });

  it('sets thinkingBySession when turn_started is received', async () => {
    const { result } = renderHook(() => useUiClient());

    // Wait for connection
    await act(async () => { await Promise.resolve(); await Promise.resolve(); });
    expect(result.current.connected).toBe(true);

    // Establish session
    await act(async () => {
      MockWebSocket.instance?.simulateMessage({
        type: 'session_created',
        data: { agent_id: 'primary', session_id: 'session-1' },
      });
    });

    // Simulate turn_started
    await act(async () => {
      MockWebSocket.instance?.simulateMessage(
        makeEventMessage('session-1', 'primary', { type: 'turn_started' })
      );
    });

    const thinking = result.current.thinkingBySession.get('session-1');
    expect(thinking).toBeDefined();
    expect(thinking?.has('primary')).toBe(true);
  });

  it('does NOT append turn_started to eventsBySession', async () => {
    const { result } = renderHook(() => useUiClient());

    await act(async () => { await Promise.resolve(); await Promise.resolve(); });

    // Establish session
    await act(async () => {
      MockWebSocket.instance?.simulateMessage({
        type: 'session_created',
        data: { agent_id: 'primary', session_id: 'session-1' },
      });
    });

    // Simulate turn_started
    await act(async () => {
      MockWebSocket.instance?.simulateMessage(
        makeEventMessage('session-1', 'primary', { type: 'turn_started' })
      );
    });

    const events = result.current.eventsBySession.get('session-1') ?? [];
    const hasTurnStarted = events.some(
      (e) => e.content === 'Event: turn_started' || (e as any).kind?.type === 'turn_started'
    );
    expect(hasTurnStarted).toBe(false);
  });
});
