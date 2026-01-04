const AGENT_COLORS = [
  '#00fff9', // cyan (primary)
  '#ff00ff', // magenta
  '#39ff14', // lime
  '#b026ff', // purple
  '#ff6b35', // orange
  '#00d4ff', // sky blue
  '#ff1493', // deep pink
  '#7fff00', // chartreuse
];

const colorCache = new Map<string, string>();

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
  const hex = getAgentColor(agentId);
  const r = parseInt(hex.slice(1, 3), 16);
  const g = parseInt(hex.slice(3, 5), 16);
  const b = parseInt(hex.slice(5, 7), 16);
  return `rgba(${r}, ${g}, ${b}, ${alpha})`;
}
