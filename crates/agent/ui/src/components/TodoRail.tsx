import { CheckCircle, Circle, XCircle, ChevronRight, ChevronLeft } from 'lucide-react';
import { TodoItem } from '../types';
import { TodoStats } from '../hooks/useTodoState';

interface TodoRailProps {
  todos: TodoItem[];
  stats: TodoStats;
  collapsed: boolean;
  onToggleCollapse: () => void;
  recentlyChangedIds: Set<string>;
}

export function TodoRail({ todos, stats, collapsed, onToggleCollapse, recentlyChangedIds }: TodoRailProps) {
  if (collapsed) {
    return (
      <div className="w-8 border-l border-cyber-border/50 bg-cyber-bg/60 backdrop-blur-sm flex flex-col items-center py-4 relative">
        <button
          onClick={onToggleCollapse}
          className="
            absolute top-4 left-0 right-0 mx-auto
            w-6 h-6 rounded-full
            bg-cyber-cyan/20 hover:bg-cyber-cyan/30
            border border-cyber-cyan/50
            flex items-center justify-center
            transition-all duration-200
            text-cyber-cyan
          "
          title="Expand tasks (Cmd+Shift+T)"
        >
          <ChevronLeft className="w-3 h-3" />
        </button>
        
        {/* Vertical "Tasks" text */}
        <div className="mt-16 flex-1 flex items-center justify-center">
          <div className="transform -rotate-90 whitespace-nowrap text-xs text-gray-500 flex items-center gap-2">
            <span>📋</span>
            <span>{stats.completed}/{stats.total}</span>
          </div>
        </div>
        
        {/* Glow indicator when recently updated */}
        {recentlyChangedIds.size > 0 && (
          <div className="absolute inset-0 animate-glow-pulse border-l-2 border-cyber-cyan/50 pointer-events-none" />
        )}
      </div>
    );
  }
  
  return (
    <div className="w-72 border-l border-cyber-border/50 bg-cyber-bg/60 backdrop-blur-sm flex flex-col relative">
      {/* Header */}
      <div className="px-4 py-2 border-b border-cyber-border/50 bg-cyber-surface/40 flex items-center justify-between">
        <div className="flex items-center gap-2">
          <span className="text-lg">📋</span>
          <span className="text-sm font-semibold text-gray-200">Tasks</span>
          <span className="text-xs text-gray-500 ml-1">
            {stats.completed}/{stats.total}
          </span>
        </div>
        
        <button
          onClick={onToggleCollapse}
          className="
            w-5 h-5 rounded
            bg-cyber-bg/40 hover:bg-cyber-cyan/20
            border border-cyber-border/50 hover:border-cyber-cyan/50
            flex items-center justify-center
            transition-all duration-200
            text-gray-400 hover:text-cyber-cyan
          "
          title="Collapse tasks (Cmd+Shift+T)"
        >
          <ChevronRight className="w-3 h-3" />
        </button>
      </div>
      
      {/* Progress bar */}
      <div className="px-4 py-3 border-b border-cyber-border/50">
        <div className="h-1.5 bg-cyber-border/30 rounded-full overflow-hidden flex">
          {/* Completed segment */}
          {stats.completed > 0 && (
            <div
              className="bg-cyber-lime h-full transition-all duration-300"
              style={{ width: `${(stats.completed / stats.total) * 100}%` }}
            />
          )}
          {/* In-progress segment */}
          {stats.inProgress > 0 && (
            <div
              className="bg-cyber-cyan/60 h-full transition-all duration-300"
              style={{ width: `${(stats.inProgress / stats.total) * 100}%` }}
            />
          )}
        </div>
        
        {/* Stats breakdown */}
        <div className="mt-2 flex items-center gap-3 text-[10px] text-gray-500">
          {stats.inProgress > 0 && (
            <span className="flex items-center gap-1">
              <span className="w-1.5 h-1.5 rounded-full bg-cyber-cyan animate-pulse" />
              {stats.inProgress} active
            </span>
          )}
          {stats.pending > 0 && (
            <span className="flex items-center gap-1">
              <span className="w-1.5 h-1.5 rounded-full bg-gray-500" />
              {stats.pending} pending
            </span>
          )}
        </div>
      </div>
      
      {/* Todo list */}
      <div className="flex-1 overflow-y-auto px-2 py-3 space-y-1.5">
        {todos.map((todo) => (
          <TodoItemRow
            key={todo.id}
            todo={todo}
            isRecentlyChanged={recentlyChangedIds.has(todo.id)}
          />
        ))}
      </div>
    </div>
  );
}

interface TodoItemRowProps {
  todo: TodoItem;
  isRecentlyChanged: boolean;
}

function TodoItemRow({ todo, isRecentlyChanged }: TodoItemRowProps) {
  const priorityColor = {
    high: 'bg-cyber-orange',
    medium: 'bg-cyber-cyan',
    low: 'bg-cyber-purple',
  }[todo.priority];
  
  const statusIcon = {
    in_progress: (
      <div className="w-4 h-4 rounded-full bg-cyber-cyan/20 flex items-center justify-center">
        <div className="w-2 h-2 rounded-full bg-cyber-cyan animate-pulse" />
      </div>
    ),
    pending: <Circle className="w-4 h-4 text-gray-500" />,
    completed: <CheckCircle className="w-4 h-4 text-cyber-lime" />,
    cancelled: <XCircle className="w-4 h-4 text-red-400" />,
  }[todo.status];
  
  const textStyle = {
    in_progress: 'text-gray-100 font-medium',
    pending: 'text-gray-300',
    completed: 'text-gray-500 line-through',
    cancelled: 'text-gray-600 line-through',
  }[todo.status];
  
  return (
    <div
      className={`
        relative pl-3 pr-2 py-2 rounded-md
        bg-cyber-surface/30 border border-transparent
        hover:bg-cyber-surface/50
        transition-all duration-200
        ${isRecentlyChanged ? 'ring-1 ring-cyber-cyan/50 shadow-neon-cyan' : ''}
      `}
    >
      {/* Priority color bar */}
      <div className={`absolute left-0 top-0 bottom-0 w-1 rounded-l-md ${priorityColor}`} />
      
      {/* Content */}
      <div className="flex items-start gap-2">
        <div className="mt-0.5 flex-shrink-0">
          {statusIcon}
        </div>
        <div className={`flex-1 text-xs leading-relaxed ${textStyle}`}>
          {todo.content}
        </div>
      </div>
    </div>
  );
}
