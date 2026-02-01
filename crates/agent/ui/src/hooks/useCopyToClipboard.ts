import { useState, useCallback, useRef, useEffect } from 'react';
import { copyToClipboard } from '../utils/clipboard';

interface UseCopyToClipboardOptions {
  /** Duration in milliseconds before resetting copiedValue. Default: 2000ms */
  resetDelay?: number;
}

/**
 * Hook for copying text to clipboard with automatic state reset.
 * 
 * @param options - Configuration options
 * @returns Object with copiedValue (the key of the last copied item) and copy function
 * 
 * @example
 * const { copiedValue, copy } = useCopyToClipboard();
 * const handleCopy = () => copy('text to copy', 'section-id');
 * const isCopied = copiedValue === 'section-id';
 */
export function useCopyToClipboard(options: UseCopyToClipboardOptions = {}) {
  const { resetDelay = 2000 } = options;
  const [copiedValue, setCopiedValue] = useState<string | null>(null);
  const timeoutRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  // Cleanup timeout on unmount
  useEffect(() => {
    return () => {
      if (timeoutRef.current) {
        clearTimeout(timeoutRef.current);
      }
    };
  }, []);

  const copy = useCallback(
    async (text: string, key?: string) => {
      const success = await copyToClipboard(text);
      
      if (success) {
        // Clear any existing timeout
        if (timeoutRef.current) {
          clearTimeout(timeoutRef.current);
        }

        // Set the copied value key
        setCopiedValue(key ?? text);

        // Auto-reset after delay
        timeoutRef.current = setTimeout(() => {
          setCopiedValue(null);
          timeoutRef.current = null;
        }, resetDelay);
      }
    },
    [resetDelay]
  );

  return { copiedValue, copy };
}
