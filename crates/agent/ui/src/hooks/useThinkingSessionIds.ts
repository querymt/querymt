import { useMemo } from 'react';
import { SessionGroup } from '../types';

/**
 * Builds an expanded set of session IDs that have thinking activity.
 * Includes parent sessions of thinking child sessions by walking up
 * the parent chain, so a parent session shows the "thinking" badge
 * when any of its delegated child sessions are active.
 */
export function useThinkingSessionIds(
  thinkingBySession: Map<string, Set<string>> | undefined,
  groups: SessionGroup[],
  sessionParentMap?: Map<string, string>,
): Set<string> {
  return useMemo(() => {
    if (!thinkingBySession) return new Set<string>();

    const result = new Set<string>();

    // Build a lookup of session_id -> parent_session_id from all groups
    const parentLookup = new Map<string, string>();
    for (const group of groups) {
      for (const session of group.sessions) {
        if (session.parent_session_id) {
          parentLookup.set(session.session_id, session.parent_session_id);
        }
      }
    }
    
    // Merge in real-time parent mappings (from session_forked events)
    // This ensures we track parent-child relationships even before the session list refreshes
    if (sessionParentMap) {
      for (const [childId, parentId] of sessionParentMap.entries()) {
        if (!parentLookup.has(childId)) {
          parentLookup.set(childId, parentId);
        }
      }
    }

    // For each session that has thinking agents, mark it AND walk up its parent chain
    for (const [sessionId, agentIds] of thinkingBySession.entries()) {
      if (agentIds.size > 0) {
        result.add(sessionId);
        let current = sessionId;
        while (parentLookup.has(current)) {
          current = parentLookup.get(current)!;
          result.add(current);
        }
      }
    }

    return result;
  }, [thinkingBySession, groups, sessionParentMap]);
}
