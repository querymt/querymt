/**
 * Syntax highlighted code viewer using shiki
 */

import { useEffect, useState } from 'react';
import { codeToHtml } from 'shiki';
import { detectLanguage } from '../utils/languageDetection';
import { useUiStore } from '../store/uiStore';
import { getShikiThemeForDashboard } from '../utils/dashboardThemes';

export interface HighlightedCodeProps {
  code: string;
  filePath?: string;
  language?: string;
  lineNumbers?: boolean;
  startLine?: number;
  maxHeight?: string;
}

export function HighlightedCode({
  code,
  filePath,
  language: providedLanguage,
  lineNumbers = true,
  startLine = 1,
  maxHeight = '24rem',
}: HighlightedCodeProps) {
  const [html, setHtml] = useState<string>('');
  const [loading, setLoading] = useState(true);
  const selectedTheme = useUiStore((state) => state.selectedTheme);
  const shikiTheme = getShikiThemeForDashboard(selectedTheme);

  useEffect(() => {
    let cancelled = false;

    async function highlight() {
      try {
        // Detect language from file path or use provided
        const lang = providedLanguage || (filePath ? detectLanguage(filePath).language : 'plaintext');
        
        const highlighted = await codeToHtml(code, {
          lang,
          theme: shikiTheme,
        });

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
  }, [code, filePath, providedLanguage, lineNumbers, shikiTheme, startLine]);

  if (loading) {
    return (
      <div className="flex items-center justify-center p-8">
        <div className="text-sm text-ui-muted">Highlighting code...</div>
      </div>
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
