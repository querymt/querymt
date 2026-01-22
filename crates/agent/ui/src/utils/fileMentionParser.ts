export interface FileMention {
  type: 'file' | 'dir';
  path: string;
  extension?: string;
}

export interface ParsedContent {
  type: 'text' | 'mention';
  content: string;
  mention?: FileMention;
}

const FILE_MENTION_REGEX = /@\{(file|dir):([^}]+)\}/g;

/**
 * Parse a string to extract file/dir mentions and return structured segments
 */
export function parseFileMentions(text: string): ParsedContent[] {
  const segments: ParsedContent[] = [];
  let lastIndex = 0;

  // Find all matches
  const matches = Array.from(text.matchAll(FILE_MENTION_REGEX));

  for (const match of matches) {
    const [fullMatch, type, path] = match;
    const matchIndex = match.index!;

    // Add text before this mention
    if (matchIndex > lastIndex) {
      const beforeText = text.slice(lastIndex, matchIndex);
      segments.push({
        type: 'text',
        content: beforeText,
      });
    }

    // Add the mention
    const extension = type === 'file' ? extractExtension(path) : undefined;
    segments.push({
      type: 'mention',
      content: fullMatch,
      mention: {
        type: type as 'file' | 'dir',
        path,
        extension,
      },
    });

    lastIndex = matchIndex + fullMatch.length;
  }

  // Add remaining text
  if (lastIndex < text.length) {
    const remainingText = text.slice(lastIndex);
    segments.push({
      type: 'text',
      content: remainingText,
    });
  }

  // If no mentions found, return the whole text as one segment
  if (segments.length === 0) {
    segments.push({
      type: 'text',
      content: text,
    });
  }

  return segments;
}

/**
 * Extract file extension from a path
 */
function extractExtension(path: string): string | undefined {
  const lastDot = path.lastIndexOf('.');
  const lastSlash = Math.max(path.lastIndexOf('/'), path.lastIndexOf('\\'));
  
  // Only consider it an extension if the dot comes after any slashes
  // and isn't a hidden file (like .gitignore at the root)
  if (lastDot > lastSlash && lastDot > 0) {
    return path.slice(lastDot + 1).toLowerCase();
  }
  
  return undefined;
}

/**
 * Check if a string contains any file mentions
 */
export function hasFileMentions(text: string): boolean {
  return FILE_MENTION_REGEX.test(text);
}
