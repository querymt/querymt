import { useMemo, useState, useEffect } from 'react';
import { EventItem, TodoItem } from '../types';

export interface TodoStats {
  total: number;
  completed: number;
  inProgress: number;
  pending: number;
  cancelled: number;
}

export interface TodoState {
  todos: TodoItem[];
  hasTodos: boolean;
  stats: TodoStats;
  lastUpdateTimestamp: number | null;
  recentlyChangedIds: Set<string>;
}

/**
 * Extracts the current todo list from the event stream.
 * Scans for todowrite tool calls and returns the latest snapshot.
 */
export function useTodoState(events: EventItem[]): TodoState {
  const [recentlyChangedIds, setRecentlyChangedIds] = useState<Set<string>>(new Set());
  
  // Extract todos from the last todowrite event
  const { todos, lastUpdateTimestamp, currentTodoIds } = useMemo(() => {
    let latestTodos: TodoItem[] = [];
    let latestTimestamp: number | null = null;
    
    // Scan events in reverse to find the most recent todowrite
    for (let i = events.length - 1; i >= 0; i--) {
      const event = events[i];
      
      // Check if this is a todowrite or mcp_todowrite tool call
      if (
        event.type === 'tool_call' &&
        (event.toolCall?.kind === 'todowrite' || event.toolCall?.kind === 'mcp_todowrite')
      ) {
        const rawInput = event.toolCall?.raw_input;
        
        if (rawInput && typeof rawInput === 'object' && 'todos' in rawInput) {
          const todosArray = (rawInput as { todos?: unknown }).todos;
          
          if (Array.isArray(todosArray)) {
            // Validate and parse todos
            latestTodos = todosArray.filter((item): item is TodoItem => {
              return (
                typeof item === 'object' &&
                item !== null &&
                'id' in item &&
                'content' in item &&
                'status' in item &&
                'priority' in item &&
                typeof item.id === 'string' &&
                typeof item.content === 'string' &&
                typeof item.status === 'string' &&
                typeof item.priority === 'string'
              );
            });
            
            latestTimestamp = event.timestamp;
            break; // Found the most recent one
          }
        }
      }
    }
    
    const currentTodoIds = new Set(latestTodos.map(t => t.id));
    
    return { todos: latestTodos, lastUpdateTimestamp: latestTimestamp, currentTodoIds };
  }, [events]);
  
  // Track which todos changed when the list updates
  useEffect(() => {
    if (todos.length > 0) {
      setRecentlyChangedIds(currentTodoIds);
      
      // Clear the "recently changed" highlight after 2.5 seconds
      const timer = setTimeout(() => {
        setRecentlyChangedIds(new Set());
      }, 2500);
      
      return () => clearTimeout(timer);
    }
  }, [lastUpdateTimestamp]); // Only trigger when timestamp changes (new todowrite)
  
  // Calculate stats
  const stats = useMemo<TodoStats>(() => ({
    total: todos.length,
    completed: todos.filter(t => t.status === 'completed').length,
    inProgress: todos.filter(t => t.status === 'in_progress').length,
    pending: todos.filter(t => t.status === 'pending').length,
    cancelled: todos.filter(t => t.status === 'cancelled').length,
  }), [todos]);
  
  return {
    todos,
    hasTodos: todos.length > 0,
    stats,
    lastUpdateTimestamp,
    recentlyChangedIds,
  };
}
