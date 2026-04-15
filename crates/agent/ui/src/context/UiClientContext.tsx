import { createContext, useContext, useMemo, ReactNode, MutableRefObject } from 'react';
import { useUiClient } from '../hooks/useUiClient';
import type {
  EventItem,
  RoutingMode,
  UiAgentInfo,
  SessionGroup,
  ModelEntry,
  RecentModelEntry,
  LlmConfigDetails,
  SessionLimits,
  AuthProviderEntry,
  AuthMethod,
  ModelDownloadStatus,
  OAuthFlowState,
  OAuthResultState,
  ProviderCapabilityEntry,
  RemoteNodeInfo,
  PluginUpdateStatus,
  PluginUpdateResult,
  AuditView,
  UiPromptBlock,
  FileIndexEntry,
  ScheduleInfo,
  KnowledgeEntryInfo,
  ConsolidationInfo,
  MeshInviteInfo,
  MeshInviteCreated,
} from '../types';

// ---------------------------------------------------------------------------
// 1. Actions Context — stable callbacks that never change
// ---------------------------------------------------------------------------

export interface UiClientActionsContextValue {
  newSession: (cwd?: string, node?: string) => Promise<string>;
  sendPrompt: (blocks: UiPromptBlock[], agentId?: string, agentMode?: string) => void;
  cancelSession: () => void;
  deleteSession: (sessionId: string, sessionLabel?: string) => void;
  loadSession: (sessionId: string, sessionLabel?: string) => void;
  attachRemoteSession: (nodeId: string, sessionId: string, sessionLabel?: string) => void;
  refreshAllModels: () => void;
  fetchRecentModels: () => void;
  requestAuthProviders: () => void;
  startOAuthLogin: (provider: string) => void;
  completeOAuthLogin: (flowId: string, response: string) => void;
  disconnectOAuth: (provider: string) => void;
  clearOAuthState: () => void;
  setApiToken: (provider: string, apiKey: string) => void;
  clearApiToken: (provider: string) => void;
  setAuthMethodPref: (provider: string, method: AuthMethod) => void;
  clearApiTokenResult: () => void;
  setSessionModel: (sessionId: string, modelId: string, node?: string) => void;
  addCustomModelFromHf: (provider: string, repo: string, filename: string, displayName?: string) => void;
  addCustomModelFromFile: (provider: string, filePath: string, displayName?: string) => void;
  deleteCustomModel: (provider: string, modelId: string) => void;
  setActiveAgent: (agentId: string) => void;
  setRoutingMode: (mode: RoutingMode) => void;
  subscribeSession: (sessionId: string, agentId?: string) => void;
  unsubscribeSession: (sessionId: string) => void;
  sendUndo: (messageId: string, turnId: string) => void;
  sendRedo: () => void;
  forkSessionAtMessage: (messageId: string) => Promise<string>;
  setFileIndexCallback: (callback: ((files: FileIndexEntry[], generatedAt: number) => void) | null) => void;
  setFileIndexErrorCallback: (callback: ((message: string) => void) | null) => void;
  requestFileIndex: () => void;
  requestLlmConfig: (configId: number, callback: (config: LlmConfigDetails) => void) => void;
  sendElicitationResponse: (elicitationId: string, action: 'accept' | 'decline' | 'cancel', content?: Record<string, unknown>) => void;
  setAgentMode: (mode: string) => void;
  cycleAgentMode: () => void;
  setReasoningEffort: (effort: string | null) => void;
  cycleReasoningEffort: () => void;
  submitWorkspacePathDialog: (cwd: string, node: string | null) => void;
  cancelWorkspacePathDialog: () => void;
  listRemoteNodes: () => void;
  createMeshInvite: (opts?: { meshName?: string; ttl?: string; maxUses?: number }) => void;
  listMeshInvites: () => void;
  revokeMeshInvite: (inviteId: string) => void;
  dismissConnectionError: (errorId: number) => void;
  dismissSessionActionNotice: (noticeId: number) => void;
  updatePlugins: () => void;
  listSchedules: (sessionId?: string) => void;
  createSchedule: (sessionId: string, prompt: string, trigger: any, opts?: { maxSteps?: number; maxCostUsd?: number; maxRuns?: number }) => void;
  pauseSchedule: (schedulePublicId: string) => void;
  resumeSchedule: (schedulePublicId: string) => void;
  triggerScheduleNow: (schedulePublicId: string) => void;
  deleteSchedule: (schedulePublicId: string) => void;
  queryKnowledge: (scope: string, question: string, limit?: number) => void;
  listKnowledge: (scope: string, filter?: Record<string, unknown>) => void;
  getKnowledgeStats: (scope: string) => void;
  sessionCreatingRef: MutableRefObject<boolean>;
  // Audio / voice
  sendTranscribe: (provider: string, model: string, audio: ArrayBuffer | Uint8Array, mimeType?: string) => void;
  sendSpeech: (provider: string, model: string, text: string, voice?: string, format?: string) => void;
  setTranscribeCallback: (cb: ((text: string) => void) | null) => void;
  setSpeechCallback: (cb: ((audio: ArrayBuffer, mimeType: string) => void) | null) => void;
  setSpeechErrorCallback: (cb: ((error: string) => void) | null) => void;
}

