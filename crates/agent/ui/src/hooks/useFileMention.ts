import { useState, useCallback, useMemo } from 'react';
import Fuse from 'fuse.js';
import { FileIndexEntry } from '../types';

export interface UseFileMentionOptions {
  maxResults?: number;
}

export interface UseFileMentionReturn {
  results: FileIndexEntry[];
  allFiles: FileIndexEntry[];
  selectedIndex: number;
  isLoading: boolean;
  hasIndex: boolean;
  search: (query: string) => void;
  clear: () => void;
  moveSelection: (delta: number) => void;
  setSelectedIndex: (index: number) => void;
  handleFileIndex: (files: FileIndexEntry[], generatedAt: number) => void;
  handleFileIndexError: (message: string) => void;
  requestIndex: () => void;
}

export function useFileMention(
  requestFileIndex: () => void,
  options: UseFileMentionOptions = {}
): UseFileMentionReturn {
  const { maxResults = 15 } = options;
  
  const [fileIndex, setFileIndex] = useState<{ files: FileIndexEntry[]; generatedAt: number } | null>(null);
  const [isLoading, setIsLoading] = useState(false);
  const [query, setQuery] = useState('');
  const [selectedIndex, setSelectedIndex] = useState(0);
  
  // Build Fuse index when files change
  const fuse = useMemo(() => {
    if (!fileIndex?.files.length) return null;
    return new Fuse(fileIndex.files, {
      keys: ['path'],
      threshold: 0.4,
      distance: 100,
      includeScore: true,
      shouldSort: true,
    });
  }, [fileIndex?.files]);
  
  // Fuzzy search results (computed locally, no network)
  const results = useMemo(() => {
    if (!fuse || !query) {
      return (fileIndex?.files ?? [])
        .slice()
        .sort((a, b) => {
          if (a.is_dir !== b.is_dir) return a.is_dir ? -1 : 1;
          return a.path.localeCompare(b.path);
        })
        .slice(0, maxResults);
    }
    
    return fuse.search(query).slice(0, maxResults).map(r => r.item);
  }, [fuse, query, fileIndex?.files, maxResults]);
  
  // Request file index from backend (only on first @)
  const requestIndex = useCallback(() => {
    setIsLoading(true);
    requestFileIndex();
  }, [requestFileIndex]);
  
  // Handle incoming file index from WebSocket
  const handleFileIndex = useCallback((files: FileIndexEntry[], generatedAt: number) => {
    setFileIndex({ files, generatedAt });
    setIsLoading(false);
  }, []);

  // Handle file index errors
  const handleFileIndexError = useCallback((message: string) => {
    console.warn('[useFileMention] File index error:', message);
    setIsLoading(false);  // Reset loading state so user can retry
  }, []);
  
  // Local search (instant, no network)
  const search = useCallback((q: string) => {
    setQuery(q);
    setSelectedIndex(0);
    if (!fileIndex && !isLoading) {
      requestIndex();
    }
  }, [fileIndex, isLoading, requestIndex]);
  
  const clear = useCallback(() => {
    setQuery('');
    setSelectedIndex(0);
  }, []);
  
  const moveSelection = useCallback((delta: number) => {
    setSelectedIndex(prev => {
      const len = results.length;
      if (len === 0) return 0;
      return ((prev + delta) % len + len) % len;
    });
  }, [results.length]);

  return {
    results,
    allFiles: fileIndex?.files ?? [],
    selectedIndex,
    isLoading: isLoading && !fileIndex,
    hasIndex: !!fileIndex,
    search,
    clear,
    moveSelection,
    setSelectedIndex,
    handleFileIndex,
    handleFileIndexError,
    requestIndex,
  };
}
