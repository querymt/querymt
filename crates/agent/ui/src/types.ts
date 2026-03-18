// Import types from ACP SDK where possible
import type { 
  SessionNotification,
  SessionUpdate,
} from '@agentclientprotocol/sdk';

// Re-export SDK types for use in other components
export type { SessionNotification, SessionUpdate };

// ── Generated types (authoritative from Rust via typeshare) ──────────────────
// These types are generated from Rust source and must not be hand-edited here.
// Re-export everything from the generated file.
// Enums must use plain `export` (not `export type`) so their values are accessible at runtime.
export {
  StopType,
  RoutingMode,
  OAuthStatus,
  AuthMethod,
  Durability,
  AlternativeStatus,
  DecisionStatus,
  DelegationStatus,
  ForkOrigin,
  ForkPointType,
  ProgressKind,
  TaskKind,
  TaskStatus,
  EventOriginKind,
  OAuthFlowKindTs,
} from './generated/types';

export type {
  // Structs
  ExecutionMetrics,
  SessionLimits,
  AgentEvent,
  DurableEvent,
  EphemeralEvent,
  EventEnvelope,
  UndoStackFrame,
  StreamCursor,
  PluginUpdateResult,
  // Structs previously hand-defined, now generated from Rust
  UiAgentInfo,
  SessionSummary,
  SessionGroup,
  AuditView,
  AuthProviderEntry,
  ModelEntry,
  ProviderCapabilityEntry,
  RecentModelEntry,
  RemoteNodeInfo,
  // AuditView sub-types (generated with richer fields than prior manual definitions)
  Task,
  IntentSnapshot,
  Decision,
  ProgressEntry,
  Artifact,
  Delegation,
  // Discriminated unions
  AgentEventKind,
  UiPromptBlock,
  UiClientMessage,
  UiServerMessage,
  // P3: Previously `any` types, now generated with proper structure
  UsageInfo,
  ToolInfo,
  FunctionToolInfo,
  McpServerInfo,
  FileIndexEntry,
  RemoteSessionInfo,
  DuplicateWarning,
  FunctionLocation,
  SimilarMatch,
  ScheduleInfo,
  KnowledgeEntryInfo,
  ConsolidationInfo,
} from './generated/types';

// FileIndexEntry is now generated from Rust via typeshare — re-exported above.

// ── Elicitation tool types (unified MCP protocol) ────────────────────────────
export interface ElicitationData {
  elicitationId: string;
  sessionId: string;
  message: string;
  requestedSchema: any;  // JSON Schema object
  source: string;  // "builtin:question" or "mcp:server_name"
}

// ── UI-only view-model types ──────────────────────────────────────────────────
// These are not generated from Rust and represent UI-specific data models.

export interface EventItem {
  id: string;
  agentId?: string;
  sessionId?: string;
  seq?: number;
  type: 'user' | 'agent' | 'tool_call' | 'tool_result' | 'system';
  content: string;
  timestamp: number;
  isMessage?: boolean;  // True for actual user/assistant messages (not internal events)
  messageId?: string;  // Message UUID from the database (for undo/redo)
  toolCall?: {
    tool_call_id?: string;
    description?: string;
    kind?: string;
    status?: 'in_progress' | 'completed' | 'failed';
    raw_input?: any;
    content?: any[];
    raw_output?: any;
  };
  // Cost and token tracking from llm_request_end events
  usage?: {
    input_tokens: number;
    output_tokens: number;
  };
  costUsd?: number;
  cumulativeCostUsd?: number;
  // Current context size (input + output tokens) from backend
  contextTokens?: number;
  // Time tracking fields
  finishReason?: string;  // 'stop', 'tool_calls', etc. from llm_request_end
  delegationId?: string;  // For delegation_requested/completed/failed events
  delegationTargetAgentId?: string;
  delegationObjective?: string;
  delegationEventType?: 'requested' | 'completed' | 'failed';
  // Session fork tracking (from session_forked events)
  forkChildSessionId?: string;
  forkDelegationId?: string;
  // Context limit from provider_changed events
  contextLimit?: number;
  provider?: string;
  model?: string;
  configId?: number;  // LLM config ID from provider_changed events
  /** Mesh node that owns the provider (from provider_changed events). Absent for local providers. */
  providerNode?: string;
  // Execution metrics from llm_request_end events
  metrics?: import('./generated/types').ExecutionMetrics;
  // Middleware stopped event data
  stopType?: import('./generated/types').StopType;
  stopReason?: string;
  stopMetrics?: import('./generated/types').ExecutionMetrics;
  // Elicitation tool fields
  elicitationData?: ElicitationData;
  // Rate limit fields
  rateLimitMessage?: string;
  rateLimitWaitSecs?: number;
  rateLimitStartedAt?: number;  // Unix timestamp in seconds
  rateLimitAttempt?: number;
  rateLimitMaxAttempts?: number;
  rateLimitResume?: boolean;  // true for rate_limit_resume event
  // Compaction event fields
  compactionTokenEstimate?: number;   // From compaction_start: original context token count
  compactionSummary?: string;         // From compaction_end: the AI-generated summary
  compactionSummaryLen?: number;      // From compaction_end: summary length in chars
  // Thinking/reasoning content (present on final assistant_message_stored if model emitted reasoning)
  thinking?: string;
  // Streaming delta fields (set on assistant_content_delta and assistant_thinking_delta events)
  isStreamDelta?: boolean;    // True while this is a live streaming accumulator
  isThinkingDelta?: boolean;  // True if accumulating thinking deltas (no text yet)
  streamMessageId?: string;   // Links delta events to the final message_id for replacement
}

