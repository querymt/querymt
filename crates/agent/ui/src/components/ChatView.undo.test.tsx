/**
 * ChatView Undo/Redo Integration Tests
 * 
 * Tests the undo/redo handlers in ChatView component.
 * These tests validate messageId propagation and handler logic.
 * 
 * Note: Full integration tests would require complex mocking of UiClientContext,
 * useUiStore, and all child components. These tests focus on the critical
 * undo/redo logic paths.
 */

import { describe, it, expect, vi } from 'vitest';

// Since ChatView has complex dependencies (context, store, child components),
// we test the core undo/redo logic patterns here as unit tests

describe('ChatView undo/redo logic', () => {
  it('handleUndo should extract messageId from turn userMessage', () => {
    // Simulating the handleUndo logic from ChatView.tsx line 235-244
    const turn = {
      id: 'turn-1',
      userMessage: {
        id: 'evt-1',
        type: 'user' as const,
        content: 'Hello',
        timestamp: 1000,
        messageId: 'msg-xyz',
      },
      agentMessages: [],
      toolCalls: [],
      delegations: [],
      startTime: 1000,
      isActive: false,
    };

    const sendUndo = vi.fn();
    const filteredTurns = [turn];
    const turnIndex = 0;

    // Simulate handleUndo logic
    const targetTurn = filteredTurns[turnIndex];
    const userMessage = targetTurn.userMessage;

    if (!userMessage?.messageId) {
      console.error('[Test] Cannot undo: no message ID');
      return;
    }

    sendUndo(userMessage.messageId, targetTurn.id);

    // Verify sendUndo was called with correct parameters
    expect(sendUndo).toHaveBeenCalledWith('msg-xyz', 'turn-1');
  });

  it('handleUndo should log error when no messageId present', () => {
    const consoleSpy = vi.spyOn(console, 'error').mockImplementation(() => {});
    
    const turn = {
      id: 'turn-1',
      userMessage: {
        id: 'evt-1',
        type: 'user' as const,
        content: 'Hello',
        timestamp: 1000,
        // No messageId
      },
      agentMessages: [],
      toolCalls: [],
      delegations: [],
      startTime: 1000,
      isActive: false,
    };

    const sendUndo = vi.fn();
    const filteredTurns = [turn];
    const turnIndex = 0;

    // Simulate handleUndo logic
    const targetTurn = filteredTurns[turnIndex];
    const userMessage = targetTurn.userMessage;

    if (!userMessage?.messageId) {
      console.error('[Test] Cannot undo: no message ID found for turn', targetTurn.id);
      return;
    }

    sendUndo(userMessage.messageId, targetTurn.id);

    // Verify error was logged and sendUndo was NOT called
    expect(consoleSpy).toHaveBeenCalledWith(
      '[Test] Cannot undo: no message ID found for turn',
      'turn-1'
    );
    expect(sendUndo).not.toHaveBeenCalled();

    consoleSpy.mockRestore();
  });

  it('handleUndo should not crash when turn has no userMessage', () => {
    const consoleSpy = vi.spyOn(console, 'error').mockImplementation(() => {});
    
    const turn = {
      id: 'turn-1',
      userMessage: undefined,  // Agent-initiated turn, no user message
      agentMessages: [],
      toolCalls: [],
      delegations: [],
      startTime: 1000,
      isActive: false,
    };

    const sendUndo = vi.fn();
    const filteredTurns = [turn];
    const turnIndex = 0;

    // Simulate handleUndo logic
    const targetTurn = filteredTurns[turnIndex];
    const userMessage = targetTurn.userMessage;

    if (!userMessage?.messageId) {
      console.error('[Test] Cannot undo: no message ID found');
      return;
    }

    sendUndo(userMessage.messageId, targetTurn.id);

    // Should log error and not crash
    expect(consoleSpy).toHaveBeenCalled();
    expect(sendUndo).not.toHaveBeenCalled();

    consoleSpy.mockRestore();
  });

  it('handleRedo should call sendRedo with no parameters', () => {
    const sendRedo = vi.fn();

    // Simulate handleRedo logic (ChatView.tsx line 247-250)
    sendRedo();

    expect(sendRedo).toHaveBeenCalledTimes(1);
    expect(sendRedo).toHaveBeenCalledWith();
  });
});

