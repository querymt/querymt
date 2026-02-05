import { useState, useCallback, useRef, useEffect } from 'react';
import { MentionsInput, Mention, SuggestionDataItem, type MentionsInputStyle } from 'react-mentions';
import { Loader, File, Folder } from 'lucide-react';
import { FileIndexEntry } from '../types';

interface MentionInputProps {
  value: string;
  onChange: (value: string) => void;
  onSubmit: () => void;
  placeholder?: string;
  disabled?: boolean;
  files: FileIndexEntry[];
  onRequestFiles: () => void;
  isLoadingFiles?: boolean;
  showIndexBuilding?: boolean;
}

// Cyberpunk theme styles for MentionsInput
const mentionsInputStyle: MentionsInputStyle = {
  control: {
    backgroundColor: 'rgb(20, 27, 61)',
    fontSize: 16,
    fontWeight: 'normal',
    width: '100%',
    maxHeight: '180px',
    overflow: 'hidden',
  },
  '&singleLine': {
    display: 'flex',
    highlighter: {
      padding: '12px 40px 12px 16px',
      border: '2px solid rgb(59, 68, 129)',
      borderRadius: '8px',
      minHeight: '48px',
    },
    input: {
      padding: '12px 40px 12px 16px',
      border: '2px solid rgb(59, 68, 129)',
      borderRadius: '8px',
      backgroundColor: 'rgb(20, 27, 61)',
      color: 'white',
      minHeight: '48px',
      outline: 'none',
      transition: 'all 0.2s',
    },
  },
  '&multiLine': {
    display: 'block',
    highlighter: {
      padding: '12px 40px 12px 16px',
      border: 'none',
      minHeight: '48px',
      whiteSpace: 'pre-wrap',
      wordBreak: 'break-word',
      boxSizing: 'border-box',
      width: '100%',
      maxHeight: '180px',
      overflow: 'hidden',
    },
    input: {
      padding: '12px 40px 12px 16px',
      border: '2px solid rgb(59, 68, 129)',
      borderRadius: '8px',
      backgroundColor: 'rgb(20, 27, 61)',
      color: 'white',
      minHeight: '48px',
      outline: 'none',
      transition: 'all 0.2s',
      resize: 'none',
      boxSizing: 'border-box',
      width: '100%',
      maxHeight: '180px',
      overflowY: 'auto',
    },
  },
  suggestions: {
    list: {
      backgroundColor: '#0a0e27',  // cyber-bg (was cyber-surface)
      border: '1px solid rgba(0, 255, 249, 0.3)',  // 1px, 30% opacity (was 2px, 100%)
      borderRadius: '12px',  // rounded-xl (was 8px)
      fontSize: 12,  // text-xs ~12px (was 14)
      maxHeight: '240px',  // match ModelPicker (was 200px)
      overflow: 'auto',
      boxShadow: '0 8px 30px rgba(0, 0, 0, 0.4), 0 0 20px rgba(0, 255, 249, 0.15)',  // layered shadow like ModelPicker
      zIndex: 9999,
      fontFamily: "'Inter', -apple-system, BlinkMacSystemFont, 'Segoe UI', 'Roboto', sans-serif",
      padding: '4px',  // inner padding like cmdk px-1 py-1
      minWidth: '320px',  // reasonable minimum
      maxWidth: '480px',  // match ModelPicker width
    },
    item: {
      padding: '6px 8px',  // px-2 py-1.5 in Tailwind units
      borderBottom: 'none',  // remove separators (cmdk uses gap)
      borderRadius: '8px',  // rounded-lg like cmdk items
      cursor: 'pointer',
      display: 'flex',
      alignItems: 'center',
      gap: '8px',
      backgroundColor: 'transparent',  // transparent default (was semi-transparent)
      transition: 'all 0.15s ease',
      margin: '1px 0',  // small gap between items
      color: '#d1d5db',  // text-gray-300
      fontSize: '12px',  // text-xs
      '&focused': {
        backgroundColor: 'rgba(0, 255, 249, 0.2)',  // data-[selected=true]:bg-cyber-cyan/20
        color: '#00fff9',  // text-cyber-cyan
        border: '1px solid rgba(0, 255, 249, 0.4)',  // border-cyber-cyan/40
        boxShadow: 'none',  // remove inset shadow
      },
    },
  },
};

