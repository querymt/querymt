/**
 * Syntax highlighted code viewer using shiki
 *
 * A module-level cache keyed by (code, language, theme) ensures that
 * remounted instances (e.g. when ReactMarkdown rebuilds its tree during
 * streaming) render the previously-highlighted HTML synchronously on first
 * paint — no loading flash or layout jump.
 */

import { useEffect, useState } from 'react';
import { codeToHtml } from 'shiki';
import { detectLanguage } from '../utils/languageDetection';
import { useUiStore } from '../store/uiStore';
import { getShikiThemeForDashboard } from '../utils/dashboardThemes';

// ---- Module-level highlight cache ----

const highlightCache = new Map<string, string>();

// The full code string is included in the key, which duplicates it in the Map.
// A fast hash would avoid that extra copy, but at typical scale (dozens of
// code blocks per session) the overhead is negligible compared to the Shiki
// HTML output stored as the value.
function cacheKey(code: string, lang: string, theme: string): string {
  return `${theme}\0${lang}\0${code}`;
}

/** Visible for testing — clears the highlight cache. */
export function clearHighlightCache(): void {
  highlightCache.clear();
}

// ---- Component ----

export interface HighlightedCodeProps {
  code: string;
  filePath?: string;
  language?: string;
  lineNumbers?: boolean;
  startLine?: number;
  maxHeight?: string;
  /** When true the code is still being streamed — skip Shiki and render plain monospace. */
  isStreaming?: boolean;
}

export function HighlightedCode({
  code,
  filePath,
  language: providedLanguage,
  lineNumbers: _lineNumbers = true,
  startLine: _startLine = 1,
  maxHeight = '24rem',
  isStreaming = false,
}: HighlightedCodeProps) {
  const selectedTheme = useUiStore((state) => state.selectedTheme);
  const shikiTheme = getShikiThemeForDashboard(selectedTheme);
  const lang = providedLanguage || (filePath ? detectLanguage(filePath).language : 'plaintext');
  const key = cacheKey(code, lang, shikiTheme);
  const cached = highlightCache.get(key);

  const [html, setHtml] = useState<string>(cached ?? '');
  const [loading, setLoading] = useState(!cached);

  useEffect(() => {
    // Skip if streaming, or if the cache already supplied the HTML.
    if (isStreaming) return;

    // Re-check cache — may have been populated by a prior mount with the same key.
    const hit = highlightCache.get(key);
    if (hit) {
      setHtml(hit);
      setLoading(false);
      return;
    }

    let cancelled = false;

    async function highlight() {
      try {
        const highlighted = await codeToHtml(code, {
          lang,
          theme: shikiTheme,
        });

        highlightCache.set(key, highlighted);

        if (!cancelled) {
          setHtml(highlighted);
          setLoading(false);
        }
      } catch (error) {
        console.error('Syntax highlighting error:', error);
        // Fallback to plain text
        if (!cancelled) {
          setHtml(`<pre>${escapeHtml(code)}</pre>`);
          setLoading(false);
        }
      }
    }

    highlight();

    return () => {
      cancelled = true;
    };
  }, [code, lang, shikiTheme, isStreaming, key]);

  // While streaming, render plain monospace — zero async cost.
  if (isStreaming) {
    return (
      <pre
        className="highlighted-code-container overflow-auto font-mono text-sm p-4 text-ui-primary"
        style={{ maxHeight }}
      >
        <code>{code}</code>
      </pre>
    );
  }

  if (loading) {
    return (
      <pre
        className="highlighted-code-container overflow-auto font-mono text-sm p-4 text-ui-primary"
        style={{ maxHeight }}
      >
        <code>{code}</code>
      </pre>
    );
  }

  return (
    <div
      className="highlighted-code-container overflow-auto font-mono text-sm"
      style={{ maxHeight }}
      dangerouslySetInnerHTML={{ __html: html }}
    />
  );
}

function escapeHtml(text: string): string {
  const div = document.createElement('div');
  div.textContent = text;
  return div.innerHTML;
}
