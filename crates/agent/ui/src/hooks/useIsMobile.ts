import { useState, useEffect } from 'react';

const DEFAULT_BREAKPOINT = 768;

/**
 * Returns `true` when `window.innerWidth` is strictly below `breakpoint` (default 768px).
 * Listens for the `resize` event so the value stays in sync with the viewport.
 */
export function useIsMobile(breakpoint: number = DEFAULT_BREAKPOINT): boolean {
  const [isMobile, setIsMobile] = useState(() => {
    if (typeof window === 'undefined') return false;
    return window.innerWidth < breakpoint;
  });

  useEffect(() => {
    const handleResize = () => {
      setIsMobile(window.innerWidth < breakpoint);
    };
    handleResize(); // sync on mount / breakpoint change
    window.addEventListener('resize', handleResize);
    return () => window.removeEventListener('resize', handleResize);
  }, [breakpoint]);

  return isMobile;
}
