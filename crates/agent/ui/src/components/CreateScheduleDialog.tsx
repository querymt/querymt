import { useEffect, useState } from 'react';
import * as Dialog from '@radix-ui/react-dialog';
import { Clock, Zap, CalendarClock } from 'lucide-react';

type TriggerType = 'interval' | 'event' | 'once';

interface CreateScheduleDialogProps {
  open: boolean;
  sessionId: string | null;
  onOpenChange: (open: boolean) => void;
  onCreate: (
    sessionId: string,
    prompt: string,
    trigger: any,
    opts?: { maxSteps?: number; maxCostUsd?: number; maxRuns?: number },
  ) => void;
}

export function CreateScheduleDialog({
  open,
  sessionId,
  onOpenChange,
  onCreate,
}: CreateScheduleDialogProps) {
  const [triggerType, setTriggerType] = useState<TriggerType>('interval');
  const [prompt, setPrompt] = useState('');

  // Interval fields
  const [intervalValue, setIntervalValue] = useState('5');
  const [intervalUnit, setIntervalUnit] = useState<'seconds' | 'minutes' | 'hours'>('minutes');

  // Event fields
  const [eventKind, setEventKind] = useState('');
  const [eventThreshold, setEventThreshold] = useState('1');
  const [eventDebounceSeconds, setEventDebounceSeconds] = useState('30');

  // Once-at fields
  const [onceAtDatetime, setOnceAtDatetime] = useState('');

  // Limits
  const [maxRuns, setMaxRuns] = useState('');
  const [showLimits, setShowLimits] = useState(false);

  // Reset state when dialog opens
  useEffect(() => {
    if (open) {
      setTriggerType('interval');
      setPrompt('');
      setIntervalValue('5');
      setIntervalUnit('minutes');
      setEventKind('');
      setEventThreshold('1');
      setEventDebounceSeconds('30');
      // Default to 1 hour from now, formatted for datetime-local input
      const defaultAt = new Date(Date.now() + 3600_000);
      defaultAt.setSeconds(0, 0);
      const pad = (n: number) => String(n).padStart(2, '0');
      setOnceAtDatetime(
        `${defaultAt.getFullYear()}-${pad(defaultAt.getMonth() + 1)}-${pad(defaultAt.getDate())}T${pad(defaultAt.getHours())}:${pad(defaultAt.getMinutes())}`,
      );
      setMaxRuns('');
      setShowLimits(false);
    }
  }, [open]);

  const handleSubmit = (e: React.FormEvent) => {
    e.preventDefault();
    if (!sessionId || !prompt.trim()) return;

    let trigger: any;
    if (triggerType === 'interval') {
      let seconds = parseInt(intervalValue, 10) || 0;
      if (intervalUnit === 'minutes') seconds *= 60;
      if (intervalUnit === 'hours') seconds *= 3600;
      if (seconds <= 0) return;
      trigger = { type: 'interval', seconds };
    } else if (triggerType === 'once') {
      const at = new Date(onceAtDatetime);
      if (isNaN(at.getTime()) || at.getTime() <= Date.now()) return;
      trigger = { type: 'once_at', at: at.toISOString() };
    } else {
      if (!eventKind.trim()) return;
      trigger = {
        type: 'event_driven',
        event_filter: {
          event_kinds: [eventKind.trim()],
          threshold: parseInt(eventThreshold, 10) || 1,
          session_public_id: null,
        },
        debounce_seconds: parseInt(eventDebounceSeconds, 10) || 30,
      };
    }

    const opts: { maxRuns?: number } = {};
    if (triggerType === 'once') {
      opts.maxRuns = 1;
    }
    const maxRunsParsed = parseInt(maxRuns, 10);
    if (maxRunsParsed > 0) opts.maxRuns = maxRunsParsed;

    onCreate(sessionId, prompt.trim(), trigger, opts);
    onOpenChange(false);
  };

  const isValid = (() => {
    if (!sessionId || !prompt.trim()) return false;
    if (triggerType === 'interval') {
      const val = parseInt(intervalValue, 10);
      return val > 0;
    }
    if (triggerType === 'once') {
      const at = new Date(onceAtDatetime);
      return !isNaN(at.getTime()) && at.getTime() > Date.now();
    }
    return eventKind.trim().length > 0;
  })();

  return (
    <Dialog.Root open={open} onOpenChange={onOpenChange}>
      <Dialog.Portal>
        <Dialog.Overlay className="fixed inset-0 z-50 bg-surface-canvas/85 animate-fade-in" />
        <Dialog.Content className="fixed left-1/2 top-1/2 z-50 w-[min(92vw,520px)] -translate-x-1/2 -translate-y-1/2 rounded-xl border-2 border-accent-primary/30 bg-surface-elevated shadow-[0_0_40px_rgba(var(--accent-primary-rgb),0.25)] p-5">
          <Dialog.Title className="text-base font-semibold text-accent-primary">
            Create Schedule
          </Dialog.Title>
          <Dialog.Description className="mt-1 text-sm text-ui-secondary">
            Set up an autonomous scheduled task for the current session.
          </Dialog.Description>

          <form className="mt-4 space-y-4" onSubmit={handleSubmit}>
            {/* Trigger type selector */}
            <div className="space-y-2">
              <label className="text-xs font-mono text-ui-muted">Trigger type</label>
              <div className="flex gap-2">
                <button
                  type="button"
                  onClick={() => setTriggerType('interval')}
                  className={`flex-1 inline-flex items-center justify-center gap-1.5 px-3 py-2 rounded-lg border text-sm transition-all ${
                    triggerType === 'interval'
                      ? 'border-accent-primary bg-accent-primary/15 text-accent-primary shadow-[0_0_8px_rgba(var(--accent-primary-rgb),0.2)]'
                      : 'border-surface-border bg-surface-canvas text-ui-secondary hover:border-surface-border/80 hover:bg-surface-elevated'
                  }`}
                >
                  <Clock className="w-3.5 h-3.5" />
                  Interval
                </button>
                <button
                  type="button"
                  onClick={() => setTriggerType('once')}
                  className={`flex-1 inline-flex items-center justify-center gap-1.5 px-3 py-2 rounded-lg border text-sm transition-all ${
                    triggerType === 'once'
                      ? 'border-accent-tertiary bg-accent-tertiary/15 text-accent-tertiary shadow-[0_0_8px_rgba(var(--accent-tertiary-rgb),0.2)]'
                      : 'border-surface-border bg-surface-canvas text-ui-secondary hover:border-surface-border/80 hover:bg-surface-elevated'
                  }`}
                >
                  <CalendarClock className="w-3.5 h-3.5" />
                  Once
                </button>
                <button
                  type="button"
                  onClick={() => setTriggerType('event')}
                  className={`flex-1 inline-flex items-center justify-center gap-1.5 px-3 py-2 rounded-lg border text-sm transition-all ${
                    triggerType === 'event'
                      ? 'border-accent-secondary bg-accent-secondary/15 text-accent-secondary shadow-[0_0_8px_rgba(var(--accent-secondary-rgb),0.2)]'
                      : 'border-surface-border bg-surface-canvas text-ui-secondary hover:border-surface-border/80 hover:bg-surface-elevated'
                  }`}
                >
                  <Zap className="w-3.5 h-3.5" />
                  Event-Driven
                </button>
              </div>
            </div>

            {/* Trigger config */}
            {triggerType === 'interval' ? (
              <div className="space-y-2">
                <label className="text-xs font-mono text-ui-muted">Run every</label>
                <div className="flex gap-2">
                  <input
                    type="number"
                    min="1"
                    value={intervalValue}
                    onChange={(e) => setIntervalValue(e.target.value)}
                    className="w-24 rounded-lg border border-surface-border bg-surface-canvas px-3 py-2 text-sm text-ui-primary focus:border-accent-primary/60 focus:outline-none"
                  />
                  <select
                    value={intervalUnit}
                    onChange={(e) => setIntervalUnit(e.target.value as any)}
                    className="rounded-lg border border-surface-border bg-surface-canvas px-3 py-2 text-sm text-ui-primary focus:border-accent-primary/60 focus:outline-none"
                  >
                    <option value="seconds">seconds</option>
                    <option value="minutes">minutes</option>
                    <option value="hours">hours</option>
                  </select>
                </div>
              </div>
            ) : triggerType === 'once' ? (
              <div className="space-y-2">
                <label className="text-xs font-mono text-ui-muted">Run at</label>
                <input
                  type="datetime-local"
                  value={onceAtDatetime}
                  onChange={(e) => setOnceAtDatetime(e.target.value)}
                  min={new Date().toISOString().slice(0, 16)}
                  className="w-full rounded-lg border border-surface-border bg-surface-canvas px-3 py-2 text-sm text-ui-primary focus:border-accent-primary/60 focus:outline-none"
                />
                <p className="text-[10px] text-ui-muted">
                  Task will run exactly once at this time, then complete.
                </p>
              </div>
            ) : (
              <div className="space-y-3">
                <div className="space-y-2">
                  <label className="text-xs font-mono text-ui-muted">Event kind</label>
                  <input
                    type="text"
                    value={eventKind}
                    onChange={(e) => setEventKind(e.target.value)}
                    placeholder="e.g., file_changed, webhook_received"
                    className="w-full rounded-lg border border-surface-border bg-surface-canvas px-3 py-2 text-sm text-ui-primary placeholder:text-ui-muted focus:border-accent-primary/60 focus:outline-none"
                  />
                </div>
                <div className="flex gap-3">
                  <div className="space-y-1 flex-1">
                    <label className="text-[10px] font-mono text-ui-muted">Threshold</label>
                    <input
                      type="number"
                      min="1"
                      value={eventThreshold}
                      onChange={(e) => setEventThreshold(e.target.value)}
                      className="w-full rounded-lg border border-surface-border bg-surface-canvas px-3 py-1.5 text-sm text-ui-primary focus:border-accent-primary/60 focus:outline-none"
                    />
                  </div>
                  <div className="space-y-1 flex-1">
                    <label className="text-[10px] font-mono text-ui-muted">Debounce (sec)</label>
                    <input
                      type="number"
                      min="0"
                      value={eventDebounceSeconds}
                      onChange={(e) => setEventDebounceSeconds(e.target.value)}
                      className="w-full rounded-lg border border-surface-border bg-surface-canvas px-3 py-1.5 text-sm text-ui-primary focus:border-accent-primary/60 focus:outline-none"
                    />
                  </div>
                </div>
              </div>
            )}

            {/* Prompt */}
            <div className="space-y-2">
              <label className="text-xs font-mono text-ui-muted">Prompt</label>
              <textarea
                value={prompt}
                onChange={(e) => setPrompt(e.target.value)}
                placeholder="What should the agent do on each run?"
                rows={3}
                autoFocus
                className="w-full rounded-lg border border-surface-border bg-surface-canvas px-3 py-2 text-sm text-ui-primary placeholder:text-ui-muted focus:border-accent-primary/60 focus:outline-none resize-none"
              />
            </div>

            {/* Optional limits */}
            <div>
              <button
                type="button"
                onClick={() => setShowLimits(!showLimits)}
                className="text-xs text-ui-muted hover:text-accent-primary transition-colors"
              >
                {showLimits ? 'Hide limits' : 'Show limits'}
              </button>
              {showLimits && (
                <div className="mt-2 space-y-2">
                  <div className="space-y-1">
                    <label className="text-[10px] font-mono text-ui-muted">Max runs (0 = unlimited)</label>
                    <input
                      type="number"
                      min="0"
                      value={maxRuns}
                      onChange={(e) => setMaxRuns(e.target.value)}
                      placeholder="Unlimited"
                      className="w-full rounded-lg border border-surface-border bg-surface-canvas px-3 py-1.5 text-sm text-ui-primary placeholder:text-ui-muted focus:border-accent-primary/60 focus:outline-none"
                    />
                  </div>
                </div>
              )}
            </div>

            {/* Actions */}
            <div className="flex items-center justify-end gap-2 pt-1">
              <button
                type="button"
                onClick={() => onOpenChange(false)}
                className="rounded-lg border border-surface-border bg-surface-canvas px-3 py-1.5 text-sm text-ui-secondary transition-colors hover:border-surface-border/80 hover:bg-surface-elevated"
              >
                Cancel
              </button>
              <button
                type="submit"
                disabled={!isValid}
                className={`rounded-lg border px-3 py-1.5 text-sm transition-colors ${
                  isValid
                    ? 'border-accent-primary/50 bg-accent-primary/12 text-accent-primary hover:bg-accent-primary/20'
                    : 'border-surface-border bg-surface-canvas text-ui-muted cursor-not-allowed'
                }`}
              >
                Create Schedule
              </button>
            </div>
          </form>
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  );
}