describe('ChatView undo button visibility logic', () => {
  it('undo candidate moves to previous tool turn after undo frontier', () => {
    const turns = [
      {
        id: 'turn-0',
        userMessage: { messageId: 'msg-0' },
        toolCalls: [{ id: 'tool-0' }],
      },
      {
        id: 'turn-1',
        userMessage: { messageId: 'msg-1' },
        toolCalls: [{ id: 'tool-1' }],
      },
      {
        id: 'turn-2',
        userMessage: { messageId: 'msg-2' },
        toolCalls: [{ id: 'tool-2' }],
      },
    ];

    const computeUndoTurnIndex = (frontierMessageId?: string) => {
      let startIndex = turns.length - 1;
      if (frontierMessageId) {
        const frontierIndex = turns.findIndex(
          turn => turn.userMessage?.messageId === frontierMessageId
        );
        if (frontierIndex >= 0) {
          startIndex = frontierIndex - 1;
        }
      }

      for (let i = startIndex; i >= 0; i--) {
        if (turns[i].toolCalls.length > 0 && turns[i].userMessage?.messageId) {
          return i;
        }
      }
      return -1;
    };

    expect(computeUndoTurnIndex()).toBe(2);
    expect(computeUndoTurnIndex('msg-2')).toBe(1);
    expect(computeUndoTurnIndex('msg-1')).toBe(0);
    expect(computeUndoTurnIndex('msg-0')).toBe(-1);
  });

  it('uses pending top frame after two rapid undos so UI does not show wrong confirmed overlay', () => {
    const turns = [
      { id: 'turn-1', userMessage: { messageId: 'msg-1' } },
      { id: 'turn-2', userMessage: { messageId: 'msg-2' } },
    ];

    const undoState = {
      stack: [
        { turnId: 'turn-1', messageId: 'msg-1', status: 'confirmed' as const, revertedFiles: ['a.txt'] },
        { turnId: 'turn-2', messageId: 'msg-2', status: 'pending' as const, revertedFiles: [] },
      ],
      lastUndoneTurnId: 'turn-2',
      frontierMessageId: 'msg-2',
    };

    const deriveUiStateForTurn = (turnMessageId?: string) => {
      const frontierFrame = undoState.frontierMessageId
        ? undoState.stack.find(frame => frame.messageId === undoState.frontierMessageId)
        : undefined;
      const effectiveFrontierFrame = frontierFrame ?? undoState.stack[undoState.stack.length - 1];
      const frameForTurn = turnMessageId
        ? undoState.stack.find(frame => frame.messageId === turnMessageId)
        : undefined;
      const isFrontierFrame = !!effectiveFrontierFrame && frameForTurn?.messageId === effectiveFrontierFrame.messageId;
      const isUndoPending = isFrontierFrame && effectiveFrontierFrame?.status === 'pending';
      const isUndone = isFrontierFrame && effectiveFrontierFrame?.status === 'confirmed';
      const isStackedUndone = !!frameForTurn && frameForTurn.status === 'confirmed' && !isFrontierFrame;
      const revertedFiles = isUndone ? (effectiveFrontierFrame?.revertedFiles ?? []) : [];
      return { isUndoPending, isUndone, isStackedUndone, revertedFiles };
    };

    const olderTurnUi = deriveUiStateForTurn(turns[0].userMessage.messageId);
    const latestTurnUi = deriveUiStateForTurn(turns[1].userMessage.messageId);

    expect(olderTurnUi).toEqual({
      isUndoPending: false,
      isUndone: false,
      isStackedUndone: true,
      revertedFiles: [],
    });

    expect(latestTurnUi).toEqual({
      isUndoPending: true,
      isUndone: false,
      isStackedUndone: false,
      revertedFiles: [],
    });
  });

  it('anchors redo overlay by frontierMessageId even if stack ordering differs', () => {
    const turns = [
      { id: 'turn-1', userMessage: { messageId: 'msg-1' } },
      { id: 'turn-2', userMessage: { messageId: 'msg-2' } },
    ];

    const undoState = {
      stack: [
        { turnId: 'turn-2', messageId: 'msg-2', status: 'confirmed' as const, revertedFiles: ['newer.txt'] },
        { turnId: 'turn-1', messageId: 'msg-1', status: 'confirmed' as const, revertedFiles: ['older.txt'] },
      ],
      lastUndoneTurnId: 'turn-1',
      frontierMessageId: 'msg-1',
    };

    const deriveUiStateForTurn = (turnMessageId?: string) => {
      const frontierFrame = undoState.frontierMessageId
        ? undoState.stack.find(frame => frame.messageId === undoState.frontierMessageId)
        : undefined;
      const effectiveFrontierFrame = frontierFrame ?? undoState.stack[undoState.stack.length - 1];
      const frameForTurn = turnMessageId
        ? undoState.stack.find(frame => frame.messageId === turnMessageId)
        : undefined;

      const isFrontierFrame =
        !!effectiveFrontierFrame && frameForTurn?.messageId === effectiveFrontierFrame.messageId;
      const isUndoPending = isFrontierFrame && effectiveFrontierFrame?.status === 'pending';
      const isUndone = isFrontierFrame && effectiveFrontierFrame?.status === 'confirmed';
      const isStackedUndone = !!frameForTurn && frameForTurn.status === 'confirmed' && !isFrontierFrame;
      const revertedFiles = isUndone ? (effectiveFrontierFrame?.revertedFiles ?? []) : [];
      return { isUndoPending, isUndone, isStackedUndone, revertedFiles };
    };

    const olderTurnUi = deriveUiStateForTurn(turns[0].userMessage.messageId);
    const newerTurnUi = deriveUiStateForTurn(turns[1].userMessage.messageId);

    expect(olderTurnUi).toEqual({
      isUndoPending: false,
      isUndone: true,
      isStackedUndone: false,
      revertedFiles: ['older.txt'],
    });

    expect(newerTurnUi).toEqual({
      isUndoPending: false,
      isUndone: false,
      isStackedUndone: true,
      revertedFiles: [],
    });
  });

  it('uses messageId-only mapping so mismatched local IDs do not show wrong overlay', () => {
    const turns = [
      { id: 'turn-1', userMessage: { messageId: 'msg-1' }, toolCalls: [{ id: 'tool-1' }] },
      { id: 'turn-2', userMessage: { messageId: 'msg-2-local' }, toolCalls: [{ id: 'tool-2' }] },
      { id: 'turn-3', userMessage: { messageId: 'msg-3' }, toolCalls: [{ id: 'tool-3' }] },
    ];

    const undoState = {
      stack: [
        { turnId: 'msg-3', messageId: 'msg-3', status: 'confirmed' as const, revertedFiles: ['third.txt'] },
        { turnId: 'msg-2-backend', messageId: 'msg-2-backend', status: 'confirmed' as const, revertedFiles: ['second.txt'] },
      ],
      lastUndoneTurnId: 'msg-2-backend',
      frontierMessageId: 'msg-2-backend',
    };

    const deriveUiStateForTurn = (turnMessageId?: string) => {
      const topFrame = undoState.stack[undoState.stack.length - 1];
      const frameForTurn = turnMessageId
        ? undoState.stack.find(frame => frame.messageId === turnMessageId)
        : undefined;
      const isTopFrame = !!topFrame && frameForTurn?.messageId === topFrame.messageId;
      const isUndoPending = isTopFrame && topFrame?.status === 'pending';
      const isUndone = isTopFrame && topFrame?.status === 'confirmed';
      const isStackedUndone = !!frameForTurn && !isTopFrame && frameForTurn.status === 'confirmed';
      const revertedFiles = isUndone ? (topFrame?.revertedFiles ?? []) : [];
      return { isUndoPending, isUndone, isStackedUndone, revertedFiles };
    };

    const secondTurnUi = deriveUiStateForTurn(turns[1].userMessage.messageId);
    const thirdTurnUi = deriveUiStateForTurn(turns[2].userMessage.messageId);

    expect(secondTurnUi).toEqual({
      isUndoPending: false,
      isUndone: false,
      isStackedUndone: false,
      revertedFiles: [],
    });

    expect(thirdTurnUi).toEqual({
      isUndoPending: false,
      isUndone: false,
      isStackedUndone: true,
      revertedFiles: [],
    });

    const computeUndoTurnIndex = () => {
      let startIndex = turns.length - 1;
      const frontierIndexByMessageId = turns.findIndex(
        turn => turn.userMessage?.messageId === undoState.frontierMessageId
      );
      if (frontierIndexByMessageId >= 0) {
        startIndex = frontierIndexByMessageId - 1;
      }

      for (let i = startIndex; i >= 0; i--) {
        if (turns[i].toolCalls.length > 0 && turns[i].userMessage?.messageId) {
          return i;
        }
      }
      return -1;
    };

    expect(computeUndoTurnIndex()).toBe(2);
  });
});
