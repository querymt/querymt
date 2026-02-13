import { describe, expect, it } from 'vitest';
import { colorWithAlpha, getAgentColor } from './agentColors';

describe('agentColors', () => {
  it('uses theme-derived color for primary agent', () => {
    expect(getAgentColor('primary')).toBe('rgb(var(--agent-accent-1-rgb))');
  });

  it('generates rgba for rgb(var(...)) colors', () => {
    const value = colorWithAlpha('rgb(var(--agent-accent-1-rgb))', 0.25);
    expect(value).toBe('rgba(var(--agent-accent-1-rgb), 0.25)');
  });

  it('generates rgba for hex colors', () => {
    const value = colorWithAlpha('#123456', 0.5);
    expect(value).toBe('rgba(18, 52, 86, 0.5)');
  });
});
