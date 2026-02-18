import { create } from 'zustand';
import type { RefObject } from 'react';
import { EventRow, RateLimitState } from '../types';
import {
  DEFAULT_DASHBOARD_THEME_ID,
  normalizeDashboardThemeId,
  type DashboardThemeId,
} from '../utils/dashboardThemes';

/**
 * UI State Store
 * Centralizes all UI-specific state that was previously scattered in App.tsx
 * Separate from UiClientContext which handles WebSocket/server state
 */

interface SessionViewState {
  activeDelegationId: string | null;
  activeTimelineView: 'chat' | 'delegations';
  delegationsPanelCollapsed: boolean;
  delegationDrawerOpen: boolean;
  chatScrollIndex: number;
  isAtBottom: boolean;
}

interface SessionTimerState {
  globalAccumulatedMs: number;
  agentAccumulatedMs: Record<string, number>;
}

export interface UiState {
  // Focus management
  mainInputRef: RefObject<HTMLTextAreaElement> | null;
  setMainInputRef: (ref: RefObject<HTMLTextAreaElement> | null) => void;
  focusMainInput: () => void;
  
  // UI visibility toggles
  todoRailCollapsed: boolean;
  setTodoRailCollapsed: (collapsed: boolean) => void;
  
  delegationsPanelCollapsed: boolean;
  setDelegationsPanelCollapsed: (collapsed: boolean) => void;
  
  // Agent mode -> model preferences (persisted to localStorage)
  modeModelPreferences: Record<string, { provider: string; model: string }>;
  setModeModelPreference: (mode: string, provider: string, model: string) => void;

  // Dashboard theme (persisted to localStorage)
  selectedTheme: DashboardThemeId;
  setSelectedTheme: (themeId: DashboardThemeId) => void;
  
  // Session/navigation state
  sessionCopied: boolean;
  setSessionCopied: (copied: boolean) => void;
  
  // Timeline view state
  activeTimelineView: 'chat' | 'delegations';
  setActiveTimelineView: (view: 'chat' | 'delegations') => void;
  
  activeDelegationId: string | null;
  setActiveDelegationId: (id: string | null) => void;
  
  // Modal/drawer state
  selectedToolEvent: EventRow | null;
  setSelectedToolEvent: (event: EventRow | null) => void;
  
  modelPickerOpen: boolean;
  setModelPickerOpen: (open: boolean) => void;
  
  // Scroll state
  isAtBottom: boolean;
  setIsAtBottom: (atBottom: boolean) => void;
  followNewMessages: boolean;
  setFollowNewMessages: (enabled: boolean) => void;
  
  chatScrollIndex: number;
  setChatScrollIndex: (index: number) => void;
  
  // Form state
  prompt: string;
  setPrompt: (prompt: string) => void;
  
  loading: boolean;
  setLoading: (loading: boolean) => void;
  
  // Session switcher (Cmd+K) state
  sessionSwitcherOpen: boolean;
  setSessionSwitcherOpen: (open: boolean) => void;
  
  // Stats drawer state
  statsDrawerOpen: boolean;
  setStatsDrawerOpen: (open: boolean) => void;
  
  // Delegation drawer state
  delegationDrawerOpen: boolean;
  setDelegationDrawerOpen: (open: boolean) => void;
  
  // Per-session view state cache
  sessionViewCache: Map<string, SessionViewState>;
  saveAndSwitchSession: (fromSessionId: string | null, toSessionId: string | null) => void;
  
  // Rate limit state per session
  rateLimitBySession: Map<string, RateLimitState>;
  setRateLimitState: (sessionId: string, state: RateLimitState | null) => void;
  updateRemainingTime: (sessionId: string) => void;
  clearRateLimitState: (sessionId: string) => void;

  // Live compaction state per session (set on compaction_start, cleared on compaction_end)
  compactingBySession: Map<string, { tokenEstimate: number; startedAt: number }>;
  setCompactingState: (sessionId: string, state: { tokenEstimate: number; startedAt: number } | null) => void;
  
