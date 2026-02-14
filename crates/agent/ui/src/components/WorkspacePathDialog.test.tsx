import { describe, expect, it, vi } from 'vitest';
import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { WorkspacePathDialog } from './WorkspacePathDialog';

describe('WorkspacePathDialog', () => {
  it('renders with the provided default value', () => {
    render(
      <WorkspacePathDialog
        open={true}
        defaultValue="/workspace/demo"
        onSubmit={vi.fn()}
        onCancel={vi.fn()}
      />,
    );

    expect(screen.getByLabelText('Workspace path (optional)')).toHaveValue('/workspace/demo');
  });

  it('calls onSubmit with the typed path', async () => {
    const user = userEvent.setup();
    const onSubmit = vi.fn();

    render(
      <WorkspacePathDialog open={true} defaultValue="" onSubmit={onSubmit} onCancel={vi.fn()} />,
    );

    const input = screen.getByLabelText('Workspace path (optional)');
    await user.type(input, '/tmp/new-workspace');
    await user.click(screen.getByRole('button', { name: 'Start Session' }));

    expect(onSubmit).toHaveBeenCalledWith('/tmp/new-workspace');
  });

  it('calls onCancel when cancel button is clicked', async () => {
    const user = userEvent.setup();
    const onCancel = vi.fn();

    render(
      <WorkspacePathDialog open={true} defaultValue="" onSubmit={vi.fn()} onCancel={onCancel} />,
    );

    await user.click(screen.getByRole('button', { name: 'Cancel' }));

    expect(onCancel).toHaveBeenCalledTimes(1);
  });
});
