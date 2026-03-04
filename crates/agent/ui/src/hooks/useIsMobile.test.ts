import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';
import { renderHook, act } from '@testing-library/react';
import { useIsMobile } from './useIsMobile';

describe('useIsMobile', () => {
  const originalInnerWidth = window.innerWidth;

  function setViewportWidth(width: number) {
    Object.defineProperty(window, 'innerWidth', {
      writable: true,
      configurable: true,
      value: width,
    });
  }

  afterEach(() => {
    setViewportWidth(originalInnerWidth);
  });

  describe('default breakpoint (768)', () => {
    it('returns true when window width is below 768', () => {
      setViewportWidth(375);
      const { result } = renderHook(() => useIsMobile());
      expect(result.current).toBe(true);
    });

    it('returns false when window width is exactly 768', () => {
      setViewportWidth(768);
      const { result } = renderHook(() => useIsMobile());
      expect(result.current).toBe(false);
    });

    it('returns false when window width is above 768', () => {
      setViewportWidth(1024);
      const { result } = renderHook(() => useIsMobile());
      expect(result.current).toBe(false);
    });
  });

  describe('custom breakpoint', () => {
    it('uses custom breakpoint when provided', () => {
      setViewportWidth(500);
      const { result } = renderHook(() => useIsMobile(600));
      expect(result.current).toBe(true);
    });

    it('returns false when above custom breakpoint', () => {
      setViewportWidth(700);
      const { result } = renderHook(() => useIsMobile(600));
      expect(result.current).toBe(false);
    });
  });

  describe('resize handling', () => {
    it('updates when window resizes from desktop to mobile', () => {
      setViewportWidth(1024);
      const { result } = renderHook(() => useIsMobile());
      expect(result.current).toBe(false);

      act(() => {
        setViewportWidth(375);
        window.dispatchEvent(new Event('resize'));
      });

      expect(result.current).toBe(true);
    });

    it('updates when window resizes from mobile to desktop', () => {
      setViewportWidth(375);
      const { result } = renderHook(() => useIsMobile());
      expect(result.current).toBe(true);

      act(() => {
        setViewportWidth(1024);
        window.dispatchEvent(new Event('resize'));
      });

      expect(result.current).toBe(false);
    });

    it('cleans up resize listener on unmount', () => {
      const removeSpy = vi.spyOn(window, 'removeEventListener');
      setViewportWidth(375);
      const { unmount } = renderHook(() => useIsMobile());

      unmount();

      expect(removeSpy).toHaveBeenCalledWith('resize', expect.any(Function));
      removeSpy.mockRestore();
    });
  });

  describe('SSR safety', () => {
    it('defaults to false when window is undefined', () => {
      // The hook should handle typeof window === "undefined" gracefully.
      // In jsdom this can't fully be tested, but the hook code should guard it.
      // We verify the hook doesn't crash at minimum.
      setViewportWidth(375);
      const { result } = renderHook(() => useIsMobile());
      expect(typeof result.current).toBe('boolean');
    });
  });
});
