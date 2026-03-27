/**
 * MobileDropdownMenu - Dropdown menu shown on mobile below the header.
 * Contains model picker, shortcuts, and remote node indicator.
 */

import { Keyboard } from 'lucide-react';
import { ModelPickerPopover } from './ModelPickerPopover';
import { RemoteNodeIndicator } from './RemoteNodeIndicator';
import type { ReactNode } from 'react';
import type { UiAgentInfo, RoutingMode } from '../types';

interface MobileDropdownMenuProps {
  // Model picker
  modelPickerOpen: boolean;
  handleMobilePickerOpenChange: (open: boolean) => void;
  connected: boolean;
  routingMode: RoutingMode;
  activeAgentId: string;
  sessionId: string | null;
  sessionsByAgent: Record<string, string>;
  agents: UiAgentInfo[];
  allModels: any;
  activeAgentModel: { provider?: string; model?: string; node?: string } | undefined;
  remoteNodes: any;
  currentWorkspace: string | null;
  recentModelsByWorkspace: any;
  agentMode: string;
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

  // Actions
  setShortcutGatewayOpen: (open: boolean) => void;
  setMobileMenuOpen: (open: boolean) => void;

  // Render slots
  mobileExtras?: ReactNode;
}

export function MobileDropdownMenu(props: MobileDropdownMenuProps) {
  return (
    <div className="md:hidden bg-surface-elevated border-b border-surface-border px-3 py-2.5 flex flex-col gap-2 z-30">
      {/* Model picker on mobile */}
      <ModelPickerPopover
        open={props.modelPickerOpen}
        onOpenChange={props.handleMobilePickerOpenChange}
        isInMobileMenu
        connected={props.connected}
        routingMode={props.routingMode}
        activeAgentId={props.activeAgentId}
        sessionId={props.sessionId}
        sessionsByAgent={props.sessionsByAgent}
        agents={props.agents}
        allModels={props.allModels}
        currentProvider={props.activeAgentModel?.provider}
        currentModel={props.activeAgentModel?.model}
        currentNode={props.activeAgentModel?.node}
        remoteNodes={props.remoteNodes}
        currentWorkspace={props.currentWorkspace}
        recentModelsByWorkspace={props.recentModelsByWorkspace}
        agentMode={props.agentMode}
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
      <div className="flex items-center gap-2">
        <button
          type="button"
          onClick={() => { props.setShortcutGatewayOpen(true); props.setMobileMenuOpen(false); }}
          className="flex-1 h-8 inline-flex items-center justify-center gap-2 rounded-lg border border-surface-border bg-surface-canvas/60 transition-colors hover:border-accent-primary/40 text-xs"
          aria-label="Open shortcut gateway"
        >
          <Keyboard className="w-3.5 h-3.5 text-accent-primary" />
          <span className="text-ui-secondary">Shortcuts</span>
        </button>
      </div>
      <div className="flex items-center gap-2">
        <RemoteNodeIndicator remoteNodes={props.remoteNodes} />
        {props.mobileExtras}
      </div>
    </div>
  );
}
