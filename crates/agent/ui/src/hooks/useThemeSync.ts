import { useEffect } from 'react';
import { getModeColors } from '../utils/modeColors';
import { applyDashboardTheme, type DashboardThemeId } from '../utils/dashboardThemes';

/**
 * Syncs CSS custom properties for the active agent mode and dashboard theme.
 * Extracted from AppShell to reduce its size.
 */
export function useThemeSync(agentMode: string, selectedTheme: DashboardThemeId) {
  // Set CSS custom properties for mode theming
  useEffect(() => {
    const colors = getModeColors(agentMode, selectedTheme);
    const root = document.documentElement;

    root.style.setProperty('--mode-rgb', colors.rgb);
    root.style.setProperty('--mode-color', colors.cssColor);

    return () => {
      root.style.removeProperty('--mode-rgb');
      root.style.removeProperty('--mode-color');
    };
  }, [agentMode, selectedTheme]);

  // Set CSS custom properties for dashboard theme
  useEffect(() => {
    applyDashboardTheme(selectedTheme);
  }, [selectedTheme]);
}
