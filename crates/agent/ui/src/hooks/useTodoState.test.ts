import { describe, it, expect, beforeEach, vi } from 'vitest';
import { renderHook, act } from '@testing-library/react';
import { useTodoState } from './useTodoState';
import { EventItem, TodoItem } from '../types';
import { resetFixtureCounter, makeToolCallEvent } from '../test/fixtures';

describe('useTodoState', () => {
  beforeEach(() => {
    resetFixtureCounter();
    vi.useRealTimers();
  });

  it('returns empty state when no events', () => {
    const { result } = renderHook(() => useTodoState([]));
    
    expect(result.current.todos).toEqual([]);
    expect(result.current.hasTodos).toBe(false);
    expect(result.current.stats).toEqual({
      total: 0,
      completed: 0,
      inProgress: 0,
      pending: 0,
      cancelled: 0,
    });
    expect(result.current.lastUpdateTimestamp).toBeNull();
    expect(result.current.recentlyChangedIds.size).toBe(0);
  });

  it('returns empty state when events have no todowrite', () => {
    const events: EventItem[] = [
      makeToolCallEvent('read_tool', {
        toolCall: {
          kind: 'read_tool',
          tool_call_id: 'read:1',
          raw_input: { path: 'test.ts' },
        },
      }),
    ];
    
    const { result } = renderHook(() => useTodoState(events));
    
    expect(result.current.todos).toEqual([]);
    expect(result.current.hasTodos).toBe(false);
    expect(result.current.lastUpdateTimestamp).toBeNull();
  });

  it('extracts todos from single todowrite event', () => {
    const todos: TodoItem[] = [
      { id: 'todo-1', content: 'Task 1', status: 'pending', priority: 'high' },
      { id: 'todo-2', content: 'Task 2', status: 'completed', priority: 'medium' },
    ];
    
    const events: EventItem[] = [
      makeToolCallEvent('todowrite', {
        timestamp: 5000,
        toolCall: {
          kind: 'todowrite',
          tool_call_id: 'todowrite:1',
          raw_input: { todos },
        },
      }),
    ];
    
    const { result } = renderHook(() => useTodoState(events));
    
    expect(result.current.todos).toEqual(todos);
    expect(result.current.hasTodos).toBe(true);
    expect(result.current.lastUpdateTimestamp).toBe(5000);
    expect(result.current.stats.total).toBe(2);
    expect(result.current.stats.pending).toBe(1);
    expect(result.current.stats.completed).toBe(1);
  });

  it('uses the LAST todowrite event when multiple exist', () => {
    const firstTodos: TodoItem[] = [
      { id: 'todo-1', content: 'Task 1', status: 'pending', priority: 'high' },
    ];
    
    const secondTodos: TodoItem[] = [
      { id: 'todo-2', content: 'Task 2', status: 'completed', priority: 'medium' },
      { id: 'todo-3', content: 'Task 3', status: 'in_progress', priority: 'low' },
    ];
    
    const events: EventItem[] = [
      makeToolCallEvent('todowrite', {
        timestamp: 1000,
        toolCall: {
          kind: 'todowrite',
          tool_call_id: 'todowrite:1',
          raw_input: { todos: firstTodos },
        },
      }),
      makeToolCallEvent('todowrite', {
        timestamp: 5000,
        toolCall: {
          kind: 'todowrite',
          tool_call_id: 'todowrite:2',
          raw_input: { todos: secondTodos },
        },
      }),
    ];
    
    const { result } = renderHook(() => useTodoState(events));
    
    expect(result.current.todos).toEqual(secondTodos);
    expect(result.current.lastUpdateTimestamp).toBe(5000);
    expect(result.current.stats.total).toBe(2);
  });

  it('recognizes both todowrite and mcp_todowrite kinds', () => {
    const todos: TodoItem[] = [
      { id: 'todo-1', content: 'Task 1', status: 'pending', priority: 'high' },
    ];
    
    const events: EventItem[] = [
      makeToolCallEvent('mcp_todowrite', {
        timestamp: 3000,
        toolCall: {
          kind: 'mcp_todowrite',
          tool_call_id: 'mcp_todowrite:1',
          raw_input: { todos },
        },
      }),
    ];
    
    const { result } = renderHook(() => useTodoState(events));
    
    expect(result.current.todos).toEqual(todos);
    expect(result.current.lastUpdateTimestamp).toBe(3000);
  });

  it('filters out invalid todos missing required fields', () => {
    const mixedTodos = [
      { id: 'todo-1', content: 'Valid', status: 'pending', priority: 'high' },
      { id: 'todo-2', content: 'Missing status', priority: 'medium' }, // missing status
      { content: 'Missing ID', status: 'pending', priority: 'low' }, // missing id
      { id: 'todo-3', status: 'completed', priority: 'high' }, // missing content
      { id: 'todo-4', content: 'Also valid', status: 'in_progress', priority: 'medium' },
    ];
    
    const events: EventItem[] = [
      makeToolCallEvent('todowrite', {
        toolCall: {
          kind: 'todowrite',
          tool_call_id: 'todowrite:1',
          raw_input: { todos: mixedTodos },
        },
      }),
    ];
    
    const { result } = renderHook(() => useTodoState(events));
    
    expect(result.current.todos).toHaveLength(2);
    expect(result.current.todos[0].id).toBe('todo-1');
    expect(result.current.todos[1].id).toBe('todo-4');
  });

  it('calculates stats correctly for mixed status todos', () => {
    const todos: TodoItem[] = [
      { id: 'todo-1', content: 'Task 1', status: 'pending', priority: 'high' },
      { id: 'todo-2', content: 'Task 2', status: 'pending', priority: 'medium' },
      { id: 'todo-3', content: 'Task 3', status: 'in_progress', priority: 'high' },
      { id: 'todo-4', content: 'Task 4', status: 'completed', priority: 'low' },
      { id: 'todo-5', content: 'Task 5', status: 'completed', priority: 'medium' },
      { id: 'todo-6', content: 'Task 6', status: 'completed', priority: 'high' },
      { id: 'todo-7', content: 'Task 7', status: 'cancelled', priority: 'low' },
    ];
    
    const events: EventItem[] = [
      makeToolCallEvent('todowrite', {
        toolCall: {
          kind: 'todowrite',
          tool_call_id: 'todowrite:1',
          raw_input: { todos },
        },
      }),
    ];
    
    const { result } = renderHook(() => useTodoState(events));
    
    expect(result.current.stats).toEqual({
      total: 7,
      completed: 3,
      inProgress: 1,
      pending: 2,
      cancelled: 1,
    });
  });

  it('handles invalid raw_input structures gracefully', () => {
    const testCases: EventItem[] = [
      // raw_input is not an object
      makeToolCallEvent('todowrite', {
        toolCall: {
          kind: 'todowrite',
          tool_call_id: 'todowrite:1',
          raw_input: 'invalid',
        },
      }),
      // raw_input missing todos key
      makeToolCallEvent('todowrite', {
        toolCall: {
          kind: 'todowrite',
          tool_call_id: 'todowrite:2',
          raw_input: { other: 'data' },
        },
      }),
      // todos is not an array
      makeToolCallEvent('todowrite', {
        toolCall: {
          kind: 'todowrite',
          tool_call_id: 'todowrite:3',
          raw_input: { todos: 'not-array' },
        },
      }),
    ];
    
    for (const event of testCases) {
      const { result } = renderHook(() => useTodoState([event]));
      expect(result.current.todos).toEqual([]);
      expect(result.current.hasTodos).toBe(false);
    }
  });

  it('populates recentlyChangedIds when todos are updated', () => {
    const todos: TodoItem[] = [
      { id: 'todo-1', content: 'Task 1', status: 'pending', priority: 'high' },
      { id: 'todo-2', content: 'Task 2', status: 'completed', priority: 'medium' },
    ];
    
    const events: EventItem[] = [
      makeToolCallEvent('todowrite', {
        toolCall: {
          kind: 'todowrite',
          tool_call_id: 'todowrite:1',
          raw_input: { todos },
        },
      }),
    ];
    
    const { result } = renderHook(() => useTodoState(events));
    
    expect(result.current.recentlyChangedIds.size).toBe(2);
    expect(result.current.recentlyChangedIds.has('todo-1')).toBe(true);
    expect(result.current.recentlyChangedIds.has('todo-2')).toBe(true);
  });

  it('clears recentlyChangedIds after 2.5s timeout', () => {
    vi.useFakeTimers();
    
    const todos: TodoItem[] = [
      { id: 'todo-1', content: 'Task 1', status: 'pending', priority: 'high' },
    ];
    
    const events: EventItem[] = [
      makeToolCallEvent('todowrite', {
        toolCall: {
          kind: 'todowrite',
          tool_call_id: 'todowrite:1',
          raw_input: { todos },
        },
      }),
    ];
    
    const { result } = renderHook(() => useTodoState(events));
    
    // Initially populated
    expect(result.current.recentlyChangedIds.size).toBe(1);
    
    // Advance time by 2.5 seconds
    act(() => {
      vi.advanceTimersByTime(2500);
    });
    
    // Should be cleared
    expect(result.current.recentlyChangedIds.size).toBe(0);
    
    vi.useRealTimers();
  });

  it('updates recentlyChangedIds when todo list changes via rerender', () => {
    const firstTodos: TodoItem[] = [
      { id: 'todo-1', content: 'Task 1', status: 'pending', priority: 'high' },
    ];
    
    const secondTodos: TodoItem[] = [
      { id: 'todo-1', content: 'Task 1', status: 'completed', priority: 'high' },
      { id: 'todo-2', content: 'Task 2', status: 'pending', priority: 'medium' },
    ];
    
    let events: EventItem[] = [
      makeToolCallEvent('todowrite', {
        timestamp: 1000,
        toolCall: {
          kind: 'todowrite',
          tool_call_id: 'todowrite:1',
          raw_input: { todos: firstTodos },
        },
      }),
    ];
    
    const { result, rerender } = renderHook(
      ({ events }) => useTodoState(events),
      { initialProps: { events } }
    );
    
    expect(result.current.recentlyChangedIds.size).toBe(1);
    expect(result.current.recentlyChangedIds.has('todo-1')).toBe(true);
    
    // Update events with new todowrite
    events = [
      ...events,
      makeToolCallEvent('todowrite', {
        timestamp: 5000,
        toolCall: {
          kind: 'todowrite',
          tool_call_id: 'todowrite:2',
          raw_input: { todos: secondTodos },
        },
      }),
    ];
    
    rerender({ events });
    
    expect(result.current.todos).toHaveLength(2);
    expect(result.current.recentlyChangedIds.size).toBe(2);
    expect(result.current.recentlyChangedIds.has('todo-1')).toBe(true);
    expect(result.current.recentlyChangedIds.has('todo-2')).toBe(true);
  });

  it('handles empty todos array in raw_input', () => {
    const events: EventItem[] = [
      makeToolCallEvent('todowrite', {
        toolCall: {
          kind: 'todowrite',
          tool_call_id: 'todowrite:1',
          raw_input: { todos: [] },
        },
      }),
    ];
    
    const { result } = renderHook(() => useTodoState(events));
    
    expect(result.current.todos).toEqual([]);
    expect(result.current.hasTodos).toBe(false);
    expect(result.current.stats.total).toBe(0);
  });
});
