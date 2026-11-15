//! ACP: the runtime's transports and protocol plus the WebSocket server.

pub use querymt_agent_runtime::acp::*;

pub mod websocket;
pub use websocket::serve_websocket;

#[cfg(test)]
pub(crate) mod websocket_tests;