export interface RateLimitState {
  isRateLimited: boolean;
  message: string;
  waitSecs: number;
  startedAt: number;  // Unix timestamp in seconds
  attempt: number;
  maxAttempts: number;
  remainingSecs: number;  // Updated by timer in UI
}

// Event filters
export interface EventFilters {
  types: Set<EventItem['type']>;
  agents: Set<string>;
  tools: Set<string>;
  searchQuery: string;
}

// Per-agent statistics
export interface AgentStats {
  agentId: string;
  messageCount: number;
  toolCallCount: number;
  toolResultCount: number;
  toolBreakdown: Record<string, number>;
  // Cost tracking
  costUsd: number;
  // Context tracking - current context size from last LLM request
  currentContextTokens: number;  // Current context size (input + output) from backend
  maxContextTokens?: number;  // Model's context limit from provider_changed event
  // Execution metrics from backend
  steps: number;  // LLM calls
  turns: number;  // User/assistant exchanges
}

// Session-level statistics
export interface SessionStats {
  totalCostUsd: number;
  /** True when at least one event carried a non-null costUsd value.
   *  False for OAuth sessions where the backend omits cost data. */
  hasCostData: boolean;
  totalMessages: number;
  totalToolCalls: number;
  startTimestamp?: number;     // First event timestamp (for live timer calculation)
  // Execution metrics from backend
  totalSteps: number;  // LLM calls
  totalTurns: number;  // User/assistant exchanges
  // Session limits (if configured)
  limits?: import('./generated/types').SessionLimits;
}

// Combined statistics result
export interface CalculatedStats {
  session: SessionStats;
  perAgent: AgentStats[];
}

// TodoItem - agent's working task list (from todowrite tool)
export interface TodoItem {
  id: string;
  content: string;
  status: 'pending' | 'in_progress' | 'completed' | 'cancelled';
  priority: 'high' | 'medium' | 'low';
}

// Extended event row with display metadata (used in App.tsx)
export interface EventRow extends EventItem {
  depth: number;
  parentId?: string;
  toolName?: string;
  mergedResult?: EventItem; // For merged tool_call + tool_result
  isDelegateToolCall?: boolean; // True if this is a delegate tool call
  delegationGroupId?: string; // ID of the delegation group this belongs to
}

// Delegation group for accordion display
export interface DelegationGroupInfo {
  id: string;
  delegateToolCallId: string;
  delegateEvent: EventRow;
  delegationId?: string;
  targetAgentId?: string;
  objective?: string;
  agentId?: string;
  childSessionId?: string;
  events: EventRow[];
  status: 'in_progress' | 'completed' | 'failed';
  startTime: number;
  endTime?: number;
}

// Compaction data attached to a turn
export interface TurnCompaction {
  tokenEstimate: number;  // Original context token count before compaction
  summary: string;        // The AI-generated compaction summary
  summaryLen: number;     // Summary length in chars
  timestamp: number;      // When compaction completed
}

// Turn-based grouping for conversation display
export interface Turn {
  id: string;
  userMessage?: EventRow; // User prompt that started this turn (if any)
  agentMessages: EventRow[]; // Agent responses/thinking
  toolCalls: EventRow[]; // All tool calls in this turn
  delegations: DelegationGroupInfo[]; // Sub-agent delegations
  agentId?: string; // Primary agent for this turn
  startTime: number;
  endTime?: number;
  isActive: boolean; // Currently in progress
  // Model info for this turn (from most recent provider_changed before/during turn)
  modelLabel?: string; // "provider / model" format
  modelConfigId?: number; // LLM config ID for fetching params
  /** Compaction that occurred after this turn (between this turn and the next) */
  compaction?: TurnCompaction;
}

export interface PluginUpdateStatus {
  plugin_name: string;
  image_reference: string;
  phase: string;
  bytes_downloaded: number;
  bytes_total?: number | null;
  percent?: number | null;
  message?: string | null;
}

export interface ModelDownloadStatus {
  provider: string;
  model_id: string;
  status: 'queued' | 'downloading' | 'completed' | 'failed' | string;
  bytes_downloaded: number;
  bytes_total?: number | null;
  percent?: number | null;
  speed_bps?: number | null;
  eta_seconds?: number | null;
  message?: string | null;
}

// OAuthFlowKind is now generated as OAuthFlowKindTs in generated/types.ts (re-exported above).
// Alias for backward compatibility with existing code that uses the old name.
export type OAuthFlowKind = 'redirect_code' | 'device_poll';

export interface OAuthFlowState {
  flow_id: string;
  provider: string;
  authorization_url: string;
  flow_kind: OAuthFlowKind;
}

export interface OAuthResultState {
  provider: string;
  success: boolean;
  message: string;
}

// Cached LLM config details for model config popover
export interface LlmConfigDetails {
  configId: number;
  provider: string;
  model: string;
  params?: Record<string, unknown> | null;
}

// RemoteSessionInfo is now generated from Rust via typeshare — re-exported above.