const UiClientActionsContext = createContext<UiClientActionsContextValue | undefined>(undefined);

// ---------------------------------------------------------------------------
// 2. Events Context — hot streaming data (every WS delta)
// ---------------------------------------------------------------------------

export interface UiClientEventsContextValue {
  events: EventItem[];
  eventsBySession: Map<string, EventItem[]>;
  mainSessionId: string | null;
}

const UiClientEventsContext = createContext<UiClientEventsContextValue | undefined>(undefined);

// ---------------------------------------------------------------------------
// 3. Session Context — medium-frequency session state
// ---------------------------------------------------------------------------

export interface UiClientSessionContextValue {
  sessionId: string | null;
  connected: boolean;
  reconnecting: boolean;
  agents: UiAgentInfo[];
  routingMode: RoutingMode;
  activeAgentId: string;
  sessionGroups: SessionGroup[];
  sessionsByAgent: Record<string, string>;
  sessionParentMap: Map<string, string>;
  thinkingBySession: Map<string, Set<string>>;
  thinkingAgentId: string | null;
  thinkingAgentIds: Set<string>;
  isConversationComplete: boolean;
  agentModels: Record<string, { provider?: string; model?: string; contextLimit?: number; node?: string }>;
  sessionLimits: SessionLimits | null;
  workspaceIndexStatus: Record<string, { status: 'building' | 'ready' | 'error'; message?: string | null }>;
  llmConfigCache: Record<number, LlmConfigDetails>;
  undoState: { stack: Array<{ turnId: string; messageId: string; status: 'pending' | 'confirmed'; revertedFiles: string[] }>; frontierMessageId?: string } | null;
  agentMode: string;
  availableModes: string[];
  reasoningEffort: string | null;
  remoteNodes: RemoteNodeInfo[];
  meshInvites: MeshInviteInfo[];
  lastCreatedMeshInvite: MeshInviteCreated | null;
  /** Session ID of the last session that failed to load. Used to navigate away and stop retry loops. */
  lastLoadErrorSessionId: string | null;
  schedules: ScheduleInfo[];
  knowledgeEntries: KnowledgeEntryInfo[];
  knowledgeConsolidations: ConsolidationInfo[];
  knowledgeStats: {
    totalEntries: number;
    unconsolidatedEntries: number;
    totalConsolidations: number;
    latestEntryAt: string | null;
    latestConsolidationAt: string | null;
  } | null;
}

const UiClientSessionContext = createContext<UiClientSessionContextValue | undefined>(undefined);

// ---------------------------------------------------------------------------
// 4. Config Context — low-frequency config/auth/model data
// ---------------------------------------------------------------------------

export interface UiClientConfigContextValue {
  allModels: ModelEntry[];
  providerCapabilities: Record<string, ProviderCapabilityEntry>;
  recentModelsByWorkspace: Record<string, RecentModelEntry[]>;
  authProviders: AuthProviderEntry[];
  oauthFlow: OAuthFlowState | null;
  oauthResult: OAuthResultState | null;
  apiTokenResult: { provider: string; success: boolean; message: string } | null;
  modelDownloads: Record<string, ModelDownloadStatus>;
  sessionAudit: AuditView | null;
  connectionErrors: { id: number; message: string }[];
  sessionActionNotices: { id: number; kind: 'success' | 'error'; message: string }[];
  pluginUpdateStatus: Record<string, PluginUpdateStatus>;
  pluginUpdateResults: PluginUpdateResult[] | null;
  isUpdatingPlugins: boolean;
  workspacePathDialogOpen: boolean;
  workspacePathDialogDefaultValue: string;
  audioCapabilities: { stt_models: { provider: string; model: string }[]; tts_models: { provider: string; model: string }[] };
}

