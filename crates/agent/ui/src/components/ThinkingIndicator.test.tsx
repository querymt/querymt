import { describe, it, expect } from 'vitest';
import { render, screen } from '@testing-library/react';
import { ThinkingIndicator } from './ThinkingIndicator';
import { UiAgentInfo } from '../types';

describe('ThinkingIndicator', () => {
  const defaultAgents: UiAgentInfo[] = [
    { id: 'primary', name: 'Primary Agent', description: 'Primary agent for handling tasks', capabilities: [] },
    { id: 'agent-1', name: 'Code Agent', description: 'Agent specialized in code tasks', capabilities: [] },
  ];

  it('renders "is thinking..."', () => {
    render(<ThinkingIndicator agentId="primary" agents={defaultAgents} />);

    expect(screen.getByText(/is thinking\.\.\./i)).toBeInTheDocument();
  });

  it('renders agent name', () => {
    render(<ThinkingIndicator agentId="agent-1" agents={defaultAgents} />);

    expect(screen.getByText('Code Agent')).toBeInTheDocument();
  });

  it('falls back to "Agent" when agentId is null', () => {
    render(<ThinkingIndicator agentId={null} agents={defaultAgents} />);

    expect(screen.getByText('Agent')).toBeInTheDocument();
  });

  it('renders pulsing indicator dot when thinking', () => {
    const { container } = render(
      <ThinkingIndicator agentId="primary" agents={defaultAgents} />
    );

    const pulsingElement = container.querySelector('.animate-pulse');
    expect(pulsingElement).toBeInTheDocument();
  });
});
