import { createContext, useContext, ReactNode } from 'react';
import { useUiClient } from '../hooks/useUiClient';

// Infer the return type from useUiClient
type UiClientContextValue = ReturnType<typeof useUiClient>;

// Create the context with undefined as default (will throw if used outside provider)
const UiClientContext = createContext<UiClientContextValue | undefined>(undefined);

interface UiClientProviderProps {
  children: ReactNode;
}

/**
 * Provider component that wraps useUiClient and provides its value to the tree.
 * Should be rendered at the top level (in main.tsx).
 */
export function UiClientProvider({ children }: UiClientProviderProps) {
  const uiClient = useUiClient();
  
  return (
    <UiClientContext.Provider value={uiClient}>
      {children}
    </UiClientContext.Provider>
  );
}

/**
 * Hook to access the UiClient context.
 * Must be used within a UiClientProvider.
 * Returns all values from useUiClient() including:
 * - events, eventsBySession, mainSessionId, sessionId
 * - connected, newSession, sendPrompt, cancelSession
 * - agents, routingMode, activeAgentId, setActiveAgent, setRoutingMode
 * - sessionGroups, loadSession
 * - thinkingAgentId, thinkingAgentIds, thinkingBySession
 * - isConversationComplete
 * - file index functions: setFileIndexCallback, setFileIndexErrorCallback, requestFileIndex
 * - workspaceIndexStatus
 * - allModels, sessionsByAgent, agentModels, refreshAllModels, setSessionModel
 * - authProviders, oauthFlow, oauthResult, requestAuthProviders, startOAuthLogin, completeOAuthLogin, disconnectOAuth
 * - llmConfigCache, requestLlmConfig
 * - sessionLimits
 * - session subscription: subscribeSession, unsubscribeSession
 * - undo/redo: sendUndo, sendRedo, undoState
 * - elicitation: sendElicitationResponse
 * - agentMode, availableModes, setAgentMode, cycleAgentMode
 */
export function useUiClientContext(): UiClientContextValue {
  const context = useContext(UiClientContext);
  
  if (context === undefined) {
    throw new Error('useUiClientContext must be used within a UiClientProvider');
  }
  
  return context;
}
