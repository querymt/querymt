/**
 * useSessionManager - Consolidated session management hook
 * 
 * This hook encapsulates ALL session transition logic, replacing the distributed
 * session management that was previously split across:
 * - useSessionRoute.ts (URL ↔ server sync)
 * - ChatView.tsx (saveAndSwitchSession effect)
 * - AppShell.tsx (handleNewSession)
 * - HomePage.tsx (handleSelectSession)
 * - SessionSwitcher.tsx (handleSelectSession)
 * 
 * Design:
 * - Single source of truth: URL parameter is the canonical "what the user intends to view"
 * - Saves/restores view state per session using Zustand store
 * - Handles URL ↔ server synchronization
 * - Provides clean interface for session navigation
 */

import { useCallback, useEffect, useRef } from 'react';
import { useParams, useNavigate } from 'react-router-dom';
import { useUiClientContext } from '../context/UiClientContext';
import { useUiStore } from '../store/uiStore';

export interface SessionManager {
  /** The session ID from the URL — what the user intends to view */
  activeSessionId: string | undefined;
  /** The session ID confirmed by the server — data is loaded for this one */
  serverSessionId: string | null;
  /** True when URL session matches server session (data is ready) */
  isSessionLoaded: boolean;
  /** Switch to a different session. Saves current view state, navigates, triggers load. */
  selectSession: (sessionId: string) => void;
  /** Create a new session and navigate to it */
  createSession: () => Promise<void>;
  /** Navigate to home page, saving current view state */
  goHome: () => void;
}

export function useSessionManager(): SessionManager {
  const { sessionId: serverSessionId, loadSession, newSession, connected, sessionCreatingRef } = useUiClientContext();
  const { sessionId: urlSessionId } = useParams<{ sessionId: string }>();
  const navigate = useNavigate();
  const { saveAndSwitchSession } = useUiStore();
  
  // Track previous URL session for save/restore
  const prevUrlSessionIdRef = useRef<string | undefined>(undefined);
  
  // --- URL → Server sync (replaces useSessionRoute) ---
  // When URL changes, load the session on the server
  useEffect(() => {
    if (!connected) return; // Wait for WebSocket
    if (!urlSessionId) return; // On home page, nothing to load
    if (urlSessionId === serverSessionId) return; // Already loaded
    if (sessionCreatingRef.current) return; // Skip during creation
    
    console.log('[useSessionManager] URL changed, loading session:', urlSessionId);
    loadSession(urlSessionId);
  }, [urlSessionId, serverSessionId, connected, loadSession, sessionCreatingRef]);
  
  // --- View state save/restore on URL change ---
  // Save the previous session's view state and restore the new session's view state
  useEffect(() => {
    const prevUrlSessionId = prevUrlSessionIdRef.current;
    prevUrlSessionIdRef.current = urlSessionId;
    
    // Only trigger save/restore when session actually changes
    if (prevUrlSessionId !== urlSessionId) {
      console.log('[useSessionManager] Session switch:', prevUrlSessionId, '→', urlSessionId);
      saveAndSwitchSession(prevUrlSessionId ?? null, urlSessionId ?? null);
    }
  }, [urlSessionId, saveAndSwitchSession]);
  
  // --- Public API ---
  
  /**
   * Switch to a different session.
   * Navigates to the session URL, which triggers the effects above.
   */
  const selectSession = useCallback((sessionId: string) => {
    if (sessionId === urlSessionId) return; // Already viewing this session
    console.log('[useSessionManager] Selecting session:', sessionId);
    navigate(`/session/${sessionId}`);
    // The URL change will trigger the effects above
  }, [urlSessionId, navigate]);
  
  /**
   * Create a new session and navigate to it.
   * Handles the full creation flow and error cases.
   */
  const createSession = useCallback(async () => {
    console.log('[useSessionManager] Creating new session');
    try {
      const newSessionId = await newSession();
      console.log('[useSessionManager] New session created:', newSessionId);
      navigate(`/session/${newSessionId}`, { replace: true });
    } catch (err) {
      console.log('[useSessionManager] Session creation cancelled or failed:', err);
      throw err; // Re-throw so caller can handle if needed
    }
  }, [newSession, navigate]);
  
  /**
   * Navigate to home page.
   * Saves current view state before leaving.
   */
  const goHome = useCallback(() => {
    console.log('[useSessionManager] Going home');
    navigate('/');
    // The URL change will trigger the save effect above
  }, [navigate]);
  
  return {
    activeSessionId: urlSessionId,
    serverSessionId,
    isSessionLoaded: !!urlSessionId && urlSessionId === serverSessionId,
    selectSession,
    createSession,
    goHome,
  };
}
