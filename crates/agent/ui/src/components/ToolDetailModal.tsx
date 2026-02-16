/**
 * Large modal for showing full tool details - input, output, diffs
 */


import * as Dialog from '@radix-ui/react-dialog';
import { X, Clock, CheckCircle, XCircle, Loader, Copy, Check } from 'lucide-react';
import { PatchDiff } from '@pierre/diffs/react';
import { EventItem } from '../types';
import { generateToolSummary } from '../utils/toolSummary';
import { HighlightedCode } from './HighlightedCode';
import { isMarkdownFile } from '../utils/languageDetection';
import { MessageContent } from './MessageContent';
import { useCopyToClipboard } from '../hooks/useCopyToClipboard';
import { useUiStore } from '../store/uiStore';
import { getDashboardThemeVariant, getDiffThemeForDashboard } from '../utils/dashboardThemes';

export interface ToolDetailModalProps {
  event: EventItem & { mergedResult?: EventItem };
  onClose: () => void;
}

export function ToolDetailModal({ event, onClose }: ToolDetailModalProps) {
  const selectedTheme = useUiStore((state) => state.selectedTheme);
  const diffTheme = getDiffThemeForDashboard(selectedTheme);
  const diffThemeType = getDashboardThemeVariant(selectedTheme);
  const toolKind = event.toolCall?.kind;
  const toolName = inferToolName(event);
  const rawInput = parseJsonMaybe(event.toolCall?.raw_input) ?? event.toolCall?.raw_input;
  const summary = generateToolSummary(toolKind, toolName, rawInput);

  // Status
  const hasMergedResult = 'mergedResult' in event && event.mergedResult;
  const resultEvent = hasMergedResult ? event.mergedResult : undefined;
  const status = hasMergedResult
    ? event.mergedResult?.toolCall?.status
    : event.toolCall?.status;
  const isInProgress = !hasMergedResult && !status;
  const isCompleted = status === 'completed';
  const isFailed = status === 'failed';

  // Copy to clipboard hook
  const { copiedValue: copiedSection, copy: copyToClipboard } = useCopyToClipboard();

  // Check for special tool types
  const isEdit = toolKind === 'edit' || toolKind === 'mcp_edit';
  const isPatch = toolKind === 'apply_patch' || toolKind === 'mcp_apply_patch';
  const isShell = toolKind === 'shell' || toolKind === 'bash' || toolKind === 'mcp_bash';
  const isRead = toolKind === 'read' || toolKind === 'read_tool' || toolKind === 'mcp_read';

  return (
    <Dialog.Root open onOpenChange={(open) => { if (!open) onClose(); }}>
      <Dialog.Portal>
        <Dialog.Overlay className="fixed inset-0 z-50 bg-surface-canvas/80 animate-fade-in" />
        <Dialog.Content
          className="
            fixed z-50 top-1/2 left-1/2 -translate-x-1/2 -translate-y-1/2
            w-[96vw] max-w-none h-[85vh] max-h-[900px]
            bg-surface-elevated border border-accent-primary/30 rounded-lg
            shadow-lg shadow-accent-primary/25
            flex flex-col overflow-hidden
            animate-fade-in
          "
          aria-describedby={undefined}
        >
          {/* Header */}
          <div className="flex items-center justify-between px-5 py-3 border-b border-surface-border bg-surface-canvas/50">
            <div className="flex items-center gap-3">
              <span className="text-xl">{summary.icon}</span>
              <div>
                <Dialog.Title className="text-lg font-semibold text-accent-primary">
                  {summary.name}
                </Dialog.Title>
                {summary.keyParam && (
                  <p className="text-xs text-ui-secondary font-mono truncate max-w-md">
                    {summary.keyParam}
                  </p>
                )}
              </div>
            </div>
            <div className="flex items-center gap-4">
              {/* Status */}
              <div className="flex items-center gap-2">
                {isInProgress && (
                  <span className="flex items-center gap-1.5 text-xs text-accent-tertiary">
                    <Loader className="w-4 h-4 animate-spin" />
                    Running...
                  </span>
                )}
                {isCompleted && (
                  <span className="flex items-center gap-1.5 text-xs text-status-success">
                    <CheckCircle className="w-4 h-4" />
                    Completed
                  </span>
                )}
                {isFailed && (
                  <span className="flex items-center gap-1.5 text-xs text-status-warning">
                    <XCircle className="w-4 h-4" />
                    Failed
                  </span>
                )}
              </div>
              {/* Timestamp */}
              <span className="flex items-center gap-1 text-xs text-ui-muted">
                <Clock className="w-3.5 h-3.5" />
                {new Date(event.timestamp).toLocaleTimeString()}
              </span>
              {/* Close button */}
              <Dialog.Close className="p-1.5 rounded hover:bg-surface-canvas transition-colors text-ui-secondary hover:text-ui-primary">
                <X className="w-5 h-5" />
              </Dialog.Close>
            </div>
          </div>

          {/* Content */}
          <div className="flex-1 overflow-auto p-5 space-y-5">
            {/* Input Section */}
            {rawInput && (
              <Section
                title="Input"
                copyable
                onCopy={() => copyToClipboard(JSON.stringify(rawInput, null, 2), 'input')}
                copied={copiedSection === 'input'}
              >
                {(isEdit || isPatch) ? (
                  <DiffView
                    toolKind={toolKind}
                    rawInput={rawInput}
                    diffTheme={diffTheme}
                    diffThemeType={diffThemeType}
                  />
                ) : (
                  <JsonView data={rawInput} />
                )}
              </Section>
            )}

            {/* Result Section */}
            {resultEvent && (
              <Section
                title="Result"
                copyable
                onCopy={() => copyToClipboard(
                  resultEvent.toolCall?.raw_output
                    ? JSON.stringify(resultEvent.toolCall.raw_output, null, 2)
                    : resultEvent.content || '',
                  'result'
                )}
                copied={copiedSection === 'result'}
              >
                {isShell ? (
                  <ShellResultView event={resultEvent} />
                ) : isRead ? (
                  <FileReadView event={resultEvent} />
                ) : (
                  <ResultView event={resultEvent} />
                )}
              </Section>
            )}

            {/* Raw Data Section (collapsed by default) */}
            <details className="group">
              <summary className="text-xs text-ui-muted cursor-pointer hover:text-ui-secondary transition-colors py-2">
                Show raw event data
              </summary>
              <div className="mt-2">
                <JsonView data={event} />
              </div>
            </details>
          </div>
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  );
}

