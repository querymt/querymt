import { describe, expect, it } from 'vitest';
import { getModeColors } from './modeColors';

describe('modeColors', () => {
  it('uses dashboard theme accents for known modes', () => {
    const build = getModeColors('build', 'base16-ocean');
    const plan = getModeColors('plan', 'base16-ocean');

    expect(build.rgb).toBe('163, 190, 140');
    expect(build.cssColor).toBe('rgb(163, 190, 140)');
    expect(plan.rgb).toBe('208, 135, 112');
    expect(plan.cssColor).toBe('rgb(208, 135, 112)');
  });

  it('falls back to static known mode color when theme is not provided', () => {
    const build = getModeColors('build');
    expect(build.hex).toBe('#39ff14');
    expect(build.cssColor).toBe('#39ff14');
  });

  it('keeps deterministic generated colors for unknown modes', () => {
    const first = getModeColors('custom-mode', 'base16-ocean');
    const second = getModeColors('custom-mode', 'base16-tomorrow-night');

    expect(first.hex).toBe(second.hex);
    expect(first.rgb).toBe(second.rgb);
  });

  it('adapts generated unknown-mode colors for light variants', () => {
    const dark = getModeColors('custom-mode', 'base16-ocean');
    const light = getModeColors('custom-mode', 'base16-default-light');
    const lightAgain = getModeColors('custom-mode', 'base16-gruvbox-light');

    expect(light.hex).toBe(lightAgain.hex);
    expect(light.rgb).toBe(lightAgain.rgb);
    expect(light.hex).not.toBe(dark.hex);
  });
});
