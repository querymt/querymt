import { useEffect, useMemo, useRef, useState } from 'react';
import { AlertTriangle, ChevronDown, ChevronRight, Copy, Trash2 } from 'lucide-react';
import type { EventItem } from '../types';

const MAX_ENTRIES = 200;

function formatTimestamp(timestamp: number) {
  return new Date(timestamp).toLocaleTimeString();
}

function buildCopyPayload(events: EventItem[]) {
  return events
    .map((event) => {
      const time = formatTimestamp(event.timestamp);
      return `[${time}] ${event.content}`;
    })
    .join('\n');
}

async function copyToClipboard(value: string) {
  if (!value) return;
  if (navigator.clipboard?.writeText) {
    await navigator.clipboard.writeText(value);
    return;
  }

  const textarea = document.createElement('textarea');
  textarea.value = value;
  textarea.setAttribute('readonly', 'true');
  textarea.style.position = 'absolute';
  textarea.style.left = '-9999px';
  document.body.appendChild(textarea);
  textarea.select();
  document.execCommand('copy');
  document.body.removeChild(textarea);
}

interface SystemLogProps {
  events: EventItem[];
  onClear: () => void;
}

export function SystemLog({ events, onClear }: SystemLogProps) {
  const [isCollapsed, setIsCollapsed] = useState(false);
  const [copiedId, setCopiedId] = useState<string | null>(null);
  const [copiedAll, setCopiedAll] = useState(false);
  const [justOpened, setJustOpened] = useState(false);
  const scrollRef = useRef<HTMLDivElement>(null);
  const prevCountRef = useRef(events.length);

  const displayEvents = useMemo(() => {
    return events.slice(-MAX_ENTRIES);
  }, [events]);

  useEffect(() => {
    let timeout: number | undefined;
    if (events.length > prevCountRef.current) {
      setIsCollapsed(false);
      setJustOpened(true);
      timeout = window.setTimeout(() => setJustOpened(false), 1200);
    }
    prevCountRef.current = events.length;
    return () => {
      if (timeout) {
        window.clearTimeout(timeout);
      }
    };
  }, [events.length]);

  useEffect(() => {
    if (!isCollapsed && scrollRef.current) {
      scrollRef.current.scrollTop = scrollRef.current.scrollHeight;
    }
  }, [displayEvents, isCollapsed]);

  const handleCopyEntry = async (eventId: string, content: string) => {
    await copyToClipboard(content);
    setCopiedId(eventId);
    window.setTimeout(() => setCopiedId(null), 1200);
  };

  const handleCopyAll = async () => {
    await copyToClipboard(buildCopyPayload(displayEvents));
    setCopiedAll(true);
    window.setTimeout(() => setCopiedAll(false), 1200);
  };

  return (
    <div className="w-full px-6 pb-2">
      <div
        className={`mx-auto w-full max-w-[960px] rounded-2xl border border-cyber-orange/50 bg-cyber-surface/95 shadow-lg shadow-cyber-orange/25 transition-all duration-300 ${
          justOpened ? 'animate-fade-in-up' : ''
        }`}
      >
        <div className="flex items-center justify-between px-4 py-3">
          <div className="flex items-center gap-3">
            <div className="flex h-9 w-9 items-center justify-center rounded-full border border-cyber-orange/40 bg-cyber-orange/10">
              <AlertTriangle className="h-4 w-4 text-cyber-orange" />
            </div>
            <div>
              <p className="text-sm font-semibold text-gray-100">System Log</p>
              <p className="text-xs text-gray-400">{events.length} error{events.length === 1 ? '' : 's'} recorded</p>
            </div>
          </div>
          <div className="flex items-center gap-2">
            <button
              type="button"
              onClick={handleCopyAll}
              className="flex items-center gap-1 rounded-md border border-cyber-orange/40 px-2 py-1 text-xs text-cyber-orange transition-colors hover:bg-cyber-orange/10"
              title="Copy all errors"
            >
              <Copy className="h-3 w-3" />
              {copiedAll ? 'Copied' : 'Copy all'}
            </button>
            <button
              type="button"
              onClick={onClear}
              className="flex items-center gap-1 rounded-md border border-cyber-border/60 px-2 py-1 text-xs text-gray-300 transition-colors hover:bg-cyber-bg/60"
              title="Clear and hide"
            >
              <Trash2 className="h-3 w-3" />
              Clear
            </button>
            <button
              type="button"
              onClick={() => setIsCollapsed((prev) => !prev)}
              className="rounded-md border border-cyber-border/60 p-1 text-gray-300 transition-colors hover:bg-cyber-bg/60"
              title={isCollapsed ? 'Expand' : 'Collapse'}
            >
              {isCollapsed ? (
                <ChevronRight className="h-4 w-4" />
              ) : (
                <ChevronDown className="h-4 w-4" />
              )}
            </button>
          </div>
        </div>

        {!isCollapsed && (
          <div className="border-t border-cyber-border/60 bg-cyber-bg/55">
            <div ref={scrollRef} className="max-h-56 overflow-y-auto px-4 py-3">
              <div className="space-y-2">
                {displayEvents.map((event) => (
                  <div
                    key={event.id}
                    className="rounded-lg border border-cyber-orange/20 bg-cyber-surface/50 px-3 py-2 text-sm text-gray-100 shadow-[0_0_12px_rgba(255,107,53,0.15)]"
                  >
                    <div className="flex items-start justify-between gap-3">
                      <div className="font-mono text-[11px] uppercase tracking-wider text-cyber-orange">
                        {formatTimestamp(event.timestamp)}
                      </div>
                      <button
                        type="button"
                        onClick={() => handleCopyEntry(event.id, event.content)}
                        className="flex items-center gap-1 rounded border border-cyber-orange/30 px-2 py-1 text-[11px] text-cyber-orange transition-colors hover:bg-cyber-orange/10"
                        title="Copy message"
                      >
                        <Copy className="h-3 w-3" />
                        {copiedId === event.id ? 'Copied' : 'Copy'}
                      </button>
                    </div>
                    <div className="mt-2 whitespace-pre-wrap font-mono text-xs text-gray-200">
                      {event.content}
                    </div>
                  </div>
                ))}
              </div>
            </div>
          </div>
        )}
      </div>
    </div>
  );
}
