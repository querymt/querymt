import { memo, useState, useCallback, useEffect } from 'react';
import {
  Brain,
  Search,
  ChevronRight,
  ChevronLeft,
  Database,
  Tag,
  Clock,
  Layers,
} from 'lucide-react';
import type { KnowledgeEntryInfo, ConsolidationInfo } from '../types';

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function formatRelativeTime(iso: string | undefined | null): string {
  if (!iso) return '--';
  const date = new Date(iso);
  const now = Date.now();
  const diffMs = now - date.getTime();
  const absDiff = Math.abs(diffMs);

  if (absDiff < 60_000) return '<1m ago';
  if (absDiff < 3_600_000) {
    const mins = Math.round(absDiff / 60_000);
    return `${mins}m ago`;
  }
  if (absDiff < 86_400_000) {
    const hrs = Math.round(absDiff / 3_600_000);
    return `${hrs}h ago`;
  }
  const days = Math.round(absDiff / 86_400_000);
  return `${days}d ago`;
}

function importanceBadge(importance: number): { label: string; className: string } {
  if (importance >= 0.8) return { label: 'High', className: 'text-status-warning' };
  if (importance >= 0.5) return { label: 'Med', className: 'text-accent-primary' };
  return { label: 'Low', className: 'text-ui-muted' };
}

// ---------------------------------------------------------------------------
// Sub-components
// ---------------------------------------------------------------------------

interface KnowledgeEntryRowProps {
  entry: KnowledgeEntryInfo;
}

const KnowledgeEntryRow = memo(function KnowledgeEntryRow({ entry }: KnowledgeEntryRowProps) {
  const imp = importanceBadge(entry.importance);
  return (
    <div className="border border-ui-border rounded-md p-3 bg-bg-secondary">
      <div className="flex items-start justify-between gap-2">
        <p className="text-sm text-text-primary flex-1">{entry.summary}</p>
        <span className={`text-xs font-mono ${imp.className}`}>{imp.label}</span>
      </div>
      <div className="flex flex-wrap gap-1 mt-2">
        {entry.topics.map((t) => (
          <span key={t} className="inline-flex items-center gap-0.5 text-xs bg-bg-tertiary text-text-secondary rounded px-1.5 py-0.5">
            <Tag size={10} />
            {t}
          </span>
        ))}
        {entry.entities.map((e) => (
          <span key={e} className="inline-flex items-center gap-0.5 text-xs bg-accent-primary/10 text-accent-primary rounded px-1.5 py-0.5">
            {e}
          </span>
        ))}
      </div>
      <div className="flex items-center gap-3 mt-2 text-xs text-ui-muted">
        <span className="flex items-center gap-1"><Clock size={10} />{formatRelativeTime(entry.created_at)}</span>
        <span>src: {entry.source}</span>
        {entry.consolidated_at && <span className="text-status-success">consolidated</span>}
      </div>
    </div>
  );
});

interface ConsolidationRowProps {
  consolidation: ConsolidationInfo;
}

const ConsolidationRow = memo(function ConsolidationRow({ consolidation }: ConsolidationRowProps) {
  return (
    <div className="border border-accent-primary/30 rounded-md p-3 bg-accent-primary/5">
      <div className="flex items-center gap-2 mb-1">
        <Layers size={14} className="text-accent-primary" />
        <p className="text-sm font-medium text-text-primary">{consolidation.summary}</p>
      </div>
      <p className="text-xs text-text-secondary ml-5">{consolidation.insight}</p>
      <div className="flex items-center gap-3 mt-2 text-xs text-ui-muted ml-5">
        <span>{consolidation.source_count} source{consolidation.source_count !== 1 ? 's' : ''}</span>
        <span className="flex items-center gap-1"><Clock size={10} />{formatRelativeTime(consolidation.created_at)}</span>
      </div>
    </div>
  );
});

// ---------------------------------------------------------------------------
// Main panel
// ---------------------------------------------------------------------------

export interface KnowledgePanelProps {
  entries: KnowledgeEntryInfo[];
  consolidations: ConsolidationInfo[];
  stats: {
    totalEntries: number;
    unconsolidatedEntries: number;
    totalConsolidations: number;
    latestEntryAt: string | null;
    latestConsolidationAt: string | null;
  } | null;
  onQuery: (scope: string, question: string) => void;
  onList: (scope: string) => void;
  onStats: (scope: string) => void;
  defaultScope?: string;
}

