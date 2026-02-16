import { describe, it, expect, vi, beforeEach, beforeAll } from 'vitest';
import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { TurnCard } from './TurnCard';
import { Turn, EventRow, UiAgentInfo } from '../types';

// Mock heavy child components to isolate TurnCard logic
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

// Mock clipboard utility
vi.mock('../utils/clipboard', () => ({
  copyToClipboard: vi.fn().mockResolvedValue(true),
}));

// Mock IntersectionObserver for TurnCard
beforeAll(() => {
  const mockIntersectionObserver = vi.fn().mockImplementation(() => ({
    observe: vi.fn(),
    unobserve: vi.fn(),
    disconnect: vi.fn(),
  }));
  vi.stubGlobal('IntersectionObserver', mockIntersectionObserver);
});

// Helper: Create Turn objects
function makeTurn(overrides: Partial<Turn> = {}): Turn {
  return {
    id: 'turn-0',
    userMessage: undefined,
    agentMessages: [],
    toolCalls: [],
    delegations: [],
    agentId: 'primary',
    startTime: 1000,
    endTime: 2000,
    isActive: false,
    ...overrides,
  };
}

// Helper: Create EventRow objects
function makeRow(overrides: Partial<EventRow> = {}): EventRow {
  return {
    id: 'row-1',
    type: 'agent',
    content: 'Hello',
    timestamp: 1000,
    depth: 0,
    isMessage: true,
    agentId: 'primary',
    ...overrides,
  } as EventRow;
}