// Style for the mention badge
const mentionStyle = {
  backgroundColor: 'rgba(0, 255, 249, 0.15)',
  border: '1px solid rgba(0, 255, 249, 0.4)',
  borderRadius: '4px',
  padding: '1px 6px',
  color: '#00fff9',
  fontFamily: "'Monaco', 'Menlo', 'Ubuntu Mono', monospace",
  fontSize: '0.9em',
  fontWeight: 500,
};

export function MentionInput({
  value,
  onChange,
  onSubmit,
  placeholder = '',
  disabled = false,
  files,
  onRequestFiles,
  isLoadingFiles = false,
  showIndexBuilding = false,
}: MentionInputProps) {
  const [isFocused, setIsFocused] = useState(false);
  const callbackRef = useRef<((data: SuggestionDataItem[]) => void) | null>(null);
  const searchRef = useRef<string>('');
  const inputRef = useRef<HTMLTextAreaElement | null>(null);
  const maxInputHeight = 180;
  
  // Advanced fuzzy matching with scoring and ranking
  const fuzzyMatchWithScore = useCallback((search: string, filePath: string): { match: boolean; score: number } => {
    if (!search) {
      return { match: true, score: 0 };
    }
    
    const searchLower = search.toLowerCase();
    const pathLower = filePath.toLowerCase();
    
    // Extract filename from path for bonus scoring
    const lastSlashIndex = filePath.lastIndexOf('/');
    const filename = lastSlashIndex >= 0 ? filePath.slice(lastSlashIndex + 1) : filePath;
    const filenameLower = filename.toLowerCase();
    
    let score = 0;
    
    // 1. Exact match (highest priority) - score 10000
    if (filePath === search) {
      return { match: true, score: 10000 };
    }
    
    // 2. Case-insensitive exact match - score 9000
    if (pathLower === searchLower) {
      return { match: true, score: 9000 };
    }
    
    // 3. Exact substring match in path - score 8000 + position bonus
    const exactIndex = pathLower.indexOf(searchLower);
    if (exactIndex >= 0) {
      // Bonus for earlier position in path
      const positionBonus = Math.max(0, 100 - exactIndex);
      score = 8000 + positionBonus;
      
      // Extra bonus if match is in filename
      if (filenameLower.indexOf(searchLower) >= 0) {
        score += 500;
      }
      
      // Extra bonus if match is at the start
      if (exactIndex === 0) {
        score += 200;
      }
      
      return { match: true, score };
    }
    
    // 4. Exact match in filename - score 7000
    const filenameExactIndex = filenameLower.indexOf(searchLower);
    if (filenameExactIndex >= 0) {
      score = 7000 + (filenameExactIndex === 0 ? 200 : 0);
      return { match: true, score };
    }
    
    // 5. Fuzzy match with scoring based on match quality
    const searchChars = searchLower.split('');
    const pathChars = pathLower.split('');
    
    let searchIndex = 0;
    let pathIndex = 0;
    let consecutiveMatches = 0;
    let matchPositions: number[] = [];
    
    // Try to match all search characters
    while (searchIndex < searchChars.length && pathIndex < pathChars.length) {
      if (searchChars[searchIndex] === pathChars[pathIndex]) {
        matchPositions.push(pathIndex);
        
        // Bonus for consecutive matches
        if (searchIndex > 0 && matchPositions[searchIndex] === matchPositions[searchIndex - 1] + 1) {
          consecutiveMatches++;
        }
        
        searchIndex++;
      }
      pathIndex++;
    }
    
    // If we didn't match all characters, it's not a match
    if (searchIndex < searchChars.length) {
      return { match: false, score: 0 };
    }
    
    // Base score for fuzzy match - 5000
    score = 5000;
    
    // Bonus for consecutive character matches (up to +1000)
    score += consecutiveMatches * 50;
    
    // Bonus for matches at word boundaries (/, -, _, .)
    let boundaryMatches = 0;
    for (let i = 0; i < matchPositions.length; i++) {
      const pos = matchPositions[i];
      if (pos === 0) {
        boundaryMatches++;
      } else {
        const prevChar = pathChars[pos - 1];
        if (prevChar === '/' || prevChar === '-' || prevChar === '_' || prevChar === '.') {
          boundaryMatches++;
        }
      }
    }
    score += boundaryMatches * 100;
    
    // Bonus for matches in filename vs directory path
    let filenameMatches = 0;
    const filenameStartIndex = lastSlashIndex + 1;
    for (const pos of matchPositions) {
      if (pos >= filenameStartIndex) {
        filenameMatches++;
      }
    }
    const filenameMatchRatio = filenameMatches / matchPositions.length;
    score += filenameMatchRatio * 300;
    
    // Penalty for longer paths (prefer shorter paths)
    const pathLengthPenalty = Math.min(100, filePath.length / 2);
    score -= pathLengthPenalty;
    
    // Bonus for match density (how close together the matches are)
    if (matchPositions.length > 0) {
      const matchSpan = matchPositions[matchPositions.length - 1] - matchPositions[0] + 1;
      const density = searchChars.length / matchSpan;
      score += density * 200;
    }
    
    // Case-sensitive exact character matches bonus
    let caseSensitiveMatches = 0;
    for (let i = 0; i < matchPositions.length; i++) {
      const searchChar = search.charAt(i);
      const pathChar = filePath.charAt(matchPositions[i]);
      if (searchChar === pathChar) {
        caseSensitiveMatches++;
      }
    }
    if (caseSensitiveMatches > 0) {
      score += caseSensitiveMatches * 20;
    }
    
    return { match: true, score };
  }, []);
  
  // When files change from empty to populated, re-trigger the callback
  useEffect(() => {
    if (files.length > 0 && callbackRef.current) {
      // Re-call with the updated file list
      const search = searchRef.current;
      
      // Score and filter files
      const scoredFiles = files
        .map(file => {
          const result = fuzzyMatchWithScore(search, file.path);
          return {
            file,
            score: result.score,
            match: result.match,
          };
        })
        .filter(item => item.match);
      
      // Sort by score (highest first) and limit to top 50
      scoredFiles.sort((a, b) => b.score - a.score);
      const topFiles = scoredFiles.slice(0, 50);
      
      const data: SuggestionDataItem[] = topFiles.map(item => ({
        id: `${item.file.is_dir ? 'dir' : 'file'}:${item.file.path}`,
        display: item.file.path,
        isDir: item.file.is_dir,
      }));
      
      callbackRef.current(data);
    }
  }, [files, fuzzyMatchWithScore]);

  useEffect(() => {
    const input = inputRef.current;
    if (!input) return;
    input.style.height = 'auto';
    const nextHeight = Math.min(input.scrollHeight, maxInputHeight);
    input.style.height = `${nextHeight}px`;
    input.style.overflowY = input.scrollHeight > maxInputHeight ? 'auto' : 'hidden';
  }, [value, maxInputHeight]);

  // Convert FileIndexEntry[] to react-mentions format or use function
  const mentionData = useCallback((search: string, callback: (data: SuggestionDataItem[]) => void) => {
    // Store refs for when files arrive
    callbackRef.current = callback;
    searchRef.current = search;
    // Request files if we don't have them yet
    if (files.length === 0 && !isLoadingFiles) {
      onRequestFiles();
    }
    
    // If loading or no files yet, show loading indicator
    if (files.length === 0) {
      callback([{
        id: '__loading__',
        display: 'Loading files...',
      }]);
      return;
    }
    
    // Score and filter files with fuzzy matching
    const scoredFiles = files
      .map(file => {
        const result = fuzzyMatchWithScore(search, file.path);
        return {
          file,
          score: result.score,
          match: result.match,
        };
      })
      .filter(item => item.match);
    
    // Sort by score (highest first) and limit to top 50 results
    scoredFiles.sort((a, b) => b.score - a.score);
    const topFiles = scoredFiles.slice(0, 50);
    
    // Convert to react-mentions format
    const data: SuggestionDataItem[] = topFiles.map(item => ({
      id: `${item.file.is_dir ? 'dir' : 'file'}:${item.file.path}`,
      display: item.file.path,
      isDir: item.file.is_dir,
    }));
    
    callback(data);
  }, [files, isLoadingFiles, onRequestFiles, fuzzyMatchWithScore]);

  const handleKeyDown = useCallback(
    (e: React.KeyboardEvent) => {
      if (e.key === 'Enter' && !e.shiftKey) {
        e.preventDefault();
        onSubmit();
      }
    },
    [onSubmit]
  );

  // Custom render suggestion for the dropdown
  const renderSuggestion = useCallback((
    suggestion: SuggestionDataItem,
    _search: string,
    _highlightedDisplay: React.ReactNode,
    _index: number,
    focused: boolean
  ) => {
    // Handle loading state
    if (suggestion.id === '__loading__') {
      return (
        <div style={{ 
          display: 'flex', 
          alignItems: 'center', 
          justifyContent: 'center',
          gap: '8px',
          opacity: 0.6,
          padding: '16px 8px',
          fontSize: '12px',
        }}>
          <Loader className="w-3.5 h-3.5 animate-spin" style={{ color: '#6b7280' }} />
          <span style={{
            color: '#6b7280',
          }}>
            {suggestion.display}
          </span>
        </div>
      );
    }
    
    const isDir = (suggestion as any).isDir;
    
    return (
      <div style={{ display: 'flex', alignItems: 'center', gap: '8px', width: '100%' }}>
        {isDir ? (
          <Folder className="w-3.5 h-3.5 flex-shrink-0" style={{ color: focused ? '#00fff9' : '#9ca3af' }} />
        ) : (
          <File className="w-3.5 h-3.5 flex-shrink-0" style={{ color: focused ? '#00fff9' : '#9ca3af' }} />
        )}
        <span style={{
          fontFamily: "'Monaco', 'Menlo', 'Ubuntu Mono', monospace",
          fontSize: '12px',
          color: focused ? '#00fff9' : '#d1d5db',
          overflow: 'hidden',
          textOverflow: 'ellipsis',
          whiteSpace: 'nowrap',
          flex: 1,
        }}>
          {suggestion.display}
        </span>
      </div>
    );
  }, []);

  return (
    <div style={{ position: 'relative', flex: 1 }}>
      <MentionsInput
        value={value}
        onChange={(e: any) => onChange(e.target.value)}
        onKeyDown={handleKeyDown}
        onFocus={() => setIsFocused(true)}
        onBlur={() => setIsFocused(false)}
        placeholder={placeholder}
        disabled={disabled}
        inputRef={inputRef}
        style={{
          ...mentionsInputStyle,
          '&multiLine': {
            ...mentionsInputStyle['&multiLine'],
            input: {
              ...mentionsInputStyle['&multiLine']!.input,
              borderColor: isFocused ? '#00fff9' : 'rgb(59, 68, 129)',
              boxShadow: isFocused ? '0 0 20px rgba(0, 255, 249, 0.3)' : 'none',
            },
          },
        }}
        suggestionsPortalHost={document.body}
        allowSuggestionsAboveCursor={true}
      >
        <Mention
          trigger="@"
          data={mentionData}
          style={mentionStyle}
          markup="@{__id__}"
          regex={/@\{([^}]+)\}/}
          displayTransform={(id: string, display: string) => {
            // Ignore loading state
            if (id === '__loading__') {
              return '';
            }
            
            // id is like "file:path" or "dir:path"
            // Extract the type and path
            const colonIndex = id.indexOf(':');
            if (colonIndex > -1) {
              const type = id.slice(0, colonIndex);
              const path = id.slice(colonIndex + 1);
              const icon = type === 'dir' ? '📁' : '📄';
              return `${icon} ${path}`;
            }
            return display;
          }}
          renderSuggestion={renderSuggestion}
          appendSpaceOnAdd={true}
          onAdd={(id: string | number) => {
            // Prevent adding the loading placeholder
            if (id === '__loading__') {
              return false;
            }
          }}
        />
      </MentionsInput>
      
      {/* Index building indicator */}
      {showIndexBuilding && (
        <div style={{
          position: 'absolute',
          right: '12px',
          top: '50%',
          transform: 'translateY(-50%)',
          color: '#00fff9',
        }}>
          <Loader className="w-4 h-4 animate-spin" />
        </div>
      )}
    </div>
  );
}
