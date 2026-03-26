import { useEffect, useRef } from 'react';
import { useUiStore } from '../store/uiStore';

/**
 * Auto-switches the session model when agent mode changes,
 * if a stored preference exists for the new mode.
 * Extracted from AppShell to reduce its size.
 */
export function useAutoModelSwitch({
  agentMode,
  sessionId,
  activeAgentId,
  agentModels,
  setSessionModel,
}: {
  agentMode: string;
  sessionId: string | null;
  activeAgentId: string;
  agentModels: Record<string, { provider?: string; model?: string } | undefined>;
  setSessionModel: (sessionId: string, modelId: string) => void;
}) {
  const prevAgentModeRef = useRef(agentMode);

  useEffect(() => {
    // Only auto-switch when agentMode actually changes, not when agentModels updates.
    // This prevents infinite loop when user manually switches model via ModelPickerPopover.
    if (prevAgentModeRef.current === agentMode) {
      return;
    }
    prevAgentModeRef.current = agentMode;

    const { modeModelPreferences } = useUiStore.getState();
    const preference = modeModelPreferences[agentMode];

    // Only auto-switch if:
    // 1. We have a stored preference for this mode
    // 2. We have an active session
    // 3. The current model is different from the preference
    if (
      preference &&
      sessionId &&
      (agentModels[activeAgentId]?.provider !== preference.provider ||
       agentModels[activeAgentId]?.model !== preference.model)
    ) {
      const modelId = `${preference.provider}/${preference.model}`;
      console.log(`[AppShell] Auto-switching to ${modelId} for mode "${agentMode}"`);
      setSessionModel(sessionId, modelId);
    }
  }, [agentMode, sessionId, activeAgentId, agentModels, setSessionModel]);
}
