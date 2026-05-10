#ifndef QUERYMT_MOBILE_FFI_H
#define QUERYMT_MOBILE_FFI_H

#include <stdint.h>
#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

// ============================================================================
// Error Codes
// ============================================================================

#define QMT_MOBILE_OK                   0
#define QMT_MOBILE_INVALID_ARGUMENT     1
#define QMT_MOBILE_NOT_FOUND            2
#define QMT_MOBILE_RUNTIME_ERROR        3
#define QMT_MOBILE_UNSUPPORTED          4
#define QMT_MOBILE_ALREADY_EXISTS       5
#define QMT_MOBILE_BUSY                 6
#define QMT_MOBILE_INVALID_STATE        7

// ============================================================================
// Lifecycle
// ============================================================================

/// Android-only: initialize rustls-platform-verifier with JNIEnv* and Context.
/// Call before qmt_mobile_init_agent or any other network/TLS work.
int32_t qmt_mobile_android_init(void *env, void *context);

/// Initialize the agent runtime. Call once per agent instance.
/// config_json: JSON representation of the agent configuration.
/// out_agent: receives an opaque handle.
/// Returns QMT_MOBILE_OK on success.
int32_t qmt_mobile_init_agent(const char *config_json, uint64_t *out_agent);

/// Shut down the agent and release all resources.
/// Returns QMT_MOBILE_BUSY if the agent has active FFI calls.
int32_t qmt_mobile_shutdown(uint64_t agent_handle);

/// Notify the runtime of app lifecycle transitions.
/// backgrounded: 1 for background, 0 for foreground.
int32_t qmt_mobile_set_backgrounded(int32_t backgrounded);

// ============================================================================
// Sessions
// ============================================================================

/// Create a new local session.
/// options_json: optional JSON with "cwd", "provider", "model" overrides.
int32_t qmt_mobile_create_session(
    uint64_t agent_handle,
    const char *options_json,
    uint64_t *out_session
);

/// Create a new local session and return both the FFI handle and the real
/// session ID.  out_session_id must be freed with qmt_mobile_free_string.
int32_t qmt_mobile_create_session_with_id(
    uint64_t agent_handle,
    const char *options_json,
    uint64_t *out_session,
    char **out_session_id
);

/// Load an existing local session from persistent storage.
int32_t qmt_mobile_load_session(
    uint64_t agent_handle,
    const char *session_id,
    uint64_t *out_session
);

/// List persisted local sessions.
/// out_json: receives a JSON array of session summaries. Must be freed with
///           qmt_mobile_free_string.
int32_t qmt_mobile_list_sessions(
    uint64_t agent_handle,
    char **out_json
);

/// Delete a local session and all associated FFI session handles.
int32_t qmt_mobile_delete_session(
    uint64_t agent_handle,
    const char *session_id
);

// ============================================================================
// Remote Mesh
// ============================================================================

/// List local and reachable remote mesh nodes.
/// out_json: receives JSON. Must be freed with qmt_mobile_free_string.
int32_t qmt_mobile_list_nodes(
    uint64_t agent_handle,
    char **out_json
);

/// Create a session on a specific node.
/// node_id: NULL, empty, or local node ID creates a local session.
int32_t qmt_mobile_create_session_on_node(
    uint64_t agent_handle,
    const char *node_id,
    const char *options_json,
    uint64_t *out_session
);

/// Create a session on a specific node and return both the FFI handle and the
/// real session ID.  out_session_id must be freed with qmt_mobile_free_string.
int32_t qmt_mobile_create_session_on_node_with_id(
    uint64_t agent_handle,
    const char *node_id,
    const char *options_json,
    uint64_t *out_session,
    char **out_session_id
);

/// List sessions available on a remote node.
int32_t qmt_mobile_list_remote_sessions(
    uint64_t agent_handle,
    const char *node_id,
    char **out_json
);

/// Attach/resume an existing remote session.
int32_t qmt_mobile_attach_remote_session(
    uint64_t agent_handle,
    const char *node_id,
    const char *session_id,
    uint64_t *out_session
);

/// Create an invite token.
/// options_json: optional JSON with mesh_name, expires_at, max_uses, can_invite.
int32_t qmt_mobile_create_invite(
    uint64_t agent_handle,
    const char *options_json,
    char **out_json
);

/// Join a mesh from an invite token after agent initialization.
int32_t qmt_mobile_join_mesh(
    uint64_t agent_handle,
    const char *invite_token,
    char **out_json
);

/// Return local mesh state for UI/debugging.
int32_t qmt_mobile_mesh_status(
    uint64_t agent_handle,
    char **out_json
);

// ============================================================================
// Prompt & Events
// ============================================================================

