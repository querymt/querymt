// Import types from ACP SDK where possible
import type { 
  SessionNotification,
  SessionUpdate,
} from '@agentclientprotocol/sdk';

// Re-export SDK types for use in other components
export type { SessionNotification, SessionUpdate };

// File index types for @ mentions
export interface FileIndexEntry {
  path: string;
  is_dir: boolean;
}

// Custom UI-specific types
export interface EventItem {
  id: string;
  agentId?: string;
  type: 'user' | 'agent' | 'tool_call' | 'tool_result';
  content: string;
  timestamp: number;
  isMessage?: boolean;  // True for actual user/assistant messages (not internal events)
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
  // Time tracking fields
  finishReason?: string;  // 'stop', 'tool_calls', etc. from llm_request_end
  delegationId?: string;  // For delegation_requested/completed events
  // Context limit from provider_changed events
  contextLimit?: number;
}

export type RoutingMode = 'single' | 'broadcast';

export interface UiAgentInfo {
  id: string;
  name: string;
  description: string;
  capabilities: string[];
}

// Session history
export interface SessionSummary {
  session_id: string;
  name?: string;
  created_at?: string;
  updated_at?: string;
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
  // Cost and token tracking
  inputTokens: number;
  outputTokens: number;
  costUsd: number;
  // Time tracking
  activeTimeMs: number;  // Accumulated active work time
  // Context tracking
  maxContextTokens?: number;  // Model's context limit from provider_changed event
}

// Session-level statistics
export interface SessionStats {
  totalInputTokens: number;
  totalOutputTokens: number;
  totalCostUsd: number;
  totalMessages: number;
  totalToolCalls: number;
  // Time tracking
  totalElapsedMs: number;      // Total wall-clock time from first to last event
  startTimestamp?: number;     // First event timestamp (for live timer calculation)
}

// Combined statistics result
export interface CalculatedStats {
  session: SessionStats;
  perAgent: AgentStats[];
}

// AgentEvent type (matches Rust AgentEvent)
export interface AgentEvent {
  seq: number;
  timestamp: number;
  session_id: string;
  kind: AgentEventKind;
}

// AgentEventKind union type
export type AgentEventKind =
  | { type: 'prompt_received'; content: string }
  | { type: 'assistant_message_stored'; content: string }
  | { type: 'tool_call_start'; tool_call_id: string; tool_name: string; arguments?: string }
  | { type: 'tool_call_end'; tool_call_id: string; tool_name: string; result?: string; is_error?: boolean }
  | { type: 'error'; message: string }
  | { type: string; [key: string]: unknown };

// Full AuditView matching Rust struct (for session loading)
export interface AuditView {
  session_id: string;
  events: AgentEvent[];
  tasks: Task[];
  intent_snapshots: IntentSnapshot[];
  decisions: Decision[];
  progress_entries: ProgressEntry[];
  artifacts: Artifact[];
  delegations: Delegation[];
  generated_at: string;  // RFC3339
}

// Supporting domain types for AuditView
export interface Task {
  public_id: string;
  session_id: string;
  status: string;
  expected_deliverable?: string;
  created_at: string;
}

export interface IntentSnapshot {
  public_id: string;
  session_id: string;
  summary: string;
  created_at: string;
}

export interface Decision {
  public_id: string;
  session_id: string;
  description: string;
  status: string;
  created_at: string;
}

export interface ProgressEntry {
  public_id: string;
  session_id: string;
  kind: string;
  content: string;
  created_at: string;
}

export interface Artifact {
  public_id: string;
  session_id: string;
  kind: string;
  summary?: string;
  created_at: string;
}

export interface Delegation {
  public_id: string;
  session_id: string;
  target_agent_id: string;
  objective: string;
  status: string;
  created_at: string;
}

export type UiServerMessage =
  | {
      type: 'state';
      routing_mode: RoutingMode;
      active_agent_id: string;
      active_session_id?: string | null;
      agents: UiAgentInfo[];
    }
  | {
      type: 'session_created';
      agent_id: string;
      session_id: string;
    }
  | {
      type: 'event';
      agent_id: string;
      event: any;
    }
  | {
      type: 'error';
      message: string;
    }
  | { type: 'session_list'; sessions: SessionSummary[] }
  | { type: 'session_loaded'; session_id: string; audit: AuditView }
  | {
      type: 'workspace_index_status';
      session_id: string;
      status: 'building' | 'ready' | 'error';
      message?: string | null;
    }
  | { type: 'file_index'; files: FileIndexEntry[]; generated_at: number };

export type UiClientMessage =
  | { type: 'init' }
  | { type: 'set_active_agent'; agent_id: string }
  | { type: 'set_routing_mode'; mode: RoutingMode }
  | { type: 'new_session'; cwd?: string | null }
  | { type: 'prompt'; text: string }
  | { type: 'list_sessions' }
  | { type: 'load_session'; session_id: string }
  | { type: 'get_file_index' };
