import { describe, it, expect, beforeEach, vi } from 'vitest';
import { renderHook, act } from '@testing-library/react';
import { useGlobalKeyboardShortcuts } from './useGlobalKeyboardShortcuts';
import { useUiStore } from '../store/uiStore';

vi.mock('../utils/debugLog', () => ({
  toggleDebugLog: vi.fn(),
}));

describe('useGlobalKeyboardShortcuts', () => {
  const defaultProps = {
    connected: true,
    thinkingAgentId: null,
    workspacePathDialogOpen: false,
    shortcutGatewayOpen: false,
    setShortcutGatewayOpen: vi.fn(),
    themeSwitcherOpen: false,
    setThemeSwitcherOpen: vi.fn(),
    providerAuthOpen: false,
    setProviderAuthOpen: vi.fn(),
    profilePickerOpen: false,
    setProfilePickerOpen: vi.fn(),
    profilesAvailable: true,
    handleNewSession: vi.fn(),
    cancelSession: vi.fn(),
    cycleAgentMode: vi.fn(),
    cycleReasoningEffort: vi.fn(),
    requestAuthProviders: vi.fn(),
    updatePlugins: vi.fn(),
    cancelWorkspacePathDialog: vi.fn(),
  };

  beforeEach(() => {
    vi.clearAllMocks();
    useUiStore.setState({
      loading: false,
      sessionSwitcherOpen: false,
      modelPickerOpen: false,
      statsDrawerOpen: false,
      createScheduleDialogOpen: false,
    });
  });

  it('opens the profile picker from the Ctrl+X P chord when profiles are available', () => {
    const setShortcutGatewayOpen = vi.fn();
    const setProfilePickerOpen = vi.fn();

    renderHook(() => useGlobalKeyboardShortcuts({
      ...defaultProps,
      shortcutGatewayOpen: true,
      setShortcutGatewayOpen,
      setProfilePickerOpen,
      profilesAvailable: true,
    }));

    const event = new KeyboardEvent('keydown', { key: 'p', bubbles: true });
    const preventDefault = vi.spyOn(event, 'preventDefault');

    act(() => {
      window.dispatchEvent(event);
    });

    expect(preventDefault).toHaveBeenCalledTimes(1);
    expect(setShortcutGatewayOpen).toHaveBeenCalledWith(false);
    expect(setProfilePickerOpen).toHaveBeenCalledWith(true);
  });

  it('toggles performance mode from the Ctrl+X G chord in the shortcut gateway', () => {
    const setShortcutGatewayOpen = vi.fn();
    const togglePerfMode = vi.fn();
    useUiStore.setState({ togglePerfMode });

    renderHook(() => useGlobalKeyboardShortcuts({
      ...defaultProps,
      shortcutGatewayOpen: true,
      setShortcutGatewayOpen,
    }));

    const event = new KeyboardEvent('keydown', { key: 'g', bubbles: true });
    const preventDefault = vi.spyOn(event, 'preventDefault');

    act(() => {
      window.dispatchEvent(event);
    });

    expect(preventDefault).toHaveBeenCalledTimes(1);
    expect(setShortcutGatewayOpen).toHaveBeenCalledWith(false);
    expect(togglePerfMode).toHaveBeenCalledTimes(1);
  });

  it('does not intercept P in the shortcut gateway when profiles are unavailable', () => {
    const setShortcutGatewayOpen = vi.fn();
    const setProfilePickerOpen = vi.fn();

    renderHook(() => useGlobalKeyboardShortcuts({
      ...defaultProps,
      shortcutGatewayOpen: true,
      setShortcutGatewayOpen,
      setProfilePickerOpen,
      profilesAvailable: false,
    }));

    const event = new KeyboardEvent('keydown', { key: 'p', bubbles: true });
    const preventDefault = vi.spyOn(event, 'preventDefault');

    act(() => {
      window.dispatchEvent(event);
    });

    expect(preventDefault).not.toHaveBeenCalled();
    expect(setShortcutGatewayOpen).not.toHaveBeenCalled();
    expect(setProfilePickerOpen).not.toHaveBeenCalled();
  });

  it('closes the profile picker on Escape', () => {
    const setProfilePickerOpen = vi.fn();

    renderHook(() => useGlobalKeyboardShortcuts({
      ...defaultProps,
      profilePickerOpen: true,
      setProfilePickerOpen,
    }));

    const event = new KeyboardEvent('keydown', { key: 'Escape', bubbles: true });
    const preventDefault = vi.spyOn(event, 'preventDefault');
    const stopImmediatePropagation = vi.spyOn(event, 'stopImmediatePropagation');

    act(() => {
      window.dispatchEvent(event);
    });

    expect(preventDefault).toHaveBeenCalledTimes(1);
    expect(stopImmediatePropagation).toHaveBeenCalledTimes(1);
    expect(setProfilePickerOpen).toHaveBeenCalledWith(false);
  });
});
