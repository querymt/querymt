/**
 * AppHeader - Main application header bar.
 *
 * Lean layout:
 *   Left:  Brand link ("QueryMT") | session mode chip
 *   Right: stats bar | model picker | remote nodes | mobile hamburger
 */

import { Link } from 'react-router-dom';
import { Menu, X } from 'lucide-react';
import { GlitchText } from './GlitchText';
import { ModelPickerPopover } from './ModelPickerPopover';
import { HeaderStatsBar } from './HeaderStatsBar';
import { RemoteNodeIndicator } from './RemoteNodeIndicator';
import { getModeDisplayName } from '../utils/modeColors';
import type { UiAgentInfo, SessionLimits, RoutingMode } from '../types';

interface AppHeaderProps {
  isHomePage: boolean;
  isMobile: boolean;
  sessionId: string | null;
  connected: boolean;
  isSessionActive: boolean;
  isConversationComplete: boolean;
  agentMode: string;
  cycleAgentMode: () => void;
  setSessionSwitcherOpen: (open: boolean) => void;

  // Stats bar
  agentModels: Record<string, { provider?: string; model?: string; contextLimit?: number; node?: string }>;
  sessionLimits?: SessionLimits | null;
  statsDrawerOpen: boolean;
  setStatsDrawerOpen: (open: boolean) => void;

  // Model picker (desktop)
  modelPickerOpen: boolean;
  setModelPickerOpen: (open: boolean) => void;
  routingMode: RoutingMode;
  activeAgentId: string;
  sessionsByAgent: Record<string, string>;
  agents: UiAgentInfo[];
  allModels: any;
  activeAgentModel: { provider?: string; model?: string; node?: string } | undefined;
  remoteNodes: any;
  currentWorkspace: string | null;
  recentModelsByWorkspace: any;
  reasoningEffort: any;
  refreshAllModels: () => void;
  setSessionModel: (sessionId: string, modelId: string) => void;
  setReasoningEffort: (effort: string | null) => void;
  cycleReasoningEffort: () => void;
  providerCapabilities: any;
  modelDownloads: any;
  addCustomModelFromHf: (provider: string, repo: string, filename: string, displayName?: string) => void;
  addCustomModelFromFile: (provider: string, filePath: string) => void;
  deleteCustomModel: (provider: string, modelId: string) => void;

  // Mobile menu
  mobileMenuOpen: boolean;
  setMobileMenuOpen: (open: boolean) => void;
}

