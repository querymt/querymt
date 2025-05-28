//! Server module for exposing LLM functionality via REST API
//!
//! Provides a REST API server that exposes LLM functionality through standardized endpoints.
//! Supports authentication, CORS, and handles chat completion requests.

mod handlers;
mod types;

use axum::Router;
use std::sync::Arc;
use tower_http::cors::CorsLayer;

use crate::chain::LLMRegistry;
use handlers::handle_chat;

pub use types::{ChatRequest, ChatResponse, Message};

/// Main server struct that manages LLM registry and authentication
pub struct Server {
    /// Registry containing LLM backends
    llms: Arc<LLMRegistry>,
    /// Optional authentication key for API requests
    pub auth_key: Option<String>,
}

/// Internal server state shared between request handlers
#[derive(Clone)]
struct ServerState {
    /// Shared reference to LLM registry
    llms: Arc<LLMRegistry>,
    /// Optional authentication key
    auth_key: Option<String>,
}

impl Server {
    /// Creates a new server instance with the given LLM registry
    ///
    /// # Arguments
    /// * `llms` - Registry containing LLM backends to expose via API
    pub fn new(llms: LLMRegistry) -> Self {
        Self {
            llms: Arc::new(llms),
            auth_key: None,
        }
    }

    /// Starts the server and listens for requests on the specified address
    ///
    /// # Arguments
    /// * `addr` - Address to bind to (e.g. "127.0.0.1:3000")
    ///
    /// # Returns
    /// * `Ok(())` if server starts successfully
    /// * `Err(LLMError)` if server fails to start
    pub async fn run(self, addr: &str) -> Result<(), crate::error::LLMError> {
        let app = Router::new()
            .route("/v1/chat/completions", axum::routing::post(handle_chat))
            .layer(CorsLayer::permissive())
            .with_state(ServerState {
                llms: self.llms,
                auth_key: self.auth_key,
            });

        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .map_err(|e| crate::error::LLMError::InvalidRequest(e.to_string()))?;

        axum::serve(listener, app)
            .await
            .map_err(|e| crate::error::LLMError::InvalidRequest(e.to_string()))?;

        Ok(())
    }

    /// Sets the authentication key required for API requests
    ///
    /// # Arguments
    /// * `key` - API key that clients must provide in Authorization header
    pub fn with_auth_key(mut self, key: impl Into<String>) -> Self {
        self.auth_key = Some(key.into());
        self
    }
}
