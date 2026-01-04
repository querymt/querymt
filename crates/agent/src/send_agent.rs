/// The SendAgent trait defines a thread-safe interface for agent interaction.
///
/// Unlike the `agent_client_protocol::Agent` trait which uses `#[async_trait(?Send)]`,
/// this trait enforces `Send` bounds on all futures, allowing agents to be used in
/// multi-threaded contexts without blocking.
///
/// This trait is used internally for:
/// 1. The core `QueryMTAgent` implementation (which is `Send + Sync`)
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
use agent_client_protocol::{
    AuthenticateRequest, AuthenticateResponse, CancelNotification, Error, ExtNotification,
    ExtRequest, ExtResponse, ForkSessionRequest, ForkSessionResponse, InitializeRequest,
    InitializeResponse, ListSessionsRequest, ListSessionsResponse, LoadSessionRequest,
    LoadSessionResponse, NewSessionRequest, NewSessionResponse, PromptRequest, PromptResponse,
    ResumeSessionRequest, ResumeSessionResponse, SetSessionModelRequest, SetSessionModelResponse,
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

    /// Change the LLM model for a session.
    ///
    /// Updates the language model configuration for the specified session.
    async fn set_session_model(
        &self,
        req: SetSessionModelRequest,
    ) -> Result<SetSessionModelResponse, Error>;

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

/// ApcClientProxy wraps an ACP SDK Client and implements SendAgent.
///
/// This proxy enables delegation to agents (local or remote) via the ACP protocol
/// while maintaining the `Send + Sync` guarantees required for thread-safe operation.
///
/// ## Usage
///
/// ```ignore
/// use std::sync::Arc;
/// use agent_client_protocol::Client;
/// use crate::send_agent::ApcClientProxy;
///
/// let client: Arc<dyn Client + Send + Sync> = /* ... */;
/// let proxy = ApcClientProxy::new(client);
///
/// // Now you can use proxy as a SendAgent
/// let response = proxy.prompt(request).await?;
/// ```
pub struct ApcClientProxy {
    client: std::sync::Arc<dyn agent_client_protocol::Client + Send + Sync>,
}

impl ApcClientProxy {
    /// Create a new ApcClientProxy wrapping an ACP SDK Client.
    pub fn new(client: std::sync::Arc<dyn agent_client_protocol::Client + Send + Sync>) -> Self {
        Self { client }
    }

    /// Get a reference to the underlying client.
    pub fn client(&self) -> &std::sync::Arc<dyn agent_client_protocol::Client + Send + Sync> {
        &self.client
    }
}

/// Implement SendAgent for ApcClientProxy by forwarding to the underlying Client.
///
/// NOTE: This implementation is currently blocked because the `agent_client_protocol::Client`
/// trait is `#[async_trait(?Send)]`, which means its methods cannot be called from a `Send`
/// context. This is a fundamental design mismatch that needs to be resolved.
///
/// TODO: Either:
/// 1. Find a Send version of the Client trait in the agent_client_protocol crate
/// 2. Wrap calls in spawn_local if delegation must work with ?Send clients
/// 3. Rethink the delegation strategy
#[async_trait]
impl SendAgent for ApcClientProxy {
    async fn initialize(&self, _req: InitializeRequest) -> Result<InitializeResponse, Error> {
        // TEMPORARY: Return unimplemented error until we resolve the ?Send issue
        Err(Error::new(
            -32601,
            "ApcClientProxy not yet implemented - blocked on ?Send Client trait",
        ))
    }

    async fn authenticate(&self, _req: AuthenticateRequest) -> Result<AuthenticateResponse, Error> {
        Err(Error::new(
            -32601,
            "ApcClientProxy not yet implemented - blocked on ?Send Client trait",
        ))
    }

    async fn new_session(&self, _req: NewSessionRequest) -> Result<NewSessionResponse, Error> {
        Err(Error::new(
            -32601,
            "ApcClientProxy not yet implemented - blocked on ?Send Client trait",
        ))
    }

    async fn prompt(&self, _req: PromptRequest) -> Result<PromptResponse, Error> {
        Err(Error::new(
            -32601,
            "ApcClientProxy not yet implemented - blocked on ?Send Client trait",
        ))
    }

    async fn cancel(&self, _notif: CancelNotification) -> Result<(), Error> {
        Err(Error::new(
            -32601,
            "ApcClientProxy not yet implemented - blocked on ?Send Client trait",
        ))
    }

    async fn load_session(&self, _req: LoadSessionRequest) -> Result<LoadSessionResponse, Error> {
        Err(Error::new(
            -32601,
            "ApcClientProxy not yet implemented - blocked on ?Send Client trait",
        ))
    }

    async fn list_sessions(
        &self,
        _req: ListSessionsRequest,
    ) -> Result<ListSessionsResponse, Error> {
        Err(Error::new(
            -32601,
            "ApcClientProxy not yet implemented - blocked on ?Send Client trait",
        ))
    }

    async fn fork_session(&self, _req: ForkSessionRequest) -> Result<ForkSessionResponse, Error> {
        Err(Error::new(
            -32601,
            "ApcClientProxy not yet implemented - blocked on ?Send Client trait",
        ))
    }

    async fn resume_session(
        &self,
        _req: ResumeSessionRequest,
    ) -> Result<ResumeSessionResponse, Error> {
        Err(Error::new(
            -32601,
            "ApcClientProxy not yet implemented - blocked on ?Send Client trait",
        ))
    }

    async fn set_session_model(
        &self,
        _req: SetSessionModelRequest,
    ) -> Result<SetSessionModelResponse, Error> {
        Err(Error::new(
            -32601,
            "ApcClientProxy not yet implemented - blocked on ?Send Client trait",
        ))
    }

    async fn ext_method(&self, _req: ExtRequest) -> Result<ExtResponse, Error> {
        Err(Error::new(
            -32601,
            "ApcClientProxy not yet implemented - blocked on ?Send Client trait",
        ))
    }

    async fn ext_notification(&self, _notif: ExtNotification) -> Result<(), Error> {
        Err(Error::new(
            -32601,
            "ApcClientProxy not yet implemented - blocked on ?Send Client trait",
        ))
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// ApcAgentAdapter wraps a `SendAgent` to provide `agent_client_protocol::Agent` compliance.
///
/// This adapter exists at the protocol boundary, allowing a thread-safe `SendAgent`
/// implementation (like `QueryMTAgent`) to be exposed via the `!Send` ACP protocol interface.
///
/// ## Purpose
///
/// The `agent_client_protocol::Agent` trait uses `#[async_trait(?Send)]` because it's designed
/// to work with both multi-threaded and single-threaded runtimes. However, our core agent
/// implementation (`QueryMTAgent`) is fully `Send + Sync` to enable true parallelism.
///
/// This adapter bridges the gap by:
/// 1. Accepting a `Send + Sync` agent at construction
/// 2. Implementing the `!Send` protocol interface by forwarding calls
/// 3. Allowing the agent to be used in both contexts
///
/// ## Usage
///
/// ```ignore
/// use std::sync::Arc;
/// use crate::agent::QueryMTAgent;
/// use crate::send_agent::ApcAgentAdapter;
/// use agent_client_protocol::Agent;
///
/// let agent = Arc::new(QueryMTAgent::new(registry, store, config));
/// let adapter = ApcAgentAdapter::new(agent);
///
/// // Now you can use adapter where Agent trait is required
/// let response = adapter.initialize(request).await?;
/// ```
pub struct ApcAgentAdapter<T: SendAgent> {
    inner: std::sync::Arc<T>,
}

impl<T: SendAgent> ApcAgentAdapter<T> {
    /// Create a new adapter wrapping a `SendAgent` implementation.
    pub fn new(inner: std::sync::Arc<T>) -> Self {
        Self { inner }
    }

    /// Get a reference to the underlying agent.
    pub fn inner(&self) -> &std::sync::Arc<T> {
        &self.inner
    }
}

impl<T: SendAgent> Clone for ApcAgentAdapter<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

/// Implement the ACP protocol interface by forwarding to the SendAgent implementation.
///
/// This implementation uses `#[async_trait(?Send)]` as required by the protocol,
/// but internally calls the `Send` methods of the underlying `SendAgent`.
#[async_trait(?Send)]
impl<T: SendAgent> agent_client_protocol::Agent for ApcAgentAdapter<T> {
    async fn initialize(&self, req: InitializeRequest) -> Result<InitializeResponse, Error> {
        self.inner.initialize(req).await
    }

    async fn authenticate(&self, req: AuthenticateRequest) -> Result<AuthenticateResponse, Error> {
        self.inner.authenticate(req).await
    }

    async fn new_session(&self, req: NewSessionRequest) -> Result<NewSessionResponse, Error> {
        self.inner.new_session(req).await
    }

    async fn prompt(&self, req: PromptRequest) -> Result<PromptResponse, Error> {
        self.inner.prompt(req).await
    }

    async fn cancel(&self, notif: CancelNotification) -> Result<(), Error> {
        self.inner.cancel(notif).await
    }
}
