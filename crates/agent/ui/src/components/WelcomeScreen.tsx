/**
 * WelcomeScreen - Shared "no sessions" welcome state with "Start New Session" button.
 * Used by both HomePage and ChatView to avoid duplication.
 */

import { Plus, Loader } from 'lucide-react';

interface WelcomeScreenProps {
  onNewSession: () => void;
  disabled: boolean;
  loading: boolean;
}

export function WelcomeScreen({ onNewSession, disabled, loading }: WelcomeScreenProps) {
  return (
    <div className="text-center space-y-4 animate-fade-in">
      <p className="text-sm text-ui-muted">Welcome to QueryMT</p>
      <button
        onClick={onNewSession}
        disabled={disabled}
        className="
          px-6 py-3 rounded-full font-medium text-sm
          bg-accent-primary text-surface-canvas
          hover:opacity-90
          disabled:opacity-30 disabled:cursor-not-allowed
          transition-all duration-150
          flex items-center justify-center gap-2 mx-auto
        "
      >
        {loading ? (
          <>
            <Loader className="w-4 h-4 animate-spin" />
            <span>Creating...</span>
          </>
        ) : (
          <>
            <Plus className="w-4 h-4" />
            <span>New Session</span>
          </>
        )}
      </button>
      <p className="text-[11px] text-ui-muted">
        <kbd className="px-1.5 py-0.5 bg-surface-canvas border border-surface-border/60 rounded font-mono text-[10px]">
          {navigator.platform.includes('Mac') ? '\u2318+X N' : 'Ctrl+X N'}
        </kbd>
      </p>
    </div>
  );
}
