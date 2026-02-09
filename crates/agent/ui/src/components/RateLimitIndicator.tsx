/**
 * RateLimitIndicator.tsx
 * 
 * Displays rate limit status with countdown timer and cancel button.
 * Shows: "Rate limited • Retry X/Y in Xs" with pulsing clock icon.
 * Cancel button allows user to abort the wait and cancel the session.
 */

import { Clock, AlertTriangle, X } from 'lucide-react';
import { useEffect } from 'react';
import { useUiStore } from '../store/uiStore';

interface RateLimitIndicatorProps {
  sessionId: string;
  message: string;
  waitSecs: number;
  startedAt: number;
  attempt: number;
  maxAttempts: number;
  remainingSecs: number;
  onCancel: () => void;
}

export function RateLimitIndicator({
  sessionId,
  message,
  attempt,
  maxAttempts,
  remainingSecs,
  onCancel,
}: RateLimitIndicatorProps) {
  const updateRemainingTime = useUiStore(state => state.updateRemainingTime);
  
  // Update countdown timer every second
  useEffect(() => {
    const interval = setInterval(() => {
      updateRemainingTime(sessionId);
    }, 1000);
    
    return () => clearInterval(interval);
  }, [sessionId, updateRemainingTime]);
  
  return (
    <div className="flex items-center justify-between gap-3 px-4 py-3 bg-amber-500/10 border border-amber-500/20 rounded-lg">
      <div className="flex items-center gap-3 flex-1">
        <AlertTriangle className="w-5 h-5 text-amber-600 dark:text-amber-400 flex-shrink-0" />
        <div className="flex flex-col gap-0.5">
          <span className="text-sm font-semibold text-amber-600 dark:text-amber-400">
            Rate Limited
          </span>
          <span className="text-xs text-amber-600/70 dark:text-amber-400/70">
            {message}
          </span>
        </div>
      </div>
      
      <div className="flex items-center gap-3">
        <div className="flex items-center gap-2">
          <Clock className="w-4 h-4 text-amber-600 dark:text-amber-400 animate-pulse" />
          <span className="text-sm font-mono font-medium text-amber-600 dark:text-amber-400">
            Retry {attempt}/{maxAttempts} in {remainingSecs}s
          </span>
        </div>
        
        <button
          onClick={onCancel}
          className="h-8 px-3 gap-2 text-amber-600 hover:text-amber-700 dark:text-amber-400 dark:hover:text-amber-300 hover:bg-amber-500/20 rounded-md transition-colors flex items-center"
          title="Cancel wait and stop request"
        >
          <X className="w-4 h-4" />
          Cancel
        </button>
      </div>
    </div>
  );
}
