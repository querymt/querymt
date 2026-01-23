/**
 * Tool summary utilities for generating compact tool representations
 */

export interface ToolSummaryInfo {
  icon: string;
  name: string;
  summary: string;
  keyParam?: string;
  diffStats?: {
    additions: number;
    deletions: number;
    filePath?: string;
  };
}

// Tool icon mapping
const TOOL_ICONS: Record<string, string> = {
  // File operations
  read: '📖',
  read_file: '📖',
  mcp_read: '📖',
  write: '📝',
  write_file: '📝',
  mcp_write: '📝',
  edit: '✏️',
  mcp_edit: '✏️',
  apply_patch: '🔧',
  
  // Search/explore
  glob: '🔍',
  mcp_glob: '🔍',
  grep: '🔎',
  mcp_grep: '🔎',
  search: '🔎',
  
  // Shell/system
  shell: '💻',
  bash: '💻',
  mcp_bash: '💻',
  terminal: '💻',
  
  // Tasks/delegation
  delegate: '🚀',
  mcp_task: '🚀',
  task: '🚀',
  
  // Web
  webfetch: '🌐',
  mcp_webfetch: '🌐',
  fetch: '🌐',
  
  // Questions/interaction
  question: '❓',
  mcp_question: '❓',
  
  // Todo
  todowrite: '📋',
  todoread: '📋',
  mcp_todowrite: '📋',
  mcp_todoread: '📋',
  
  // Skills
  skill: '🎯',
  mcp_skill: '🎯',
  
  // Default
  default: '⚡',
};

/**
 * Get icon for a tool
 */
export function getToolIcon(toolKind: string | undefined): string {
  if (!toolKind) return TOOL_ICONS.default;
  const normalized = toolKind.toLowerCase();
  return TOOL_ICONS[normalized] || TOOL_ICONS.default;
}

/**
 * Get display name for a tool (without mcp_ prefix)
 */
export function getToolDisplayName(toolKind: string | undefined): string {
  if (!toolKind) return 'Tool';
  // Remove mcp_ prefix if present
  const name = toolKind.replace(/^mcp_/, '');
  // Capitalize first letter
  return name.charAt(0).toUpperCase() + name.slice(1);
}

/**
 * Extract key parameter from tool input for summary display
 */
export function extractKeyParam(toolKind: string | undefined, rawInput: unknown): string | undefined {
  if (!rawInput) return undefined;
  
  const input = typeof rawInput === 'string' ? parseJsonSafe(rawInput) : rawInput;
  if (!input || typeof input !== 'object') return undefined;
  
  const obj = input as Record<string, unknown>;
  const normalized = toolKind?.toLowerCase().replace(/^mcp_/, '') || '';
  
  switch (normalized) {
    case 'read':
    case 'read_file':
      return extractFilePath(obj) || truncate(String(obj.path || obj.filePath || ''), 50);
      
    case 'write':
    case 'write_file':
      return extractFilePath(obj) || truncate(String(obj.path || obj.filePath || ''), 50);
      
    case 'edit':
      return extractFilePath(obj);
      
    case 'glob':
      return truncate(String(obj.pattern || ''), 40);
      
    case 'grep':
      return truncate(String(obj.pattern || ''), 40);
      
    case 'bash':
    case 'shell':
      return truncate(extractCommand(obj), 40);
      
    case 'task':
    case 'delegate':
      return extractDelegateInfo(obj);
      
    case 'webfetch':
    case 'fetch':
      return truncate(String(obj.url || ''), 50);
      
    case 'apply_patch':
      return extractPatchFilePath(obj);
      
    default:
      // Try to find any file-like parameter
      const filePath = extractFilePath(obj);
      if (filePath) return filePath;
      
      // Otherwise return first string parameter that looks meaningful
      for (const [key, value] of Object.entries(obj)) {
        if (typeof value === 'string' && value.length > 0 && value.length < 100) {
          if (['path', 'file', 'pattern', 'command', 'url', 'query', 'name', 'description'].includes(key.toLowerCase())) {
            return truncate(value, 50);
          }
        }
      }
      return undefined;
  }
}

/**
 * Calculate diff stats from edit/patch input
 */
