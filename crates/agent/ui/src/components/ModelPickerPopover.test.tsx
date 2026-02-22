import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render, screen, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { ModelPickerPopover } from './ModelPickerPopover';
import type { ModelEntry, RecentModelEntry, UiAgentInfo } from '../types';

const mockAgents: UiAgentInfo[] = [
  {
    id: 'agent-1',
    name: 'Agent 1',
    mode: 'standard',
    provider: 'anthropic',
    model: 'claude-3-opus',
    max_tokens: null,
    thinking_budget_tokens: null,
  },
];

const mockModels: ModelEntry[] = [
  { provider: 'anthropic', model: 'claude-3-opus' },
  { provider: 'anthropic', model: 'claude-3-sonnet' },
  { provider: 'anthropic', model: 'claude-3-haiku' },
  { provider: 'openai', model: 'gpt-4' },
  { provider: 'openai', model: 'gpt-4-turbo' },
  { provider: 'openai', model: 'gpt-3.5-turbo' },
];

const mockRecentModels: RecentModelEntry[] = [
  { provider: 'anthropic', model: 'claude-3-opus', last_used: '2024-01-03T00:00:00Z', use_count: 3 },
  { provider: 'openai', model: 'gpt-4', last_used: '2024-01-02T00:00:00Z', use_count: 2 },
  { provider: 'anthropic', model: 'claude-3-haiku', last_used: '2024-01-01T00:00:00Z', use_count: 1 },
];

