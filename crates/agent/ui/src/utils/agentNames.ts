import { UiAgentInfo } from '../types';

/**
 * Resolves an agent ID to a display name.
 * Returns format: "Name (id)" when both name and id are different,
 * otherwise returns just the name or id.
 */
export function getAgentDisplayName(agentId: string, agents: UiAgentInfo[]): string {
  // Find the agent in the agents array
  const agent = agents.find(a => a.id === agentId);
  
  if (!agent) {
    // If agent not found, return the raw ID
    return agentId;
  }
  
  // If name and id are the same, just return one
  if (agent.name === agent.id) {
    return agent.name;
  }
  
  // If agent has a name different from ID, show both
  if (agent.name && agent.name.trim() !== '') {
    return `${agent.name} (${agent.id})`;
  }
  
  // Fallback to just the ID
  return agent.id;
}

/**
 * Gets a short display name (just the name, not the ID).
 * Useful for compact displays like badges.
 */
export function getAgentShortName(agentId: string, agents: UiAgentInfo[]): string {
  const agent = agents.find(a => a.id === agentId);
  
  if (!agent) {
    return agentId;
  }
  
  return agent.name || agent.id;
}
