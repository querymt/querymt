import { 
  FileText, 
  File, 
  Folder,
  Code,
  Braces,
  Palette,
  FileJson,
  FileCode,
  Image,
  Database,
  Settings,
  type LucideIcon
} from 'lucide-react';

/**
 * Get the appropriate icon component for a file based on extension
 */
export function getFileIcon(extension?: string, isDir?: boolean): LucideIcon {
  if (isDir) {
    return Folder;
  }

  if (!extension) {
    return File;
  }

  const iconMap: Record<string, LucideIcon> = {
    // TypeScript/JavaScript
    'ts': Code,
    'tsx': Code,
    'js': FileCode,
    'jsx': FileCode,
    'mjs': FileCode,
    'cjs': FileCode,
    
    // Rust
    'rs': Settings,
    'toml': Settings,
    
    // Config/Data
    'json': FileJson,
    'yaml': Braces,
    'yml': Braces,
    'xml': Braces,
    
    // Styles
    'css': Palette,
    'scss': Palette,
    'sass': Palette,
    'less': Palette,
    
    // Markdown/Docs
    'md': FileText,
    'mdx': FileText,
    'txt': FileText,
    
    // Images
    'png': Image,
    'jpg': Image,
    'jpeg': Image,
    'gif': Image,
    'svg': Image,
    'webp': Image,
    
    // Database
    'sql': Database,
    'db': Database,
    'sqlite': Database,
    
    // Other code
    'py': Code,
    'go': Code,
    'java': Code,
    'cpp': Code,
    'c': Code,
    'h': Code,
    'hpp': Code,
    'sh': Code,
    'bash': Code,
    'zsh': Code,
  };

  return iconMap[extension.toLowerCase()] || File;
}

/**
 * Get a color class for the file icon based on type
 */
export function getFileIconColor(extension?: string, isDir?: boolean): string {
  if (isDir) {
    return 'text-cyber-purple';
  }

  if (!extension) {
    return 'text-gray-400';
  }

  const colorMap: Record<string, string> = {
    // TypeScript - cyan
    'ts': 'text-cyber-cyan',
    'tsx': 'text-cyber-cyan',
    
    // JavaScript - lime
    'js': 'text-cyber-lime',
    'jsx': 'text-cyber-lime',
    'mjs': 'text-cyber-lime',
    'cjs': 'text-cyber-lime',
    
    // Rust - orange
    'rs': 'text-cyber-orange',
    'toml': 'text-cyber-orange',
    
    // JSON - lime
    'json': 'text-cyber-lime',
    
    // Styles - magenta
    'css': 'text-cyber-magenta',
    'scss': 'text-cyber-magenta',
    'sass': 'text-cyber-magenta',
    'less': 'text-cyber-magenta',
    
    // Markdown - cyan
    'md': 'text-cyber-cyan',
    'mdx': 'text-cyber-cyan',
  };

  return colorMap[extension.toLowerCase()] || 'text-gray-400';
}
