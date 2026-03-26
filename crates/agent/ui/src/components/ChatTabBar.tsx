/**
 * ChatTabBar - Chat/Delegations tab switcher.
 * Extracted from ChatView to reduce its size.
 */

import type { DelegationGroupInfo } from '../types';

interface ChatTabBarProps {
  activeTimelineView: 'chat' | 'delegations';
  setActiveTimelineView: (view: 'chat' | 'delegations') => void;
  setDelegationDrawerOpen: (open: boolean) => void;
  delegations: DelegationGroupInfo[];
  activeDelegationId: string | null;
  setActiveDelegationId: (id: string | null) => void;
  hasDelegations: boolean;
}

export function ChatTabBar({
  activeTimelineView,
  setActiveTimelineView,
  setDelegationDrawerOpen,
  delegations,
  activeDelegationId,
  setActiveDelegationId,
  hasDelegations,
}: ChatTabBarProps) {
  return (
    <div className="px-3 md:px-6 py-2 border-b border-surface-border/60 bg-surface-elevated/40 flex items-center gap-2">
      <button
        type="button"
        onClick={() => {
          setActiveTimelineView('chat');
          setDelegationDrawerOpen(false);
        }}
        className={`text-xs uppercase tracking-wider px-3 py-1.5 rounded-full border transition-colors ${
          activeTimelineView === 'chat'
            ? 'border-accent-primary text-accent-primary bg-accent-primary/10'
            : 'border-surface-border/60 text-ui-secondary hover:border-accent-primary/40 hover:text-ui-primary'
        }`}
      >
        Chat
      </button>
      <button
        type="button"
        onClick={() => {
          setActiveTimelineView('delegations');
          if (delegations.length > 0) {
            const currentValid = delegations.some(d => d.id === activeDelegationId);
            if (!activeDelegationId || !currentValid) {
              setActiveDelegationId(delegations[0].id);
            }
          } else {
            setActiveDelegationId(null);
          }
        }}
        className={`text-xs uppercase tracking-wider px-3 py-1.5 rounded-full border transition-colors ${
          activeTimelineView === 'delegations'
            ? 'border-accent-tertiary text-accent-tertiary bg-accent-tertiary/10'
            : 'border-surface-border/60 text-ui-secondary hover:border-accent-tertiary/40 hover:text-ui-primary'
        }`}
      >
        Delegations
        {hasDelegations && (
          <span className="ml-2 text-[10px] text-ui-muted">{delegations.length}</span>
        )}
      </button>
    </div>
  );
}
