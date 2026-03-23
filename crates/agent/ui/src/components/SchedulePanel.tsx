import { memo, useState, useCallback } from 'react';
import {
  Clock,
  Play,
  Pause,
  Trash2,
  Zap,
  ChevronRight,
  ChevronLeft,
  Plus,
  AlertTriangle,
} from 'lucide-react';
import type { ScheduleInfo } from '../types';

// ---------------------------------------------------------------------------
// State badge colors
// ---------------------------------------------------------------------------

const STATE_STYLES: Record<string, { label: string; dotClass: string; textClass: string }> = {
  armed:     { label: 'Armed',     dotClass: 'bg-status-success',       textClass: 'text-status-success' },
  running:   { label: 'Running',   dotClass: 'bg-accent-primary animate-pulse', textClass: 'text-accent-primary' },
  paused:    { label: 'Paused',    dotClass: 'bg-ui-muted',             textClass: 'text-ui-muted' },
  failed:    { label: 'Failed',    dotClass: 'bg-status-warning',       textClass: 'text-status-warning' },
  exhausted: { label: 'Exhausted', dotClass: 'bg-accent-tertiary',      textClass: 'text-accent-tertiary' },
};

function getStateStyle(state: string) {
  return STATE_STYLES[state.toLowerCase()] ?? STATE_STYLES.armed;
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function formatRelativeTime(iso: string | undefined): string {
  if (!iso) return '--';
  const date = new Date(iso);
  const now = Date.now();
  const diffMs = date.getTime() - now;
  const absDiff = Math.abs(diffMs);

  if (absDiff < 60_000) return diffMs > 0 ? 'in <1m' : '<1m ago';
  if (absDiff < 3_600_000) {
    const mins = Math.round(absDiff / 60_000);
    return diffMs > 0 ? `in ${mins}m` : `${mins}m ago`;
  }
  if (absDiff < 86_400_000) {
    const hrs = Math.round(absDiff / 3_600_000);
    return diffMs > 0 ? `in ${hrs}h` : `${hrs}h ago`;
  }
  const days = Math.round(absDiff / 86_400_000);
  return diffMs > 0 ? `in ${days}d` : `${days}d ago`;
}

function describeTrigger(trigger: any): string {
  if (!trigger) return 'Unknown';
  // Interval trigger: { type: "interval", seconds: N }
  if (trigger.type === 'interval') {
    const secs = trigger.seconds;
    if (!secs) return 'Interval';
    if (secs < 60) return `Every ${secs}s`;
    if (secs < 3600) return `Every ${Math.round(secs / 60)}m`;
    return `Every ${Math.round(secs / 3600)}h`;
  }
  // Once-at trigger: { type: "once_at", at: "ISO8601" }
  if (trigger.type === 'once_at') {
    try {
      return `Once at ${new Date(trigger.at).toLocaleString(undefined, { month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit' })}`;
    } catch {
      return 'Once';
    }
  }
  // Event-driven trigger: { type: "event_driven", event_filter: { event_kinds: [...] }, debounce_seconds: N }
  if (trigger.type === 'event_driven') {
    const kinds: string[] = trigger.event_filter?.event_kinds ?? [];
    const kind = kinds.length > 0 ? kinds[0] : 'event';
    return `On ${kind}`;
  }
  return 'Custom';
}

// ---------------------------------------------------------------------------
// SchedulePanel (collapsible rail)
// ---------------------------------------------------------------------------

interface SchedulePanelProps {
  schedules: ScheduleInfo[];
  collapsed: boolean;
  onToggleCollapse: () => void;
  onPause: (id: string) => void;
  onResume: (id: string) => void;
  onTriggerNow: (id: string) => void;
  onDelete: (id: string) => void;
  onCreateNew: () => void;
}

export function SchedulePanel({
  schedules,
  collapsed,
  onToggleCollapse,
  onPause,
  onResume,
  onTriggerNow,
  onDelete,
  onCreateNew,
}: SchedulePanelProps) {
  const activeCount = schedules.filter(s => s.state.toLowerCase() === 'armed' || s.state.toLowerCase() === 'running').length;

  if (collapsed) {
    return (
      <div className="w-8 border-l border-surface-border/50 bg-surface-canvas/80 hidden md:flex flex-col items-center py-4 relative">
        <button
          onClick={onToggleCollapse}
          className="
            absolute top-4 left-0 right-0 mx-auto
            w-6 h-6 rounded-full
            bg-accent-primary/20 hover:bg-accent-primary/30
            border border-accent-primary/50
            flex items-center justify-center
            transition-all duration-200
            text-accent-primary
          "
          title="Expand schedules"
        >
          <ChevronLeft className="w-3 h-3" />
        </button>
        
        <div className="mt-16 flex-1 flex items-center justify-center">
          <div className="transform -rotate-90 whitespace-nowrap text-xs text-ui-muted flex items-center gap-2">
            <Clock className="w-3 h-3" />
            <span>{activeCount}/{schedules.length}</span>
          </div>
        </div>
      </div>
    );
  }

  return (
    <div className="w-72 border-l border-surface-border/50 bg-surface-canvas/80 hidden md:flex flex-col relative">
      {/* Header */}
      <div className="px-4 py-2 border-b border-surface-border/50 bg-surface-elevated/40 flex items-center justify-between">
        <div className="flex items-center gap-2">
          <Clock className="w-4 h-4 text-accent-primary" />
          <span className="text-sm font-semibold text-ui-primary">Schedules</span>
          <span className="text-xs text-ui-muted ml-1">
            {activeCount}/{schedules.length}
          </span>
        </div>
        
        <div className="flex items-center gap-1">
          <button
            onClick={onCreateNew}
            className="
              w-5 h-5 rounded
              bg-accent-primary/20 hover:bg-accent-primary/30
              border border-accent-primary/50
              flex items-center justify-center
              transition-all duration-200
              text-accent-primary
            "
            title="Create new schedule"
          >
            <Plus className="w-3 h-3" />
          </button>
          <button
            onClick={onToggleCollapse}
            className="
              w-5 h-5 rounded
              bg-surface-canvas/40 hover:bg-accent-primary/20
              border border-surface-border/50 hover:border-accent-primary/50
              flex items-center justify-center
              transition-all duration-200
              text-ui-secondary hover:text-accent-primary
            "
            title="Collapse schedules"
          >
            <ChevronRight className="w-3 h-3" />
          </button>
        </div>
      </div>

      {/* Schedule list */}
      <div className="flex-1 overflow-y-auto px-2 py-3 space-y-1.5">
        {schedules.length === 0 ? (
          <div className="text-center py-8 text-ui-muted text-xs">
            <Clock className="w-6 h-6 mx-auto mb-2 opacity-40" />
            <p>No schedules</p>
            <button
              onClick={onCreateNew}
              className="mt-2 text-accent-primary hover:underline"
            >
              Create one
            </button>
          </div>
        ) : (
          schedules.map((schedule) => (
            <ScheduleRow
              key={schedule.public_id}
              schedule={schedule}
              onPause={onPause}
              onResume={onResume}
              onTriggerNow={onTriggerNow}
              onDelete={onDelete}
            />
          ))
        )}
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------
// ScheduleRow
// ---------------------------------------------------------------------------

interface ScheduleRowProps {
  schedule: ScheduleInfo;
  onPause: (id: string) => void;
  onResume: (id: string) => void;
  onTriggerNow: (id: string) => void;
  onDelete: (id: string) => void;
}

const ScheduleRow = memo(function ScheduleRow({
  schedule,
  onPause,
  onResume,
  onTriggerNow,
  onDelete,
}: ScheduleRowProps) {
  const [confirmDelete, setConfirmDelete] = useState(false);
  const style = getStateStyle(schedule.state);
  const isPaused = schedule.state.toLowerCase() === 'paused';
  const isActive = schedule.state.toLowerCase() === 'armed' || schedule.state.toLowerCase() === 'running';

  const handleDelete = useCallback(() => {
    if (confirmDelete) {
      onDelete(schedule.public_id);
      setConfirmDelete(false);
    } else {
      setConfirmDelete(true);
      setTimeout(() => setConfirmDelete(false), 3000);
    }
  }, [confirmDelete, onDelete, schedule.public_id]);

  return (
    <div className="relative pl-3 pr-2 py-2 rounded-md bg-surface-elevated/30 border border-transparent hover:bg-surface-elevated/50 transition-all duration-200 group">
      {/* State color bar */}
      <div className={`absolute left-0 top-0 bottom-0 w-1 rounded-l-md ${style.dotClass}`} />
      
      {/* Top: trigger description + state badge */}
      <div className="flex items-center justify-between gap-1 mb-1">
        <span className="text-xs font-medium text-ui-primary truncate flex-1">
          {describeTrigger(schedule.trigger)}
        </span>
        <span className={`text-[10px] font-medium ${style.textClass} flex items-center gap-1`}>
          <span className={`w-1.5 h-1.5 rounded-full ${style.dotClass}`} />
          {style.label}
        </span>
      </div>
      
      {/* Meta: run count, next run, failures */}
      <div className="flex items-center gap-2 text-[10px] text-ui-muted mb-1.5">
        <span>Runs: {schedule.run_count}{schedule.max_runs ? `/${schedule.max_runs}` : ''}</span>
        {schedule.next_run_at && isActive && (
          <span>Next: {formatRelativeTime(schedule.next_run_at)}</span>
        )}
        {schedule.consecutive_failures > 0 && (
          <span className="text-status-warning flex items-center gap-0.5">
            <AlertTriangle className="w-2.5 h-2.5" />
            {schedule.consecutive_failures}
          </span>
        )}
      </div>

      {/* Session ID (truncated) */}
      <div className="text-[10px] text-ui-muted font-mono truncate mb-1.5">
        {schedule.session_public_id.substring(0, 16)}...
      </div>

      {/* Actions — visible on hover */}
      <div className="flex items-center gap-1 opacity-0 group-hover:opacity-100 transition-opacity duration-150">
        {isPaused ? (
          <button
            onClick={() => onResume(schedule.public_id)}
            className="p-1 rounded hover:bg-status-success/20 text-status-success transition-colors"
            title="Resume"
          >
            <Play className="w-3 h-3" />
          </button>
        ) : isActive ? (
          <button
            onClick={() => onPause(schedule.public_id)}
            className="p-1 rounded hover:bg-ui-muted/20 text-ui-muted transition-colors"
            title="Pause"
          >
            <Pause className="w-3 h-3" />
          </button>
        ) : null}

        <button
          onClick={() => onTriggerNow(schedule.public_id)}
          className="p-1 rounded hover:bg-accent-primary/20 text-accent-primary transition-colors"
          title="Trigger now"
        >
          <Zap className="w-3 h-3" />
        </button>

        <button
          onClick={handleDelete}
          className={`p-1 rounded transition-colors ${
            confirmDelete
              ? 'bg-status-warning/20 text-status-warning'
              : 'hover:bg-status-warning/20 text-ui-muted hover:text-status-warning'
          }`}
          title={confirmDelete ? 'Click again to confirm delete' : 'Delete'}
        >
          <Trash2 className="w-3 h-3" />
        </button>
      </div>
    </div>
  );
});