export function AppHeader(props: AppHeaderProps) {
  const {
    isHomePage,
    isMobile,
    sessionId,
    connected,
    isSessionActive,
    isConversationComplete,
    agentMode,
    cycleAgentMode,
    setSessionSwitcherOpen,
    agentModels,
    sessionLimits,
    statsDrawerOpen,
    setStatsDrawerOpen,
    modelPickerOpen,
    setModelPickerOpen,
    activeAgentModel,
    remoteNodes,
    mobileMenuOpen,
    setMobileMenuOpen,
  } = props;

  return (
    <header className="flex items-center justify-between gap-3 px-3 md:px-5 py-2 bg-surface-elevated border-b border-surface-border">
      {/* Left section: brand + session mode */}
      <div className="flex items-center gap-3 min-w-0">
        {/* Brand link — acts as Home */}
        <Link
          to="/"
          className={`h-8 flex items-center text-lg font-semibold leading-none whitespace-nowrap transition-colors ${
            isHomePage
              ? 'text-accent-primary/60 cursor-default'
              : 'text-accent-primary hover:text-accent-primary/80'
          }`}
        >
          <span className="hidden md:inline glow-text-primary">
            <GlitchText text="QueryMT" variant="3" hoverOnly />
          </span>
          <span className="md:hidden text-base font-semibold text-accent-primary">QMT</span>
        </Link>

        {/* Session mode chip — click status to switch sessions, click mode to cycle */}
        {sessionId && (
          <div className="h-8 flex items-center gap-px rounded-full bg-surface-canvas border border-surface-border/60 overflow-hidden min-w-0">
            <button
              type="button"
              onClick={() => setSessionSwitcherOpen(true)}
              title={`Switch sessions (${navigator.platform.includes('Mac') ? 'Cmd' : 'Ctrl'}+/)`}
              className="h-full flex items-center gap-1.5 pl-2.5 pr-2 hover:bg-surface-elevated/60 transition-colors"
            >
              <span
                className={`w-1.5 h-1.5 rounded-full flex-shrink-0 ${
                  !connected
                    ? 'bg-status-warning'
                    : isSessionActive
                    ? 'bg-accent-primary animate-pulse'
                    : isConversationComplete
                    ? 'bg-ui-muted'
                    : 'bg-status-success'
                }`}
                title={
                  !connected
                    ? 'Disconnected'
                    : isSessionActive
                    ? 'Thinking'
                    : isConversationComplete
                    ? 'Complete'
                    : 'Idle'
                }
              />
            </button>
            <button
              type="button"
              onClick={cycleAgentMode}
              title={`Mode: ${agentMode} (${navigator.platform.includes('Mac') ? '\u2318E' : 'Ctrl+E'} to cycle)`}
              className="h-full px-2 pr-2.5 text-xs font-medium transition-colors hover:bg-surface-elevated/60 whitespace-nowrap"
              style={{ color: 'var(--mode-color)' }}
            >
              {getModeDisplayName(agentMode)}
            </button>
          </div>
        )}
      </div>

      {/* Right section: stats, model picker, mesh, menu */}
      <div className="flex items-center gap-2 md:gap-3 min-w-0 flex-shrink-0">
        {sessionId && (
          <HeaderStatsBar
            agentModels={agentModels}
            sessionLimits={sessionLimits}
            compact={isMobile}
            onClick={() => setStatsDrawerOpen(!statsDrawerOpen)}
          />
        )}

        {/* Desktop controls */}
        {!isMobile && (
          <div className="hidden md:flex items-center gap-2 min-w-0">
            <ModelPickerPopover
              open={modelPickerOpen}
              onOpenChange={setModelPickerOpen}
              connected={connected}
              routingMode={props.routingMode}
              activeAgentId={props.activeAgentId}
              sessionId={sessionId}
              sessionsByAgent={props.sessionsByAgent}
              agents={props.agents}
              allModels={props.allModels}
              currentProvider={activeAgentModel?.provider}
              currentModel={activeAgentModel?.model}
              currentNode={activeAgentModel?.node}
              remoteNodes={remoteNodes}
              currentWorkspace={props.currentWorkspace}
              recentModelsByWorkspace={props.recentModelsByWorkspace}
              agentMode={agentMode}
              reasoningEffort={props.reasoningEffort}
              onRefresh={props.refreshAllModels}
              onSetSessionModel={props.setSessionModel}
              onSetReasoningEffort={props.setReasoningEffort}
              onCycleReasoningEffort={props.cycleReasoningEffort}
              providerCapabilities={props.providerCapabilities}
              modelDownloads={props.modelDownloads}
              onAddCustomModelFromHf={props.addCustomModelFromHf}
              onAddCustomModelFromFile={props.addCustomModelFromFile}
              onDeleteCustomModel={props.deleteCustomModel}
            />

            <RemoteNodeIndicator remoteNodes={remoteNodes} />
          </div>
        )}

        {/* Mobile: hamburger */}
        <button
          type="button"
          onClick={() => setMobileMenuOpen(!mobileMenuOpen)}
          className="md:hidden p-1.5 rounded-lg transition-colors hover:bg-surface-canvas"
          aria-label="Toggle mobile menu"
        >
          {mobileMenuOpen ? (
            <X className="w-4 h-4 text-ui-secondary" />
          ) : (
            <Menu className="w-4 h-4 text-ui-secondary" />
          )}
        </button>
      </div>
    </header>
  );
}
