import { describe, it, expect, vi, beforeEach } from 'vitest';
import { renderHook, act } from '@testing-library/react';
import { useSessionManager } from './useSessionManager';

// Mock react-router-dom
const mockNavigate = vi.fn();
let mockUrlSessionId: string | undefined = undefined;

vi.mock('react-router-dom', () => ({
  useParams: () => ({ sessionId: mockUrlSessionId }),
  useNavigate: () => mockNavigate,
}));

// Mock UiClientContext
const mockLoadSession = vi.fn();
const mockNewSession = vi.fn();
const mockSessionCreatingRef = { current: false };
let mockServerSessionId: string | null = null;
let mockConnected = true;

vi.mock('../context/UiClientContext', () => ({
  useUiClientContext: () => ({
    sessionId: mockServerSessionId,
    loadSession: mockLoadSession,
    newSession: mockNewSession,
    connected: mockConnected,
    sessionCreatingRef: mockSessionCreatingRef,
  }),
}));

// Mock Zustand store
const mockSaveAndSwitchSession = vi.fn();
vi.mock('../store/uiStore', () => ({
  useUiStore: () => ({
    saveAndSwitchSession: mockSaveAndSwitchSession,
  }),
}));

describe('useSessionManager', () => {
  beforeEach(() => {
    vi.clearAllMocks();
    mockUrlSessionId = undefined;
    mockServerSessionId = null;
    mockConnected = true;
    mockSessionCreatingRef.current = false;
  });

  describe('selectSession', () => {
    it('navigates to the session URL when selecting a different session', () => {
      mockUrlSessionId = 'session-A';
      const { result } = renderHook(() => useSessionManager());
      
      act(() => {
        result.current.selectSession('session-B');
      });
      
      expect(mockNavigate).toHaveBeenCalledWith('/session/session-B');
    });

    it('does not navigate when selecting the already active session', () => {
      mockUrlSessionId = 'session-A';
      const { result } = renderHook(() => useSessionManager());
      
      act(() => {
        result.current.selectSession('session-A');
      });
      
      expect(mockNavigate).not.toHaveBeenCalled();
    });
  });

  describe('createSession', () => {
    it('creates session and navigates to it', async () => {
      mockNewSession.mockResolvedValue('new-session-123');
      const { result } = renderHook(() => useSessionManager());
      
      await act(async () => {
        await result.current.createSession();
      });
      
      expect(mockNewSession).toHaveBeenCalled();
      expect(mockNavigate).toHaveBeenCalledWith('/session/new-session-123', { replace: true });
    });

    it('re-throws when session creation fails', async () => {
      mockNewSession.mockRejectedValue(new Error('cancelled'));
      const { result } = renderHook(() => useSessionManager());
      
      await expect(
        act(async () => {
          await result.current.createSession();
        })
      ).rejects.toThrow('cancelled');
    });
  });

  describe('goHome', () => {
    it('navigates to root', () => {
      mockUrlSessionId = 'session-A';
      const { result } = renderHook(() => useSessionManager());
      
      act(() => {
        result.current.goHome();
      });
      
      expect(mockNavigate).toHaveBeenCalledWith('/');
    });
  });

  describe('URL → Server sync', () => {
    it('calls loadSession when URL has a session ID not matching server', () => {
      mockUrlSessionId = 'session-A';
      mockServerSessionId = null;
      mockConnected = true;
      
      renderHook(() => useSessionManager());
      
      expect(mockLoadSession).toHaveBeenCalledWith('session-A');
    });

    it('does not call loadSession when URL matches server session', () => {
      mockUrlSessionId = 'session-A';
      mockServerSessionId = 'session-A';
      
      renderHook(() => useSessionManager());
      
      expect(mockLoadSession).not.toHaveBeenCalled();
    });

    it('does not call loadSession when not connected', () => {
      mockUrlSessionId = 'session-A';
      mockServerSessionId = null;
      mockConnected = false;
      
      renderHook(() => useSessionManager());
      
      expect(mockLoadSession).not.toHaveBeenCalled();
    });

    it('does not call loadSession during session creation', () => {
      mockUrlSessionId = 'session-A';
      mockServerSessionId = null;
      mockSessionCreatingRef.current = true;
      
      renderHook(() => useSessionManager());
      
      expect(mockLoadSession).not.toHaveBeenCalled();
    });

    it('does not call loadSession when on home page (no URL session)', () => {
      mockUrlSessionId = undefined;
      mockServerSessionId = null;
      
      renderHook(() => useSessionManager());
      
      expect(mockLoadSession).not.toHaveBeenCalled();
    });
  });

  describe('View state save/restore', () => {
    it('calls saveAndSwitchSession on initial mount with undefined → current URL', () => {
      mockUrlSessionId = 'session-A';
      
      renderHook(() => useSessionManager());
      
      // On mount: prevUrlSessionId is undefined, current is session-A
      expect(mockSaveAndSwitchSession).toHaveBeenCalledWith(null, 'session-A');
    });

    it('calls saveAndSwitchSession when URL changes (simulated via rerender)', () => {
      mockUrlSessionId = 'session-A';
      const { rerender } = renderHook(() => useSessionManager());
      
      mockSaveAndSwitchSession.mockClear();
      mockUrlSessionId = 'session-B';
      rerender();
      
      expect(mockSaveAndSwitchSession).toHaveBeenCalledWith('session-A', 'session-B');
    });

    it('does not call saveAndSwitchSession on rerender with same URL', () => {
      mockUrlSessionId = 'session-A';
      const { rerender } = renderHook(() => useSessionManager());
      
      mockSaveAndSwitchSession.mockClear();
      rerender(); // same urlSessionId
      
      expect(mockSaveAndSwitchSession).not.toHaveBeenCalled();
    });
  });

  describe('derived state', () => {
    it('returns activeSessionId from URL params', () => {
      mockUrlSessionId = 'session-A';
      const { result } = renderHook(() => useSessionManager());
      expect(result.current.activeSessionId).toBe('session-A');
    });

    it('returns undefined activeSessionId when on home page', () => {
      mockUrlSessionId = undefined;
      const { result } = renderHook(() => useSessionManager());
      expect(result.current.activeSessionId).toBeUndefined();
    });

    it('returns isSessionLoaded true when URL matches server', () => {
      mockUrlSessionId = 'session-A';
      mockServerSessionId = 'session-A';
      const { result } = renderHook(() => useSessionManager());
      expect(result.current.isSessionLoaded).toBe(true);
    });

    it('returns isSessionLoaded false when URL does not match server', () => {
      mockUrlSessionId = 'session-A';
      mockServerSessionId = 'session-B';
      const { result } = renderHook(() => useSessionManager());
      expect(result.current.isSessionLoaded).toBe(false);
    });

    it('returns isSessionLoaded false when on home page', () => {
      mockUrlSessionId = undefined;
      mockServerSessionId = 'session-A';
      const { result } = renderHook(() => useSessionManager());
      expect(result.current.isSessionLoaded).toBe(false);
    });
  });
});