export const KnowledgePanel = memo(function KnowledgePanel({
  entries,
  consolidations,
  stats,
  onQuery,
  onList,
  onStats,
  defaultScope = 'global',
}: KnowledgePanelProps) {
  const [collapsed, setCollapsed] = useState(true);
  const [scope, setScope] = useState(defaultScope);
  const [searchQuery, setSearchQuery] = useState('');

  // Fetch stats on mount / scope change
  useEffect(() => {
    if (!collapsed) {
      onStats(scope);
      onList(scope);
    }
  }, [collapsed, scope, onStats, onList]);

  const handleSearch = useCallback(() => {
    if (searchQuery.trim()) {
      onQuery(scope, searchQuery.trim());
    } else {
      onList(scope);
    }
  }, [scope, searchQuery, onQuery, onList]);

  const handleKeyDown = useCallback((e: React.KeyboardEvent) => {
    if (e.key === 'Enter') handleSearch();
  }, [handleSearch]);

  if (collapsed) {
    return (
      <button
        onClick={() => setCollapsed(false)}
        className="flex items-center gap-2 px-3 py-2 text-sm text-text-secondary hover:text-text-primary hover:bg-bg-secondary rounded-md transition-colors"
        title="Knowledge Store"
      >
        <Brain size={16} />
        <span>Knowledge</span>
        {stats && <span className="text-xs text-ui-muted">({stats.totalEntries})</span>}
        <ChevronRight size={14} />
      </button>
    );
  }

  return (
    <div className="border border-ui-border rounded-lg bg-bg-primary overflow-hidden">
      {/* Header */}
      <div className="flex items-center justify-between px-4 py-2 bg-bg-secondary border-b border-ui-border">
        <div className="flex items-center gap-2">
          <Brain size={16} className="text-accent-primary" />
          <span className="text-sm font-medium text-text-primary">Knowledge Store</span>
        </div>
        <button onClick={() => setCollapsed(true)} className="text-ui-muted hover:text-text-primary">
          <ChevronLeft size={16} />
        </button>
      </div>

      {/* Stats bar */}
      {stats && (
        <div className="flex items-center gap-4 px-4 py-2 text-xs text-text-secondary border-b border-ui-border bg-bg-tertiary">
          <span className="flex items-center gap-1"><Database size={12} />{stats.totalEntries} entries</span>
          <span>{stats.unconsolidatedEntries} unconsolidated</span>
          <span className="flex items-center gap-1"><Layers size={12} />{stats.totalConsolidations} consolidations</span>
          {stats.latestEntryAt && (
            <span className="flex items-center gap-1"><Clock size={12} />Last: {formatRelativeTime(stats.latestEntryAt)}</span>
          )}
        </div>
      )}

      {/* Scope selector + search */}
      <div className="px-4 py-2 flex items-center gap-2 border-b border-ui-border">
        <select
          value={scope}
          onChange={(e) => setScope(e.target.value)}
          className="text-xs bg-bg-secondary border border-ui-border rounded px-2 py-1 text-text-primary"
        >
          <option value="global">Global</option>
          <option value={defaultScope}>{defaultScope === 'global' ? 'Global' : `Session: ${defaultScope.slice(0, 8)}...`}</option>
        </select>
        <div className="flex-1 relative">
          <input
            type="text"
            value={searchQuery}
            onChange={(e) => setSearchQuery(e.target.value)}
            onKeyDown={handleKeyDown}
            placeholder="Search knowledge..."
            className="w-full text-xs bg-bg-secondary border border-ui-border rounded px-2 py-1 pr-7 text-text-primary placeholder-ui-muted"
          />
          <button
            onClick={handleSearch}
            className="absolute right-1 top-1/2 -translate-y-1/2 text-ui-muted hover:text-text-primary"
          >
            <Search size={12} />
          </button>
        </div>
      </div>

      {/* Results */}
      <div className="max-h-80 overflow-y-auto px-4 py-2 space-y-2">
        {consolidations.length > 0 && (
          <div className="space-y-2">
            <p className="text-xs font-medium text-text-secondary uppercase tracking-wider">Consolidations</p>
            {consolidations.map((c) => (
              <ConsolidationRow key={c.public_id} consolidation={c} />
            ))}
          </div>
        )}
        {entries.length > 0 && (
          <div className="space-y-2">
            <p className="text-xs font-medium text-text-secondary uppercase tracking-wider">Entries</p>
            {entries.map((e) => (
              <KnowledgeEntryRow key={e.public_id} entry={e} />
            ))}
          </div>
        )}
        {entries.length === 0 && consolidations.length === 0 && (
          <p className="text-xs text-ui-muted text-center py-4">No knowledge entries found.</p>
        )}
      </div>
    </div>
  );
});

export default KnowledgePanel;