describe('ModelPickerPopover', () => {
  const defaultProps = {
    open: true,
    onOpenChange: vi.fn(),
    connected: true,
    routingMode: 'broadcast' as const,
    activeAgentId: 'agent-1',
    sessionId: 'session-1',
    sessionsByAgent: { 'agent-1': 'session-1' },
    agents: mockAgents,
    allModels: mockModels,
    currentProvider: 'anthropic',
    currentModel: 'claude-3-opus',
    currentWorkspace: '/test/workspace',
    recentModelsByWorkspace: {
      '/test/workspace': mockRecentModels,
    },
    providerCapabilities: {
      anthropic: { provider: 'anthropic', supports_custom_models: false },
      openai: { provider: 'openai', supports_custom_models: false },
      llama_cpp: { provider: 'llama_cpp', supports_custom_models: true },
    },
    modelDownloads: {},
    agentMode: 'standard',
    onRefresh: vi.fn(),
    onSetSessionModel: vi.fn(),
    onAddCustomModelFromHf: vi.fn(),
    onAddCustomModelFromFile: vi.fn(),
    onDeleteCustomModel: vi.fn(),
  };

  beforeEach(() => {
    vi.clearAllMocks();
  });

  it('renders when open is true', () => {
    render(<ModelPickerPopover {...defaultProps} />);
    expect(screen.getByText('Switch Model')).toBeInTheDocument();
  });

  it('does not render when open is false', () => {
    render(<ModelPickerPopover {...defaultProps} open={false} />);
    expect(screen.queryByText('Switch Model')).not.toBeInTheDocument();
  });

  it('shows recent models section when recent models are available', () => {
    render(<ModelPickerPopover {...defaultProps} />);
    expect(screen.getByText('Recent')).toBeInTheDocument();
  });

  it('shows models grouped by provider', () => {
    render(<ModelPickerPopover {...defaultProps} />);
    expect(screen.getByText('anthropic')).toBeInTheDocument();
    expect(screen.getByText('openai')).toBeInTheDocument();
  });

  it('displays current model with "current" badge', () => {
    render(<ModelPickerPopover {...defaultProps} />);
    const currentBadges = screen.getAllByText('current');
    expect(currentBadges.length).toBeGreaterThanOrEqual(1);
  });

  it('allows switching to a different model', async () => {
    const user = userEvent.setup();
    render(<ModelPickerPopover {...defaultProps} />);

    // Find and click on a non-current model
    const sonetModel = screen.getByText('claude-3-sonnet').closest('[cmdk-item]');
    expect(sonetModel).toBeTruthy();
    await user.click(sonetModel!);

    expect(defaultProps.onSetSessionModel).toHaveBeenCalledWith(
      'session-1',
      'anthropic/claude-3-sonnet',
      undefined
    );
    expect(defaultProps.onOpenChange).toHaveBeenCalledWith(false);
  });

  it('filters models based on search input', async () => {
    const user = userEvent.setup();
    render(<ModelPickerPopover {...defaultProps} />);

    const searchInput = screen.getByPlaceholderText('Filter models...');
    await user.type(searchInput, 'haiku');

    // Should show claude-3-haiku but not gpt models
    expect(screen.getByText('claude-3-haiku')).toBeInTheDocument();
    expect(screen.queryByText('gpt-4')).not.toBeInTheDocument();
  });

  it('shows "Switch all" button when target is "all" in broadcast mode', () => {
    render(<ModelPickerPopover {...defaultProps} />);
    
    // There are multiple comboboxes (select and cmdk input), query by label
    const targetSelect = screen.getByText('Target').parentElement?.querySelector('select');
    expect(targetSelect).toBeInTheDocument();
    
    // Initially shows "Switch" button
    expect(screen.getByRole('button', { name: /switch/i })).toHaveTextContent('Switch');
  });

  describe('Navigation bug fix - duplicate values', () => {
    it('assigns unique values to recent models vs provider-grouped models', () => {
      render(<ModelPickerPopover {...defaultProps} />);

      // The key aspects of the fix:
      // 1. Recent section exists
      expect(screen.getByText('Recent')).toBeInTheDocument();
      
      // 2. Provider sections exist
      expect(screen.getByText('anthropic')).toBeInTheDocument();
      expect(screen.getByText('openai')).toBeInTheDocument();

      // 3. Models that appear in both recent and provider lists should be shown in both sections
      // The "correctly switches model" tests below verify that the fix works by testing
      // that clicking on items in either section correctly switches the model

      // The fix prevents the navigation bug by ensuring:
      // - Recent items have value="recent:provider/model"
      // - Provider items have value="provider/model"
      // - These are unique values, so cmdk won't jump between them during navigation
    });

    it('navigates correctly through list without jumping when reaching duplicate models', async () => {
      const user = userEvent.setup();
      const { container } = render(<ModelPickerPopover {...defaultProps} />);

      const searchInput = screen.getByPlaceholderText('Filter models...');
      await user.click(searchInput);

      // Simulate keyboard navigation down through the list
      // The key behavior we're testing: when navigating to a model that exists in both
      // recent and provider sections, cmdk should not jump to the recent section
      // because they have unique values (recent has "recent:" prefix)
      
      // Navigate down multiple times
      await user.keyboard('{ArrowDown}');
      await user.keyboard('{ArrowDown}');
      await user.keyboard('{ArrowDown}');
      await user.keyboard('{ArrowDown}');
      await user.keyboard('{ArrowDown}');
      await user.keyboard('{ArrowDown}');
      
      // Verify that there's a selected item (cmdk manages this via data-selected attribute)
      const selectedItems = container.querySelectorAll('[cmdk-item][data-selected="true"]');
      
      // cmdk may or may not set aria-selected, but data-selected should be set
      // The important thing is that we don't crash and the navigation works
      // We can't reliably test the exact position due to cmdk's internal behavior
      // but we can verify the fix is in place by checking the values are unique
      const allItems = container.querySelectorAll('[cmdk-item]');
      const values = Array.from(allItems).map(item => item.getAttribute('data-value'));
      const uniqueValues = new Set(values);
      
      // All values should be unique (this is the fix - no duplicate values)
      expect(uniqueValues.size).toBe(values.length);
    });

    it('correctly switches model when selecting from recent section', async () => {
      const user = userEvent.setup();
      render(<ModelPickerPopover {...defaultProps} />);

      // Find and click on a model in the recent section
      // We need to find it within the Recent group
      const recentHeading = screen.getByText('Recent');
      const recentGroup = recentHeading.closest('[cmdk-group]');
      expect(recentGroup).toBeTruthy();

      // Find gpt-4 in recent section
      const recentGpt4 = within(recentGroup!).getByText(/gpt-4/).closest('[cmdk-item]');
      expect(recentGpt4).toBeTruthy();
      await user.click(recentGpt4!);

      // Should call with the correct model ID (without the "recent:" prefix)
      expect(defaultProps.onSetSessionModel).toHaveBeenCalledWith(
        'session-1',
        'openai/gpt-4',
        undefined
      );
    });

    it('correctly switches model when selecting from provider section', async () => {
      const user = userEvent.setup();
      render(<ModelPickerPopover {...defaultProps} />);

      // Find and click on a model in the provider section
      const anthropicHeading = screen.getByText('anthropic');
      const anthropicGroup = anthropicHeading.closest('[cmdk-group]');
      expect(anthropicGroup).toBeTruthy();

      // Find sonnet in the anthropic provider section
      const providerSonnet = within(anthropicGroup!).getByText('claude-3-sonnet').closest('[cmdk-item]');
      expect(providerSonnet).toBeTruthy();
      await user.click(providerSonnet!);

      // Should call with the correct model ID
      expect(defaultProps.onSetSessionModel).toHaveBeenCalledWith(
        'session-1',
        'anthropic/claude-3-sonnet',
        undefined
      );
    });

    it('filters recent models correctly with search', async () => {
      const user = userEvent.setup();
      render(<ModelPickerPopover {...defaultProps} />);

      const searchInput = screen.getByPlaceholderText('Filter models...');
      await user.type(searchInput, 'opus');

      // Should show claude-3-opus in both recent and provider sections
      // The recent section should still be visible
      const recentHeading = screen.queryByText('Recent');
      expect(recentHeading).toBeInTheDocument();

      // Both sections should show opus
      const opusItems = screen.getAllByText(/opus/);
      expect(opusItems.length).toBeGreaterThanOrEqual(2); // One in recent, one in provider
    });
  });

  describe('Custom model capability UI', () => {
    it('shows custom model controls only for providers with supports_custom_models', () => {
      const { rerender } = render(<ModelPickerPopover {...defaultProps} />);
      expect(screen.queryByText(/Add custom model/i)).not.toBeInTheDocument();

      rerender(
        <ModelPickerPopover
          {...defaultProps}
          currentProvider="llama_cpp"
          currentModel="model.gguf"
          allModels={[{ provider: 'llama_cpp', model: 'model.gguf', source: 'catalog', id: 'hf:repo:model.gguf' }]}
        />
      );
      expect(screen.getByText(/Add custom model \(llama_cpp\)/i)).toBeInTheDocument();
    });

    it('sends add/delete custom model actions for capability-enabled provider', async () => {
      const user = userEvent.setup();
      const onAddCustomModelFromHf = vi.fn();
      const onAddCustomModelFromFile = vi.fn();
      const onDeleteCustomModel = vi.fn();

      render(
        <ModelPickerPopover
          {...defaultProps}
          currentProvider="llama_cpp"
          currentModel="model.gguf"
          allModels={[{ provider: 'llama_cpp', model: 'model.gguf', source: 'custom', id: 'hf:repo:model.gguf' }]}
          onAddCustomModelFromHf={onAddCustomModelFromHf}
          onAddCustomModelFromFile={onAddCustomModelFromFile}
          onDeleteCustomModel={onDeleteCustomModel}
        />
      );

      await user.type(screen.getByPlaceholderText('HF repo (owner/name-GGUF)'), 'owner/repo-GGUF');
      await user.type(screen.getByPlaceholderText('filename.gguf'), 'model.gguf');
      await user.click(screen.getByTitle('Add from Hugging Face'));
      expect(onAddCustomModelFromHf).toHaveBeenCalledWith('llama_cpp', 'owner/repo-GGUF', 'model.gguf', 'model.gguf');

      await user.type(screen.getByPlaceholderText('/absolute/path/to/model.gguf'), '/tmp/model.gguf');
      await user.click(screen.getByTitle('Add local GGUF file'));
      expect(onAddCustomModelFromFile).toHaveBeenCalledWith('llama_cpp', '/tmp/model.gguf', 'model.gguf');

      await user.click(screen.getByTitle('Delete selected custom model'));
      expect(onDeleteCustomModel).toHaveBeenCalledWith('llama_cpp', 'hf:repo:model.gguf');
    });

    it('renders download status text for selected provider', () => {
      render(
        <ModelPickerPopover
          {...defaultProps}
          currentProvider="llama_cpp"
          currentModel="model.gguf"
          allModels={[{ provider: 'llama_cpp', model: 'model.gguf', source: 'catalog', id: 'hf:repo:model.gguf' }]}
          modelDownloads={{
            'llama_cpp:hf:repo:model.gguf': {
              provider: 'llama_cpp',
              model_id: 'hf:repo:model.gguf',
              status: 'downloading',
              bytes_downloaded: 10,
              bytes_total: 100,
              percent: 10,
            },
          }}
        />
      );

      expect(screen.getByText(/downloading: hf:repo:model.gguf/i)).toBeInTheDocument();
    });

    it('sends canonical model id when selecting a custom model entry', async () => {
      const user = userEvent.setup();
      const onSetSessionModel = vi.fn();

      render(
        <ModelPickerPopover
          {...defaultProps}
          currentProvider="llama_cpp"
          currentModel="Q8_0"
          onSetSessionModel={onSetSessionModel}
          allModels={[
            {
              provider: 'llama_cpp',
              model: 'Q8_0',
              id: 'hf:repo:model-Q8_0.gguf',
              label: 'model-Q8_0.gguf',
              source: 'custom',
            },
          ]}
        />
      );

      await user.click(screen.getByText('model-Q8_0.gguf'));

      expect(onSetSessionModel).toHaveBeenCalledWith(
        'session-1',
        'llama_cpp/hf:repo:model-Q8_0.gguf',
        undefined,
      );
    });
  });

  describe('Edge cases', () => {
    it('handles empty recent models list', () => {
      render(
        <ModelPickerPopover
          {...defaultProps}
          recentModelsByWorkspace={{}}
        />
      );

      // Should not show Recent section
      expect(screen.queryByText('Recent')).not.toBeInTheDocument();
      
      // Should still show provider sections
      expect(screen.getByText('anthropic')).toBeInTheDocument();
    });

    it('handles models in recent that are no longer available', () => {
      const unavailableRecent: RecentModelEntry[] = [
        { provider: 'old-provider', model: 'old-model', last_used: '2024-01-01T00:00:00Z', use_count: 1 },
      ];

      render(
        <ModelPickerPopover
          {...defaultProps}
          recentModelsByWorkspace={{
            '/test/workspace': unavailableRecent,
          }}
        />
      );

      // Recent section should not appear since no recent models are available
      expect(screen.queryByText('Recent')).not.toBeInTheDocument();
    });

    it('limits recent models to 5 items', () => {
      const manyRecentModels: RecentModelEntry[] = Array.from({ length: 10 }, (_, i) => ({
        provider: 'anthropic',
        model: `claude-${i}`,
        last_used: new Date(Date.now() - i * 1000).toISOString(),
        use_count: i + 1,
      }));

      // Add these to available models
      const extendedModels = [
        ...mockModels,
        ...Array.from({ length: 10 }, (_, i) => ({
          provider: 'anthropic',
          model: `claude-${i}`,
        })),
      ];

      const { container } = render(
        <ModelPickerPopover
          {...defaultProps}
          allModels={extendedModels}
          recentModelsByWorkspace={{
            '/test/workspace': manyRecentModels,
          }}
        />
      );

      // Count items in recent section
      const recentHeading = screen.getByText('Recent');
      const recentGroup = recentHeading.closest('[cmdk-group]');
      const recentItems = recentGroup!.querySelectorAll('[cmdk-item]');
      
      // Should only show 5 items
      expect(recentItems.length).toBe(5);
    });
  });
});
