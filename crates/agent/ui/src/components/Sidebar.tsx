import { useState } from 'react';
import { X, CheckCircle, XCircle, AlertCircle, Plus, Loader, Clock } from 'lucide-react';
import type { RoutingMode, UiAgentInfo, SessionSummary } from '../types';

interface SidebarProps {
  isOpen: boolean;
  onClose: () => void;
  sessionId: string | null;
  connected: boolean;
  onNewSession: () => Promise<void>;
  sessionInfo?: {
    messageCount: number;
    createdAt: number;
  };
  agents: UiAgentInfo[];
  routingMode: RoutingMode;
  activeAgentId: string;
  onSetActiveAgent: (agentId: string) => void;
  onSetRoutingMode: (mode: RoutingMode) => void;
  sessionHistory: SessionSummary[];
  onLoadSession: (sessionId: string) => void;
}

export function Sidebar({
  isOpen,
  onClose,
  sessionId,
  connected,
  onNewSession,
  sessionInfo,
  agents,
  routingMode,
  activeAgentId,
  onSetActiveAgent,
  onSetRoutingMode,
  sessionHistory,
  onLoadSession,
}: SidebarProps) {
  const [isCreating, setIsCreating] = useState(false);

  const handleNewSession = async () => {
    setIsCreating(true);
    try {
      await onNewSession();
      onClose(); // Close sidebar after creating session
    } catch (err) {
      console.error('Failed to create session:', err);
    } finally {
      setIsCreating(false);
    }
  };

  return (
    <>
      {/* Backdrop overlay */}
      {isOpen && (
        <div 
          onClick={onClose}
          className="fixed inset-0 bg-black/50 backdrop-blur-sm z-40 md:hidden"
        />
      )}
      
      {/* Sidebar panel */}
      <aside
        className={`
          fixed top-0 left-0 h-full w-80 bg-cyber-surface border-r border-cyber-border
          transform transition-transform duration-300 ease-out z-50
          ${isOpen ? 'translate-x-0' : '-translate-x-full'}
          shadow-[0_0_20px_rgba(0,255,249,0.1)]
        `}
      >
        {/* Header */}
        <div className="flex items-center justify-between p-6 border-b border-cyber-border">
          <h2 className="text-xl font-bold neon-text-cyan">Session Control</h2>
          <button
            onClick={onClose}
            className="text-gray-400 hover:text-cyber-cyan transition-colors"
          >
            <X className="w-6 h-6" />
          </button>
        </div>
        
        {/* Content */}
        <div className="flex-1 overflow-y-auto">
          {/* Session History */}
          {sessionHistory.length > 0 && (
            <div className="border-b border-cyber-border">
              <div className="p-4">
                <div className="flex items-center gap-2 mb-3">
                  <Clock className="w-4 h-4 text-cyber-cyan" />
                  <span className="text-sm font-medium text-gray-300">Recent Sessions</span>
                </div>
                <div className="space-y-2 max-h-64 overflow-y-auto">
                  {sessionHistory.map((session) => (
                    <button
                      key={session.session_id}
                      onClick={() => {
                        onLoadSession(session.session_id);
                        onClose();
                      }}
                      disabled={!connected || session.session_id === sessionId}
                      className={`w-full p-2 rounded-lg border text-left text-xs transition-colors ${
                        session.session_id === sessionId
                          ? 'border-cyber-cyan bg-cyber-cyan/10 text-cyber-cyan cursor-default'
                          : 'border-cyber-border bg-cyber-bg/40 text-gray-300 hover:border-cyber-cyan/60'
                      } ${!connected ? 'opacity-50 cursor-not-allowed' : ''}`}
                    >
                      <div className="font-mono truncate">
                        {session.session_id.substring(0, 16)}...
                      </div>
                      {session.created_at && (
                        <div className="text-[10px] text-gray-500 mt-1">
                          {new Date(session.created_at).toLocaleString()}
                        </div>
                      )}
                    </button>
                  ))}
                </div>
              </div>
            </div>
          )}

        <div className="p-6 space-y-6">
          {/* Connection Status */}
          <div className="space-y-2">
            <h3 className="text-sm font-medium text-gray-400">Connection Status</h3>
            <div className="flex items-center gap-2">
              {connected ? (
                <>
                  <CheckCircle className="w-5 h-5 text-cyber-lime animate-glow-pulse" />
                  <span className="text-cyber-lime">Connected</span>
                </>
              ) : (
                <>
                  <XCircle className="w-5 h-5 text-cyber-orange" />
                  <span className="text-cyber-orange">Disconnected</span>
                </>
              )}
            </div>
          </div>

          {/* Current Session Info */}
          {sessionId ? (
            <div className="space-y-3 p-4 rounded-lg bg-cyber-bg/50 border border-cyber-border">
              <h3 className="text-sm font-medium text-gray-400">Active Session</h3>
              <div className="space-y-2">
                <div>
                  <p className="text-xs text-gray-500">Session ID</p>
                  <p className="text-sm font-mono text-cyber-cyan">{String(sessionId).substring(0, 16)}...</p>
                </div>
                {sessionInfo && (
                  <>
                    <div>
                      <p className="text-xs text-gray-500">Messages</p>
                      <p className="text-sm text-gray-300">{sessionInfo.messageCount}</p>
                    </div>
                    <div>
                      <p className="text-xs text-gray-500">Created</p>
                      <p className="text-sm text-gray-300">
                        {new Date(sessionInfo.createdAt).toLocaleTimeString()}
                      </p>
                    </div>
                  </>
                )}
              </div>
            </div>
          ) : (
            <div className="space-y-3 p-4 rounded-lg bg-cyber-bg/50 border border-cyber-border border-dashed">
              <div className="flex items-center gap-2 text-gray-400">
                <AlertCircle className="w-5 h-5" />
                <h3 className="text-sm font-medium">No Active Session</h3>
              </div>
              <p className="text-xs text-gray-500">
                Create a new session to start chatting with the agent.
              </p>
            </div>
          )}
          
          {/* New Session Button */}
          <button
            onClick={handleNewSession}
            disabled={!connected || isCreating}
            className="
              w-full px-6 py-3 rounded-lg font-medium
              bg-cyber-cyan/10 border-2 border-cyber-cyan
              text-cyber-cyan
              hover:bg-cyber-cyan/20 hover:shadow-neon-cyan
              disabled:opacity-50 disabled:cursor-not-allowed
              transition-all duration-200
              flex items-center justify-center gap-2
            "
          >
            {isCreating ? (
              <>
                <Loader className="w-5 h-5 animate-spin" />
                <span>Creating...</span>
              </>
            ) : (
              <>
                <Plus className="w-5 h-5" />
                <span>Create New Session</span>
              </>
            )}
          </button>

          {/* Routing Controls */}
          <div className="pt-6 border-t border-cyber-border space-y-4">
            <div>
              <h3 className="text-sm font-medium text-gray-400">Routing Mode</h3>
              <select
                value={routingMode}
                onChange={(event) => onSetRoutingMode(event.target.value as RoutingMode)}
                className="
                  mt-2 w-full px-3 py-2 bg-cyber-bg border border-cyber-border rounded-lg
                  text-sm text-gray-200 focus:outline-none focus:border-cyber-cyan
                "
                disabled={!connected}
              >
                <option value="single">Single (active agent)</option>
                <option value="broadcast">Broadcast (all agents)</option>
              </select>
            </div>
            <div>
              <h3 className="text-sm font-medium text-gray-400">Active Agent</h3>
              <div className="mt-2 space-y-2">
                {agents.length === 0 && (
                  <p className="text-xs text-gray-500">No agents available.</p>
                )}
                {agents.map((agent) => (
                  <button
                    key={agent.id}
                    type="button"
                    onClick={() => onSetActiveAgent(agent.id)}
                    disabled={!connected || routingMode !== 'single'}
                    className={`
                      w-full px-3 py-2 rounded-lg border text-left text-sm transition-colors
                      ${agent.id === activeAgentId
                        ? 'border-cyber-cyan text-cyber-cyan bg-cyber-cyan/10'
                        : 'border-cyber-border text-gray-300 bg-cyber-bg/40'}
                      ${routingMode !== 'single' ? 'opacity-60 cursor-not-allowed' : 'hover:border-cyber-cyan/60'}
                    `}
                  >
                    <div className="flex items-center justify-between">
                      <span className="font-medium">{agent.name}</span>
                      {agent.id === activeAgentId && (
                        <span className="text-[10px] font-mono text-cyber-lime">active</span>
                      )}
                    </div>
                    {agent.description && (
                      <p className="mt-1 text-xs text-gray-500">{agent.description}</p>
                    )}
                  </button>
                ))}
              </div>
            </div>
          </div>

            {/* Info Section */}
            <div className="pt-6 border-t border-cyber-border space-y-2">
              <h3 className="text-sm font-medium text-gray-400">About</h3>
              <p className="text-xs text-gray-500">
                QueryMT Agent Dashboard uses WebSocket for real-time communication with the backend.
              </p>
            </div>
          </div>
        </div>
      </aside>
    </>
  );
}
