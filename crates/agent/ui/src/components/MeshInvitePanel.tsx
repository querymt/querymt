import { useMemo, useState } from 'react';
import * as Popover from '@radix-ui/react-popover';
import { QrCode, Plus, RefreshCw, Ban } from 'lucide-react';
import type { MeshInviteInfo, MeshInviteCreated } from '../types';

interface MeshInvitePanelProps {
  connected: boolean;
  meshInvites: MeshInviteInfo[];
  lastCreatedMeshInvite: MeshInviteCreated | null;
  createMeshInvite: (opts?: { meshName?: string; ttl?: string; maxUses?: number }) => void;
  listMeshInvites: () => void;
  revokeMeshInvite: (inviteId: string) => void;
}

export function MeshInvitePanel({
  connected,
  meshInvites,
  lastCreatedMeshInvite,
  createMeshInvite,
  listMeshInvites,
  revokeMeshInvite,
}: MeshInvitePanelProps) {
  const [open, setOpen] = useState(false);
  const [meshName, setMeshName] = useState('');
  const [ttl, setTtl] = useState('24h');
  const [maxUses, setMaxUses] = useState('1');

  const pendingInvites = useMemo(
    () => meshInvites.filter((inv) => inv.status === 'pending'),
    [meshInvites]
  );

  const onCreate = () => {
    const parsedUses = Number.parseInt(maxUses, 10);
    createMeshInvite({
      meshName: meshName.trim().length > 0 ? meshName.trim() : undefined,
      ttl: ttl.trim().length > 0 ? ttl.trim() : undefined,
      maxUses: Number.isFinite(parsedUses) ? parsedUses : 1,
    });
    setMeshName('');
  };

  if (!connected) {
    return null;
  }

  return (
    <Popover.Root open={open} onOpenChange={setOpen}>
      <Popover.Trigger asChild>
        <button
          type="button"
          title="Mesh invites"
          className="relative h-8 inline-flex items-center gap-1.5 px-2.5 rounded-lg border border-surface-border bg-surface-canvas/60 hover:border-accent-secondary/40 hover:bg-surface-elevated/50 transition-colors"
        >
          <QrCode className="w-3.5 h-3.5 text-accent-secondary" />
          <span className="text-xs font-mono text-accent-secondary">{pendingInvites.length}</span>
        </button>
      </Popover.Trigger>

      <Popover.Portal>
        <Popover.Content
          side="bottom"
          align="end"
          sideOffset={8}
          className="z-50 w-[34rem] rounded-xl border border-surface-border bg-surface-elevated shadow-[0_8px_30px_rgba(0,0,0,0.4),0_0_20px_rgba(var(--accent-secondary-rgb),0.1)] animate-fade-in"
        >
          <div className="flex items-center gap-2 px-4 py-3 border-b border-surface-border/50">
            <QrCode className="w-3.5 h-3.5 text-accent-secondary" />
            <span className="text-xs font-semibold text-accent-secondary uppercase tracking-wider">
              Mesh Invites
            </span>
            <button
              type="button"
              onClick={listMeshInvites}
              className="ml-auto p-1 rounded hover:bg-surface-canvas/50 text-ui-muted hover:text-ui-primary"
              title="Refresh"
            >
              <RefreshCw className="w-3.5 h-3.5" />
            </button>
          </div>

          <div className="px-4 py-3 border-b border-surface-border/50 space-y-2">
            <div className="grid grid-cols-3 gap-2">
              <input
                value={meshName}
                onChange={(e) => setMeshName(e.target.value)}
                placeholder="Mesh name (optional)"
                className="col-span-1 h-8 px-2 rounded border border-surface-border bg-surface-canvas/50 text-xs"
              />
              <input
                value={ttl}
                onChange={(e) => setTtl(e.target.value)}
                placeholder="TTL (24h, 7d, none)"
                className="col-span-1 h-8 px-2 rounded border border-surface-border bg-surface-canvas/50 text-xs"
              />
              <input
                value={maxUses}
                onChange={(e) => setMaxUses(e.target.value)}
                placeholder="Uses (1, 10, 0)"
                className="col-span-1 h-8 px-2 rounded border border-surface-border bg-surface-canvas/50 text-xs"
              />
            </div>
            <button
              type="button"
              onClick={onCreate}
              className="h-8 inline-flex items-center gap-1.5 px-3 rounded border border-accent-secondary/40 text-accent-secondary hover:bg-accent-secondary/10 text-xs"
            >
              <Plus className="w-3.5 h-3.5" />
              Create Invite
            </button>
          </div>

          {lastCreatedMeshInvite && (
            <div className="px-4 py-3 border-b border-surface-border/50 space-y-2">
              <div className="text-[11px] text-ui-muted">Last created</div>
              <div className="text-[11px] font-mono break-all text-ui-primary">
                {lastCreatedMeshInvite.url}
              </div>
              {lastCreatedMeshInvite.qr_code && (
                <pre className="text-[8px] leading-[8px] font-mono p-2 rounded bg-surface-canvas/50 overflow-x-auto text-ui-primary">
                  {lastCreatedMeshInvite.qr_code}
                </pre>
              )}
            </div>
          )}

          <div className="py-2 max-h-56 overflow-y-auto custom-scrollbar">
            {pendingInvites.length === 0 ? (
              <div className="px-4 py-4 text-xs text-ui-muted">No pending invites.</div>
            ) : (
              pendingInvites.map((invite) => (
                <div key={invite.invite_id} className="px-4 py-2.5 border-b border-surface-border/30 last:border-b-0">
                  <div className="flex items-center gap-2">
                    <span className="text-xs font-mono text-ui-primary truncate">{invite.invite_id}</span>
                    <span className="text-[10px] px-1.5 py-0.5 rounded bg-accent-secondary/15 text-accent-secondary border border-accent-secondary/25">
                      uses left: {invite.uses_remaining}
                    </span>
                    <button
                      type="button"
                      onClick={() => revokeMeshInvite(invite.invite_id)}
                      className="ml-auto p-1 rounded hover:bg-status-warning/10 text-ui-muted hover:text-status-warning"
                      title="Revoke invite"
                    >
                      <Ban className="w-3.5 h-3.5" />
                    </button>
                  </div>
                  {invite.mesh_name && (
                    <div className="text-[11px] text-ui-muted mt-0.5">{invite.mesh_name}</div>
                  )}
                </div>
              ))
            )}
          </div>

          <Popover.Arrow className="fill-surface-border" />
        </Popover.Content>
      </Popover.Portal>
    </Popover.Root>
  );
}
