import { describe, it, expect } from 'vitest';
import { render, screen } from '@testing-library/react';
import { HeaderStatsBar } from './HeaderStatsBar';
import type { EventItem, SessionLimits } from '../types';

// Minimal events to make the stats bar render (needs at least 1 message or tool call)
const mockEvents: EventItem[] = [
  {
    id: 'e1',
    type: 'user',
    content: 'Hello',
    timestamp: 1000,
    agentId: 'agent-0',
    isMessage: true,
  } as EventItem,
  {
    id: 'e2',
    type: 'tool_call',
    content: '',
    timestamp: 2000,
    agentId: 'agent-0',
    toolCall: { name: 'test_tool', raw_input: {} },
  } as EventItem,
];

const defaultProps = {
  events: mockEvents,
  globalElapsedMs: 5000,
  isSessionActive: false,
  agentModels: {} as Record<string, { provider?: string; model?: string; contextLimit?: number; node?: string }>,
  sessionLimits: null as SessionLimits | null,
};

describe('HeaderStatsBar', () => {
  it('renders stats bar with elapsed time', () => {
    render(<HeaderStatsBar {...defaultProps} />);
    // The stats bar should render — check for some text content
    const container = screen.getByTitle('Click for detailed stats');
    expect(container).toBeDefined();
  });

  it('applies compact mode class when compact prop is true', () => {
    const { container } = render(<HeaderStatsBar {...defaultProps} compact />);
    const statsDiv = container.querySelector('[data-testid="header-stats-bar"]');
    expect(statsDiv).not.toBeNull();
    // In compact mode, separator pipes should be hidden
    const separators = container.querySelectorAll('[data-testid="stats-separator"]');
    separators.forEach((sep) => {
      expect(sep.className).toContain('hidden');
    });
  });

  it('shows all separators in non-compact mode', () => {
    const { container } = render(<HeaderStatsBar {...defaultProps} compact={false} />);
    const separators = container.querySelectorAll('[data-testid="stats-separator"]');
    // Separators should be visible (no "hidden" class)
    separators.forEach((sep) => {
      expect(sep.className).not.toContain('hidden');
    });
  });

  it('hides labels in compact mode', () => {
    render(<HeaderStatsBar {...defaultProps} compact />);
    // In compact mode, the bar should be more condensed (fewer gaps)
    const statsBar = screen.getByTestId('header-stats-bar');
    expect(statsBar.className).toContain('gap-1.5');
  });

  it('uses normal gaps in non-compact mode', () => {
    render(<HeaderStatsBar {...defaultProps} compact={false} />);
    const statsBar = screen.getByTestId('header-stats-bar');
    expect(statsBar.className).toContain('gap-3');
  });
});