  // Per-session timer state cache
  sessionTimerCache: Map<string, SessionTimerState>;
  saveSessionTimer: (sessionId: string, state: SessionTimerState) => void;
  getSessionTimer: (sessionId: string) => SessionTimerState | undefined;
  clearSessionTimer: (sessionId: string) => void;
  
  // Utility actions
  resetChatView: () => void; // Reset view state when switching sessions
  
  // Load persisted state from localStorage
  loadPersistedState: () => void;
}

export const useUiStore = create<UiState>((set, get) => ({
  // Initial state
  mainInputRef: null,
  todoRailCollapsed: false,
  delegationsPanelCollapsed: false,
  sessionCopied: false,
  activeTimelineView: 'chat',
  activeDelegationId: null,
  selectedToolEvent: null,
  modelPickerOpen: false,
  isAtBottom: true,
  followNewMessages: true,
  chatScrollIndex: 0,
  prompt: '',
  loading: false,
  sessionSwitcherOpen: false,
  statsDrawerOpen: false,
  delegationDrawerOpen: false,
  sessionViewCache: new Map(),
  modeModelPreferences: {},
  selectedTheme: DEFAULT_DASHBOARD_THEME_ID,
  rateLimitBySession: new Map(),
  compactingBySession: new Map(),
  sessionTimerCache: new Map(),
  
  // Actions
  setMainInputRef: (ref) => set({ mainInputRef: ref }),
  
  focusMainInput: () => {
    const { mainInputRef } = get();
    // Small delay ensures focus happens after modal close animations complete
    setTimeout(() => {
      mainInputRef?.current?.focus();
    }, 0);
  },
  
  setTodoRailCollapsed: (collapsed) => {
    set({ todoRailCollapsed: collapsed });
    // Persist to localStorage
    localStorage.setItem('todoRailCollapsed', collapsed.toString());
  },
  
  setDelegationsPanelCollapsed: (collapsed) => set({ delegationsPanelCollapsed: collapsed }),
  
  setModeModelPreference: (mode, provider, model) => {
    set((state) => {
      const updated = { ...state.modeModelPreferences, [mode]: { provider, model } };
      localStorage.setItem('modeModelPreferences', JSON.stringify(updated));
      return { modeModelPreferences: updated };
    });
  },

  setSelectedTheme: (themeId) => {
    const normalizedThemeId = normalizeDashboardThemeId(themeId) ?? DEFAULT_DASHBOARD_THEME_ID;
    set({ selectedTheme: normalizedThemeId });
    localStorage.setItem('dashboardTheme', normalizedThemeId);
  },
  
  setSessionCopied: (copied) => set({ sessionCopied: copied }),
  setActiveTimelineView: (view) => set({ activeTimelineView: view }),
  setActiveDelegationId: (id) => set({ activeDelegationId: id }),
  setSelectedToolEvent: (event) => set({ selectedToolEvent: event }),
  setModelPickerOpen: (open) => set({ modelPickerOpen: open }),
  setIsAtBottom: (atBottom) => set({ isAtBottom: atBottom }),
  setFollowNewMessages: (enabled) => {
    set({ followNewMessages: enabled });
    localStorage.setItem('followNewMessages', enabled.toString());
  },
  setChatScrollIndex: (index) => set({ chatScrollIndex: index }),
  setPrompt: (prompt) => set({ prompt }),
  setLoading: (loading) => set({ loading }),
  setSessionSwitcherOpen: (open) => set({ sessionSwitcherOpen: open }),
  setStatsDrawerOpen: (open) => set({ statsDrawerOpen: open }),
  setDelegationDrawerOpen: (open) => set({ delegationDrawerOpen: open }),
  
  // Save and restore per-session view state when switching sessions
  saveAndSwitchSession: (fromSessionId, toSessionId) => set((state) => {
    const cache = new Map(state.sessionViewCache);
    
    // Save current state for the session we're leaving
    if (fromSessionId) {
      cache.set(fromSessionId, {
        activeDelegationId: state.activeDelegationId,
        activeTimelineView: state.activeTimelineView,
        delegationsPanelCollapsed: state.delegationsPanelCollapsed,
        delegationDrawerOpen: state.delegationDrawerOpen,
        chatScrollIndex: state.chatScrollIndex,
        isAtBottom: state.isAtBottom,
      });
    }
    
    // Restore state for the session we're switching to (or defaults)
    const restored = toSessionId ? cache.get(toSessionId) : undefined;
    
    return {
      sessionViewCache: cache,
      activeDelegationId: restored?.activeDelegationId ?? null,
      activeTimelineView: restored?.activeTimelineView ?? 'chat',
      delegationsPanelCollapsed: restored?.delegationsPanelCollapsed ?? false,
      delegationDrawerOpen: restored?.delegationDrawerOpen ?? false,
      chatScrollIndex: restored?.chatScrollIndex ?? 0,
      isAtBottom: restored?.isAtBottom ?? true,
      selectedToolEvent: null, // Always clear this on switch
    };
  }),
  
  // Reset chat view state when switching sessions
  resetChatView: () => set({
    activeDelegationId: null,
    activeTimelineView: 'chat',
    selectedToolEvent: null,
    isAtBottom: true,
    chatScrollIndex: 0,
    delegationsPanelCollapsed: false,
    delegationDrawerOpen: false,
  }),
  
  // Rate limit actions
  setRateLimitState: (sessionId, state) => set((prev) => {
    const updated = new Map(prev.rateLimitBySession);
    if (state === null) {
      updated.delete(sessionId);
    } else {
      updated.set(sessionId, state);
    }
    return { rateLimitBySession: updated };
  }),
  
  updateRemainingTime: (sessionId) => set((prev) => {
    const current = prev.rateLimitBySession.get(sessionId);
    if (!current) return prev;
    
    const now = Date.now() / 1000;
    const elapsed = now - current.startedAt;
    const remaining = Math.max(0, current.waitSecs - elapsed);
    
    if (remaining <= 0) {
      // Time's up - clear the state
      const updated = new Map(prev.rateLimitBySession);
      updated.delete(sessionId);
      return { rateLimitBySession: updated };
    }
    
    // Update remaining time
    const updated = new Map(prev.rateLimitBySession);
    updated.set(sessionId, {
      ...current,
      remainingSecs: Math.ceil(remaining),
    });
    return { rateLimitBySession: updated };
  }),
  
  clearRateLimitState: (sessionId) => set((prev) => {
    const updated = new Map(prev.rateLimitBySession);
    updated.delete(sessionId);
    return { rateLimitBySession: updated };
  }),

  // Compaction state actions
  setCompactingState: (sessionId, state) => set((prev) => {
    const updated = new Map(prev.compactingBySession);
    if (state === null) {
      updated.delete(sessionId);
    } else {
      updated.set(sessionId, state);
    }
    return { compactingBySession: updated };
  }),

  // Timer state actions
  saveSessionTimer: (sessionId, state) => set((prev) => {
    const updated = new Map(prev.sessionTimerCache);
    updated.set(sessionId, state);
    return { sessionTimerCache: updated };
  }),
  
  getSessionTimer: (sessionId: string): SessionTimerState | undefined => {
    return useUiStore.getState().sessionTimerCache.get(sessionId);
  },
  
  clearSessionTimer: (sessionId) => set((prev) => {
    const updated = new Map(prev.sessionTimerCache);
    updated.delete(sessionId);
    return { sessionTimerCache: updated };
  }),
  
  // Load persisted state from localStorage
  loadPersistedState: () => {
    const todoRailCollapsed = localStorage.getItem('todoRailCollapsed') === 'true';
    const modeModelPreferencesRaw = localStorage.getItem('modeModelPreferences');
    const modeModelPreferences = modeModelPreferencesRaw ? JSON.parse(modeModelPreferencesRaw) : {};
    const followNewMessages = localStorage.getItem('followNewMessages') !== 'false';
    const selectedThemeRaw = localStorage.getItem('dashboardTheme');
    const selectedTheme = selectedThemeRaw
      ? (normalizeDashboardThemeId(selectedThemeRaw) ?? DEFAULT_DASHBOARD_THEME_ID)
      : DEFAULT_DASHBOARD_THEME_ID;
    set({ todoRailCollapsed, modeModelPreferences, followNewMessages, selectedTheme });
  },
}));
