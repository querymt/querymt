import { create } from 'zustand';
import { EventRow } from '../types';

/**
 * UI State Store
 * Centralizes all UI-specific state that was previously scattered in App.tsx
 * Separate from UiClientContext which handles WebSocket/server state
 */

interface UiState {
  // UI visibility toggles
  todoRailCollapsed: boolean;
  setTodoRailCollapsed: (collapsed: boolean) => void;
  
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
  
  // Utility actions
  resetChatView: () => void; // Reset view state when switching sessions
  
  // Load persisted state from localStorage
  loadPersistedState: () => void;
}

export const useUiStore = create<UiState>((set) => ({
  // Initial state
  todoRailCollapsed: false,
  sessionCopied: false,
  activeTimelineView: 'chat',
  activeDelegationId: null,
  selectedToolEvent: null,
  modelPickerOpen: false,
  isAtBottom: true,
  prompt: '',
  loading: false,
  sessionSwitcherOpen: false,
  statsDrawerOpen: false,
  
  // Actions
  setTodoRailCollapsed: (collapsed) => {
    set({ todoRailCollapsed: collapsed });
    // Persist to localStorage
    localStorage.setItem('todoRailCollapsed', collapsed.toString());
  },
  
  setSessionCopied: (copied) => set({ sessionCopied: copied }),
  setActiveTimelineView: (view) => set({ activeTimelineView: view }),
  setActiveDelegationId: (id) => set({ activeDelegationId: id }),
  setSelectedToolEvent: (event) => set({ selectedToolEvent: event }),
  setModelPickerOpen: (open) => set({ modelPickerOpen: open }),
  setIsAtBottom: (atBottom) => set({ isAtBottom: atBottom }),
  setPrompt: (prompt) => set({ prompt }),
  setLoading: (loading) => set({ loading }),
  setSessionSwitcherOpen: (open) => set({ sessionSwitcherOpen: open }),
  setStatsDrawerOpen: (open) => set({ statsDrawerOpen: open }),
  
  // Reset chat view state when switching sessions
  resetChatView: () => set({
    activeDelegationId: null,
    activeTimelineView: 'chat',
    selectedToolEvent: null,
    isAtBottom: true,
  }),
  
  // Load persisted state from localStorage
  loadPersistedState: () => {
    const todoRailCollapsed = localStorage.getItem('todoRailCollapsed') === 'true';
    set({ todoRailCollapsed });
  },
}));