// Section component
function Section({
  title,
  children,
  copyable,
  onCopy,
  copied,
}: {
  title: string;
  children: React.ReactNode;
  copyable?: boolean;
  onCopy?: () => void;
  copied?: boolean;
}) {
  return (
    <div className="space-y-2">
      <div className="flex items-center justify-between">
        <h3 className="text-sm font-medium text-ui-secondary uppercase tracking-wider">
          {title}
        </h3>
        {copyable && onCopy && (
          <button
            onClick={onCopy}
            className="flex items-center gap-1 text-xs text-ui-muted hover:text-accent-primary transition-colors"
          >
            {copied ? (
              <>
                <Check className="w-3.5 h-3.5" />
                Copied
              </>
            ) : (
              <>
                <Copy className="w-3.5 h-3.5" />
                Copy
              </>
            )}
          </button>
        )}
      </div>
      <div className="rounded-md border border-surface-border/50 bg-surface-canvas/50 overflow-hidden">
        {children}
      </div>
    </div>
  );
}

// JSON viewer
function JsonView({ data }: { data: unknown }) {
  const formatted = JSON.stringify(data, null, 2);
  return (
    <pre className="p-4 text-xs font-mono text-ui-secondary overflow-auto max-h-96 whitespace-pre-wrap break-words">
      {formatted}
    </pre>
  );
}

// Diff viewer for edit/patch tools
function DiffView({
  toolKind,
  rawInput,
  diffTheme,
  diffThemeType,
}: {
  toolKind?: string;
  rawInput: unknown;
  diffTheme: string;
  diffThemeType: 'dark' | 'light';
}) {
  const input = rawInput as Record<string, unknown>;
  
  // Handle write tool - show as diff with empty left side
  if (toolKind === 'write' || toolKind === 'mcp_write' || toolKind === 'write_file') {
    const filePath = input.filePath || input.file_path || input.path;
    const content = input.content;
    
    if (typeof filePath === 'string' && typeof content === 'string') {
      const normalizedPath = (filePath as string).replace(/^\/+/, '') || 'file';
      const newLines = content.split('\n').length;
      const newBlock = content.split('\n').map((line: string) => `+${line}`).join('\n');
      const patch = [
        `diff --git a/${normalizedPath} b/${normalizedPath}`,
        `new file mode 100644`,
        `--- /dev/null`,
        `+++ b/${normalizedPath}`,
        `@@ -0,0 +1,${newLines} @@`,
        newBlock,
      ].join('\n');
      
      return (
        <div>
          <div className="text-[11px] text-ui-secondary mb-2 font-mono">
            Writing to: <span className="text-accent-primary">{filePath as string}</span>
          </div>
          <div className="event-diff-container m-0 border-0">
            <PatchDiff
              patch={patch}
              options={{
                theme: diffTheme,
                themeType: diffThemeType,
                diffStyle: 'split',
                diffIndicators: 'bars',
                lineDiffType: 'word-alt',
                overflow: 'wrap',
                disableLineNumbers: false,
                useCSSClasses: true,
                disableBackground: true,
              }}
            />
          </div>
        </div>
      );
    }
  }
  
  // Handle edit tool
  if (toolKind === 'edit' || toolKind === 'mcp_edit') {
    const editInput = extractEditInput(input);
    if (editInput?.oldString || editInput?.newString) {
      const patch = buildEditPatch(editInput);
      return (
        <div className="event-diff-container m-0 border-0">
          <PatchDiff
            patch={patch}
            options={{
              theme: diffTheme,
              themeType: diffThemeType,
              diffStyle: 'split',
              diffIndicators: 'bars',
              lineDiffType: 'word-alt',
              overflow: 'wrap',
              disableLineNumbers: false,
              useCSSClasses: true,
              disableBackground: true,
            }}
          />
        </div>
      );
    }
  }
  
  // Handle apply_patch tool
  const patchValue = extractPatchValue(input);
  if (patchValue) {
    return (
      <div className="event-diff-container m-0 border-0">
        <PatchDiff
          patch={patchValue}
          options={{
            theme: diffTheme,
            themeType: diffThemeType,
            diffStyle: 'unified',
            diffIndicators: 'bars',
            overflow: 'wrap',
            useCSSClasses: true,
            disableBackground: true,
          }}
        />
      </div>
    );
  }

  // Fallback to JSON
  return <JsonView data={rawInput} />;
}

