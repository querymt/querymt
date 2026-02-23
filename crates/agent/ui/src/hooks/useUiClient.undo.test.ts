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
      stack: [{ turnId: 'turn-5', messageId: 'msg-123', status: 'pending', revertedFiles: [] }],
      frontierMessageId: 'msg-123',
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

    expect(result.current.undoState?.stack[0].revertedFiles).toEqual([]);

    // Simulate success response
    await act(async () => {
      MockWebSocket.instance?.simulateMessage({
        type: 'undo_result',
        success: true,
        message_id: 'msg-123',
        reverted_files: ['a.txt', 'b.txt'],
        undo_stack: [{ message_id: 'msg-123' }],
      });
    });

    expect(result.current.undoState).toEqual({
      stack: [{ turnId: 'turn-5', messageId: 'msg-123', status: 'confirmed', revertedFiles: ['a.txt', 'b.txt'] }],
      frontierMessageId: 'msg-123',
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
        undo_stack: [],
        cursor_seq: 0,
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
        undo_stack: [],
        cursor_seq: 0,
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

    await act(async () => {
      result.current.sendUndo('msg-123', 'turn-5');
    });

    await act(async () => {
      MockWebSocket.instance?.simulateMessage({
        type: 'undo_result',
        success: true,
        message_id: 'msg-123',
        reverted_files: ['a.txt'],
        undo_stack: [{ message_id: 'msg-123' }],
      });
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
        undo_stack: [{ message_id: 'msg-123' }],
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
        undo_stack: [],
        cursor_seq: 0,
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
        undo_stack: [{ message_id: 'msg-123' }],
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
        undo_stack: [{ message_id: 'msg-123' }],
      });
    });

    // State should remain unchanged
    expect(result.current.undoState).toEqual(undoStateBefore);
  });

  it('sendRedo is ignored while undo confirmation is pending', async () => {
    const { result } = renderHook(() => useUiClient());

    await act(async () => {
      await Promise.resolve();
      await Promise.resolve();
    });

    MockWebSocket.instance!.sentMessages = [];

    await act(async () => {
      result.current.sendUndo('msg-123', 'turn-5');
      result.current.sendRedo();
    });

    expect(MockWebSocket.instance!.sentMessages).toEqual([
      {
        type: 'undo',
        message_id: 'msg-123',
      },
    ]);
  });

  it('sendUndo is ignored while undo confirmation is pending', async () => {
    const { result } = renderHook(() => useUiClient());

    await act(async () => {
      await Promise.resolve();
      await Promise.resolve();
    });

    MockWebSocket.instance!.sentMessages = [];

    await act(async () => {
      result.current.sendUndo('msg-123', 'turn-5');
      result.current.sendUndo('msg-122', 'turn-4');
    });

    expect(MockWebSocket.instance!.sentMessages).toEqual([
      {
        type: 'undo',
        message_id: 'msg-123',
      },
    ]);

    expect(result.current.undoState).toEqual({
      stack: [{ turnId: 'turn-5', messageId: 'msg-123', status: 'pending', revertedFiles: [] }],
      frontierMessageId: 'msg-123',
    });
  });

  it('stacked undo confirmations keep per-frame reverted files', async () => {
    const { result } = renderHook(() => useUiClient());

    await act(async () => {
      await Promise.resolve();
      await Promise.resolve();
    });

    await act(async () => {
      result.current.sendUndo('msg-1', 'turn-1');
    });

    await act(async () => {
      MockWebSocket.instance?.simulateMessage({
        type: 'undo_result',
        success: true,
        message_id: 'msg-1',
        reverted_files: ['old.txt'],
        undo_stack: [{ message_id: 'msg-1' }],
      });
    });

    await act(async () => {
      result.current.sendUndo('msg-2', 'turn-2');
    });

    await act(async () => {
      MockWebSocket.instance?.simulateMessage({
        type: 'undo_result',
        success: true,
        message_id: 'msg-2',
        reverted_files: ['new.txt'],
        undo_stack: [{ message_id: 'msg-1' }, { message_id: 'msg-2' }],
      });
    });

    expect(result.current.undoState?.stack).toEqual([
      { turnId: 'turn-1', messageId: 'msg-1', status: 'confirmed', revertedFiles: ['old.txt'] },
      { turnId: 'turn-2', messageId: 'msg-2', status: 'confirmed', revertedFiles: ['new.txt'] },
    ]);
    expect(result.current.undoState?.frontierMessageId).toBe('msg-2');
  });

  it('redo pops only top frame and preserves prior frame file list', async () => {
    const { result } = renderHook(() => useUiClient());

    await act(async () => {
      await Promise.resolve();
      await Promise.resolve();
    });

    await act(async () => {
      result.current.sendUndo('msg-1', 'turn-1');
    });

    await act(async () => {
      MockWebSocket.instance?.simulateMessage({
        type: 'undo_result',
        success: true,
        message_id: 'msg-1',
        reverted_files: ['old.txt'],
        undo_stack: [{ message_id: 'msg-1' }],
      });
    });

    await act(async () => {
      result.current.sendUndo('msg-2', 'turn-2');
    });

    await act(async () => {
      MockWebSocket.instance?.simulateMessage({
        type: 'undo_result',
        success: true,
        message_id: 'msg-2',
        reverted_files: ['new.txt'],
        undo_stack: [{ message_id: 'msg-1' }, { message_id: 'msg-2' }],
      });
    });

    await act(async () => {
      result.current.sendRedo();
      MockWebSocket.instance?.simulateMessage({
        type: 'redo_result',
        success: true,
        undo_stack: [{ message_id: 'msg-1' }],
      });
    });

    expect(result.current.undoState?.stack).toEqual([
      { turnId: 'turn-1', messageId: 'msg-1', status: 'confirmed', revertedFiles: ['old.txt'] },
    ]);
    expect(result.current.undoState?.frontierMessageId).toBe('msg-1');
  });

  // ==================== Test Suite 3a.3: Prompt Branch Commit Cleanup ====================

  it('undoState cleared on prompt_received for main session', async () => {
    const { result } = renderHook(() => useUiClient());

    await act(async () => {
      await Promise.resolve();
      await Promise.resolve();
    });

    // Set main session
    await act(async () => {
      MockWebSocket.instance?.simulateMessage({
        type: 'state',
        routing_mode: 'single',
        active_agent_id: 'primary',
        active_session_id: 'main-session',
        agents: [],
        sessions_by_agent: {},
        agent_mode: 'build',
      });
    });

    // Build stacked undo state with two frames
    await act(async () => {
      result.current.sendUndo('msg-1', 'turn-1');
    });

    await act(async () => {
      MockWebSocket.instance?.simulateMessage({
        type: 'undo_result',
        success: true,
        message_id: 'msg-1',
        reverted_files: ['old.txt'],
        undo_stack: [{ message_id: 'msg-1' }],
      });
    });

    await act(async () => {
      result.current.sendUndo('msg-2', 'turn-2');
    });

    await act(async () => {
      MockWebSocket.instance?.simulateMessage({
        type: 'undo_result',
        success: true,
        message_id: 'msg-2',
        reverted_files: ['new.txt'],
        undo_stack: [{ message_id: 'msg-1' }, { message_id: 'msg-2' }],
      });
    });

    expect(result.current.undoState?.stack.length).toBe(2);

    // A new prompt commits the branch and invalidates redo stack
    await act(async () => {
      MockWebSocket.instance?.simulateMessage({
        type: 'event',
        session_id: 'main-session',
        agent_id: 'primary',
        event: {
          seq: 1,
          timestamp: 1,
          kind: {
            type: 'prompt_received',
            content: 'new prompt',
            message_id: 'msg-new',
          },
        },
      });
    });

    expect(result.current.undoState).toBeNull();
  });

  // ==================== Test Suite 3a.4: Session Switch Cleanup (Bug Fix Validation) ====================

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
        undo_stack: [{ message_id: 'msg-123' }],
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
        undo_stack: [{ message_id: 'msg-123' }],
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
        undo_stack: [],
        cursor_seq: 0,
      });
    });

    // Empty backend stack should clear prior local undo state
    expect(result.current.undoState).toBeNull();
  });

  it('hydrates undoState from session_loaded undo_stack', async () => {
    const { result } = renderHook(() => useUiClient());

    await act(async () => {
      await Promise.resolve();
      await Promise.resolve();
    });

    await act(async () => {
      MockWebSocket.instance?.simulateMessage({
        type: 'session_loaded',
        session_id: 'session-hydrated',
        agent_id: 'primary',
        audit: {
          session_id: 'session-hydrated',
          cwd: '/test',
          events: [],
          delegations: [],
          file_mentions: [],
        },
        undo_stack: [
          { message_id: 'msg-1' },
          { message_id: 'msg-2' },
        ],
      });
    });

    expect(result.current.undoState).toEqual({
      stack: [
        { turnId: 'msg-1', messageId: 'msg-1', status: 'confirmed', revertedFiles: [] },
        { turnId: 'msg-2', messageId: 'msg-2', status: 'confirmed', revertedFiles: [] },
      ],
      frontierMessageId: 'msg-2',
    });
  });

  it('merges thinking deltas and replaces with final assistant message', async () => {
    const { result } = renderHook(() => useUiClient());

    await act(async () => {
      await Promise.resolve();
      await Promise.resolve();
    });

    await act(async () => {
      MockWebSocket.instance?.simulateMessage({
        type: 'session_created',
        session_id: 'session-thinking',
        agent_id: 'primary',
      });
    });

    await act(async () => {
      MockWebSocket.instance?.simulateMessage({
        type: 'event',
        session_id: 'session-thinking',
        agent_id: 'primary',
        event: {
          seq: 1,
          timestamp: 1,
          kind: {
            type: 'assistant_thinking_delta',
            content: 'Plan: ',
            message_id: 'm-1',
          },
        },
      });
      MockWebSocket.instance?.simulateMessage({
        type: 'event',
        session_id: 'session-thinking',
        agent_id: 'primary',
        event: {
          seq: 2,
          timestamp: 2,
          kind: {
            type: 'assistant_thinking_delta',
            content: 'step by step',
            message_id: 'm-1',
          },
        },
      });
      MockWebSocket.instance?.simulateMessage({
        type: 'event',
        session_id: 'session-thinking',
        agent_id: 'primary',
        event: {
          seq: 3,
          timestamp: 3,
          kind: {
            type: 'assistant_message_stored',
            content: 'Final answer',
            message_id: 'm-1',
          },
        },
      });
    });

    const rows = result.current.eventsBySession.get('session-thinking') ?? [];
    expect(rows).toHaveLength(1);
    expect(rows[0].content).toBe('Final answer');
    expect(rows[0].thinking).toBe('Plan: step by step');
    expect(rows[0].isStreamDelta).toBeUndefined();
  });

  it('shows resumed prompt immediately after session_loaded without reload', async () => {
    const { result } = renderHook(() => useUiClient());

    await act(async () => {
      await Promise.resolve();
      await Promise.resolve();
    });

    await act(async () => {
      MockWebSocket.instance?.simulateMessage({
        type: 'session_loaded',
        session_id: 'rehydrated-session',
        agent_id: 'primary',
        audit: {
          session_id: 'rehydrated-session',
          cwd: '/test',
          events: [
            {
              seq: 10,
              timestamp: 10,
              kind: {
                type: 'assistant_message_stored',
                content: 'Older assistant message',
                message_id: 'assistant-10',
              },
            },
          ],
          delegations: [],
          file_mentions: [],
        },
        undo_stack: [],
        cursor_seq: 10,
      });
    });

    await act(async () => {
      MockWebSocket.instance?.simulateMessage({
        type: 'event',
        session_id: 'rehydrated-session',
        agent_id: 'primary',
        event: {
          seq: 11,
          timestamp: 11,
          kind: {
            type: 'prompt_received',
            content: 'resumed prompt',
            message_id: 'prompt-11',
          },
        },
      });
    });

    const rows = result.current.eventsBySession.get('rehydrated-session') ?? [];
    expect(rows.some((row) => row.type === 'user' && row.content === 'resumed prompt')).toBe(true);
  });

  it('replaces live accumulator even when final message_id is missing', async () => {
    const { result } = renderHook(() => useUiClient());

    await act(async () => {
      await Promise.resolve();
      await Promise.resolve();
    });

    await act(async () => {
      MockWebSocket.instance?.simulateMessage({
        type: 'session_created',
        session_id: 'session-thinking-2',
        agent_id: 'primary',
      });
    });

    await act(async () => {
      MockWebSocket.instance?.simulateMessage({
        type: 'event',
        session_id: 'session-thinking-2',
        agent_id: 'primary',
        event: {
          seq: 1,
          timestamp: 1,
          kind: {
            type: 'assistant_thinking_delta',
            content: 'hidden rationale',
            message_id: 'stream-only-id',
          },
        },
      });
      MockWebSocket.instance?.simulateMessage({
        type: 'event',
        session_id: 'session-thinking-2',
        agent_id: 'primary',
        event: {
          seq: 2,
          timestamp: 2,
          kind: {
            type: 'assistant_message_stored',
            content: 'Final without message id',
          },
        },
      });
    });

    const rows = result.current.eventsBySession.get('session-thinking-2') ?? [];
    expect(rows).toHaveLength(1);
    expect(rows[0].content).toBe('Final without message id');
    expect(rows[0].thinking).toBe('hidden rationale');
    expect(rows[0].isStreamDelta).toBeUndefined();
  });
});
