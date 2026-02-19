import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render, screen } from '@testing-library/react';
import { DelegationDrawer } from './DelegationDrawer';
import { DelegationGroupInfo, EventRow, UiAgentInfo } from '../types';

vi.mock('./ToolSummary', () => ({
  ToolSummary: ({ event }: { event: EventRow }) => (
    <div data-testid={`tool-summary-${event.id}`}>{event.content}</div>
  ),
}));

vi.mock('./ElicitationCard', () => ({
  ElicitationCard: ({ data }: { data: { elicitationId: string } }) => (
    <div data-testid={`elicitation-card-${data.elicitationId}`}>Elicitation request</div>
  ),
}));

vi.mock('./ModelConfigPopover', () => ({
  ModelConfigPopover: ({ children }: { children: React.ReactNode }) => <>{children}</>,
}));

vi.mock('./MessageContent', () => ({
  MessageContent: ({ content }: { content: string }) => <div>{content}</div>,
}));

vi.mock('../hooks/useCopyToClipboard', () => ({
  useCopyToClipboard: () => ({
    copiedValue: null,
    copy: vi.fn(),
  }),
}));

function makeToolEvent(overrides: Partial<EventRow> = {}): EventRow {
  return {
    id: 'tool-1',
    type: 'tool_call',
    content: 'Ask permission',
    timestamp: 1000,
    depth: 1,
    agentId: 'delegate-agent',
    toolCall: {
      tool_call_id: 'question:tool-1',
      kind: 'question',
      status: 'in_progress',
    },
    ...overrides,
  } as EventRow;
}

function makeDelegateEvent(): EventRow {
  return {
    id: 'delegate-call-1',
    type: 'tool_call',
    content: 'Delegating task',
    timestamp: 900,
    depth: 0,
    agentId: 'planner',
    isDelegateToolCall: true,
    toolCall: {
      tool_call_id: 'delegate:1',
      kind: 'delegate',
      status: 'in_progress',
      raw_input: {
        target_agent_id: 'delegate-agent',
        objective: 'Investigate issue',
      },
    },
  } as EventRow;
}

function makeDelegation(events: EventRow[]): DelegationGroupInfo {
  return {
    id: 'delegation-1',
    delegateToolCallId: 'delegate:1',
    delegateEvent: makeDelegateEvent(),
    targetAgentId: 'delegate-agent',
    objective: 'Investigate issue',
    events,
    status: 'in_progress',
    startTime: 900,
  };
}

describe('DelegationDrawer', () => {
  const agents: UiAgentInfo[] = [
    {
      id: 'delegate-agent',
      name: 'Delegate Agent',
      description: 'Handles delegated tasks',
      capabilities: [],
    },
  ];

  beforeEach(() => {
    vi.clearAllMocks();
  });

  it('renders elicitation card for tool calls with elicitation data', () => {
    const toolEvent = makeToolEvent({
      elicitationData: {
        elicitationId: 'elic-1',
        sessionId: 'sess-1',
        message: 'Need confirmation',
        requestedSchema: {},
        source: 'builtin:shell_guard',
      },
    });

    render(
      <DelegationDrawer
        delegation={makeDelegation([toolEvent])}
        agents={agents}
        onClose={vi.fn()}
        onToolClick={vi.fn()}
      />,
    );

    expect(screen.getByTestId('tool-summary-tool-1')).toBeInTheDocument();
    expect(screen.getByTestId('elicitation-card-elic-1')).toBeInTheDocument();
  });

  it('does not render elicitation card for normal tool calls', () => {
    const toolEvent = makeToolEvent({ elicitationData: undefined });

    render(
      <DelegationDrawer
        delegation={makeDelegation([toolEvent])}
        agents={agents}
        onClose={vi.fn()}
        onToolClick={vi.fn()}
      />,
    );

    expect(screen.getByTestId('tool-summary-tool-1')).toBeInTheDocument();
    expect(screen.queryByTestId('elicitation-card-elic-1')).not.toBeInTheDocument();
  });
});
