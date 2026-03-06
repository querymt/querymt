import { describe, it, expect } from 'vitest';
import { render, screen } from '@testing-library/react';
import { HeaderStatsBar } from './HeaderStatsBar';
import type { EventItem, SessionLimits } from '../types';
import { vi } from 'vitest';

// Provide a stable timer context so HeaderStatsBar can be tested in isolation
// without needing a full UiClientProvider stack.
vi.mock('../context/SessionTimerContext', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../context/SessionTimerContext')>();
  return {
    ...actual,
    useSessionTimerContext: () => ({
      globalElapsedMs: 5000,
      agentElapsedMs: new Map<string, number>(),
      isSessionActive: false,
    }),
    useSessionTimerElapsed: () => 5000,
    useSessionTimerAgents: () => new Map<string, number>(),
    useSessionTimerActive: () => false,
  };
});

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

  it('shows separators progressively in non-compact mode', () => {
    const { container } = render(<HeaderStatsBar {...defaultProps} compact={false} />);
    const separators = container.querySelectorAll('[data-testid="stats-separator"]');
    // Separators use responsive classes (hidden md:inline / hidden lg:inline) to appear at wider breakpoints
    separators.forEach((sep) => {
      expect(sep.className).toMatch(/md:inline|lg:inline|xl:inline/);
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