// Shell result viewer
function ShellResultView({ event }: { event: EventItem }) {
  const rawOutput = event.toolCall?.raw_output ?? event.content;
  const parsed = parseJsonMaybe(rawOutput);
  
  const stdout = typeof parsed?.stdout === 'string' ? parsed.stdout : '';
  const stderr = typeof parsed?.stderr === 'string' ? parsed.stderr : '';
  const exitCode = typeof parsed?.exit_code === 'number' ? parsed.exit_code : undefined;

  return (
    <div className="font-mono text-xs">
      {/* Header with exit code */}
      <div className="flex items-center justify-between px-4 py-2 border-b border-surface-border/50 bg-surface-canvas/50">
        <span className="text-ui-muted uppercase tracking-wide text-[10px]">Terminal Output</span>
        {exitCode !== undefined && (
          <span className={`text-[10px] ${exitCode === 0 ? 'text-status-success' : 'text-status-warning'}`}>
            exit {exitCode}
          </span>
        )}
      </div>
      
      <div className="p-4 space-y-3 max-h-96 overflow-auto bg-surface-canvas/60">
        {stdout && (
          <div>
            <div className="text-[10px] uppercase tracking-wide text-ui-muted mb-1">stdout</div>
            <pre className="whitespace-pre-wrap break-words text-ui-primary">{stdout}</pre>
          </div>
        )}
        {stderr && (
          <div>
            <div className="text-[10px] uppercase tracking-wide text-ui-muted mb-1">stderr</div>
            <pre className="whitespace-pre-wrap break-words text-status-warning/80">{stderr}</pre>
          </div>
        )}
        {!stdout && !stderr && (
          <div className="text-ui-muted italic">No output</div>
        )}
      </div>
    </div>
  );
}

// File read result viewer
function FileReadView({ event }: { event: EventItem }) {
  const rawOutput = event.toolCall?.raw_output ?? event.content;
  const parsed = parseJsonMaybe(rawOutput);
  
  const filePath = typeof parsed?.path === 'string' ? parsed.path : undefined;
  const content = typeof parsed?.content === 'string' ? parsed.content : event.content || '';
  const startLine = typeof parsed?.start_line === 'number' ? parsed.start_line : undefined;
  const endLine = typeof parsed?.end_line === 'number' ? parsed.end_line : undefined;

  // Check if markdown file - render it
  const isMarkdown = filePath && isMarkdownFile(filePath);

  return (
    <div className="text-xs">
      {/* Header with file info */}
      <div className="flex items-center justify-between px-4 py-2 border-b border-surface-border/50 bg-surface-canvas/50">
        <span className="text-accent-primary truncate max-w-lg font-mono">
          {filePath || 'File Content'}
        </span>
        {startLine !== undefined && endLine !== undefined && (
          <span className="text-ui-muted text-[10px]">
            Lines {startLine}-{endLine}
          </span>
        )}
      </div>
      
      {/* Content with syntax highlighting */}
      <div className="p-4 bg-surface-canvas">
        {isMarkdown ? (
          <div className="prose prose-invert prose-sm max-w-none">
            <MessageContent content={content} />
          </div>
        ) : filePath ? (
          <HighlightedCode
            code={content || 'No content'}
            filePath={filePath}
            lineNumbers={true}
            startLine={startLine}
            maxHeight="24rem"
          />
        ) : (
          <pre className="max-h-96 overflow-auto whitespace-pre-wrap break-words text-ui-primary font-mono">
            {content || 'No content'}
          </pre>
        )}
      </div>
    </div>
  );
}

