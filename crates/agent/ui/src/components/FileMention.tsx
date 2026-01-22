import { useState } from 'react';
import { Check } from 'lucide-react';
import { getFileIcon, getFileIconColor } from '../utils/fileIcons';
import type { FileMention as FileMentionType } from '../utils/fileMentionParser';

interface FileMentionProps {
  mention: FileMentionType;
}

export function FileMention({ mention }: FileMentionProps) {
  const [copied, setCopied] = useState(false);
  const Icon = getFileIcon(mention.extension, mention.type === 'dir');
  const iconColor = getFileIconColor(mention.extension, mention.type === 'dir');

  const handleClick = async (e: React.MouseEvent) => {
    e.preventDefault();
    
    try {
      await navigator.clipboard.writeText(mention.path);
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch (err) {
      console.error('Failed to copy path:', err);
    }
  };

  return (
    <span
      onClick={handleClick}
      className="file-mention inline-flex items-center gap-1 cursor-pointer group relative"
      title={`Click to copy: ${mention.path}`}
    >
      <Icon className={`w-3.5 h-3.5 ${iconColor} flex-shrink-0`} />
      <span className="file-mention-path font-mono text-sm">
        {mention.path}
      </span>
      {copied && (
        <span className="absolute -top-6 left-1/2 -translate-x-1/2 bg-cyber-surface border border-cyber-lime px-2 py-1 rounded text-xs text-cyber-lime whitespace-nowrap flex items-center gap-1 animate-fade-in">
          <Check className="w-3 h-3" />
          Copied!
        </span>
      )}
    </span>
  );
}
