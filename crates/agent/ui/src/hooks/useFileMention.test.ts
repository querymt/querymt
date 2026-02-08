import { describe, it, expect, beforeEach, vi } from 'vitest';
import { renderHook, act } from '@testing-library/react';
import { useFileMention } from './useFileMention';
import { FileIndexEntry } from '../types';

describe('useFileMention', () => {
  const mockFiles: FileIndexEntry[] = [
    { path: 'src/main.ts', is_dir: false },
    { path: 'src/utils/', is_dir: true },
    { path: 'src/App.tsx', is_dir: false },
    { path: 'README.md', is_dir: false },
    { path: 'package.json', is_dir: false },
    { path: 'tests/', is_dir: true },
    { path: 'docs/guide.md', is_dir: false },
  ];

  let mockRequestFileIndex: () => void;

  beforeEach(() => {
    mockRequestFileIndex = vi.fn();
  });

  it('returns initial state correctly', () => {
    const { result } = renderHook(() => useFileMention(mockRequestFileIndex));
    
    expect(result.current.results).toEqual([]);
    expect(result.current.allFiles).toEqual([]);
    expect(result.current.selectedIndex).toBe(0);
    expect(result.current.isLoading).toBe(false);
    expect(result.current.hasIndex).toBe(false);
  });

  it('calls requestFileIndex callback when requestIndex is invoked', () => {
    const { result } = renderHook(() => useFileMention(mockRequestFileIndex));
    
    act(() => {
      result.current.requestIndex();
    });
    
    expect(mockRequestFileIndex).toHaveBeenCalledTimes(1);
    expect(result.current.isLoading).toBe(true);
  });

  it('stores files and updates state when handleFileIndex is called', () => {
    const { result } = renderHook(() => useFileMention(mockRequestFileIndex));
    
    act(() => {
      result.current.requestIndex();
    });
    
    expect(result.current.isLoading).toBe(true);
    
    act(() => {
      result.current.handleFileIndex(mockFiles, Date.now());
    });
    
    expect(result.current.isLoading).toBe(false);
    expect(result.current.hasIndex).toBe(true);
    expect(result.current.allFiles).toEqual(mockFiles);
  });

  it('sets isLoading to false on error without setting hasIndex', () => {
    const { result } = renderHook(() => useFileMention(mockRequestFileIndex));
    
    act(() => {
      result.current.requestIndex();
    });
    
    expect(result.current.isLoading).toBe(true);
    
    act(() => {
      result.current.handleFileIndexError('Failed to index files');
    });
    
    expect(result.current.isLoading).toBe(false);
    expect(result.current.hasIndex).toBe(false);
    expect(result.current.allFiles).toEqual([]);
  });

  it('returns sorted results (dirs first, then alphabetical) without query', () => {
    const { result } = renderHook(() => useFileMention(mockRequestFileIndex));
    
    act(() => {
      result.current.handleFileIndex(mockFiles, Date.now());
    });
    
    // Directories should come first, then files, both alphabetically sorted
    expect(result.current.results.length).toBeGreaterThan(0);
    const results = result.current.results;
    
    // Find the first non-directory
    const firstFileIndex = results.findIndex(r => !r.is_dir);
    
    // All items before firstFileIndex should be directories
    if (firstFileIndex > 0) {
      for (let i = 0; i < firstFileIndex; i++) {
        expect(results[i].is_dir).toBe(true);
      }
    }
    
    // All items from firstFileIndex onward should be files
    if (firstFileIndex !== -1) {
      for (let i = firstFileIndex; i < results.length; i++) {
        expect(results[i].is_dir).toBe(false);
      }
    }
  });

  it('respects maxResults option', () => {
    const { result } = renderHook(() => 
      useFileMention(mockRequestFileIndex, { maxResults: 3 })
    );
    
    act(() => {
      result.current.handleFileIndex(mockFiles, Date.now());
    });
    
    expect(result.current.results).toHaveLength(3);
  });

  it('filters results with fuzzy search', () => {
    const { result } = renderHook(() => useFileMention(mockRequestFileIndex));
    
    act(() => {
      result.current.handleFileIndex(mockFiles, Date.now());
    });
    
    act(() => {
      result.current.search('main');
    });
    
    expect(result.current.results.length).toBeGreaterThan(0);
    expect(result.current.results.some(r => r.path.includes('main'))).toBe(true);
  });

  it('resets selectedIndex to 0 when searching', () => {
    const { result } = renderHook(() => useFileMention(mockRequestFileIndex));
    
    act(() => {
      result.current.handleFileIndex(mockFiles, Date.now());
    });
    
    act(() => {
      result.current.setSelectedIndex(3);
    });
    
    expect(result.current.selectedIndex).toBe(3);
    
    act(() => {
      result.current.search('test');
    });
    
    expect(result.current.selectedIndex).toBe(0);
  });

  it('automatically requests index when searching without index', () => {
    const { result } = renderHook(() => useFileMention(mockRequestFileIndex));
    
    expect(result.current.hasIndex).toBe(false);
    
    act(() => {
      result.current.search('test');
    });
    
    expect(mockRequestFileIndex).toHaveBeenCalledTimes(1);
    expect(result.current.isLoading).toBe(true);
  });

  it('clears query and resets selectedIndex when clear is called', () => {
    const { result } = renderHook(() => useFileMention(mockRequestFileIndex));
    
    act(() => {
      result.current.handleFileIndex(mockFiles, Date.now());
    });
    
    act(() => {
      result.current.search('test');
      result.current.setSelectedIndex(2);
    });
    
    expect(result.current.selectedIndex).toBe(2);
    
    act(() => {
      result.current.clear();
    });
    
    expect(result.current.selectedIndex).toBe(0);
    // After clear, should show all results again (sorted, no query)
    expect(result.current.results.length).toBeGreaterThan(0);
  });

  it('wraps selection forward with moveSelection', () => {
    const { result } = renderHook(() => useFileMention(mockRequestFileIndex));
    
    act(() => {
      result.current.handleFileIndex(mockFiles.slice(0, 3), Date.now());
    });
    
    expect(result.current.results).toHaveLength(3);
    expect(result.current.selectedIndex).toBe(0);
    
    act(() => {
      result.current.moveSelection(1);
    });
    expect(result.current.selectedIndex).toBe(1);
    
    act(() => {
      result.current.moveSelection(1);
    });
    expect(result.current.selectedIndex).toBe(2);
    
    // Wrap around to 0
    act(() => {
      result.current.moveSelection(1);
    });
    expect(result.current.selectedIndex).toBe(0);
  });

  it('wraps selection backward with moveSelection', () => {
    const { result } = renderHook(() => useFileMention(mockRequestFileIndex));
    
    act(() => {
      result.current.handleFileIndex(mockFiles.slice(0, 3), Date.now());
    });
    
    expect(result.current.selectedIndex).toBe(0);
    
    // Move backward from 0 should wrap to last index
    act(() => {
      result.current.moveSelection(-1);
    });
    expect(result.current.selectedIndex).toBe(2);
    
    act(() => {
      result.current.moveSelection(-1);
    });
    expect(result.current.selectedIndex).toBe(1);
  });

  it('handles moveSelection with no results', () => {
    const { result } = renderHook(() => useFileMention(mockRequestFileIndex));
    
    expect(result.current.results).toHaveLength(0);
    
    act(() => {
      result.current.moveSelection(1);
    });
    
    expect(result.current.selectedIndex).toBe(0);
    
    act(() => {
      result.current.moveSelection(-1);
    });
    
    expect(result.current.selectedIndex).toBe(0);
  });

  it('returns isLoading=false when index already exists even if loading again', () => {
    const { result } = renderHook(() => useFileMention(mockRequestFileIndex));
    
    // First, load the index
    act(() => {
      result.current.handleFileIndex(mockFiles, Date.now());
    });
    
    expect(result.current.hasIndex).toBe(true);
    expect(result.current.isLoading).toBe(false);
    
    // Now request again (shouldn't show loading since index exists)
    act(() => {
      result.current.requestIndex();
    });
    
    // isLoading should be false because index already exists
    expect(result.current.isLoading).toBe(false);
  });

  it('allows setting selectedIndex directly', () => {
    const { result } = renderHook(() => useFileMention(mockRequestFileIndex));
    
    act(() => {
      result.current.handleFileIndex(mockFiles, Date.now());
    });
    
    act(() => {
      result.current.setSelectedIndex(4);
    });
    
    expect(result.current.selectedIndex).toBe(4);
  });
});