/// Send a user prompt to an active session.
/// content_json: ACP ContentBlock array JSON or plain text.
/// request_id: optional client-generated correlation ID.
int32_t qmt_mobile_prompt(
    uint64_t agent_handle,
    uint64_t session_handle,
    const char *content_json,
    const char *request_id
);

/// Cancel active execution in a session.
int32_t qmt_mobile_cancel(
    uint64_t agent_handle,
    uint64_t session_handle
);

/// Get persisted session history.
/// out_json: receives JSON. Must be freed with qmt_mobile_free_string.
int32_t qmt_mobile_get_session_history(
    uint64_t agent_handle,
    const char *session_id,
    char **out_json
);

/// Get durable agent events for a session.
/// out_json: receives JSON. Must be freed with qmt_mobile_free_string.
int32_t qmt_mobile_get_session_events(
    uint64_t agent_handle,
    const char *session_id,
    char **out_json
);

/// Get the full durable event stream for a session from the attached session
/// actor (works for both local and remote sessions).
int32_t qmt_mobile_get_remote_session_events(
    uint64_t agent_handle,
    const char *session_id,
    char **out_json
);

// ============================================================================
// Models & Providers
// ============================================================================

/// List available local and mesh-routable models.
/// out_json: receives JSON. traceparent: optional W3C traceparent.
int32_t qmt_mobile_list_models(
    uint64_t agent_handle,
    char **out_json,
    const char *traceparent
);

/// Set the model/provider for a session.
/// node_id: optional; NULL/empty means local provider.
int32_t qmt_mobile_set_session_model(
    uint64_t agent_handle,
    uint64_t session_handle,
    const char *provider,
    const char *model,
    const char *node_id
);

/// Set a session config option (mode, reasoning effort, etc.).
/// request_json: ACP SetSessionConfigOptionRequest JSON.
/// out_json: receives the SetSessionConfigOptionResponse JSON.
///            Must be freed with qmt_mobile_free_string.
int32_t qmt_mobile_set_session_config_option(
    uint64_t agent_handle,
    uint64_t session_handle,
    const char *request_json,
    char **out_json
);

// ============================================================================
// MCP Server Registration
// ============================================================================

/// Callback function type for in-process MCP request handling.
/// Receives a JSON request string and returns a JSON response string
/// allocated with malloc (or the free function's counterpart).
typedef char *(*qmt_mobile_mcp_handler_fn)(
    const char *request_json,
    void *user_data
);

/// Callback function type for freeing MCP response strings.
typedef void (*qmt_mobile_mcp_free_fn)(
    char *ptr,
    void *user_data
);

/// Register an in-process MCP server via callback functions.
int32_t qmt_mobile_register_inproc_mcp(
    uint64_t agent_handle,
    const char *server_name,
    qmt_mobile_mcp_handler_fn handler,
    qmt_mobile_mcp_free_fn free_response,
    void *user_data
);

/// Register an in-process MCP server via platform pipes.
int32_t qmt_mobile_register_inproc_mcp_pipe(
    uint64_t agent_handle,
    const char *server_name,
    int32_t *out_read_fd,
    int32_t *out_write_fd
);

/// Unregister a previously registered MCP server.
int32_t qmt_mobile_unregister_inproc_mcp(
    uint64_t agent_handle,
    const char *server_name
);

// ============================================================================
// Callbacks
// ============================================================================

/// Event handler callback; fires for every agent event.
typedef void (*qmt_mobile_event_handler_fn)(
    uint64_t agent_handle,
    uint64_t session_handle,
    const char *event_json,
    void *user_data
);

/// Set the event handler callback for an agent.
/// Pass NULL handler and NULL user_data to clear.
int32_t qmt_mobile_set_event_handler(
    uint64_t agent_handle,
    qmt_mobile_event_handler_fn handler,
    void *user_data
);

/// Log handler callback; fires for Rust log messages.
typedef void (*qmt_mobile_log_handler_fn)(
    int32_t level,
    const char *message,
    void *user_data
);

/// Set the global log handler callback.
/// Pass NULL handler and NULL user_data to clear.
int32_t qmt_mobile_set_log_handler(
    qmt_mobile_log_handler_fn handler,
    void *user_data
);

// ============================================================================
// Error Reporting & Memory
// ============================================================================

/// Return the last error code for the calling thread.
int32_t qmt_mobile_last_error_code(void);

/// Return a human-readable name for an error code.
/// Caller must free with qmt_mobile_free_string.
char *qmt_mobile_error_name(int32_t error_code);

/// Return the last error message for the calling thread.
/// Caller must free with qmt_mobile_free_string.
char *qmt_mobile_last_error_message(void);

/// Free a string allocated by the FFI layer. NULL is a no-op.
void qmt_mobile_free_string(char *ptr);

#ifdef __cplusplus
}
#endif

#endif /* QUERYMT_MOBILE_FFI_H */
