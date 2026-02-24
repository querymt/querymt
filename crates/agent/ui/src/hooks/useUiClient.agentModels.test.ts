/**
 * Tests for agentModels state management in useUiClient
 * 
 * This test file specifically verifies the fix for the model badge issue:
 * When creating a new session, agentModels should NOT be cleared so the
 * model badge continues to show the last-used model.
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

describe('useUiClient - agentModels tracking', () => {
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

  it('should preserve agentModels when session_created is received (the fix)', async () => {
    const { result } = renderHook(() => useUiClient());

    // Wait for connection
    await act(async () => { await Promise.resolve(); await Promise.resolve(); });
    expect(result.current.connected).toBe(true);

    // Manually set agentModels to simulate existing model state
    // (In real usage, this would come from a previous session's provider_changed event)
    await act(async () => {
      // We need to trigger a provider_changed via session_loaded which sets mainSessionId
      MockWebSocket.instance?.simulateMessage({
        type: 'session_loaded',
        session_id: 'session-1',
        agent_id: 'primary',
        audit: {
          session_id: 'session-1',
          cwd: '/test',
          events: [{
            seq: 1,
            timestamp: Date.now() / 1000,
            kind: { type: 'provider_changed', provider: 'anthropic', model: 'claude-3-5-sonnet-20241022' },
          }],
          delegations: [],
          file_mentions: [],
        },
        undo_stack: [],
        cursor_seq: 1,
      });
    });

    // Verify model was set from session_loaded
    expect(result.current.agentModels['primary']).toBeDefined();
    expect(result.current.agentModels['primary'].model).toBe('claude-3-5-sonnet-20241022');
    
    const modelBeforeNewSession = { ...result.current.agentModels['primary'] };

    // Now create a NEW session - this is what we're testing!
    await act(async () => {
      MockWebSocket.instance?.simulateMessage({
        type: 'session_created',
        agent_id: 'primary', 
        session_id: 'session-2',
      });
    });

    // THE FIX: agentModels should NOT be cleared after session_created
    expect(result.current.sessionId).toBe('session-2');
    expect(result.current.agentModels['primary']).toBeDefined();
    expect(result.current.agentModels['primary']).toEqual(modelBeforeNewSession);
    expect(result.current.agentModels['primary'].model).toBe('claude-3-5-sonnet-20241022');
  });

  it('should update agentModels from session_loaded', async () => {
    const { result } = renderHook(() => useUiClient());

    await act(async () => { await Promise.resolve(); await Promise.resolve(); });
    expect(result.current.connected).toBe(true);

    // Load a session with model info
    await act(async () => {
      MockWebSocket.instance?.simulateMessage({
        type: 'session_loaded',
        session_id: 'session-1',
        agent_id: 'primary',
        audit: {
          session_id: 'session-1',
          cwd: '/test',
          events: [{
            seq: 1,
            timestamp: Date.now() / 1000,
            kind: { type: 'provider_changed', provider: 'openai', model: 'gpt-4-turbo', context_limit: 128000 },
          }],
          delegations: [],
          file_mentions: [],
        },
        undo_stack: [],
        cursor_seq: 1,
      });
    });

    expect(result.current.agentModels['primary']).toBeDefined();
    expect(result.current.agentModels['primary'].provider).toBe('openai');
    expect(result.current.agentModels['primary'].model).toBe('gpt-4-turbo');
    expect(result.current.agentModels['primary'].contextLimit).toBe(128000);
  });

  it('should not append connection-level error messages into the active session timeline', async () => {
    const { result } = renderHook(() => useUiClient());

    await act(async () => { await Promise.resolve(); await Promise.resolve(); });
    expect(result.current.connected).toBe(true);

    await act(async () => {
      MockWebSocket.instance?.simulateMessage({
        type: 'session_loaded',
        session_id: 'session-codex',
        agent_id: 'primary',
        audit: {
          session_id: 'session-codex',
          cwd: '/test',
          events: [{
            seq: 1,
            timestamp: Date.now() / 1000,
            kind: { type: 'provider_changed', provider: 'codex', model: 'gpt-5.3-codex' },
          }],
          delegations: [],
          file_mentions: [],
        },
        undo_stack: [],
        cursor_seq: 1,
      });
    });

    const before = result.current.eventsBySession.get('session-codex') ?? [];

    await act(async () => {
      MockWebSocket.instance?.simulateMessage({
        type: 'error',
        message: 'anthropic auth failed',
      });
    });

    const after = result.current.eventsBySession.get('session-codex') ?? [];
    expect(after).toHaveLength(before.length);
    expect(after.some((event) => event.content.includes('anthropic auth failed'))).toBe(false);
  });
});
