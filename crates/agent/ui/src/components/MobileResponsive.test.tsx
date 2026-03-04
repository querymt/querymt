/**
 * Mobile Responsive Integration Tests
 *
 * Verifies that key components include responsive Tailwind classes
 * to prevent overflow and ensure usability on mobile viewports (<=768px).
 *
 * These are structural tests — they assert CSS class presence, not pixel-level layout.
 */
import { describe, it, expect, vi, beforeAll } from 'vitest';
import { render, screen } from '@testing-library/react';
import { TodoRail } from './TodoRail';
import { HeaderStatsBar } from './HeaderStatsBar';
import { TurnCard } from './TurnCard';
import type { TodoItem, EventItem, SessionLimits, Turn, UiAgentInfo, EventRow } from '../types';
import type { TodoStats } from '../hooks/useTodoState';

// ---- Mocks for TurnCard (same as TurnCard.test.tsx) ----
vi.mock('./MessageContent', () => ({
  MessageContent: ({ content }: { content: string }) => <div data-testid="message-content">{content}</div>,
}));
vi.mock('./ActivitySection', () => ({
  ActivitySection: ({ toolCalls }: { toolCalls: any[] }) => (
    <div data-testid="activity-section">{toolCalls.length} tool calls</div>
  ),
}));
vi.mock('./PinnedUserMessage', () => ({
  PinnedUserMessage: () => <div data-testid="pinned-message" />,
}));
vi.mock('./ModelConfigPopover', () => ({
  ModelConfigPopover: ({ children }: { children: React.ReactNode }) => <>{children}</>,
}));
vi.mock('./ElicitationCard', () => ({
  ElicitationCard: () => <div data-testid="elicitation-card" />,
}));
vi.mock('../utils/clipboard', () => ({
  copyToClipboard: vi.fn().mockResolvedValue(true),
}));

beforeAll(() => {
  const mockIntersectionObserver = vi.fn().mockImplementation(() => ({
    observe: vi.fn(),
    unobserve: vi.fn(),
    disconnect: vi.fn(),
  }));
  vi.stubGlobal('IntersectionObserver', mockIntersectionObserver);
});

// ---- Helpers ----

function makeTurn(overrides: Partial<Turn> = {}): Turn {
  return {
    id: 'turn-0',
    userMessage: { id: 'u0', type: 'user', content: 'Hello', timestamp: 1000, agentId: 'primary', isMessage: true } as any,
    agentMessages: [{ id: 'a0', type: 'agent', content: 'Hi', timestamp: 2000, agentId: 'primary', isMessage: true }] as any,
    toolCalls: [],
    delegations: [],
    agentId: 'primary',
    startTime: 1000,
    endTime: 2000,
    isActive: false,
    ...overrides,
  };
}

const mockEvents: EventItem[] = [
  { id: 'e1', type: 'user', content: 'Hello', timestamp: 1000, agentId: 'agent-0', isMessage: true } as EventItem,
  { id: 'e2', type: 'tool_call', content: '', timestamp: 2000, agentId: 'agent-0', toolCall: { name: 'test', raw_input: {} } } as EventItem,
];

const defaultTodoStats: TodoStats = { total: 3, completed: 1, inProgress: 1, pending: 1, cancelled: 0 };
const defaultTodos: TodoItem[] = [
  { id: 't1', content: 'Task one', status: 'pending', priority: 'high' },
  { id: 't2', content: 'Task two', status: 'in_progress', priority: 'medium' },
];

// ---- Tests ----

describe('Mobile Responsive Audit', () => {
  describe('TodoRail hides on mobile', () => {
    it('expanded rail uses hidden md:flex', () => {
      const { container } = render(
        <TodoRail todos={defaultTodos} stats={defaultTodoStats} collapsed={false} onToggleCollapse={vi.fn()} recentlyChangedIds={new Set()} />
      );
      const rail = container.firstElementChild as HTMLElement;
      expect(rail.className).toContain('hidden');
      expect(rail.className).toContain('md:flex');
    });

    it('collapsed rail uses hidden md:flex', () => {
      const { container } = render(
        <TodoRail todos={defaultTodos} stats={defaultTodoStats} collapsed={true} onToggleCollapse={vi.fn()} recentlyChangedIds={new Set()} />
      );
      const rail = container.firstElementChild as HTMLElement;
      expect(rail.className).toContain('hidden');
      expect(rail.className).toContain('md:flex');
    });
  });

  describe('HeaderStatsBar compact mode', () => {
    it('compact mode uses smaller gap', () => {
      const { container } = render(
        <HeaderStatsBar
          events={mockEvents}
          globalElapsedMs={5000}
          isSessionActive={false}
          agentModels={{}}
          sessionLimits={null}
          compact
        />
      );
      const bar = container.querySelector('[data-testid="header-stats-bar"]') as HTMLElement;
      expect(bar).not.toBeNull();
      expect(bar.className).toContain('gap-1.5');
    });

    it('non-compact mode uses normal gap', () => {
      const { container } = render(
        <HeaderStatsBar
          events={mockEvents}
          globalElapsedMs={5000}
          isSessionActive={false}
          agentModels={{}}
          sessionLimits={null}
          compact={false}
        />
      );
      const bar = container.querySelector('[data-testid="header-stats-bar"]') as HTMLElement;
      expect(bar).not.toBeNull();
      expect(bar.className).toContain('gap-3');
    });
  });

  describe('TurnCard overflow prevention', () => {
    it('has overflow-hidden on turn card wrapper', () => {
      const { container } = render(
        <TurnCard
          turn={makeTurn()}
          agents={[]}
          onToolClick={vi.fn()}
          onDelegateClick={vi.fn()}
        />
      );
      const turnCard = container.querySelector('.turn-card') as HTMLElement;
      expect(turnCard).not.toBeNull();
      expect(turnCard.className).toContain('overflow-hidden');
    });

    it('has responsive padding classes', () => {
      const { container } = render(
        <TurnCard
          turn={makeTurn()}
          agents={[]}
          onToolClick={vi.fn()}
          onDelegateClick={vi.fn()}
        />
      );
      const turnCard = container.querySelector('.turn-card') as HTMLElement;
      expect(turnCard).not.toBeNull();
      expect(turnCard.className).toContain('px-2');
      expect(turnCard.className).toContain('md:px-4');
    });
  });

  describe('useIsMobile hook integration', () => {
    it('hook module is importable', async () => {
      const mod = await import('../hooks/useIsMobile');
      expect(typeof mod.useIsMobile).toBe('function');
    });
  });
});