describe('TurnCard', () => {
  const defaultAgents: UiAgentInfo[] = [
    { id: 'primary', name: 'Primary Agent', description: 'Primary agent for handling tasks', capabilities: [] },
  ];

  const defaultProps = {
    turn: makeTurn(),
    agents: defaultAgents,
    onToolClick: vi.fn(),
    onDelegateClick: vi.fn(),
  };

  beforeEach(() => {
    vi.clearAllMocks();
  });

  it('renders user message when turn has userMessage', () => {
    const turn = makeTurn({
      userMessage: makeRow({ type: 'user', content: 'Hello AI' }),
    });

    render(<TurnCard {...defaultProps} turn={turn} />);

    expect(screen.getByText('User')).toBeInTheDocument();
    expect(screen.getByText('Hello AI')).toBeInTheDocument();
  });

  it('renders agent name from agents list', () => {
    const turn = makeTurn({ agentId: 'agent-1' });
    const agents: UiAgentInfo[] = [{ id: 'agent-1', name: 'Code Agent', description: 'Code agent', capabilities: [] }];

    render(<TurnCard {...defaultProps} turn={turn} agents={agents} />);

    expect(screen.getByText('Code Agent')).toBeInTheDocument();
  });

  it('falls back to "Agent" when agentId is undefined', () => {
    const turn = makeTurn({ agentId: undefined });

    render(<TurnCard {...defaultProps} turn={turn} />);

    expect(screen.getByText('Agent')).toBeInTheDocument();
  });

  it('renders agent messages via MessageContent', () => {
    const turn = makeTurn({
      agentMessages: [makeRow({ content: 'Here is the answer' })],
    });

    render(<TurnCard {...defaultProps} turn={turn} />);

    expect(screen.getByText('Here is the answer')).toBeInTheDocument();
  });

  it('renders "thinking..." when turn is active', () => {
    const turn = makeTurn({ isActive: true });

    render(<TurnCard {...defaultProps} turn={turn} />);

    expect(screen.getByText('thinking...')).toBeInTheDocument();
  });

  it('does NOT render "thinking..." when turn is inactive', () => {
    const turn = makeTurn({ isActive: false });

    render(<TurnCard {...defaultProps} turn={turn} />);

    expect(screen.queryByText('thinking...')).not.toBeInTheDocument();
  });

  it('renders tool calls via ActivitySection', () => {
    const turn = makeTurn({
      toolCalls: [makeRow({ type: 'tool_call' })],
    });

    render(<TurnCard {...defaultProps} turn={turn} />);

    expect(screen.getByTestId('activity-section')).toBeInTheDocument();
    expect(screen.getByText('1 tool calls')).toBeInTheDocument();
  });

  it('renders "Working..." when turn is active with no messages or tools', () => {
    const turn = makeTurn({
      isActive: true,
      agentMessages: [],
      toolCalls: [],
    });

    render(<TurnCard {...defaultProps} turn={turn} />);

    expect(screen.getByText('Working...')).toBeInTheDocument();
  });

  it('shows model label when showModelLabel is true', () => {
    const turn = makeTurn({
      modelLabel: 'anthropic / claude-3',
      modelConfigId: 1,
    });

    render(<TurnCard {...defaultProps} turn={turn} showModelLabel={true} />);

    expect(screen.getByText('anthropic / claude-3')).toBeInTheDocument();
  });

  it('hides model label when showModelLabel is false', () => {
    const turn = makeTurn({
      modelLabel: 'anthropic / claude-3',
      modelConfigId: 1,
    });

    render(<TurnCard {...defaultProps} turn={turn} showModelLabel={false} />);

    expect(screen.queryByText('anthropic / claude-3')).not.toBeInTheDocument();
  });

  it('shows undo button when canUndo is true and turn is not active', () => {
    const turn = makeTurn({ isActive: false });

    render(<TurnCard {...defaultProps} turn={turn} canUndo={true} onUndo={vi.fn()} />);

    expect(screen.getByText('Undo')).toBeInTheDocument();
  });

  it('hides undo button when turn is active', () => {
    const turn = makeTurn({ isActive: true });

    render(<TurnCard {...defaultProps} turn={turn} canUndo={true} onUndo={vi.fn()} />);

    expect(screen.queryByText('Undo')).not.toBeInTheDocument();
  });

  it('shows "Changes Undone" overlay when isUndone is true', () => {
    const turn = makeTurn();

    render(
      <TurnCard
        {...defaultProps}
        turn={turn}
        isUndone={true}
        revertedFiles={['src/foo.ts', 'src/bar.ts']}
        onRedo={vi.fn()}
      />
    );

    expect(screen.getByText('Changes Undone')).toBeInTheDocument();
    expect(screen.getByText('2 files reverted')).toBeInTheDocument();
    expect(screen.getByText('Redo Changes')).toBeInTheDocument();
  });

  it('shows file count in undone overlay', () => {
    const turn = makeTurn();
    const revertedFiles = [
      'src/a.ts',
      'src/b.ts',
      'src/c.ts',
      'src/d.ts',
      'src/e.ts',
      'src/f.ts',
    ];

    render(
      <TurnCard
        {...defaultProps}
        turn={turn}
        isUndone={true}
        revertedFiles={revertedFiles}
      />
    );

    expect(screen.getByText('6 files reverted')).toBeInTheDocument();
    expect(screen.getByText('+1 more file')).toBeInTheDocument();
  });

  it('calls onUndo when undo button clicked', async () => {
    const user = userEvent.setup();
    const onUndo = vi.fn();
    const turn = makeTurn({ isActive: false });

    render(<TurnCard {...defaultProps} turn={turn} canUndo={true} onUndo={onUndo} />);

    const undoButton = screen.getByText('Undo').closest('button');
    expect(undoButton).toBeTruthy();
    await user.click(undoButton!);

    expect(onUndo).toHaveBeenCalledTimes(1);
  });

  it('calls onRedo when redo button clicked in undone overlay', async () => {
    const user = userEvent.setup();
    const onRedo = vi.fn();
    const turn = makeTurn();

    render(
      <TurnCard
        {...defaultProps}
        turn={turn}
        isUndone={true}
        revertedFiles={['src/foo.ts']}
        onRedo={onRedo}
      />
    );

    const redoButton = screen.getByText('Redo Changes').closest('button');
    expect(redoButton).toBeTruthy();
    await user.click(redoButton!);

    expect(onRedo).toHaveBeenCalledTimes(1);
  });

  // ==================== Edge Case Tests ====================

  it('undo button hidden when canUndo false', () => {
    const turn = makeTurn({ isActive: false });

    render(<TurnCard {...defaultProps} turn={turn} canUndo={false} />);

    expect(screen.queryByText('Undo')).not.toBeInTheDocument();
  });

  it('undo overlay does not show redo when onRedo not provided', () => {
    const turn = makeTurn();

    render(
      <TurnCard
        {...defaultProps}
        turn={turn}
        isUndone={true}
        revertedFiles={['src/foo.ts']}
        // No onRedo callback
      />
    );

    expect(screen.getByText('Changes Undone')).toBeInTheDocument();
    expect(screen.queryByText('Redo Changes')).not.toBeInTheDocument();
  });

  it('undone overlay with 0 reverted files does not crash', () => {
    const turn = makeTurn();

    render(
      <TurnCard
        {...defaultProps}
        turn={turn}
        isUndone={true}
        revertedFiles={[]}
      />
    );

    expect(screen.getByText('Changes Undone')).toBeInTheDocument();
    expect(screen.getByText('No filesystem changes were made in this turn')).toBeInTheDocument();
  });

  it('undo button hidden when isUndone is true', () => {
    const turn = makeTurn({ isActive: false });

    render(
      <TurnCard
        {...defaultProps}
        turn={turn}
        canUndo={true}
        isUndone={true}
        revertedFiles={['src/foo.ts']}
        onUndo={vi.fn()}
      />
    );

    // Can't undo an already undone turn
    expect(screen.queryByText('Undo')).not.toBeInTheDocument();
  });

  it('shows stacked undo placeholder without redo action', () => {
    const turn = makeTurn({ isActive: false });

    render(
      <TurnCard
        {...defaultProps}
        turn={turn}
        isStackedUndone={true}
        onRedo={vi.fn()}
      />
    );

    expect(screen.getByText('Undone in stack. Redo newer undo first.')).toBeInTheDocument();
    expect(screen.queryByText('Redo Changes')).not.toBeInTheDocument();
  });

  it('dims turn content when stacked undone', () => {
    const turn = makeTurn({ isActive: false });

    const { container } = render(
      <TurnCard
        {...defaultProps}
        turn={turn}
        isStackedUndone={true}
      />
    );

    const root = container.querySelector('.turn-card');
    expect(root).toHaveAttribute('data-stacked-undone', 'true');
    expect(root).toHaveClass('opacity-45');
  });

  it('shows pending undo overlay and hides redo action', () => {
    const turn = makeTurn({ isActive: false });

    render(
      <TurnCard
        {...defaultProps}
        turn={turn}
        isUndoPending={true}
        onRedo={vi.fn()}
      />
    );

    expect(screen.getByText('Undoing Changes...')).toBeInTheDocument();
    expect(screen.queryByText('Redo Changes')).not.toBeInTheDocument();
  });

  it('undo button hidden when turn is stacked undone', () => {
    const turn = makeTurn({ isActive: false });

    render(
      <TurnCard
        {...defaultProps}
        turn={turn}
        canUndo={true}
        isStackedUndone={true}
        onUndo={vi.fn()}
      />
    );

    expect(screen.queryByText('Undo')).not.toBeInTheDocument();
  });

  it('undo button hidden when top undo is pending', () => {
    const turn = makeTurn({ isActive: false });

    render(
      <TurnCard
        {...defaultProps}
        turn={turn}
        canUndo={true}
        isUndoPending={true}
        onUndo={vi.fn()}
      />
    );

    expect(screen.queryByText('Undo')).not.toBeInTheDocument();
  });
});
