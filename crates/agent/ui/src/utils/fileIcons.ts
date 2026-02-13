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
    return 'text-accent-tertiary';
  }

  if (!extension) {
    return 'text-ui-secondary';
  }

  const colorMap: Record<string, string> = {
    // TypeScript - cyan
    'ts': 'text-accent-primary',
    'tsx': 'text-accent-primary',
    
    // JavaScript - lime
    'js': 'text-status-success',
    'jsx': 'text-status-success',
    'mjs': 'text-status-success',
    'cjs': 'text-status-success',
    
    // Rust - orange
    'rs': 'text-status-warning',
    'toml': 'text-status-warning',
    
    // JSON - lime
    'json': 'text-status-success',
    
    // Styles - magenta
    'css': 'text-accent-secondary',
    'scss': 'text-accent-secondary',
    'sass': 'text-accent-secondary',
    'less': 'text-accent-secondary',
    
    // Markdown - cyan
    'md': 'text-accent-primary',
    'mdx': 'text-accent-primary',
  };

  return colorMap[extension.toLowerCase()] || 'text-ui-secondary';
}
