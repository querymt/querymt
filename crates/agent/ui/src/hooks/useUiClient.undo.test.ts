/**
 * Tests for undo/redo functionality in useUiClient
 * 
 * This test file validates:
 * - WebSocket message handling for undo/redo
 * - undoState management and cleanup
 * - Session switch cleanup (Bug #6 fix validation)
 */

import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';
import { renderHook, act } from '@testing-library/react';
import { useUiClient } from './useUiClient';

// Mock WebSocket
class MockWebSocket {
  static instance: MockWebSocket | null = null;
  sentMessages: any[] = [];
  
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
  
  send(data: string) {
    this.sentMessages.push(JSON.parse(data));
  }
  
  close() {
    this.readyState = WebSocket.CLOSED;
    this.onclose?.(new CloseEvent('close'));
  }
  
  simulateMessage(data: any) {
    this.onmessage?.(new MessageEvent('message', { data: JSON.stringify(data) }));
  }
}

const OriginalWebSocket = globalThis.WebSocket;

describe('useUiClient - undo/redo', () => {
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

  // ==================== Test Suite 3a.1: Undo WebSocket Flow ====================

  it('sendUndo sends correct WS message', async () => {
    const { result } = renderHook(() => useUiClient());

    // Wait for connection
    await act(async () => { 
      await Promise.resolve(); 
      await Promise.resolve(); 
    });
    expect(result.current.connected).toBe(true);

    // Clear sent messages
    MockWebSocket.instance!.sentMessages = [];

    // Send undo
    await act(async () => {
      result.current.sendUndo('msg-123', 'turn-5');
    });

    // Verify message sent
    expect(MockWebSocket.instance!.sentMessages).toHaveLength(1);
    expect(MockWebSocket.instance!.sentMessages[0]).toEqual({
      type: 'undo',
      message_id: 'msg-123',
    });
  });

  it('sendUndo sets optimistic undoState', async () => {
    const { result } = renderHook(() => useUiClient());

    await act(async () => { 
      await Promise.resolve(); 
      await Promise.resolve(); 
    });

    expect(result.current.undoState).toBeNull();

    await act(async () => {
      result.current.sendUndo('msg-123', 'turn-5');
    });

    expect(result.current.undoState).toEqual({
      turnId: 'turn-5',
      revertedFiles: [],
    });
  });

  it('undo_result success updates revertedFiles', async () => {
    const { result } = renderHook(() => useUiClient());

    await act(async () => { 
      await Promise.resolve(); 
      await Promise.resolve(); 
    });

    // Send undo (sets optimistic state)
    await act(async () => {
      result.current.sendUndo('msg-123', 'turn-5');
    });

    expect(result.current.undoState?.revertedFiles).toEqual([]);

    // Simulate success response
    await act(async () => {
      MockWebSocket.instance?.simulateMessage({
        type: 'undo_result',
        success: true,
        reverted_files: ['a.txt', 'b.txt'],
      });
    });

    expect(result.current.undoState).toEqual({
      turnId: 'turn-5',
      revertedFiles: ['a.txt', 'b.txt'],
    });
  });

  it('undo_result failure clears undoState', async () => {
    const { result } = renderHook(() => useUiClient());

    await act(async () => { 
      await Promise.resolve(); 
      await Promise.resolve(); 
    });

    // Send undo
    await act(async () => {
      result.current.sendUndo('msg-123', 'turn-5');
    });

    expect(result.current.undoState).not.toBeNull();

    // Simulate failure response
    await act(async () => {
      MockWebSocket.instance?.simulateMessage({
        type: 'undo_result',
        success: false,
        error: 'Snapshot not found',
      });
    });

    expect(result.current.undoState).toBeNull();
  });

  it('undo_result success with no prior state does not crash', async () => {
    const { result } = renderHook(() => useUiClient());

    await act(async () => { 
      await Promise.resolve(); 
      await Promise.resolve(); 
    });

    // No undo was sent, undoState is null
    expect(result.current.undoState).toBeNull();

    // Simulate success response (race condition scenario)
    await act(async () => {
      MockWebSocket.instance?.simulateMessage({
        type: 'undo_result',
        success: true,
        reverted_files: ['a.txt'],
      });
    });

    // Should remain null (no crash, guard against race)
    expect(result.current.undoState).toBeNull();
  });

  // ==================== Test Suite 3a.2: Redo WebSocket Flow ====================

  it('sendRedo sends correct WS message', async () => {
    const { result } = renderHook(() => useUiClient());

    await act(async () => { 
      await Promise.resolve(); 
      await Promise.resolve(); 
    });

    MockWebSocket.instance!.sentMessages = [];

    await act(async () => {
      result.current.sendRedo();
    });

    expect(MockWebSocket.instance!.sentMessages).toHaveLength(1);
    expect(MockWebSocket.instance!.sentMessages[0]).toEqual({
      type: 'redo',
    });
  });

  it('redo_result success clears undoState', async () => {
    const { result } = renderHook(() => useUiClient());

    await act(async () => { 
      await Promise.resolve(); 
      await Promise.resolve(); 
    });

    // Set undo state first
    await act(async () => {
      result.current.sendUndo('msg-123', 'turn-5');
    });

    await act(async () => {
      MockWebSocket.instance?.simulateMessage({
        type: 'undo_result',
        success: true,
        reverted_files: ['a.txt'],
      });
    });

    expect(result.current.undoState).not.toBeNull();

    // Send redo and simulate success
    await act(async () => {
      result.current.sendRedo();
    });

    await act(async () => {
      MockWebSocket.instance?.simulateMessage({
        type: 'redo_result',
        success: true,
      });
    });

    expect(result.current.undoState).toBeNull();
  });

  it('redo_result failure keeps undoState', async () => {
    const { result } = renderHook(() => useUiClient());

    await act(async () => { 
      await Promise.resolve(); 
      await Promise.resolve(); 
    });

    // Set undo state
    await act(async () => {
      result.current.sendUndo('msg-123', 'turn-5');
    });

    await act(async () => {
      MockWebSocket.instance?.simulateMessage({
        type: 'undo_result',
        success: true,
        reverted_files: ['a.txt'],
      });
    });

    const undoStateBefore = { ...result.current.undoState! };

    // Simulate redo failure
    await act(async () => {
      result.current.sendRedo();
    });

    await act(async () => {
      MockWebSocket.instance?.simulateMessage({
        type: 'redo_result',
        success: false,
        error: 'No redo available',
      });
    });

    // State should remain unchanged
    expect(result.current.undoState).toEqual(undoStateBefore);
  });

  // ==================== Test Suite 3a.3: Session Switch Cleanup (Bug Fix Validation) ====================

  it('undoState cleared on session_created', async () => {
    const { result } = renderHook(() => useUiClient());

    await act(async () => { 
      await Promise.resolve(); 
      await Promise.resolve(); 
    });

    // Set undo state
    await act(async () => {
      result.current.sendUndo('msg-123', 'turn-3');
    });

    await act(async () => {
      MockWebSocket.instance?.simulateMessage({
        type: 'undo_result',
        success: true,
        reverted_files: ['a.txt'],
      });
    });

    expect(result.current.undoState).not.toBeNull();

    // Switch session
    await act(async () => {
      MockWebSocket.instance?.simulateMessage({
        type: 'session_created',
        session_id: 'new-session',
        agent_id: 'primary',
      });
    });

    // Bug #6 fix: undoState should be cleared
    expect(result.current.undoState).toBeNull();
  });

  it('undoState cleared on session_loaded', async () => {
    const { result } = renderHook(() => useUiClient());

    await act(async () => { 
      await Promise.resolve(); 
      await Promise.resolve(); 
    });

    // Set undo state
    await act(async () => {
      result.current.sendUndo('msg-123', 'turn-3');
    });

    await act(async () => {
      MockWebSocket.instance?.simulateMessage({
        type: 'undo_result',
        success: true,
        reverted_files: ['a.txt'],
      });
    });

    expect(result.current.undoState).not.toBeNull();

    // Load different session
    await act(async () => {
      MockWebSocket.instance?.simulateMessage({
        type: 'session_loaded',
        session_id: 'another-session',
        agent_id: 'primary',
        audit: {
          session_id: 'another-session',
          cwd: '/test',
          events: [],
          delegations: [],
          file_mentions: [],
        },
      });
    });

    // Bug #6 fix: undoState should be cleared
    expect(result.current.undoState).toBeNull();
  });
});
