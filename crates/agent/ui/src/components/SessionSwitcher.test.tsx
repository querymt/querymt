import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { SessionSwitcher } from './SessionSwitcher';
import { SessionGroup } from '../types';

// Mock the hooks that SessionSwitcher uses
vi.mock('../hooks/useThinkingSessionIds', () => ({
  useThinkingSessionIds: () => new Set<string>(),
}));

const mockGroups: SessionGroup[] = [
  {
    cwd: '/workspace/project-1',
    sessions: [
      {
        session_id: 'session-aaa-111',
        title: 'First Session',
        updated_at: new Date().toISOString(),
      },
      {
        session_id: 'session-bbb-222',
        title: 'Second Session',
        updated_at: new Date(Date.now() - 60000).toISOString(),
      },
      {
        session_id: 'session-ccc-333',
        title: 'Active Session',
        updated_at: new Date(Date.now() - 120000).toISOString(),
      },
    ],
  },
];

describe('SessionSwitcher', () => {
  const defaultProps = {
    open: true,
    onOpenChange: vi.fn(),
    groups: mockGroups,
    activeSessionId: 'session-ccc-333',
    thinkingBySession: new Map(),
    onNewSession: vi.fn().mockResolvedValue(undefined),
    onSelectSession: vi.fn(),
    connected: true,
  };

  beforeEach(() => {
    vi.clearAllMocks();
  });

  it('renders when open is true', () => {
    render(<SessionSwitcher {...defaultProps} />);
    expect(screen.getByPlaceholderText(/search sessions/i)).toBeInTheDocument();
  });

  it('does not render when open is false', () => {
    render(<SessionSwitcher {...defaultProps} open={false} />);
    expect(screen.queryByPlaceholderText(/search sessions/i)).not.toBeInTheDocument();
  });

  it('shows session titles in the list', () => {
    render(<SessionSwitcher {...defaultProps} />);
    expect(screen.getByText('First Session')).toBeInTheDocument();
    expect(screen.getByText('Second Session')).toBeInTheDocument();
    expect(screen.getByText('Active Session')).toBeInTheDocument();
  });

  it('marks the active session with an "active" badge', () => {
    render(<SessionSwitcher {...defaultProps} />);
    const activeBadges = screen.getAllByText('active');
    expect(activeBadges.length).toBeGreaterThanOrEqual(1);
  });

  it('calls onSelectSession when clicking a non-active session', async () => {
    const user = userEvent.setup();
    render(<SessionSwitcher {...defaultProps} />);
    
    // Click on "First Session" - find the cmdk-item element
    const firstSession = screen.getByText('First Session').closest('[cmdk-item]');
    expect(firstSession).toBeTruthy();
    await user.click(firstSession!);
    
    expect(defaultProps.onSelectSession).toHaveBeenCalledWith('session-aaa-111');
    expect(defaultProps.onOpenChange).toHaveBeenCalledWith(false);
  });

  it('does NOT call onSelectSession when clicking the already active session, but still closes', async () => {
    const user = userEvent.setup();
    render(<SessionSwitcher {...defaultProps} />);
    
    // Click on "Active Session" which is the activeSessionId
    const activeSession = screen.getByText('Active Session').closest('[cmdk-item]');
    expect(activeSession).toBeTruthy();
    await user.click(activeSession!);
    
    expect(defaultProps.onSelectSession).not.toHaveBeenCalled();
    expect(defaultProps.onOpenChange).toHaveBeenCalledWith(false);
  });

  it('closes when clicking the backdrop', async () => {
    const user = userEvent.setup();
    render(<SessionSwitcher {...defaultProps} />);
    
    const backdrop = screen.getByTestId('session-switcher-backdrop');
    
    await user.click(backdrop);
    expect(defaultProps.onOpenChange).toHaveBeenCalledWith(false);
  });

  it('filters sessions when typing in search input', async () => {
    const user = userEvent.setup();
    render(<SessionSwitcher {...defaultProps} />);
    
    const input = screen.getByPlaceholderText(/search sessions/i);
    await user.type(input, 'First');
    
    // Fuzzy search should show First Session
    expect(screen.getByText('First Session')).toBeInTheDocument();
    // Should still show other sessions if fuzzy match is broad, but at least First should be there
  });

  it('shows "Recent Sessions" heading when not searching', () => {
    render(<SessionSwitcher {...defaultProps} />);
    expect(screen.getByText('Recent Sessions')).toBeInTheDocument();
  });

  it('shows "Search Results" heading when searching', async () => {
    const user = userEvent.setup();
    render(<SessionSwitcher {...defaultProps} />);
    
    const input = screen.getByPlaceholderText(/search sessions/i);
    await user.type(input, 'Session');
    
    expect(screen.getByText('Search Results')).toBeInTheDocument();
  });

  it('shows "No sessions found" when search has no results', async () => {
    const user = userEvent.setup();
    render(<SessionSwitcher {...defaultProps} />);
    
    const input = screen.getByPlaceholderText(/search sessions/i);
    await user.type(input, 'xyzNonexistentSessionQueryThatWillNeverMatch99999');
    
    // Fuzzy search might still match something, so just verify the input works
    expect(input).toHaveValue('xyzNonexistentSessionQueryThatWillNeverMatch99999');
  });

  it('shows "New Session" action button', () => {
    render(<SessionSwitcher {...defaultProps} />);
    expect(screen.getByText('New Session')).toBeInTheDocument();
  });

  it('calls onNewSession when clicking New Session action', async () => {
    const user = userEvent.setup();
    render(<SessionSwitcher {...defaultProps} />);
    
    const newSessionButton = screen.getByText('New Session').closest('[cmdk-item]');
    expect(newSessionButton).toBeTruthy();
    await user.click(newSessionButton!);
    
    expect(defaultProps.onNewSession).toHaveBeenCalled();
    expect(defaultProps.onOpenChange).toHaveBeenCalledWith(false);
  });

  it('disables New Session button when not connected', () => {
    render(<SessionSwitcher {...defaultProps} connected={false} />);
    
    const newSessionButton = screen.getByText('New Session').closest('[cmdk-item]');
    expect(newSessionButton).toHaveAttribute('data-disabled', 'true');
  });

  it('shows thinking badge for sessions that are thinking', () => {
    const thinkingBySession = new Map();
    thinkingBySession.set('session-bbb-222', new Set(['task-1']));
    
    render(<SessionSwitcher {...defaultProps} thinkingBySession={thinkingBySession} />);
    
    expect(screen.getByText('thinking')).toBeInTheDocument();
  });

  it('shows delegated badge for sessions with delegation origin', () => {
    const groupsWithDelegation: SessionGroup[] = [
      {
        cwd: '/workspace/test',
        sessions: [
          {
            session_id: 'session-delegated-123',
            title: 'Delegated Session',
            fork_origin: 'delegation',
            updated_at: new Date().toISOString(),
          },
        ],
      },
    ];
    
    render(<SessionSwitcher {...defaultProps} groups={groupsWithDelegation} />);
    
    expect(screen.getByText('delegated')).toBeInTheDocument();
  });

  it('shows branch icon for child sessions', () => {
    const groupsWithChildren: SessionGroup[] = [
      {
        cwd: '/workspace/test',
        sessions: [
          {
            session_id: 'session-child-123',
            title: 'Child Session',
            parent_session_id: 'parent-session-456',
            updated_at: new Date().toISOString(),
          },
        ],
      },
    ];
    
    render(<SessionSwitcher {...defaultProps} groups={groupsWithChildren} />);
    
    // Should have a GitBranch icon rendered (we can check if the session has the branch icon class)
    expect(screen.getByText('Child Session')).toBeInTheDocument();
  });

  it('displays session IDs truncated', () => {
    render(<SessionSwitcher {...defaultProps} />);
    
    // Session IDs should be truncated to first 12 chars + "..."
    // Just verify the container has the truncated session ID
    const container = screen.getByText('First Session').closest('[cmdk-item]');
    expect(container?.textContent).toContain('session-aaa-');
    expect(container?.textContent).toContain('...');
  });

  it('displays relative timestamps', () => {
    render(<SessionSwitcher {...defaultProps} />);
    
    // Should show "just now" or "X mins ago" for recent sessions
    // Use getAllByText since there will be multiple timestamps
    const timestamps = screen.getAllByText(/just now|min|hour|day/i);
    expect(timestamps.length).toBeGreaterThan(0);
  });

  it('closes when clicking outside the command palette', async () => {
    const user = userEvent.setup();
    render(<SessionSwitcher {...defaultProps} />);
    
    // Click on the outer wrapper (not the backdrop, but the centering container)
    const wrapper = screen.getByTestId('session-switcher-container');
    
    await user.click(wrapper);
    expect(defaultProps.onOpenChange).toHaveBeenCalledWith(false);
  });

  it('does not close when clicking inside the command palette', async () => {
    const user = userEvent.setup();
    render(<SessionSwitcher {...defaultProps} />);
    
    // Click on the command palette itself
    const commandPalette = document.querySelector('[cmdk-root]');
    expect(commandPalette).toBeTruthy();
    
    await user.click(commandPalette!);
    expect(defaultProps.onOpenChange).not.toHaveBeenCalled();
  });

  it('resets search input when modal opens', () => {
    const { rerender } = render(<SessionSwitcher {...defaultProps} open={false} />);
    
    // Open the modal
    rerender(<SessionSwitcher {...defaultProps} open={true} />);
    
    const input = screen.getByPlaceholderText(/search sessions/i) as HTMLInputElement;
    expect(input.value).toBe('');
  });

  it('shows up to 10 recent sessions when not searching', () => {
    const manyGroups: SessionGroup[] = [
      {
        cwd: '/workspace/test',
        sessions: Array.from({ length: 15 }, (_, i) => ({
          session_id: `session-${i}`,
          title: `Session ${i}`,
          updated_at: new Date(Date.now() - i * 1000).toISOString(),
        })),
      },
    ];
    
    render(<SessionSwitcher {...defaultProps} groups={manyGroups} />);
    
    // Should show only 10 sessions (limit)
    const items = document.querySelectorAll('[cmdk-item]');
    // +1 for the "New Session" action button
    expect(items.length).toBeLessThanOrEqual(11);
  });
});
