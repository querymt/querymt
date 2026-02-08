import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { TodoRail } from './TodoRail';
import { TodoItem } from '../types';
import { TodoStats } from '../hooks/useTodoState';

describe('TodoRail', () => {
  const defaultStats: TodoStats = {
    total: 5,
    completed: 2,
    inProgress: 1,
    pending: 2,
    cancelled: 0,
  };

  const defaultTodos: TodoItem[] = [
    {
      id: 'todo-1',
      content: 'First task',
      status: 'pending',
      priority: 'high',
    },
    {
      id: 'todo-2',
      content: 'Second task',
      status: 'in_progress',
      priority: 'medium',
    },
  ];

  const defaultProps = {
    todos: defaultTodos,
    stats: defaultStats,
    collapsed: false,
    onToggleCollapse: vi.fn(),
    recentlyChangedIds: new Set<string>(),
  };

  beforeEach(() => {
    vi.clearAllMocks();
  });

  it('renders collapsed state with stats', () => {
    render(<TodoRail {...defaultProps} collapsed={true} />);

    expect(screen.getByText('2/5')).toBeInTheDocument();
    // Tasks header should not be visible in collapsed state
    expect(screen.queryByText('Tasks')).not.toBeInTheDocument();
  });

  it('renders expand button in collapsed state', () => {
    render(<TodoRail {...defaultProps} collapsed={true} />);

    const expandButton = screen.getByTitle(/Expand tasks/i);
    expect(expandButton).toBeInTheDocument();
  });

  it('calls onToggleCollapse when expand button clicked', async () => {
    const user = userEvent.setup();
    const onToggleCollapse = vi.fn();

    render(<TodoRail {...defaultProps} collapsed={true} onToggleCollapse={onToggleCollapse} />);

    const expandButton = screen.getByTitle(/Expand tasks/i);
    await user.click(expandButton);

    expect(onToggleCollapse).toHaveBeenCalledTimes(1);
  });

  it('renders expanded state with header and stats', () => {
    render(<TodoRail {...defaultProps} collapsed={false} />);

    expect(screen.getByText('Tasks')).toBeInTheDocument();
    expect(screen.getByText('2/5')).toBeInTheDocument();
  });

  it('renders todo items with content', () => {
    render(<TodoRail {...defaultProps} collapsed={false} />);

    expect(screen.getByText('First task')).toBeInTheDocument();
    expect(screen.getByText('Second task')).toBeInTheDocument();
  });

  it('renders collapse button in expanded state', () => {
    render(<TodoRail {...defaultProps} collapsed={false} />);

    const collapseButton = screen.getByTitle(/Collapse tasks/i);
    expect(collapseButton).toBeInTheDocument();
  });

  it('calls onToggleCollapse when collapse button clicked', async () => {
    const user = userEvent.setup();
    const onToggleCollapse = vi.fn();

    render(<TodoRail {...defaultProps} collapsed={false} onToggleCollapse={onToggleCollapse} />);

    const collapseButton = screen.getByTitle(/Collapse tasks/i);
    await user.click(collapseButton);

    expect(onToggleCollapse).toHaveBeenCalledTimes(1);
  });

  it('shows active count in stats breakdown', () => {
    const stats: TodoStats = {
      total: 5,
      completed: 2,
      inProgress: 2,
      pending: 1,
      cancelled: 0,
    };

    render(<TodoRail {...defaultProps} stats={stats} />);

    expect(screen.getByText('2 active')).toBeInTheDocument();
  });

  it('shows pending count in stats breakdown', () => {
    const stats: TodoStats = {
      total: 5,
      completed: 2,
      inProgress: 0,
      pending: 3,
      cancelled: 0,
    };

    render(<TodoRail {...defaultProps} stats={stats} />);

    expect(screen.getByText('3 pending')).toBeInTheDocument();
  });

  it('applies highlight ring to recently changed todos', () => {
    const recentlyChangedIds = new Set(['todo-1']);

    render(
      <TodoRail {...defaultProps} recentlyChangedIds={recentlyChangedIds} />
    );

    // Find the todo item div that contains 'First task'
    const firstTaskElement = screen.getByText('First task');
    const todoItemDiv = firstTaskElement.closest('div[class*="ring-"]');
    
    expect(todoItemDiv).toBeInTheDocument();
    expect(todoItemDiv?.className).toContain('ring-1');
  });

  it('renders completed todos with line-through style', () => {
    const todos: TodoItem[] = [
      {
        id: 'todo-completed',
        content: 'Completed task',
        status: 'completed',
        priority: 'low',
      },
    ];

    render(<TodoRail {...defaultProps} todos={todos} />);

    const completedText = screen.getByText('Completed task');
    expect(completedText.className).toContain('line-through');
  });
});
