import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { SessionPicker } from './SessionPicker';
import { SessionGroup } from '../types';

vi.mock('../hooks/useThinkingSessionIds', () => ({
  useThinkingSessionIds: () => new Set<string>(),
}));

const groups: SessionGroup[] = [
  {
    cwd: '/workspace/project',
    sessions: [
      {
        session_id: 'root-session-111',
        title: 'Root Session',
        has_children: true,
        fork_count: 2,
        updated_at: new Date().toISOString(),
      },
    ],
  },
];

describe('SessionPicker', () => {
  const defaultProps = {
    groups,
    onSelectSession: vi.fn(),
    onDeleteSession: vi.fn(),
    onNewSession: vi.fn(),
    onLoadSessionChildren: vi.fn(),
  };

  beforeEach(() => {
    vi.clearAllMocks();
  });

  it('loads forks when expanding a root session', async () => {
    const user = userEvent.setup();
    render(<SessionPicker {...defaultProps} />);

    await user.click(screen.getByLabelText(/expand forks for root session/i));

    expect(defaultProps.onLoadSessionChildren).toHaveBeenCalledWith('root-session-111');
  });

  it('shows the fork count in the root expand control', () => {
    render(<SessionPicker {...defaultProps} />);

    const expandButton = screen.getByLabelText(/expand forks for root session/i);
    expect(expandButton).toHaveTextContent('2');
  });

  it('does not render expand control when backend reports no user forks', () => {
    render(
      <SessionPicker
        {...defaultProps}
        groups={[
          {
            ...groups[0],
            sessions: [{ ...groups[0].sessions[0], has_children: false, fork_count: 0 }],
          },
        ]}
      />
    );

    expect(screen.queryByLabelText(/expand forks for root session/i)).not.toBeInTheDocument();
  });

  it('does not render expand control for child forks', () => {
    render(
      <SessionPicker
        {...defaultProps}
        groups={[
          {
            ...groups[0],
            sessions: [
              {
                ...groups[0].sessions[0],
                parent_session_id: 'parent-session-000',
                has_children: true,
                fork_count: 1,
              },
            ],
          },
        ]}
      />
    );

    expect(screen.queryByLabelText(/expand forks for root session/i)).not.toBeInTheDocument();
  });

  it('preserves orphan child sessions as top-level rows', () => {
    render(
      <SessionPicker
        {...defaultProps}
        groups={[
          {
            cwd: '/workspace/project',
            sessions: [
              {
                session_id: 'orphan-fork-333',
                title: 'Orphan Fork',
                parent_session_id: 'missing-parent-000',
                fork_origin: 'user',
              },
            ],
          },
        ] as SessionGroup[]}
      />
    );

    expect(screen.getByText('Orphan Fork')).toBeInTheDocument();
  });

  it('shows loaded forks without delegated badges', async () => {
    const user = userEvent.setup();
    const groupsWithFork = [
      {
        ...groups[0],
        sessions: [
          {
            ...groups[0].sessions[0],
            children: [
              {
                session_id: 'fork-session-222',
                title: 'User Fork',
                parent_session_id: 'root-session-111',
                fork_origin: 'user',
              },
            ],
          },
        ],
      },
    ];

    render(<SessionPicker {...defaultProps} groups={groupsWithFork as SessionGroup[]} />);
    await user.click(screen.getByLabelText(/expand forks for root session/i));

    expect(screen.getByText('User Fork')).toBeInTheDocument();
    expect(screen.queryByText('delegated')).not.toBeInTheDocument();
  });
});
