import { useState, useCallback, useRef, useEffect } from 'react';
import { MentionsInput, Mention, SuggestionDataItem, type MentionsInputStyle } from 'react-mentions';
import { Loader } from 'lucide-react';
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
    },
  },
  suggestions: {
    list: {
      backgroundColor: 'rgb(20, 27, 61)',
      border: '2px solid #00fff9',
      borderRadius: '8px',
      fontSize: 14,
      maxHeight: '200px',
      overflow: 'auto',
      boxShadow: '0 0 20px rgba(0, 255, 249, 0.3)',
    },
    item: {
      padding: '8px 12px',
      borderBottom: '1px solid rgba(0, 255, 249, 0.1)',
      cursor: 'pointer',
      display: 'flex',
      alignItems: 'center',
      gap: '8px',
      backgroundColor: 'rgba(20, 27, 61, 0.8)',
      '&focused': {
        backgroundColor: 'rgba(0, 255, 249, 0.15)',
        boxShadow: 'inset 0 0 10px rgba(0, 255, 249, 0.2)',
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
  
  // When files change from empty to populated, re-trigger the callback
  useEffect(() => {
    if (files.length > 0 && callbackRef.current) {
      // Re-call with the updated file list
      const search = searchRef.current;
      const data: SuggestionDataItem[] = files
        .filter(file => {
          if (!search) return true;
          return file.path.toLowerCase().includes(search.toLowerCase());
        })
        .map(file => ({
          id: `${file.is_dir ? 'dir' : 'file'}:${file.path}`,
          display: file.path,
          isDir: file.is_dir,
        }));
      
      callbackRef.current(data);
    }
  }, [files]);

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
    
    // Convert to react-mentions format
    // Store type in the id: "file:path" or "dir:path"
    const data: SuggestionDataItem[] = files
      .filter(file => {
        if (!search) return true;
        return file.path.toLowerCase().includes(search.toLowerCase());
      })
      .map(file => ({
        id: `${file.is_dir ? 'dir' : 'file'}:${file.path}`,
        display: file.path,
        isDir: file.is_dir,
      }));
    
    callback(data);
  }, [files, isLoadingFiles, onRequestFiles]);

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
          gap: '8px',
          opacity: 0.7,
          fontStyle: 'italic',
        }}>
          <Loader className="w-4 h-4 animate-spin" style={{ color: '#00fff9' }} />
          <span style={{
            fontFamily: "'Monaco', 'Menlo', 'Ubuntu Mono', monospace",
            fontSize: '0.9em',
            color: '#d1d5db',
          }}>
            {suggestion.display}
          </span>
        </div>
      );
    }
    
    const isDir = (suggestion as any).isDir;
    const icon = isDir ? '📁' : '📄';
    
    return (
      <div style={{ display: 'flex', alignItems: 'center', gap: '8px' }}>
        <span style={{ fontSize: '1.2em' }}>{icon}</span>
        <span style={{
          fontFamily: "'Monaco', 'Menlo', 'Ubuntu Mono', monospace",
          fontSize: '0.9em',
          color: focused ? '#00fff9' : '#d1d5db',
          textShadow: focused ? '0 0 5px rgba(0, 255, 249, 0.4)' : 'none',
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
