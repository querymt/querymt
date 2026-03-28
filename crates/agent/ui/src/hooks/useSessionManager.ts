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
import { useUiClientActions, useUiClientSession } from '../context/UiClientContext';
import { useUiStore } from '../store/uiStore';
import type { SessionSummary } from '../types';

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
  const {
    loadSession,
    attachRemoteSession,
    newSession,
    sessionCreatingRef,
  } = useUiClientActions();
  const {
    sessionId: serverSessionId,
    connected,
    sessionGroups: rawSessionGroups,
    lastLoadErrorSessionId,
  } = useUiClientSession();
  const sessionGroups = rawSessionGroups ?? [];
  const { sessionId: urlSessionId } = useParams<{ sessionId: string }>();
  const navigate = useNavigate();
  const { saveAndSwitchSession } = useUiStore();

  // Keep a ref to sessionGroups so the URL-sync effect can look up session
  // metadata without taking sessionGroups as a reactive dependency (which would
  // cause an infinite retry loop when a load error triggers list_sessions and
  // sessionGroups updates).
  const sessionGroupsRef = useRef(sessionGroups);
  sessionGroupsRef.current = sessionGroups;

  // --- Load failure guard (Fix B3) ---
  // If the session the URL is pointing at just failed to load (e.g. a remote
  // session whose node_id was missing or stale), navigate back to home so the
  // URL-sync effect above doesn't keep retrying forever.
  useEffect(() => {
    if (lastLoadErrorSessionId && lastLoadErrorSessionId === urlSessionId) {
      console.log('[useSessionManager] Load failed for current session, navigating home:', lastLoadErrorSessionId);
      navigate('/');
    }
  }, [lastLoadErrorSessionId, urlSessionId, navigate]);

  // Track previous URL session for save/restore
  const prevUrlSessionIdRef = useRef<string | undefined>(undefined);
  
  // --- URL → Server sync (replaces useSessionRoute) ---
  // When URL changes, load the session on the server.
  // NOTE: sessionGroups is intentionally NOT in the dependency array — we read
  // it via a ref to prevent the error→list_sessions→effect re-trigger loop that
  // occurs when a remote-session load fails and refreshes the session list.
  useEffect(() => {
    if (!connected) return; // Wait for WebSocket
    if (!urlSessionId) return; // On home page, nothing to load
    
    // If session creation just completed and URL now matches server state, clear the flag
    if (sessionCreatingRef.current && urlSessionId === serverSessionId) {
      sessionCreatingRef.current = false;
      return; // Already loaded by creation flow
    }
    
    if (urlSessionId === serverSessionId) return; // Already loaded
    
    // During creation the server sets sessionId via session_created before we navigate,
    // so skip the loadSession call to avoid re-loading what was just created.
    if (sessionCreatingRef.current) return;
    
    // Look up this session in the current groups (via ref — not a reactive dep).
    let currentSession: SessionSummary | undefined;
    for (const group of sessionGroupsRef.current) {
      const found = group.sessions.find((s) => s.session_id === urlSessionId);
      if (found) { currentSession = found; break; }
    }
    const sessionLabel = currentSession
      ? currentSession.title || currentSession.name || currentSession.session_id
      : undefined;

    // Remote sessions that have not yet been attached need attach_remote_session
    // (which does a DHT lookup and wires up the actor), not load_session (which
    // only queries the local database and would always fail with "Query returned
    // no rows" for sessions that live on a remote peer).
    // If this is a remote session and it's not currently attached, attach first.
    // Treat missing `attached` as unattached for backward compatibility.
    if (currentSession?.node_id && currentSession.attached !== true) {
      console.log('[useSessionManager] Attaching remote session:', urlSessionId, 'on node', currentSession.node_id);
      attachRemoteSession(currentSession.node_id, urlSessionId, sessionLabel);
      return;
    }

    console.log('[useSessionManager] URL changed, loading session:', urlSessionId);
    loadSession(urlSessionId, sessionLabel);
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [urlSessionId, serverSessionId, connected, loadSession, attachRemoteSession, sessionCreatingRef]);
  // ^ sessionGroups is deliberately omitted — use sessionGroupsRef instead.
  
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
