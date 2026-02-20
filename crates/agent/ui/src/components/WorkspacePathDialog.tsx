import { useEffect, useState } from 'react';
import * as Dialog from '@radix-ui/react-dialog';
import { Monitor, Globe } from 'lucide-react';
import type { RemoteNodeInfo } from '../types';

interface WorkspacePathDialogProps {
  open: boolean;
  defaultValue: string;
  remoteNodes?: RemoteNodeInfo[];
  onSubmit: (value: string, node: string | null) => void;
  onCancel: () => void;
}

export function WorkspacePathDialog({
  open,
  defaultValue,
  remoteNodes = [],
  onSubmit,
  onCancel,
}: WorkspacePathDialogProps) {
  const [workspacePath, setWorkspacePath] = useState(defaultValue);
  const [selectedNode, setSelectedNode] = useState<string | null>(null);

  useEffect(() => {
    if (open) {
      setWorkspacePath(defaultValue);
      setSelectedNode(null);
    }
  }, [open, defaultValue]);

  const hasRemoteNodes = remoteNodes.length > 0;

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
            {hasRemoteNodes
              ? 'Choose a node and set a workspace path, or leave it blank.'
              : 'Set a workspace path for this session, or leave it blank.'}
          </Dialog.Description>

          <form
            className="mt-4 space-y-4"
            onSubmit={(event) => {
              event.preventDefault();
              onSubmit(workspacePath, selectedNode);
            }}
          >
            {/* Node selector - only shown when remote nodes are available */}
            {hasRemoteNodes && (
              <div className="space-y-2">
                <label className="text-xs font-mono text-ui-muted">
                  Target node
                </label>
                <div className="flex flex-wrap gap-2">
                  {/* Local node pill */}
                  <button
                    type="button"
                    onClick={() => setSelectedNode(null)}
                    className={`inline-flex items-center gap-1.5 px-3 py-1.5 rounded-lg border text-sm transition-all ${
                      selectedNode === null
                        ? 'border-accent-primary bg-accent-primary/15 text-accent-primary shadow-[0_0_8px_rgba(var(--accent-primary-rgb),0.2)]'
                        : 'border-surface-border bg-surface-canvas text-ui-secondary hover:border-surface-border/80 hover:bg-surface-elevated'
                    }`}
                  >
                    <Monitor className="w-3.5 h-3.5" />
                    <span>Local</span>
                  </button>

                  {/* Remote node pills */}
                  {remoteNodes.map((node) => (
                    <button
                      key={node.label}
                      type="button"
                      onClick={() => setSelectedNode(node.label)}
                      title={
                        node.capabilities.length > 0
                          ? `Capabilities: ${node.capabilities.join(', ')}`
                          : `${node.active_sessions} active session${node.active_sessions !== 1 ? 's' : ''}`
                      }
                      className={`inline-flex items-center gap-1.5 px-3 py-1.5 rounded-lg border text-sm transition-all ${
                        selectedNode === node.label
                          ? 'border-accent-secondary bg-accent-secondary/15 text-accent-secondary shadow-[0_0_8px_rgba(var(--accent-secondary-rgb),0.2)]'
                          : 'border-surface-border bg-surface-canvas text-ui-secondary hover:border-surface-border/80 hover:bg-surface-elevated'
                      }`}
                    >
                      <Globe className="w-3.5 h-3.5" />
                      <span>{node.label}</span>
                      {node.active_sessions > 0 && (
                        <span className="text-[10px] px-1 py-0.5 rounded bg-surface-canvas/60 text-ui-muted">
                          {node.active_sessions}
                        </span>
                      )}
                    </button>
                  ))}
                </div>
              </div>
            )}

            <div className="space-y-2">
              <label htmlFor="workspace-path-input" className="text-xs font-mono text-ui-muted">
                Workspace path {selectedNode ? '(on remote machine)' : '(optional)'}
              </label>
              <input
                id="workspace-path-input"
                type="text"
                value={workspacePath}
                onChange={(event) => setWorkspacePath(event.target.value)}
                placeholder={selectedNode ? '/remote/path/to/workspace' : '/path/to/workspace'}
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
                className={`rounded-lg border px-3 py-1.5 text-sm transition-colors ${
                  selectedNode
                    ? 'border-accent-secondary/50 bg-accent-secondary/12 text-accent-secondary hover:bg-accent-secondary/20'
                    : 'border-accent-primary/50 bg-accent-primary/12 text-accent-primary hover:bg-accent-primary/20'
                }`}
              >
                {selectedNode ? `Start on ${selectedNode}` : 'Start Session'}
              </button>
            </div>
          </form>
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  );
}
