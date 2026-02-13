import { beforeEach, describe, expect, it } from 'vitest';
import {
  DEFAULT_DASHBOARD_THEME_ID,
  applyDashboardTheme,
  getDashboardTheme,
  getDashboardThemes,
  getDiffThemeForDashboard,
  getShikiThemeForDashboard,
  isDashboardThemeId,
} from './dashboardThemes';

describe('dashboardThemes', () => {
  beforeEach(() => {
    const root = document.documentElement;
    root.removeAttribute('data-theme');
    root.style.removeProperty('--cyber-bg-rgb');
    root.style.removeProperty('--cyber-cyan-rgb');
    root.style.removeProperty('color-scheme');
  });

  it('exposes querymate plus many dark tinted base16 themes', () => {
    const themes = getDashboardThemes();
    expect(themes.length).toBeGreaterThan(200);
    expect(themes.every((theme) => theme.id.startsWith('base16-'))).toBe(true);
    expect(themes.some((theme) => theme.id === 'base16-querymate')).toBe(true);
    expect(themes.some((theme) => theme.id === 'base16-ocean')).toBe(true);
    expect(themes.some((theme) => theme.id === 'base16-kanagawa-dragon')).toBe(true);
  });

  it('applies theme CSS variables to the document root', () => {
    const root = document.documentElement;
    applyDashboardTheme('base16-querymate', root);

    expect(root.getAttribute('data-theme')).toBe('base16-querymate');
    expect(root.style.getPropertyValue('--cyber-bg-rgb').trim()).toBe('10, 14, 39');
    expect(root.style.getPropertyValue('--cyber-cyan-rgb').trim()).toBe('0, 255, 249');
    expect(root.style.getPropertyValue('--cyber-purple-rgb').trim()).toBe('176, 38, 255');
    expect(root.style.getPropertyValue('color-scheme').trim()).toBe('dark');
  });

  it('validates known and unknown theme ids', () => {
    expect(isDashboardThemeId('base16-gruvbox-dark')).toBe(true);
    expect(isDashboardThemeId('kanagawa-wave')).toBe(true);
    expect(isDashboardThemeId('cyberpunk-neon')).toBe(false);
  });

  it('returns theme metadata for syntax and diff renderers', () => {
    expect(getShikiThemeForDashboard('base16-querymate')).toBe('github-dark');
    expect(getDiffThemeForDashboard('base16-querymate')).toBe('pierre-dark');
    expect(getShikiThemeForDashboard('base16-gruvbox-dark')).toBe('dark-plus');
    expect(getDiffThemeForDashboard('base16-atelier-forest')).toBe('everforest-dark');
    expect(getShikiThemeForDashboard('base16-kanagawa')).toBe('kanagawa-wave');
    expect(getDiffThemeForDashboard('base16-kanagawa-dragon')).toBe('kanagawa-dragon');

    const defaultTheme = getDashboardTheme(DEFAULT_DASHBOARD_THEME_ID);
    expect(defaultTheme.id).toBe('base16-querymate');
  });
});