// Generic result viewer
function ResultView({ event }: { event: EventItem }) {
  const rawOutput = event.toolCall?.raw_output;
  const content = event.content;
  
  // Try to parse as JSON for better display
  const parsed = parseJsonMaybe(rawOutput) ?? parseJsonMaybe(content);
  
  if (parsed && typeof parsed === 'object') {
    return <JsonView data={parsed} />;
  }
  
  // Plain text
  return (
    <pre className="p-4 text-xs font-mono text-ui-secondary overflow-auto max-h-96 whitespace-pre-wrap break-words">
      {rawOutput ?? content ?? 'No result'}
    </pre>
  );
}

// Helper functions (duplicated from App.tsx for encapsulation - could be moved to utils)

function inferToolName(event: EventItem): string | undefined {
  const toolCallId = event.toolCall?.tool_call_id;
  if (typeof toolCallId === 'string' && toolCallId.includes(':')) {
    const name = toolCallId.split(':')[0];
    if (name) return name;
  }
  const desc = event.toolCall?.description;
  if (typeof desc === 'string') {
    const match = desc.match(/run\s+([a-z0-9_.:-]+)/i);
    if (match?.[1]) return match[1];
  }
  return event.toolCall?.kind;
}

function parseJsonMaybe(value: unknown): any | undefined {
  if (typeof value === 'string') {
    try {
      const parsed = JSON.parse(value);
      if (typeof parsed === 'string') {
        const trimmed = parsed.trim();
        if (
          (trimmed.startsWith('{') && trimmed.endsWith('}')) ||
          (trimmed.startsWith('[') && trimmed.endsWith(']'))
        ) {
          try {
            return JSON.parse(trimmed);
          } catch {
            return parsed;
          }
        }
      }
      return parsed;
    } catch {
      return undefined;
    }
  }
  if (typeof value === 'object' && value !== null) {
    return value;
  }
  return undefined;
}

type EditInput = {
  filePath?: string;
  oldString?: string;
  newString?: string;
};

function extractEditInput(rawInput: unknown): EditInput | undefined {
  if (!rawInput) return undefined;
  if (typeof rawInput === 'object' && rawInput !== null) {
    const direct = rawInput as EditInput & { arguments?: unknown };
    if (direct.oldString || direct.newString || direct.filePath) {
      return {
        filePath: direct.filePath,
        oldString: direct.oldString,
        newString: direct.newString,
      };
    }
    const args = direct.arguments;
    if (typeof args === 'string') {
      const parsed = parseJsonMaybe(args);
      if (parsed && typeof parsed === 'object') {
        const parsedEdit = parsed as EditInput;
        return {
          filePath: parsedEdit.filePath,
          oldString: parsedEdit.oldString,
          newString: parsedEdit.newString,
        };
      }
    }
    if (typeof args === 'object' && args !== null) {
      const parsedEdit = args as EditInput;
      return {
        filePath: parsedEdit.filePath,
        oldString: parsedEdit.oldString,
        newString: parsedEdit.newString,
      };
    }
  }
  return undefined;
}

function buildEditPatch(editInput: EditInput): string {
  const rawPath = editInput.filePath ?? 'file';
  const normalizedPath = rawPath.replace(/^\/+/, '') || 'file';
  const oldText = editInput.oldString ?? '';
  const newText = editInput.newString ?? '';
  const oldLines = oldText.split('\n').length;
  const newLines = newText.split('\n').length;
  const oldBlock = oldText
    .split('\n')
    .map((line) => `-${line}`)
    .join('\n');
  const newBlock = newText
    .split('\n')
    .map((line) => `+${line}`)
    .join('\n');
  return [
    `diff --git a/${normalizedPath} b/${normalizedPath}`,
    `--- a/${normalizedPath}`,
    `+++ b/${normalizedPath}`,
    `@@ -1,${oldLines} +1,${newLines} @@`,
    oldBlock,
    newBlock,
  ].join('\n');
}

function extractPatchValue(rawInput: unknown): string | undefined {
  if (!rawInput) return undefined;
  if (typeof rawInput === 'object' && rawInput !== null) {
    const direct = (rawInput as { patch?: unknown }).patch;
    if (typeof direct === 'string') return direct;
    const args = (rawInput as { arguments?: unknown }).arguments;
    if (typeof args === 'string') {
      const parsed = parseJsonMaybe(args);
      if (typeof parsed?.patch === 'string') return parsed.patch;
    }
    if (typeof args === 'object' && args !== null) {
      const argPatch = (args as { patch?: unknown }).patch;
      if (typeof argPatch === 'string') return argPatch;
    }
  }
  return undefined;
}
