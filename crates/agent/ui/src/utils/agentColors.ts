const AGENT_COLORS = [
  'rgb(var(--agent-accent-1-rgb))', // primary contrast accent
  'rgb(var(--agent-accent-2-rgb))', // secondary accent
  'rgb(var(--agent-accent-3-rgb))', // tertiary accent
  'rgb(var(--cyber-cyan-rgb))',
  'rgb(var(--cyber-magenta-rgb))',
  'rgb(var(--cyber-lime-rgb))',
  'rgb(var(--cyber-orange-rgb))',
  'rgb(var(--cyber-purple-rgb))',
];

const colorCache = new Map<string, string>();

export function colorWithAlpha(color: string, alpha: number): string {
  const clampedAlpha = Math.max(0, Math.min(1, alpha));

  if (color.startsWith('#') && color.length === 7) {
    const r = parseInt(color.slice(1, 3), 16);
    const g = parseInt(color.slice(3, 5), 16);
    const b = parseInt(color.slice(5, 7), 16);
    return `rgba(${r}, ${g}, ${b}, ${clampedAlpha})`;
  }

  const rgbMatch = color.match(/^rgb\((.+)\)$/i);
  if (rgbMatch?.[1]) {
    return `rgba(${rgbMatch[1]}, ${clampedAlpha})`;
  }

  return color;
}

export function getAgentColor(agentId: string): string {
  if (colorCache.has(agentId)) {
    return colorCache.get(agentId)!;
  }
  
  if (agentId === 'primary') {
    colorCache.set(agentId, AGENT_COLORS[0]);
    return AGENT_COLORS[0];
  }
  
  let hash = 0;
  for (let i = 0; i < agentId.length; i++) {
    hash = ((hash << 5) - hash) + agentId.charCodeAt(i);
    hash = hash & hash;
  }
  const color = AGENT_COLORS[Math.abs(hash) % AGENT_COLORS.length];
  colorCache.set(agentId, color);
  return color;
}

export function getAgentColorWithAlpha(agentId: string, alpha: number): string {
  return colorWithAlpha(getAgentColor(agentId), alpha);
}
