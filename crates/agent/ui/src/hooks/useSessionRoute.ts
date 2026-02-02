/**
 * useSessionRoute - Synchronize URL params with session state
 * 
 * This hook bridges the URL parameter (`:sessionId` from react-router) with
 * the useUiClient session loading machinery.
 * 
 * Handles:
 * - URL → State: Load session when URL changes
 * - State → URL: Navigate when session is created/changed
 * - Error handling: Navigate home on invalid session
 * - Page refresh: Reload session from URL on mount
 */

import { useEffect, useRef } from 'react';
import { useParams, useNavigate } from 'react-router-dom';
import { useUiClientContext } from '../context/UiClientContext';

export function useSessionRoute() {
  const { sessionId, loadSession, connected } = useUiClientContext();
  const { sessionId: urlSessionId } = useParams<{ sessionId: string }>();
  const navigate = useNavigate();
  
  // Track the last URL we processed to avoid infinite loops
  const lastUrlSessionIdRef = useRef<string | undefined>(undefined);
  const lastSessionIdRef = useRef<string | null>(null);
  
  // URL → State: When URL has a session ID that doesn't match current, load it
  useEffect(() => {
    if (!connected) return; // Wait for WebSocket
    if (!urlSessionId) return; // On home page, nothing to load
    if (urlSessionId === sessionId) return; // Already loaded
    if (lastUrlSessionIdRef.current === urlSessionId) return; // Already processed this URL
    
    console.log('[useSessionRoute] URL changed, loading session:', urlSessionId);
    lastUrlSessionIdRef.current = urlSessionId;
    loadSession(urlSessionId);
  }, [urlSessionId, sessionId, connected, loadSession]);
  
  // State → URL: When session changes (e.g., session_created), update URL
  useEffect(() => {
    if (!sessionId) return; // No session active
    if (sessionId === urlSessionId) return; // URL already matches
    if (lastSessionIdRef.current === sessionId) return; // Already navigated for this session
    
    console.log('[useSessionRoute] Session changed, navigating to:', sessionId);
    lastSessionIdRef.current = sessionId;
    navigate(`/session/${sessionId}`, { replace: true });
  }, [sessionId, urlSessionId, navigate]);
  
  // Reset tracking when navigating to home
  useEffect(() => {
    if (!urlSessionId) {
      lastUrlSessionIdRef.current = undefined;
      lastSessionIdRef.current = null;
    }
  }, [urlSessionId]);
}
