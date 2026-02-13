import {
  TINTED_BASE16_SCHEMES,
  TINTED_BASE16_SOURCE,
  type TintedBase16Theme,
} from './generated/tintedBase16Themes';

export type DashboardThemeId = string;

type Base16Key =
  | 'base00'
  | 'base01'
  | 'base02'
  | 'base03'
  | 'base04'
  | 'base05'
  | 'base06'
  | 'base07'
  | 'base08'
  | 'base09'
  | 'base0A'
  | 'base0B'
  | 'base0C'
  | 'base0D'
  | 'base0E'
  | 'base0F';

type Base16Palette = Record<Base16Key, string>;

type DashboardThemeVariant = 'dark' | 'light';

export interface DashboardTheme {
  id: DashboardThemeId;
  label: string;
  description: string;
  variant: DashboardThemeVariant;
  palette: Base16Palette;
  shikiTheme: string;
  diffTheme: string;
}

export const DASHBOARD_THEME_SOURCE = TINTED_BASE16_SOURCE;
export const DEFAULT_DASHBOARD_THEME_ID: DashboardThemeId = 'base16-querymate';

const SUPPORTED_VARIANTS: ReadonlySet<DashboardThemeVariant> = new Set(['dark']);

const SHIKI_BUNDLED_THEME_IDS: ReadonlySet<string> = new Set([
  'andromeeda',
  'aurora-x',
  'ayu-dark',
  'catppuccin-frappe',
  'catppuccin-latte',
  'catppuccin-macchiato',
  'catppuccin-mocha',
  'dark-plus',
  'dracula',
  'dracula-soft',
  'everforest-dark',
  'everforest-light',
  'github-dark',
  'github-dark-default',
  'github-dark-dimmed',
  'github-dark-high-contrast',
  'github-light',
  'github-light-default',
  'github-light-high-contrast',
  'houston',
  'kanagawa-dragon',
  'kanagawa-lotus',
  'kanagawa-wave',
  'laserwave',
  'light-plus',
  'material-theme',
  'material-theme-darker',
  'material-theme-lighter',
  'material-theme-ocean',
  'material-theme-palenight',
  'min-dark',
  'min-light',
  'monokai',
  'night-owl',
  'nord',
  'one-dark-pro',
  'one-light',
  'plastic',
  'poimandres',
  'red',
  'rose-pine',
  'rose-pine-dawn',
  'rose-pine-moon',
  'slack-dark',
  'slack-ochin',
  'snazzy-light',
  'solarized-dark',
  'solarized-light',
  'synthwave-84',
  'tokyo-night',
  'vesper',
  'vitesse-black',
  'vitesse-dark',
  'vitesse-light',
]);

const SHIKI_THEME_FALLBACK_BY_VARIANT: Record<DashboardThemeVariant, string> = {
  dark: 'github-dark',
  light: 'github-light',
};

const SHIKI_THEME_ALIASES: Record<string, string> = {
  'base16-default-dark': 'github-dark',
  'base16-default-light': 'github-light',
  'base16-ocean': 'nord',
  'base16-tomorrow-night': 'dark-plus',
  'base16-tomorrow': 'light-plus',
  'base16-atelier-forest': 'everforest-dark',
  'base16-atelier-forest-light': 'everforest-light',
  'base16-gruvbox-dark': 'dark-plus',
  'base16-gruvbox-light': 'light-plus',
  'base16-kanagawa': 'kanagawa-wave',
};

const LEGACY_THEME_ID_ALIASES: Record<string, DashboardThemeId> = {
  'kanagawa-wave': 'base16-kanagawa',
  'kanagawa-dragon': 'base16-kanagawa-dragon',
  'base16-kanagawa-wave': 'base16-kanagawa',
  'querymate-classic': 'base16-querymate',
  querymate: 'base16-querymate',
};

const TOKEN_BY_CSS_VAR: Record<string, Base16Key> = {
  '--cyber-bg-rgb': 'base00',
  '--cyber-surface-rgb': 'base01',
  '--cyber-border-rgb': 'base02',
  '--cyber-cyan-rgb': 'base0C',
  '--cyber-magenta-rgb': 'base0E',
  '--cyber-purple-rgb': 'base0D',
  '--cyber-lime-rgb': 'base0B',
  '--cyber-orange-rgb': 'base09',
  '--ui-text-primary-rgb': 'base05',
  '--ui-text-secondary-rgb': 'base04',
  '--ui-text-muted-rgb': 'base03',
  '--code-bg-rgb': 'base00',
  '--glitch-fg-rgb': 'base06',
  '--glitch-shadow-red-rgb': 'base08',
  '--glitch-shadow-cyan-rgb': 'base0C',
  '--agent-accent-1-rgb': 'base0D',
  '--agent-accent-2-rgb': 'base0E',
  '--agent-accent-3-rgb': 'base0A',
};

