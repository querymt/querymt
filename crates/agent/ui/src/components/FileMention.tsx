import { Check } from 'lucide-react';
import { getFileIcon, getFileIconColor } from '../utils/fileIcons';
import { useCopyToClipboard } from '../hooks/useCopyToClipboard';
import type { FileMention as FileMentionType } from '../utils/fileMentionParser';

interface FileMentionProps {
  mention: FileMentionType;
}

export function FileMention({ mention }: FileMentionProps) {
  const Icon = getFileIcon(mention.extension, mention.type === 'dir');
  const iconColor = getFileIconColor(mention.extension, mention.type === 'dir');
  const { copiedValue, copy } = useCopyToClipboard({ resetDelay: 1500 });
  
  const copied = copiedValue === mention.path;

  const handleClick = async (e: React.MouseEvent) => {
    e.preventDefault();
    await copy(mention.path, mention.path);
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
        <span className="absolute -top-6 left-1/2 -translate-x-1/2 bg-surface-elevated border border-status-success px-2 py-1 rounded text-xs text-status-success whitespace-nowrap flex items-center gap-1 animate-fade-in">
          <Check className="w-3 h-3" />
          Copied!
        </span>
      )}
    </span>
  );
}
