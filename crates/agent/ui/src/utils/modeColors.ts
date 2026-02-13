/**
 * Mode color utility - generates consistent colors for agent modes
 * 
 * Known modes (build, plan, review) use theme-derived accents when available.
 * Unknown modes get deterministic hash-based colors in the accent palette.
 */

import { getDashboardTheme, type DashboardThemeId } from './dashboardThemes';

// Known modes with fallback colors (RGB and hex for CSS usage)
const KNOWN_MODE_FALLBACKS: Record<string, { rgb: string; hex: string }> = {
  build: { rgb: '57, 255, 20', hex: '#39ff14' },      // status-success
  plan: { rgb: '255, 107, 53', hex: '#ff6b35' },      // status-warning
  review: { rgb: '176, 38, 255', hex: '#b026ff' },    // accent-tertiary
};

// Known modes mapped to base16 palette tokens for theme-aware accents
const KNOWN_MODE_THEME_TOKENS: Record<string, 'base0B' | 'base09' | 'base0D'> = {
  build: 'base0B',
  plan: 'base09',
  review: 'base0D',
};

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

/**
 * Simple string hash function
 */
function hashString(str: string): number {
  let hash = 0;
  for (let i = 0; i < str.length; i++) {
    const char = str.charCodeAt(i);
    hash = ((hash << 5) - hash) + char;
    hash = hash & hash; // Convert to 32-bit integer
  }
  return Math.abs(hash);
}

/**
 * Convert HSL to RGB
 */
function hslToRgb(h: number, s: number, l: number): { r: number; g: number; b: number } {
  h = h / 360;
  s = s / 100;
  l = l / 100;

  let r, g, b;

  if (s === 0) {
    r = g = b = l;
  } else {
    const hue2rgb = (p: number, q: number, t: number) => {
      if (t < 0) t += 1;
      if (t > 1) t -= 1;
      if (t < 1/6) return p + (q - p) * 6 * t;
      if (t < 1/2) return q;
      if (t < 2/3) return p + (q - p) * (2/3 - t) * 6;
      return p;
    };

    const q = l < 0.5 ? l * (1 + s) : l + s - l * s;
    const p = 2 * l - q;
    r = hue2rgb(p, q, h + 1/3);
    g = hue2rgb(p, q, h);
    b = hue2rgb(p, q, h - 1/3);
  }

  return {
    r: Math.round(r * 255),
    g: Math.round(g * 255),
    b: Math.round(b * 255),
  };
}

/**
 * Generate an accent color from mode name hash
 * Uses variant-aware saturation/lightness for contrast in dark/light themes
 */
function hashToRgb(mode: string, variant: 'dark' | 'light'): { rgb: string; hex: string } {
  const hash = hashString(mode);
  
  // Generate hue from hash (0-360)
  const hue = hash % 360;
  
  const saturation = variant === 'light' ? 70 + (hash % 16) : 85 + (hash % 11);
  const lightness = variant === 'light' ? 30 + (hash % 16) : 55 + (hash % 16);
  
  const { r, g, b } = hslToRgb(hue, saturation, lightness);
  
  return {
    rgb: `${r}, ${g}, ${b}`,
    hex: `#${r.toString(16).padStart(2, '0')}${g.toString(16).padStart(2, '0')}${b.toString(16).padStart(2, '0')}`,
  };
}

/**
 * Get colors (RGB and hex) for a given mode
 * Returns known colors for build/plan/review, generates hash-based color for unknown modes
 */
export function getModeColors(
  mode: string,
  themeId?: DashboardThemeId,
): { rgb: string; hex: string; cssColor: string } {
  const normalized = mode.toLowerCase().trim();

  if (KNOWN_MODE_FALLBACKS[normalized]) {
    if (themeId) {
      const theme = getDashboardTheme(themeId);
      const token = KNOWN_MODE_THEME_TOKENS[normalized];
      const hex = theme.palette[token];
      const rgb = hexToRgbTuple(hex);
      return {
        rgb,
        hex,
        cssColor: `rgb(${rgb})`,
      };
    }

    const fallback = KNOWN_MODE_FALLBACKS[normalized];
    return {
      ...fallback,
      cssColor: fallback.hex,
    };
  }

  const variant = themeId ? getDashboardTheme(themeId).variant : 'dark';
  const generated = hashToRgb(mode, variant);
  return {
    ...generated,
    cssColor: generated.hex,
  };
}

/**
 * Get display name for mode (capitalize first letter)
 */
export function getModeDisplayName(mode: string): string {
  if (!mode) return 'Unknown';
  return mode.charAt(0).toUpperCase() + mode.slice(1).toLowerCase();
}

/**
 * Get all known mode names
 */
export function getKnownModes(): string[] {
  return Object.keys(KNOWN_MODE_FALLBACKS);
}
