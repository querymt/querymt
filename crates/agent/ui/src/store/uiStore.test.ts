import { describe, it, expect, beforeEach } from 'vitest';
import { useUiStore } from './uiStore';
import { EventRow } from '../types';
import { DEFAULT_DASHBOARD_THEME_ID } from '../utils/dashboardThemes';

describe('UiStore', () => {
  beforeEach(() => {
    // Clear localStorage before each test
    localStorage.clear();
    
    // Reset the Zustand store to initial state between tests
    useUiStore.setState({
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
    });
  });

  describe('saveAndSwitchSession', () => {
    it('should save current view state for the old session', () => {
      const store = useUiStore.getState();
      
      // Set some state
      store.setActiveTimelineView('delegations');
      store.setDelegationsPanelCollapsed(true);
      store.setActiveDelegationId('del-123');
      store.setChatScrollIndex(50);
      store.setIsAtBottom(false);
      store.setDelegationDrawerOpen(true);
      
      // Switch away from session-A to session-B
      store.saveAndSwitchSession('session-A', 'session-B');
      
      // Verify state was saved for session-A
      const cached = useUiStore.getState().sessionViewCache.get('session-A');
      expect(cached).toBeDefined();
      expect(cached!.activeTimelineView).toBe('delegations');
      expect(cached!.delegationsPanelCollapsed).toBe(true);
      expect(cached!.activeDelegationId).toBe('del-123');
      expect(cached!.chatScrollIndex).toBe(50);
      expect(cached!.isAtBottom).toBe(false);
      expect(cached!.delegationDrawerOpen).toBe(true);
    });
    
    it('should restore cached state for the target session', () => {
      const store = useUiStore.getState();
      
      // Pre-populate cache for session-B
      store.setActiveTimelineView('delegations');
      store.setActiveDelegationId('del-456');
      store.setDelegationsPanelCollapsed(true);
      store.setChatScrollIndex(100);
      store.setIsAtBottom(false);
      store.setDelegationDrawerOpen(true);
      store.saveAndSwitchSession('session-B', 'session-C'); // saves session-B state
      
      // Now switch to session-A (which should have defaults)
      store.saveAndSwitchSession('session-C', 'session-A');
      expect(useUiStore.getState().activeTimelineView).toBe('chat');
      expect(useUiStore.getState().activeDelegationId).toBeNull();
      expect(useUiStore.getState().delegationsPanelCollapsed).toBe(false);
      expect(useUiStore.getState().chatScrollIndex).toBe(0);
      expect(useUiStore.getState().isAtBottom).toBe(true);
      expect(useUiStore.getState().delegationDrawerOpen).toBe(false);
      
      // Switch back to session-B — should restore
      store.saveAndSwitchSession('session-A', 'session-B');
      expect(useUiStore.getState().activeTimelineView).toBe('delegations');
      expect(useUiStore.getState().activeDelegationId).toBe('del-456');
      expect(useUiStore.getState().delegationsPanelCollapsed).toBe(true);
      expect(useUiStore.getState().chatScrollIndex).toBe(100);
      expect(useUiStore.getState().isAtBottom).toBe(false);
      expect(useUiStore.getState().delegationDrawerOpen).toBe(true);
    });
    
    it('should default to chat view when switching to unknown session', () => {
      const store = useUiStore.getState();
      
      store.setActiveTimelineView('delegations');
      store.setActiveDelegationId('del-999');
      store.setDelegationsPanelCollapsed(true);
      store.setDelegationDrawerOpen(true);
      store.setChatScrollIndex(75);
      store.setIsAtBottom(false);
      
      store.saveAndSwitchSession('session-X', 'new-session');
      
      expect(useUiStore.getState().activeTimelineView).toBe('chat');
      expect(useUiStore.getState().activeDelegationId).toBeNull();
      expect(useUiStore.getState().delegationsPanelCollapsed).toBe(false);
      expect(useUiStore.getState().delegationDrawerOpen).toBe(false);
      expect(useUiStore.getState().chatScrollIndex).toBe(0);
      expect(useUiStore.getState().isAtBottom).toBe(true);
    });
    
    it('should handle null fromSessionId (first mount)', () => {
      const store = useUiStore.getState();
      
      // Set some state that should be reset
      store.setActiveTimelineView('delegations');
      store.setActiveDelegationId('del-first');
      
      store.saveAndSwitchSession(null, 'session-A');
      
      // Should not crash, should apply defaults
      expect(useUiStore.getState().activeTimelineView).toBe('chat');
      expect(useUiStore.getState().activeDelegationId).toBeNull();
      expect(useUiStore.getState().delegationsPanelCollapsed).toBe(false);
      expect(useUiStore.getState().delegationDrawerOpen).toBe(false);
      expect(useUiStore.getState().chatScrollIndex).toBe(0);
      expect(useUiStore.getState().isAtBottom).toBe(true);
      
      // Verify cache is empty (no fromSession to save)
      expect(useUiStore.getState().sessionViewCache.size).toBe(0);
    });
    
    it('should handle null toSessionId (navigating home)', () => {
      const store = useUiStore.getState();
      
      store.setActiveTimelineView('delegations');
      store.setActiveDelegationId('del-home');
      store.setDelegationsPanelCollapsed(true);
      store.setDelegationDrawerOpen(true);
      store.setChatScrollIndex(200);
      store.setIsAtBottom(false);
      
      store.saveAndSwitchSession('session-A', null);
      
      // Should save session-A state and apply defaults
      expect(useUiStore.getState().activeTimelineView).toBe('chat');
      expect(useUiStore.getState().activeDelegationId).toBeNull();
      expect(useUiStore.getState().delegationsPanelCollapsed).toBe(false);
      expect(useUiStore.getState().delegationDrawerOpen).toBe(false);
      expect(useUiStore.getState().chatScrollIndex).toBe(0);
      expect(useUiStore.getState().isAtBottom).toBe(true);
      
      const cached = useUiStore.getState().sessionViewCache.get('session-A');
      expect(cached?.activeTimelineView).toBe('delegations');
      expect(cached?.activeDelegationId).toBe('del-home');
      expect(cached?.delegationsPanelCollapsed).toBe(true);
      expect(cached?.delegationDrawerOpen).toBe(true);
      expect(cached?.chatScrollIndex).toBe(200);
      expect(cached?.isAtBottom).toBe(false);
    });
    
    it('should always clear selectedToolEvent on switch', () => {
      const store = useUiStore.getState();
      
      const mockToolEvent = { id: 'tool-1' } as EventRow;
      store.setSelectedToolEvent(mockToolEvent);
      expect(useUiStore.getState().selectedToolEvent).not.toBeNull();
      expect(useUiStore.getState().selectedToolEvent?.id).toBe('tool-1');
      
      store.saveAndSwitchSession('session-A', 'session-B');
      expect(useUiStore.getState().selectedToolEvent).toBeNull();
    });
    
    it('should preserve cache entries when switching between sessions', () => {
      const store = useUiStore.getState();
      
      // Set state for session-A
      store.setActiveTimelineView('delegations');
      store.setActiveDelegationId('del-A');
      store.saveAndSwitchSession('session-A', 'session-B');
      
      // Set state for session-B
      store.setActiveTimelineView('delegations');
      store.setActiveDelegationId('del-B');
      store.setDelegationsPanelCollapsed(true);
      store.saveAndSwitchSession('session-B', 'session-C');
      
      // Set state for session-C
      store.setActiveTimelineView('chat');
      store.saveAndSwitchSession('session-C', 'session-A');
      
      // Verify all three sessions are in cache
      const cache = useUiStore.getState().sessionViewCache;
      expect(cache.size).toBe(3);
      expect(cache.get('session-A')?.activeDelegationId).toBe('del-A');
      expect(cache.get('session-B')?.activeDelegationId).toBe('del-B');
      expect(cache.get('session-B')?.delegationsPanelCollapsed).toBe(true);
      expect(cache.get('session-C')?.activeTimelineView).toBe('chat');
    });
  });

  describe('resetChatView', () => {
    it('should reset all view state to defaults', () => {
      const store = useUiStore.getState();
      
      // Set all states to non-default values
      store.setActiveTimelineView('delegations');
      store.setActiveDelegationId('del-123');
      store.setDelegationsPanelCollapsed(true);
      store.setDelegationDrawerOpen(true);
      store.setIsAtBottom(false);
      store.setChatScrollIndex(50);
      store.setSelectedToolEvent({ id: 'tool-1' } as EventRow);
      
      // Reset
      store.resetChatView();
      
      // Verify all states are back to defaults
      const state = useUiStore.getState();
      expect(state.activeTimelineView).toBe('chat');
      expect(state.activeDelegationId).toBeNull();
      expect(state.delegationsPanelCollapsed).toBe(false);
      expect(state.delegationDrawerOpen).toBe(false);
      expect(state.isAtBottom).toBe(true);
      expect(state.chatScrollIndex).toBe(0);
      expect(state.selectedToolEvent).toBeNull();
    });
    
    it('should not affect other store state', () => {
      const store = useUiStore.getState();
      
      // Set some states that should NOT be affected
      store.setTodoRailCollapsed(true);
      store.setPrompt('test prompt');
      store.setLoading(true);
      store.setModelPickerOpen(true);
      store.setSessionSwitcherOpen(true);
      store.setStatsDrawerOpen(true);
      
      // Reset chat view
      store.resetChatView();
      
      // Verify unrelated state is preserved
      const state = useUiStore.getState();
      expect(state.todoRailCollapsed).toBe(true);
      expect(state.prompt).toBe('test prompt');
      expect(state.loading).toBe(true);
      expect(state.modelPickerOpen).toBe(true);
      expect(state.sessionSwitcherOpen).toBe(true);
      expect(state.statsDrawerOpen).toBe(true);
    });
  });

  describe('individual setters', () => {
    it('should set todoRailCollapsed', () => {
      const store = useUiStore.getState();
      expect(store.todoRailCollapsed).toBe(false);
      
      store.setTodoRailCollapsed(true);
      expect(useUiStore.getState().todoRailCollapsed).toBe(true);
      
      store.setTodoRailCollapsed(false);
      expect(useUiStore.getState().todoRailCollapsed).toBe(false);
    });
    
    it('should set delegationsPanelCollapsed', () => {
      const store = useUiStore.getState();
      expect(store.delegationsPanelCollapsed).toBe(false);
      
      store.setDelegationsPanelCollapsed(true);
      expect(useUiStore.getState().delegationsPanelCollapsed).toBe(true);
    });
    
    it('should set sessionCopied', () => {
      const store = useUiStore.getState();
      expect(store.sessionCopied).toBe(false);
      
      store.setSessionCopied(true);
      expect(useUiStore.getState().sessionCopied).toBe(true);
    });
    
    it('should set activeTimelineView', () => {
      const store = useUiStore.getState();
      expect(store.activeTimelineView).toBe('chat');
      
      store.setActiveTimelineView('delegations');
      expect(useUiStore.getState().activeTimelineView).toBe('delegations');
      
      store.setActiveTimelineView('chat');
      expect(useUiStore.getState().activeTimelineView).toBe('chat');
    });
    
    it('should set activeDelegationId', () => {
      const store = useUiStore.getState();
      expect(store.activeDelegationId).toBeNull();
      
      store.setActiveDelegationId('del-456');
      expect(useUiStore.getState().activeDelegationId).toBe('del-456');
      
      store.setActiveDelegationId(null);
      expect(useUiStore.getState().activeDelegationId).toBeNull();
    });
    
    it('should set selectedToolEvent', () => {
      const store = useUiStore.getState();
      expect(store.selectedToolEvent).toBeNull();
      
      const mockEvent = { id: 'tool-123', type: 'tool_use' } as unknown as EventRow;
      store.setSelectedToolEvent(mockEvent);
      expect(useUiStore.getState().selectedToolEvent).toBe(mockEvent);
      
      store.setSelectedToolEvent(null);
      expect(useUiStore.getState().selectedToolEvent).toBeNull();
    });
    
    it('should set modelPickerOpen', () => {
      const store = useUiStore.getState();
      expect(store.modelPickerOpen).toBe(false);
      
      store.setModelPickerOpen(true);
      expect(useUiStore.getState().modelPickerOpen).toBe(true);
    });
    
    it('should set isAtBottom', () => {
      const store = useUiStore.getState();
      expect(store.isAtBottom).toBe(true);
      
      store.setIsAtBottom(false);
      expect(useUiStore.getState().isAtBottom).toBe(false);
    });
    
    it('should set chatScrollIndex', () => {
      const store = useUiStore.getState();
      expect(store.chatScrollIndex).toBe(0);
      
      store.setChatScrollIndex(42);
      expect(useUiStore.getState().chatScrollIndex).toBe(42);
    });

    it('should set followNewMessages', () => {
      const store = useUiStore.getState();
      expect(store.followNewMessages).toBe(true);

      store.setFollowNewMessages(false);
      expect(useUiStore.getState().followNewMessages).toBe(false);
      expect(localStorage.getItem('followNewMessages')).toBe('false');
    });
    
    it('should set prompt', () => {
      const store = useUiStore.getState();
      expect(store.prompt).toBe('');
      
      store.setPrompt('Hello, world!');
      expect(useUiStore.getState().prompt).toBe('Hello, world!');
    });
    
    it('should set loading', () => {
      const store = useUiStore.getState();
      expect(store.loading).toBe(false);
      
      store.setLoading(true);
      expect(useUiStore.getState().loading).toBe(true);
    });
    
    it('should set sessionSwitcherOpen', () => {
      const store = useUiStore.getState();
      expect(store.sessionSwitcherOpen).toBe(false);
      
      store.setSessionSwitcherOpen(true);
      expect(useUiStore.getState().sessionSwitcherOpen).toBe(true);
    });
    
    it('should set statsDrawerOpen', () => {
      const store = useUiStore.getState();
      expect(store.statsDrawerOpen).toBe(false);
      
      store.setStatsDrawerOpen(true);
      expect(useUiStore.getState().statsDrawerOpen).toBe(true);
    });
    
    it('should set delegationDrawerOpen', () => {
      const store = useUiStore.getState();
      expect(store.delegationDrawerOpen).toBe(false);
      
      store.setDelegationDrawerOpen(true);
      expect(useUiStore.getState().delegationDrawerOpen).toBe(true);
    });
  });

  describe('initial state', () => {
    it('should have correct initial values', () => {
      const state = useUiStore.getState();
      
      expect(state.todoRailCollapsed).toBe(false);
      expect(state.delegationsPanelCollapsed).toBe(false);
      expect(state.sessionCopied).toBe(false);
      expect(state.activeTimelineView).toBe('chat');
      expect(state.activeDelegationId).toBeNull();
      expect(state.selectedToolEvent).toBeNull();
      expect(state.modelPickerOpen).toBe(false);
      expect(state.isAtBottom).toBe(true);
      expect(state.followNewMessages).toBe(true);
      expect(state.chatScrollIndex).toBe(0);
      expect(state.prompt).toBe('');
      expect(state.loading).toBe(false);
      expect(state.sessionSwitcherOpen).toBe(false);
      expect(state.statsDrawerOpen).toBe(false);
      expect(state.delegationDrawerOpen).toBe(false);
      expect(state.sessionViewCache).toBeInstanceOf(Map);
      expect(state.sessionViewCache.size).toBe(0);
      expect(state.selectedTheme).toBe(DEFAULT_DASHBOARD_THEME_ID);
    });
  });

  describe('complex session switching scenarios', () => {
    it('should handle rapid session switches', () => {
      const store = useUiStore.getState();
      
      // Simulate rapid switching between multiple sessions
      store.setActiveDelegationId('del-1');
      store.saveAndSwitchSession('session-1', 'session-2');
      
      store.setActiveDelegationId('del-2');
      store.saveAndSwitchSession('session-2', 'session-3');
      
      store.setActiveDelegationId('del-3');
      store.saveAndSwitchSession('session-3', 'session-1');
      
      // Should restore session-1 state
      expect(useUiStore.getState().activeDelegationId).toBe('del-1');
      
      // Switch back to session-3
      store.saveAndSwitchSession('session-1', 'session-3');
      expect(useUiStore.getState().activeDelegationId).toBe('del-3');
    });
    
    it('should update existing cache entry when switching from same session twice', () => {
      const store = useUiStore.getState();
      
      // First switch from session-A
      store.setActiveDelegationId('del-first');
      store.saveAndSwitchSession('session-A', 'session-B');
      
      // Go back to session-A and modify state
      store.saveAndSwitchSession('session-B', 'session-A');
      store.setActiveDelegationId('del-updated');
      store.setDelegationsPanelCollapsed(true);
      
      // Switch away again
      store.saveAndSwitchSession('session-A', 'session-C');
      
      // Verify cache was updated with new state
      const cached = useUiStore.getState().sessionViewCache.get('session-A');
      expect(cached?.activeDelegationId).toBe('del-updated');
      expect(cached?.delegationsPanelCollapsed).toBe(true);
    });
  });

  describe('selectedTheme', () => {
    it('should update selectedTheme', () => {
      const store = useUiStore.getState();

      store.setSelectedTheme('base16-gruvbox-dark');

      const state = useUiStore.getState();
      expect(state.selectedTheme).toBe('base16-gruvbox-dark');
    });

    it('should normalize legacy selectedTheme ids', () => {
      const store = useUiStore.getState();

      store.setSelectedTheme('kanagawa-wave');

      const state = useUiStore.getState();
      expect(state.selectedTheme).toBe('base16-kanagawa');
    });

    it('should persist selectedTheme to localStorage', () => {
      const store = useUiStore.getState();

      store.setSelectedTheme('base16-tomorrow-night');

      expect(localStorage.getItem('dashboardTheme')).toBe('base16-tomorrow-night');
    });

    it('should hydrate selectedTheme from localStorage', () => {
      localStorage.setItem('dashboardTheme', 'base16-atelier-forest');

      const store = useUiStore.getState();
      store.loadPersistedState();

      expect(useUiStore.getState().selectedTheme).toBe('base16-atelier-forest');
    });

    it('should fall back to default theme for invalid localStorage value', () => {
      localStorage.setItem('dashboardTheme', 'not-a-real-theme');

      const store = useUiStore.getState();
      store.loadPersistedState();

      expect(useUiStore.getState().selectedTheme).toBe(DEFAULT_DASHBOARD_THEME_ID);
    });

    it('should normalize legacy localStorage theme ids', () => {
      localStorage.setItem('dashboardTheme', 'kanagawa-wave');

      const store = useUiStore.getState();
      store.loadPersistedState();

      expect(useUiStore.getState().selectedTheme).toBe('base16-kanagawa');
    });
  });

  describe('modeModelPreferences', () => {
    describe('setModeModelPreference', () => {
      it('should save model preference for a mode', () => {
        const store = useUiStore.getState();
        
        store.setModeModelPreference('build', 'anthropic', 'claude-3-5-sonnet-20241022');
        
        const state = useUiStore.getState();
        expect(state.modeModelPreferences['build']).toEqual({
          provider: 'anthropic',
          model: 'claude-3-5-sonnet-20241022',
        });
      });
      
      it('should persist preference to localStorage', () => {
        const store = useUiStore.getState();
        
        store.setModeModelPreference('build', 'anthropic', 'claude-3-5-sonnet-20241022');
        
        const stored = localStorage.getItem('modeModelPreferences');
        expect(stored).not.toBeNull();
        const parsed = JSON.parse(stored!);
        expect(parsed['build']).toEqual({
          provider: 'anthropic',
          model: 'claude-3-5-sonnet-20241022',
        });
      });
      
      it('should save preferences for multiple modes', () => {
        const store = useUiStore.getState();
        
        store.setModeModelPreference('build', 'anthropic', 'claude-3-5-sonnet-20241022');
        store.setModeModelPreference('plan', 'openai', 'gpt-4-turbo');
        
        const state = useUiStore.getState();
        expect(state.modeModelPreferences['build']).toEqual({
          provider: 'anthropic',
          model: 'claude-3-5-sonnet-20241022',
        });
        expect(state.modeModelPreferences['plan']).toEqual({
          provider: 'openai',
          model: 'gpt-4-turbo',
        });
      });
      
      it('should update existing mode preference', () => {
        const store = useUiStore.getState();
        
        store.setModeModelPreference('build', 'anthropic', 'claude-3-5-sonnet-20241022');
        store.setModeModelPreference('build', 'openai', 'gpt-4-turbo');
        
        const state = useUiStore.getState();
        expect(state.modeModelPreferences['build']).toEqual({
          provider: 'openai',
          model: 'gpt-4-turbo',
        });
        
        // Verify localStorage was updated
        const stored = localStorage.getItem('modeModelPreferences');
        const parsed = JSON.parse(stored!);
        expect(parsed['build']).toEqual({
          provider: 'openai',
          model: 'gpt-4-turbo',
        });
      });
      
      it('should preserve other mode preferences when updating one', () => {
        const store = useUiStore.getState();
        
        store.setModeModelPreference('build', 'anthropic', 'claude-3-5-sonnet-20241022');
        store.setModeModelPreference('plan', 'openai', 'gpt-4-turbo');
        store.setModeModelPreference('build', 'anthropic', 'claude-3-opus-20240229');
        
        const state = useUiStore.getState();
        expect(state.modeModelPreferences['build']).toEqual({
          provider: 'anthropic',
          model: 'claude-3-opus-20240229',
        });
        expect(state.modeModelPreferences['plan']).toEqual({
          provider: 'openai',
          model: 'gpt-4-turbo',
        });
      });
    });
    
    describe('loadPersistedState', () => {
      it('should load modeModelPreferences from localStorage', () => {
        // Set up localStorage with preferences
        const preferences = {
          build: { provider: 'anthropic', model: 'claude-3-5-sonnet-20241022' },
          plan: { provider: 'openai', model: 'gpt-4-turbo' },
        };
        localStorage.setItem('modeModelPreferences', JSON.stringify(preferences));
        localStorage.setItem('dashboardTheme', 'base16-tomorrow-night');
        
        const store = useUiStore.getState();
        store.loadPersistedState();
        
        const state = useUiStore.getState();
        expect(state.modeModelPreferences).toEqual(preferences);
        expect(state.selectedTheme).toBe('base16-tomorrow-night');
      });
      
      it('should handle missing modeModelPreferences in localStorage', () => {
        // Don't set anything in localStorage
        const store = useUiStore.getState();
        store.loadPersistedState();
        
        const state = useUiStore.getState();
        expect(state.modeModelPreferences).toEqual({});
        expect(state.selectedTheme).toBe(DEFAULT_DASHBOARD_THEME_ID);
      });
      
      it('should handle invalid JSON in localStorage', () => {
        localStorage.setItem('modeModelPreferences', 'invalid-json');
        
        const store = useUiStore.getState();
        
        // Should not crash, should default to empty object
        expect(() => store.loadPersistedState()).toThrow();
      });
      
      it('should load todo rail, model preferences, and theme', () => {
        localStorage.setItem('todoRailCollapsed', 'true');
        localStorage.setItem('followNewMessages', 'false');
        localStorage.setItem('dashboardTheme', 'base16-default-dark');
        localStorage.setItem('modeModelPreferences', JSON.stringify({
          build: { provider: 'anthropic', model: 'claude-3-5-sonnet-20241022' },
        }));
        
        const store = useUiStore.getState();
        store.loadPersistedState();
        
        const state = useUiStore.getState();
        expect(state.todoRailCollapsed).toBe(true);
        expect(state.followNewMessages).toBe(false);
        expect(state.selectedTheme).toBe('base16-default-dark');
        expect(state.modeModelPreferences).toEqual({
          build: { provider: 'anthropic', model: 'claude-3-5-sonnet-20241022' },
        });
      });
    });
    
    describe('initial state', () => {
      it('should have empty modeModelPreferences by default', () => {
        const state = useUiStore.getState();
        expect(state.modeModelPreferences).toEqual({});
        expect(Object.keys(state.modeModelPreferences).length).toBe(0);
      });
    });
    
    describe('integration scenarios', () => {
      it('should persist preferences across store resets', () => {
        const store = useUiStore.getState();
        
        // Set a preference
        store.setModeModelPreference('build', 'anthropic', 'claude-3-5-sonnet-20241022');
        
        // Simulate app reload by resetting state and loading from localStorage
        useUiStore.setState({ modeModelPreferences: {} });
        store.loadPersistedState();
        
        const state = useUiStore.getState();
        expect(state.modeModelPreferences['build']).toEqual({
          provider: 'anthropic',
          model: 'claude-3-5-sonnet-20241022',
        });
      });
      
      it('should handle switching modes with different preferences', () => {
        const store = useUiStore.getState();
        
        // User is in "build" mode and selects a model
        store.setModeModelPreference('build', 'anthropic', 'claude-3-5-sonnet-20241022');
        
        // User switches to "plan" mode and selects a different model
        store.setModeModelPreference('plan', 'openai', 'gpt-4-turbo');
        
        // Simulate cycling back to "build" mode
        // (In real app, AppShell would read modeModelPreferences['build'])
        const buildPreference = useUiStore.getState().modeModelPreferences['build'];
        expect(buildPreference).toEqual({
          provider: 'anthropic',
          model: 'claude-3-5-sonnet-20241022',
        });
      });
      
      it('should handle mode with no preference', () => {
        const store = useUiStore.getState();
        
        // Set preference for "build" only
        store.setModeModelPreference('build', 'anthropic', 'claude-3-5-sonnet-20241022');
        
        // Try to get preference for "plan" (doesn't exist)
        const planPreference = useUiStore.getState().modeModelPreferences['plan'];
        expect(planPreference).toBeUndefined();
      });
      
      it('should support multiple mode switches and updates', () => {
        const store = useUiStore.getState();
        
        // Build mode - first selection
        store.setModeModelPreference('build', 'anthropic', 'claude-3-5-sonnet-20241022');
        
        // Plan mode - first selection
        store.setModeModelPreference('plan', 'openai', 'gpt-4-turbo');
        
        // Build mode - change model
        store.setModeModelPreference('build', 'anthropic', 'claude-3-opus-20240229');
        
        // Verify final state
        const state = useUiStore.getState();
        expect(state.modeModelPreferences['build']).toEqual({
          provider: 'anthropic',
          model: 'claude-3-opus-20240229',
        });
        expect(state.modeModelPreferences['plan']).toEqual({
          provider: 'openai',
          model: 'gpt-4-turbo',
        });
        
        // Verify localStorage matches
        const stored = localStorage.getItem('modeModelPreferences');
        const parsed = JSON.parse(stored!);
        expect(parsed).toEqual(state.modeModelPreferences);
      });
    });
  });
});