export function calculateDiffStats(toolKind: string | undefined, rawInput: unknown): ToolSummaryInfo['diffStats'] | undefined {
  if (!rawInput) return undefined;
  
  const input = typeof rawInput === 'string' ? parseJsonSafe(rawInput) : rawInput;
  if (!input || typeof input !== 'object') return undefined;
  
  const obj = input as Record<string, unknown>;
  const normalized = toolKind?.toLowerCase().replace(/^mcp_/, '') || '';
  
  if (normalized === 'edit') {
    const oldString = String(obj.oldString || obj.old_string || '');
    const newString = String(obj.newString || obj.new_string || '');
    const filePath = extractFilePath(obj);
    
    if (oldString || newString) {
      const oldLines = oldString.split('\n').length;
      const newLines = newString.split('\n').length;
      return {
        additions: Math.max(0, newLines - oldLines + countNewLines(oldString, newString)),
        deletions: Math.max(0, oldLines - newLines + countDeletedLines(oldString, newString)),
        filePath,
      };
    }
  }
  
  if (normalized === 'apply_patch') {
    const patch = String(obj.patch || '');
    const filePath = extractPatchFilePath(obj);
    
    if (patch) {
      const additions = (patch.match(/^\+[^+]/gm) || []).length;
      const deletions = (patch.match(/^-[^-]/gm) || []).length;
      return { additions, deletions, filePath };
    }
  }
  
  return undefined;
}

/**
 * Generate a complete tool summary
 */
export function generateToolSummary(
  toolKind: string | undefined,
  toolName: string | undefined,
  rawInput: unknown
): ToolSummaryInfo {
  const icon = getToolIcon(toolKind || toolName);
  const name = getToolDisplayName(toolKind || toolName);
  const keyParam = extractKeyParam(toolKind || toolName, rawInput);
  const diffStats = calculateDiffStats(toolKind || toolName, rawInput);
  
  let summary = name;
  if (keyParam) {
    summary = `${name}: ${keyParam}`;
  }
  
  // Add diff stats to summary for edit tools
  if (diffStats && (diffStats.additions > 0 || diffStats.deletions > 0)) {
    const statsStr = `(+${diffStats.additions} -${diffStats.deletions})`;
    summary = keyParam ? `${name}: ${keyParam} ${statsStr}` : `${name} ${statsStr}`;
  }
  
  return {
    icon,
    name,
    summary,
    keyParam,
    diffStats,
  };
}

// Helper functions

function parseJsonSafe(value: string): unknown {
  try {
    return JSON.parse(value);
  } catch {
    return undefined;
  }
}

function truncate(str: string, maxLen: number): string {
  if (str.length <= maxLen) return str;
  return str.slice(0, maxLen - 3) + '...';
}

function extractFilePath(obj: Record<string, unknown>): string | undefined {
  const path = obj.filePath || obj.file_path || obj.path || obj.file;
  if (typeof path === 'string') {
    // Get just the filename if path is long
    if (path.length > 60) {
      const parts = path.split('/');
      const filename = parts[parts.length - 1];
      if (parts.length > 2) {
        return `.../${parts[parts.length - 2]}/${filename}`;
      }
      return filename;
    }
    return path;
  }
  return undefined;
}

function extractCommand(obj: Record<string, unknown>): string {
  const cmd = obj.command || obj.cmd || '';
  if (typeof cmd === 'string') {
    // Get first line of command
    const firstLine = cmd.split('\n')[0];
    return firstLine;
  }
  return '';
}

function extractDelegateInfo(obj: Record<string, unknown>): string | undefined {
  const agentType = obj.subagent_type || obj.agent_type || obj.agent;
  const description = obj.description;
  
  if (typeof agentType === 'string') {
    if (typeof description === 'string' && description.length < 30) {
      return `${agentType}: ${description}`;
    }
    return agentType;
  }
  if (typeof description === 'string') {
    return truncate(description, 40);
  }
  return undefined;
}

function extractPatchFilePath(obj: Record<string, unknown>): string | undefined {
  // Try direct file path first
  const direct = extractFilePath(obj);
  if (direct) return direct;
  
  // Try to extract from patch content
  const patch = String(obj.patch || '');
  const match = patch.match(/^(?:---|\+\+\+)\s+[ab]\/(.+)$/m);
  if (match?.[1]) {
    return truncate(match[1], 50);
  }
  
  return undefined;
}

function countNewLines(oldStr: string, newStr: string): number {
  // Simple heuristic - count lines that appear in new but not old
  const oldLines = new Set(oldStr.split('\n'));
  return newStr.split('\n').filter(line => !oldLines.has(line)).length;
}

function countDeletedLines(oldStr: string, newStr: string): number {
  // Simple heuristic - count lines that appear in old but not new
  const newLines = new Set(newStr.split('\n'));
  return oldStr.split('\n').filter(line => !newLines.has(line)).length;
}
