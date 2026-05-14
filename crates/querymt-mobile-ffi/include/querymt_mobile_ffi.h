#ifndef QUERYMT_MOBILE_FFI_H
#define QUERYMT_MOBILE_FFI_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

// ============================================================================
// Error Codes
// ============================================================================

#define QMT_FFI_OK 0
#define QMT_FFI_INVALID_ARGUMENT 1
#define QMT_FFI_NOT_FOUND 2
#define QMT_FFI_RUNTIME_ERROR 3
#define QMT_FFI_UNSUPPORTED 4
#define QMT_FFI_ALREADY_EXISTS 5
#define QMT_FFI_BUSY 6
#define QMT_FFI_INVALID_STATE 7

// ============================================================================
// Callback Types
// ============================================================================

typedef void (*qmt_ffi_log_handler_fn)(
    int32_t level,
    const char *message,
    void *user_data
);

typedef void (*qmt_ffi_acp_message_handler_fn)(
    uint64_t connection_handle,
    const char *message_json,
    void *user_data
);

// ============================================================================
// Lifecycle
// ============================================================================

int32_t qmt_ffi_init_agent(const char *config_toml, uint64_t *out_agent);
int32_t qmt_ffi_shutdown(uint64_t agent_handle);
int32_t qmt_ffi_set_lifecycle_state(uint64_t agent_handle, int32_t backgrounded);
int32_t qmt_ffi_set_log_handler(qmt_ffi_log_handler_fn handler, void *user_data);

// ============================================================================
// Embedded ACP Transport
// ============================================================================

int32_t qmt_ffi_acp_open(uint64_t agent_handle, uint64_t *out_connection);
int32_t qmt_ffi_acp_close(uint64_t connection_handle);
int32_t qmt_ffi_acp_send(uint64_t connection_handle, const char *message_json);
int32_t qmt_ffi_acp_next_message(
    uint64_t connection_handle,
    int32_t timeout_ms,
    char **out_message_json
);
int32_t qmt_ffi_acp_set_message_handler(
    uint64_t connection_handle,
    qmt_ffi_acp_message_handler_fn handler,
    void *user_data
);

// ============================================================================
// Error / Memory
// ============================================================================

int32_t qmt_ffi_last_error_code(void);
char *qmt_ffi_last_error_message(void);
void qmt_ffi_free_string(char *ptr);

#ifdef __cplusplus
}
#endif

#endif /* QUERYMT_MOBILE_FFI_H */