const UiClientConfigContext = createContext<UiClientConfigContextValue | undefined>(undefined);

// ---------------------------------------------------------------------------
// Provider
// ---------------------------------------------------------------------------

interface UiClientProviderProps {
  children: ReactNode;
}

/**
 * Provider component that wraps useUiClient and splits its return value
 * into four independent contexts so consumers only re-render when the
 * slice they subscribe to actually changes.
 *
 * Context split:
 *   - Actions  — stable callbacks (never triggers re-render)
 *   - Events   — hot streaming data (events, eventsBySession, mainSessionId)
 *   - Session  — medium-frequency session state
 *   - Config   — low-frequency config/auth/model data
 */
export function UiClientProvider({ children }: UiClientProviderProps) {
  const uiClient = useUiClient();

  // -- Actions (stable — callbacks are all useCallback with [] deps) --
  const actions = useMemo<UiClientActionsContextValue>(() => ({
    newSession: uiClient.newSession,
    sendPrompt: uiClient.sendPrompt,
    cancelSession: uiClient.cancelSession,
    deleteSession: uiClient.deleteSession,
    loadSession: uiClient.loadSession,
    attachRemoteSession: uiClient.attachRemoteSession,
    refreshAllModels: uiClient.refreshAllModels,
    fetchRecentModels: uiClient.fetchRecentModels,
    requestAuthProviders: uiClient.requestAuthProviders,
    startOAuthLogin: uiClient.startOAuthLogin,
    completeOAuthLogin: uiClient.completeOAuthLogin,
    disconnectOAuth: uiClient.disconnectOAuth,
    clearOAuthState: uiClient.clearOAuthState,
    setApiToken: uiClient.setApiToken,
    clearApiToken: uiClient.clearApiToken,
    setAuthMethodPref: uiClient.setAuthMethodPref,
    clearApiTokenResult: uiClient.clearApiTokenResult,
    setSessionModel: uiClient.setSessionModel,
    addCustomModelFromHf: uiClient.addCustomModelFromHf,
    addCustomModelFromFile: uiClient.addCustomModelFromFile,
    deleteCustomModel: uiClient.deleteCustomModel,
    setActiveAgent: uiClient.setActiveAgent,
    setRoutingMode: uiClient.setRoutingMode,
    subscribeSession: uiClient.subscribeSession,
    unsubscribeSession: uiClient.unsubscribeSession,
    sendUndo: uiClient.sendUndo,
    sendRedo: uiClient.sendRedo,
    forkSessionAtMessage: uiClient.forkSessionAtMessage,
    setFileIndexCallback: uiClient.setFileIndexCallback,
    setFileIndexErrorCallback: uiClient.setFileIndexErrorCallback,
    requestFileIndex: uiClient.requestFileIndex,
    requestLlmConfig: uiClient.requestLlmConfig,
    sendElicitationResponse: uiClient.sendElicitationResponse,
    setAgentMode: uiClient.setAgentMode,
    cycleAgentMode: uiClient.cycleAgentMode,
    setReasoningEffort: uiClient.setReasoningEffort,
    cycleReasoningEffort: uiClient.cycleReasoningEffort,
    submitWorkspacePathDialog: uiClient.submitWorkspacePathDialog,
    cancelWorkspacePathDialog: uiClient.cancelWorkspacePathDialog,
    listRemoteNodes: uiClient.listRemoteNodes,
    createMeshInvite: uiClient.createMeshInvite,
    listMeshInvites: uiClient.listMeshInvites,
    revokeMeshInvite: uiClient.revokeMeshInvite,
    dismissConnectionError: uiClient.dismissConnectionError,
    dismissSessionActionNotice: uiClient.dismissSessionActionNotice,
    updatePlugins: uiClient.updatePlugins,
    listSchedules: uiClient.listSchedules,
    createSchedule: uiClient.createSchedule,
    pauseSchedule: uiClient.pauseSchedule,
    resumeSchedule: uiClient.resumeSchedule,
    triggerScheduleNow: uiClient.triggerScheduleNow,
    deleteSchedule: uiClient.deleteSchedule,
    queryKnowledge: uiClient.queryKnowledge,
    listKnowledge: uiClient.listKnowledge,
    getKnowledgeStats: uiClient.getKnowledgeStats,
    sessionCreatingRef: uiClient.sessionCreatingRef,
    sendTranscribe: uiClient.sendTranscribe,
    sendSpeech: uiClient.sendSpeech,
    setTranscribeCallback: uiClient.setTranscribeCallback,
    setSpeechCallback: uiClient.setSpeechCallback,
    setSpeechErrorCallback: uiClient.setSpeechErrorCallback,
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }), []);
  // NOTE: empty deps is intentional — all callbacks are stable useCallback([])

  // -- Events (hot path) --
  const eventsCtx = useMemo<UiClientEventsContextValue>(() => ({
    events: uiClient.events,
    eventsBySession: uiClient.eventsBySession,
    mainSessionId: uiClient.mainSessionId,
  }), [uiClient.events, uiClient.eventsBySession, uiClient.mainSessionId]);

  // -- Session (medium frequency) --
  const sessionCtx = useMemo<UiClientSessionContextValue>(() => ({
    sessionId: uiClient.sessionId,
    connected: uiClient.connected,
    reconnecting: uiClient.reconnecting,
    agents: uiClient.agents,
    routingMode: uiClient.routingMode,
    activeAgentId: uiClient.activeAgentId,
    sessionGroups: uiClient.sessionGroups,
    sessionsByAgent: uiClient.sessionsByAgent,
    sessionParentMap: uiClient.sessionParentMap,
    thinkingBySession: uiClient.thinkingBySession,
    thinkingAgentId: uiClient.thinkingAgentId,
    thinkingAgentIds: uiClient.thinkingAgentIds,
    isConversationComplete: uiClient.isConversationComplete,
    agentModels: uiClient.agentModels,
    sessionLimits: uiClient.sessionLimits,
    workspaceIndexStatus: uiClient.workspaceIndexStatus,
    llmConfigCache: uiClient.llmConfigCache,
    undoState: uiClient.undoState,
    agentMode: uiClient.agentMode,
    availableModes: uiClient.availableModes,
    reasoningEffort: uiClient.reasoningEffort,
    remoteNodes: uiClient.remoteNodes,
    meshInvites: uiClient.meshInvites,
    lastCreatedMeshInvite: uiClient.lastCreatedMeshInvite,
    lastLoadErrorSessionId: uiClient.lastLoadErrorSessionId,
    schedules: uiClient.schedules,
    knowledgeEntries: uiClient.knowledgeEntries,
    knowledgeConsolidations: uiClient.knowledgeConsolidations,
    knowledgeStats: uiClient.knowledgeStats,
  }), [
    uiClient.sessionId,
    uiClient.connected,
    uiClient.reconnecting,
    uiClient.agents,
    uiClient.routingMode,
    uiClient.activeAgentId,
    uiClient.sessionGroups,
    uiClient.sessionsByAgent,
    uiClient.sessionParentMap,
    uiClient.thinkingBySession,
    uiClient.thinkingAgentId,
    uiClient.thinkingAgentIds,
    uiClient.isConversationComplete,
    uiClient.agentModels,
    uiClient.sessionLimits,
    uiClient.workspaceIndexStatus,
    uiClient.llmConfigCache,
    uiClient.undoState,
    uiClient.agentMode,
    uiClient.availableModes,
    uiClient.reasoningEffort,
    uiClient.remoteNodes,
    uiClient.meshInvites,
    uiClient.lastCreatedMeshInvite,
    uiClient.lastLoadErrorSessionId,
    uiClient.schedules,
    uiClient.knowledgeEntries,
    uiClient.knowledgeConsolidations,
    uiClient.knowledgeStats,
  ]);

  // -- Config (low frequency) --
  const configCtx = useMemo<UiClientConfigContextValue>(() => ({
    allModels: uiClient.allModels,
    providerCapabilities: uiClient.providerCapabilities,
    recentModelsByWorkspace: uiClient.recentModelsByWorkspace,
    authProviders: uiClient.authProviders,
    oauthFlow: uiClient.oauthFlow,
    oauthResult: uiClient.oauthResult,
    apiTokenResult: uiClient.apiTokenResult,
    modelDownloads: uiClient.modelDownloads,
    sessionAudit: uiClient.sessionAudit,
    connectionErrors: uiClient.connectionErrors,
    sessionActionNotices: uiClient.sessionActionNotices,
    pluginUpdateStatus: uiClient.pluginUpdateStatus,
    pluginUpdateResults: uiClient.pluginUpdateResults,
    isUpdatingPlugins: uiClient.isUpdatingPlugins,
    workspacePathDialogOpen: uiClient.workspacePathDialogOpen,
    workspacePathDialogDefaultValue: uiClient.workspacePathDialogDefaultValue,
    audioCapabilities: uiClient.audioCapabilities,
  }), [
    uiClient.allModels,
    uiClient.providerCapabilities,
    uiClient.recentModelsByWorkspace,
    uiClient.authProviders,
    uiClient.oauthFlow,
    uiClient.oauthResult,
    uiClient.apiTokenResult,
    uiClient.modelDownloads,
    uiClient.sessionAudit,
    uiClient.connectionErrors,
    uiClient.sessionActionNotices,
    uiClient.pluginUpdateStatus,
    uiClient.pluginUpdateResults,
    uiClient.isUpdatingPlugins,
    uiClient.workspacePathDialogOpen,
    uiClient.workspacePathDialogDefaultValue,
    uiClient.audioCapabilities,
  ]);

  return (
    <UiClientActionsContext.Provider value={actions}>
      <UiClientEventsContext.Provider value={eventsCtx}>
        <UiClientSessionContext.Provider value={sessionCtx}>
          <UiClientConfigContext.Provider value={configCtx}>
            {children}
          </UiClientConfigContext.Provider>
        </UiClientSessionContext.Provider>
      </UiClientEventsContext.Provider>
    </UiClientActionsContext.Provider>
  );
}

