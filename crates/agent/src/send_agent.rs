/// The SendAgent trait defines a thread-safe interface for agent interaction.
///
/// Unlike the `agent_client_protocol::Agent` trait which uses `#[async_trait(?Send)]`,
/// this trait enforces `Send` bounds on all futures, allowing agents to be used in
/// multi-threaded contexts without blocking.
///
/// This trait is used internally for:
/// 1. The `AgentHandle` facade (which is `Send + Sync`)
/// 2. Proxies that wrap ACP SDK Clients for delegation
/// 3. The agent registry to store heterogeneous agents
///
/// ## Full Protocol Lifecycle (Option A)
///
/// This trait mirrors the complete `agent_client_protocol::Agent` interface,
/// supporting full protocol lifecycle for delegation:
/// - `initialize`: Protocol handshake and capability negotiation
/// - `authenticate`: Authentication flow
/// - `new_session`: Session creation with CWD and MCP servers
/// - `prompt`: The main interaction method
/// - `cancel`: Cancellation support
use agent_client_protocol::schema::{
    AuthenticateRequest, AuthenticateResponse, CancelNotification, CloseSessionRequest,
    CloseSessionResponse, Error, ExtNotification, ExtRequest, ExtResponse, ForkSessionRequest,
    ForkSessionResponse, InitializeRequest, InitializeResponse, ListSessionsRequest,
    ListSessionsResponse, LoadSessionRequest, LoadSessionResponse, NewSessionRequest,
    NewSessionResponse, PromptRequest, PromptResponse, ResumeSessionRequest, ResumeSessionResponse,
    SetSessionConfigOptionRequest, SetSessionConfigOptionResponse, SetSessionModeRequest,
    SetSessionModeResponse, SetSessionModelRequest, SetSessionModelResponse,
};
use async_trait::async_trait;
use std::any::Any;

/// A thread-safe agent interface that returns `Send` futures.
///
/// This trait mirrors `agent_client_protocol::Agent` but enforces `Send` bounds
/// on all async methods to enable true parallelism across multiple sessions.
#[async_trait]
pub trait SendAgent: Send + Sync + Any {
    /// Initialize the agent protocol.
    ///
    /// This is the first step in the protocol lifecycle, where the client and agent
    /// negotiate protocol version, exchange capabilities, and determine authentication
    /// requirements.
    async fn initialize(&self, req: InitializeRequest) -> Result<InitializeResponse, Error>;

    /// Authenticate with the agent.
    ///
    /// If the agent requires authentication (indicated in `initialize` response),
    /// this method must be called before creating sessions.
    async fn authenticate(&self, req: AuthenticateRequest) -> Result<AuthenticateResponse, Error>;

    /// Create a new session.
    ///
    /// Sessions are isolated contexts for agent interaction, each with their own:
    /// - Working directory (CWD)
    /// - MCP server connections
    /// - Message history
    /// - Permission state
    async fn new_session(&self, req: NewSessionRequest) -> Result<NewSessionResponse, Error>;

    /// Execute a prompt within a session.
    ///
    /// This is the core method for agent interaction. The agent processes the prompt,
    /// potentially calling tools, and returns a response when the turn is complete.
    async fn prompt(&self, req: PromptRequest) -> Result<PromptResponse, Error>;

    /// Cancel an ongoing prompt.
    ///
    /// Requests cancellation of a running prompt in the specified session.
    async fn cancel(&self, notif: CancelNotification) -> Result<(), Error>;

    /// Load an existing session with full history streaming.
    ///
    /// This method loads a previously created session and streams its complete
    /// message history to the client for reconstruction.
    async fn load_session(&self, req: LoadSessionRequest) -> Result<LoadSessionResponse, Error>;

    /// List all available sessions.
    ///
    /// Returns metadata about all sessions, optionally filtered by working directory.
    /// Supports pagination via cursor-based navigation.
    async fn list_sessions(&self, req: ListSessionsRequest) -> Result<ListSessionsResponse, Error>;

    /// Fork an existing session at a specific point.
    ///
    /// Creates a new session that branches off from the parent session,
    /// preserving history up to the fork point.
    async fn fork_session(&self, req: ForkSessionRequest) -> Result<ForkSessionResponse, Error>;

    /// Resume an existing session without history replay.
    ///
    /// Similar to load_session but skips streaming the message history,
    /// useful for reconnecting to an active session.
    async fn resume_session(
        &self,
        req: ResumeSessionRequest,
    ) -> Result<ResumeSessionResponse, Error>;

    /// Close an active session and release associated runtime resources.
    async fn close_session(&self, req: CloseSessionRequest) -> Result<CloseSessionResponse, Error>;

    /// Change the LLM model for a session.
    ///
    /// Updates the language model configuration for the specified session.
    async fn set_session_model(
        &self,
        req: SetSessionModelRequest,
    ) -> Result<SetSessionModelResponse, Error>;

    /// Set the current mode for a session.
    ///
    /// Allows switching between different agent modes (e.g., "build", "plan", "review")
    /// that affect system prompts, tool availability, and permission behaviors.
    async fn set_session_mode(
        &self,
        _req: SetSessionModeRequest,
    ) -> Result<SetSessionModeResponse, Error> {
        Err(Error::method_not_found())
    }

    /// Set a configuration option for a session.
    ///
    /// Configuration options allow agents to expose arbitrary selectors (like mode,
    /// reasoning effort, etc.) that clients can display and modify. The response
    /// returns the full list of configuration options with their current values,
    /// as changing one option may affect others.
    async fn set_session_config_option(
        &self,
        _req: SetSessionConfigOptionRequest,
    ) -> Result<SetSessionConfigOptionResponse, Error> {
        Err(Error::method_not_found())
    }

    /// Handle extension method calls.
    ///
    /// Provides a mechanism for protocol extensions beyond the core ACP specification.
    async fn ext_method(&self, req: ExtRequest) -> Result<ExtResponse, Error>;

    /// Handle extension notifications.
    ///
    /// Receives extension notifications that don't require a response.
    async fn ext_notification(&self, notif: ExtNotification) -> Result<(), Error>;

    /// Expose dynamic type for downcasting.
    fn as_any(&self) -> &dyn Any;
}
