import { create } from 'zustand';
import { EventRow } from '../types';

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

interface UiState {
  // UI visibility toggles
  todoRailCollapsed: boolean;
  setTodoRailCollapsed: (collapsed: boolean) => void;
  
  delegationsPanelCollapsed: boolean;
  setDelegationsPanelCollapsed: (collapsed: boolean) => void;
  
  // Agent mode -> model preferences (persisted to localStorage)
  modeModelPreferences: Record<string, { provider: string; model: string }>;
  setModeModelPreference: (mode: string, provider: string, model: string) => void;
  
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
  
  // Utility actions
  resetChatView: () => void; // Reset view state when switching sessions
  
  // Load persisted state from localStorage
  loadPersistedState: () => void;
}

export const useUiStore = create<UiState>((set) => ({
  // Initial state
  todoRailCollapsed: false,
  delegationsPanelCollapsed: false,
  sessionCopied: false,
  activeTimelineView: 'chat',
  activeDelegationId: null,
  selectedToolEvent: null,
  modelPickerOpen: false,
  isAtBottom: true,
  chatScrollIndex: 0,
  prompt: '',
  loading: false,
  sessionSwitcherOpen: false,
  statsDrawerOpen: false,
  delegationDrawerOpen: false,
  sessionViewCache: new Map(),
  modeModelPreferences: {},
  
  // Actions
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
  
  setSessionCopied: (copied) => set({ sessionCopied: copied }),
  setActiveTimelineView: (view) => set({ activeTimelineView: view }),
  setActiveDelegationId: (id) => set({ activeDelegationId: id }),
  setSelectedToolEvent: (event) => set({ selectedToolEvent: event }),
  setModelPickerOpen: (open) => set({ modelPickerOpen: open }),
  setIsAtBottom: (atBottom) => set({ isAtBottom: atBottom }),
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
  
  // Load persisted state from localStorage
  loadPersistedState: () => {
    const todoRailCollapsed = localStorage.getItem('todoRailCollapsed') === 'true';
    const modeModelPreferencesRaw = localStorage.getItem('modeModelPreferences');
    const modeModelPreferences = modeModelPreferencesRaw ? JSON.parse(modeModelPreferencesRaw) : {};
    set({ todoRailCollapsed, modeModelPreferences });
  },
}));