const QUERYMATE_THEME: DashboardTheme = {
  id: 'base16-querymate',
  label: 'QueryMate',
  description: 'Original neon QueryMate colors.',
  variant: 'dark',
  palette: {
    base00: '#0a0e27',
    base01: '#141b3d',
    base02: '#1e2a5e',
    base03: '#6b7280',
    base04: '#9ca3af',
    base05: '#f3f4f6',
    base06: '#ffffff',
    base07: '#ff66ff',
    base08: '#ff0000',
    base09: '#ff6b35',
    base0A: '#7fff00',
    base0B: '#39ff14',
    base0C: '#00fff9',
    base0D: '#b026ff',
    base0E: '#ff00ff',
    base0F: '#00d4ff',
  },
  shikiTheme: 'github-dark',
  diffTheme: 'pierre-dark',
};

function resolveShikiTheme(themeId: DashboardThemeId, variant: DashboardThemeVariant): string {
  const alias = SHIKI_THEME_ALIASES[themeId];
  if (alias && SHIKI_BUNDLED_THEME_IDS.has(alias)) {
    return alias;
  }

  const normalizedId = themeId.replace(/^base16-/, '');
  if (SHIKI_BUNDLED_THEME_IDS.has(normalizedId)) {
    return normalizedId;
  }

  return SHIKI_THEME_FALLBACK_BY_VARIANT[variant];
}

function mapTintedThemeToDashboardTheme(theme: TintedBase16Theme): DashboardTheme {
  const shikiTheme = resolveShikiTheme(theme.id, theme.variant);

  return {
    id: theme.id,
    label: theme.label,
    description: theme.description,
    variant: theme.variant,
    palette: theme.palette as Base16Palette,
    shikiTheme,
    diffTheme: shikiTheme,
  };
}

const TINTED_DASHBOARD_THEME_LIST: DashboardTheme[] = TINTED_BASE16_SCHEMES
  .filter((theme) => SUPPORTED_VARIANTS.has(theme.variant))
  .map(mapTintedThemeToDashboardTheme)
  .filter((theme) => theme.id !== QUERYMATE_THEME.id)
  .sort((left, right) => left.label.localeCompare(right.label));

const DASHBOARD_THEME_LIST: DashboardTheme[] = [QUERYMATE_THEME, ...TINTED_DASHBOARD_THEME_LIST];

const DASHBOARD_THEMES: Record<string, DashboardTheme> = Object.fromEntries(
  DASHBOARD_THEME_LIST.map((theme) => [theme.id, theme]),
);

function resolveDashboardThemeId(value: string): DashboardThemeId | null {
  if (Object.prototype.hasOwnProperty.call(DASHBOARD_THEMES, value)) {
    return value;
  }

  const mappedThemeId = LEGACY_THEME_ID_ALIASES[value];
  if (mappedThemeId && Object.prototype.hasOwnProperty.call(DASHBOARD_THEMES, mappedThemeId)) {
    return mappedThemeId;
  }

  return null;
}

function hexToRgbTuple(hex: string): string {
  const normalized = hex.trim().replace('#', '');
  if (normalized.length !== 6) {
    return '255, 255, 255';
  }

  const r = parseInt(normalized.slice(0, 2), 16);
  const g = parseInt(normalized.slice(2, 4), 16);
  const b = parseInt(normalized.slice(4, 6), 16);
  return `${r}, ${g}, ${b}`;
}

export function getDashboardThemes(): DashboardTheme[] {
  return DASHBOARD_THEME_LIST;
}

export function normalizeDashboardThemeId(value: string): DashboardThemeId | null {
  return resolveDashboardThemeId(value);
}

export function isDashboardThemeId(value: string): value is DashboardThemeId {
  return resolveDashboardThemeId(value) !== null;
}

export function getDashboardTheme(themeId: DashboardThemeId): DashboardTheme {
  const resolvedThemeId = resolveDashboardThemeId(themeId);

  if (resolvedThemeId) {
    return DASHBOARD_THEMES[resolvedThemeId];
  }

  return DASHBOARD_THEMES[DEFAULT_DASHBOARD_THEME_ID];
}

export function getShikiThemeForDashboard(themeId: DashboardThemeId): string {
  return getDashboardTheme(themeId).shikiTheme;
}

export function getDiffThemeForDashboard(themeId: DashboardThemeId): string {
  return getDashboardTheme(themeId).diffTheme;
}

export function applyDashboardTheme(
  themeId: DashboardThemeId,
  root: HTMLElement = document.documentElement,
): DashboardTheme {
  const theme = getDashboardTheme(themeId);
  root.setAttribute('data-theme', theme.id);
  root.style.setProperty('color-scheme', theme.variant);

  for (const [cssVar, token] of Object.entries(TOKEN_BY_CSS_VAR)) {
    root.style.setProperty(cssVar, hexToRgbTuple(theme.palette[token]));
  }

  return theme;
}