// ---------------------------------------------------------------------------
// Granular hooks — prefer these in all new code
// ---------------------------------------------------------------------------

/** Stable callbacks — never triggers re-render. */
export function useUiClientActions(): UiClientActionsContextValue {
  const context = useContext(UiClientActionsContext);
  if (context === undefined) {
    throw new Error('useUiClientActions must be used within a UiClientProvider');
  }
  return context;
}

/** Hot streaming data — events, eventsBySession, mainSessionId. */
export function useUiClientEvents(): UiClientEventsContextValue {
  const context = useContext(UiClientEventsContext);
  if (context === undefined) {
    throw new Error('useUiClientEvents must be used within a UiClientProvider');
  }
  return context;
}

/** Medium-frequency session state. */
export function useUiClientSession(): UiClientSessionContextValue {
  const context = useContext(UiClientSessionContext);
  if (context === undefined) {
    throw new Error('useUiClientSession must be used within a UiClientProvider');
  }
  return context;
}

/** Low-frequency config/auth/model data. */
export function useUiClientConfig(): UiClientConfigContextValue {
  const context = useContext(UiClientConfigContext);
  if (context === undefined) {
    throw new Error('useUiClientConfig must be used within a UiClientProvider');
  }
  return context;
}

// ---------------------------------------------------------------------------
// Legacy combined hook — backward compatibility
// ---------------------------------------------------------------------------

/**
 * @deprecated Prefer the granular hooks (useUiClientActions, useUiClientEvents,
 * useUiClientSession, useUiClientConfig) so that components only re-render
 * when the slice they need actually changes.
 *
 * This combined hook subscribes to ALL four contexts, so any change in any
 * context will re-render the consumer.
 */
export function useUiClientContext(): UiClientActionsContextValue & UiClientEventsContextValue & UiClientSessionContextValue & UiClientConfigContextValue {
  const actions = useUiClientActions();
  const events = useUiClientEvents();
  const session = useUiClientSession();
  const config = useUiClientConfig();
  return { ...actions, ...events, ...session, ...config };
}
