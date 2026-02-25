import { describe, expect, it, vi } from 'vitest';
import { act, render, screen } from '@testing-library/react';
import { PluginUpdateIndicator } from './PluginUpdateIndicator';
import type { PluginUpdateResult, PluginUpdateStatus } from '../types';

describe('PluginUpdateIndicator', () => {
  it('renders nothing when isUpdatingPlugins is false and results is null', () => {
    const { container } = render(
      <PluginUpdateIndicator
        isUpdatingPlugins={false}
        pluginUpdateStatus={{}}
        pluginUpdateResults={null}
      />
    );
    expect(container.firstChild).toBeNull();
  });

  it('shows plugin name and phase when isUpdatingPlugins is true', () => {
    const status: Record<string, PluginUpdateStatus> = {
      'my-plugin': {
        plugin_name: 'my-plugin',
        image_reference: 'registry.example.com/my-plugin:latest',
        phase: 'resolving',
        bytes_downloaded: 0,
      },
    };

    render(
      <PluginUpdateIndicator
        isUpdatingPlugins={true}
        pluginUpdateStatus={status}
        pluginUpdateResults={null}
      />
    );

    expect(screen.getByText('my-plugin')).toBeInTheDocument();
    expect(screen.getByText(/resolving/i)).toBeInTheDocument();
  });

  it('shows download percent when phase is downloading', () => {
    const status: Record<string, PluginUpdateStatus> = {
      'my-plugin': {
        plugin_name: 'my-plugin',
        image_reference: 'registry.example.com/my-plugin:latest',
        phase: 'downloading',
        bytes_downloaded: 512,
        bytes_total: 1024,
        percent: 50,
      },
    };

    render(
      <PluginUpdateIndicator
        isUpdatingPlugins={true}
        pluginUpdateStatus={status}
        pluginUpdateResults={null}
      />
    );

    expect(screen.getByText(/50%/)).toBeInTheDocument();
  });

  it('shows success/failure summary from results after completion', () => {
    const results: PluginUpdateResult[] = [
      { plugin_name: 'plugin-a', success: true },
      { plugin_name: 'plugin-b', success: false, message: 'connection refused' },
    ];

    render(
      <PluginUpdateIndicator
        isUpdatingPlugins={false}
        pluginUpdateStatus={{}}
        pluginUpdateResults={results}
      />
    );

    expect(screen.getByText(/1 succeeded/i)).toBeInTheDocument();
    expect(screen.getByText(/1 failed/i)).toBeInTheDocument();
  });

  it('auto-dismisses results after 5 seconds', () => {
    vi.useFakeTimers();
    const results: PluginUpdateResult[] = [
      { plugin_name: 'plugin-a', success: true },
    ];

    const { container } = render(
      <PluginUpdateIndicator
        isUpdatingPlugins={false}
        pluginUpdateStatus={{}}
        pluginUpdateResults={results}
      />
    );

    expect(container.firstChild).not.toBeNull();

    act(() => {
      vi.advanceTimersByTime(5000);
    });

    expect(container.firstChild).toBeNull();
    vi.useRealTimers();
  });
});
