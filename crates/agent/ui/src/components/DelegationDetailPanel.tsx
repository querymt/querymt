import { CheckCircle, Clock, XCircle, Copy, Check, Cpu, Wrench, DollarSign } from 'lucide-react';
import { DelegationGroupInfo, Turn, UiAgentInfo, LlmConfigDetails } from '../types';
import { TurnCard } from './TurnCard';
import { colorWithAlpha, getAgentColor } from '../utils/agentColors';
import { getAgentShortName } from '../utils/agentNames';
import { useCopyToClipboard } from '../hooks/useCopyToClipboard';
import { calculateDelegationStats } from '../utils/statsCalculator';
import { formatTokensAbbrev, formatCost, formatDurationFromTimestamps } from '../utils/formatters';

interface DelegationDetailPanelProps {
  delegation?: DelegationGroupInfo;
  turn?: Turn | null;
  agents: UiAgentInfo[];
  onToolClick: (event: Turn['toolCalls'][number]) => void;
  llmConfigCache?: Record<number, LlmConfigDetails>;
  requestLlmConfig?: (configId: number, callback: (config: LlmConfigDetails) => void) => void;
}

export function DelegationDetailPanel({
  delegation,
  turn,
  agents,
  onToolClick,
  llmConfigCache = {},
  requestLlmConfig,
}: DelegationDetailPanelProps) {
  const { copiedValue, copy: copyToClipboard } = useCopyToClipboard();

  if (!delegation || !turn) {
    return (
      <div className="flex-1 flex items-center justify-center text-gray-500">
        Select a delegation to view details.
      </div>
    );
  }

  const agentId = delegation.targetAgentId ?? delegation.agentId;
  const agentName = agentId ? getAgentShortName(agentId, agents) : 'Sub-agent';
  const agentColor = agentId ? getAgentColor(agentId) : 'rgb(var(--cyber-purple-rgb))';
  const durationLabel = formatDurationFromTimestamps(delegation.startTime, delegation.endTime);
  const stats = calculateDelegationStats(delegation);
  const objective = delegation.objective ??
    (delegation.delegateEvent.toolCall?.raw_input as { objective?: string } | undefined)?.objective;

  return (
    <div className="flex-1 flex flex-col overflow-hidden">
      <div className="group px-6 py-4 border-b border-cyber-border/50 bg-cyber-surface/40">
        <div className="flex items-center gap-2">
          <span
            className="text-xs font-semibold uppercase tracking-wide px-2 py-0.5 rounded"
            style={{
              color: agentColor,
              backgroundColor: colorWithAlpha(agentColor, 0.12),
              border: `1px solid ${colorWithAlpha(agentColor, 0.24)}`,
            }}
          >
            {agentName}
          </span>
          <span className="flex-shrink-0">
            {delegation.status === 'completed' && (
              <CheckCircle className="w-3.5 h-3.5 text-cyber-lime" />
            )}
            {delegation.status === 'failed' && (
              <XCircle className="w-3.5 h-3.5 text-cyber-orange" />
            )}
          </span>
          <span className="text-xs text-gray-400 truncate flex-1">
            {objective ?? 'Delegated task'}
          </span>
          {objective && (
            <button
              onClick={() => copyToClipboard(objective, 'delegation-detail-objective')}
              className="opacity-0 group-hover:opacity-100 transition-opacity p-1 rounded hover:bg-cyber-bg/50"
              title="Copy objective"
            >
              {copiedValue === 'delegation-detail-objective' ? (
                <Check className="w-3.5 h-3.5 text-cyber-lime" />
              ) : (
                <Copy className="w-3.5 h-3.5 text-gray-400 hover:text-cyber-cyan" />
              )}
            </button>
          )}
          <span className="text-[10px] text-gray-500 flex items-center gap-1">
            <Clock className="w-3 h-3" />
            {durationLabel}
          </span>
        </div>
        {/* Delegation stats row */}
        <div className="flex items-center gap-3 mt-2 text-[11px]">
          {/* Context usage */}
          <span className={`flex items-center gap-1 ${
            (stats.contextPercent ?? 0) >= 80 ? 'text-cyber-orange' :
            (stats.contextPercent ?? 0) >= 70 ? 'text-cyber-cyan' :
            'text-gray-400'
          }`}>
            <Cpu className="w-3 h-3" />
            {stats.contextPercent !== undefined
              ? `${stats.contextPercent}% (${formatTokensAbbrev(stats.contextTokens)}/${formatTokensAbbrev(stats.contextLimit!)})`
              : stats.contextTokens > 0
                ? formatTokensAbbrev(stats.contextTokens)
                : 'no ctx data'}
          </span>
          <span className="text-cyber-border/60">·</span>
          <span className="flex items-center gap-1 text-gray-400">
            <Wrench className="w-3 h-3" />
            {stats.toolCallCount} tool call{stats.toolCallCount === 1 ? '' : 's'}
          </span>
          <span className="text-cyber-border/60">·</span>
          <span className="text-gray-400">{stats.messageCount} message{stats.messageCount === 1 ? '' : 's'}</span>
          {stats.costUsd > 0 && (
            <>
              <span className="text-cyber-border/60">·</span>
              <span className="flex items-center gap-1 text-cyber-cyan">
                <DollarSign className="w-3 h-3" />
                {formatCost(stats.costUsd)}
              </span>
            </>
          )}
        </div>
      </div>
      <div className="flex-1 overflow-y-auto">
        <TurnCard
          turn={turn}
          agents={agents}
          onToolClick={onToolClick}
          onDelegateClick={() => {}}
          isLastUserMessage={false}
          showModelLabel={true}
          llmConfigCache={llmConfigCache}
          requestLlmConfig={requestLlmConfig}
        />
      </div>
    </div>
  );
}
