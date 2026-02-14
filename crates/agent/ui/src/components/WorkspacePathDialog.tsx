import { useEffect, useState } from 'react';
import * as Dialog from '@radix-ui/react-dialog';

interface WorkspacePathDialogProps {
  open: boolean;
  defaultValue: string;
  onSubmit: (value: string) => void;
  onCancel: () => void;
}

export function WorkspacePathDialog({
  open,
  defaultValue,
  onSubmit,
  onCancel,
}: WorkspacePathDialogProps) {
  const [workspacePath, setWorkspacePath] = useState(defaultValue);

  useEffect(() => {
    if (open) {
      setWorkspacePath(defaultValue);
    }
  }, [open, defaultValue]);

  return (
    <Dialog.Root
      open={open}
      onOpenChange={(nextOpen) => {
        if (!nextOpen) {
          onCancel();
        }
      }}
    >
      <Dialog.Portal>
        <Dialog.Overlay className="fixed inset-0 z-50 bg-surface-canvas/75 backdrop-blur-sm animate-fade-in" />
        <Dialog.Content className="fixed left-1/2 top-1/2 z-50 w-[min(92vw,560px)] -translate-x-1/2 -translate-y-1/2 rounded-xl border-2 border-accent-primary/30 bg-surface-elevated shadow-[0_0_40px_rgba(var(--accent-primary-rgb),0.25)] p-5">
          <Dialog.Title className="text-base font-semibold text-accent-primary">
            Start New Session
          </Dialog.Title>
          <Dialog.Description className="mt-1 text-sm text-ui-secondary">
            Set a workspace path for this session, or leave it blank.
          </Dialog.Description>

          <form
            className="mt-4 space-y-4"
            onSubmit={(event) => {
              event.preventDefault();
              onSubmit(workspacePath);
            }}
          >
            <div className="space-y-2">
              <label htmlFor="workspace-path-input" className="text-xs font-mono text-ui-muted">
                Workspace path (optional)
              </label>
              <input
                id="workspace-path-input"
                type="text"
                value={workspacePath}
                onChange={(event) => setWorkspacePath(event.target.value)}
                placeholder="/path/to/workspace"
                autoFocus
                className="w-full rounded-lg border border-surface-border bg-surface-canvas px-3 py-2 text-sm text-ui-primary placeholder:text-ui-muted focus:border-accent-primary/60 focus:outline-none"
              />
            </div>

            <div className="flex items-center justify-end gap-2">
              <button
                type="button"
                onClick={onCancel}
                className="rounded-lg border border-surface-border bg-surface-canvas px-3 py-1.5 text-sm text-ui-secondary transition-colors hover:border-surface-border/80 hover:bg-surface-elevated"
              >
                Cancel
              </button>
              <button
                type="submit"
                className="rounded-lg border border-accent-primary/50 bg-accent-primary/12 px-3 py-1.5 text-sm text-accent-primary transition-colors hover:bg-accent-primary/20"
              >
                Start Session
              </button>
            </div>
          </form>
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  );
}
