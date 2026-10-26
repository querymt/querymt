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

// Storage callback statuses are separate from qmt_ffi_* return codes.
#define QMT_FFI_STORAGE_CALLBACK_OK 0
#define QMT_FFI_STORAGE_CALLBACK_INVALID 1
#define QMT_FFI_STORAGE_CALLBACK_NOT_FOUND 2
#define QMT_FFI_STORAGE_CALLBACK_BACKEND_ERROR 3
#define QMT_FFI_STORAGE_CALLBACK_BUFFER_TOO_SMALL 8

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

// Synchronous host storage callbacks. Calls may arrive on arbitrary Rust
// threads; callbacks and user_data must be thread-safe. Android user_data must
// own a JNI GlobalRef (not a JNIEnv*) and the host must attach each calling
// thread to the JVM. Callbacks must not throw or unwind across this C ABI.
typedef int32_t (*qmt_ffi_storage_get_fn)(
    void *user_data,
    const uint8_t *service,
    size_t service_len,
    const uint8_t *key,
    size_t key_len,
    uint8_t *out,
    size_t out_cap,
    size_t *out_len
);
typedef int32_t (*qmt_ffi_storage_set_fn)(
    void *user_data,
    const uint8_t *service,
    size_t service_len,
    const uint8_t *key,
    size_t key_len,
    const uint8_t *value,
    size_t value_len
);
typedef int32_t (*qmt_ffi_storage_remove_fn)(
    void *user_data,
    const uint8_t *service,
    size_t service_len,
    const uint8_t *key,
    size_t key_len
);

typedef struct qmt_ffi_storage_callbacks {
    uint32_t abi_version;
    size_t struct_size;
    void *user_data;
    qmt_ffi_storage_get_fn get;
    qmt_ffi_storage_set_fn set;
    qmt_ffi_storage_remove_fn remove;
} qmt_ffi_storage_callbacks;

// ============================================================================
// Lifecycle
// ============================================================================

#ifdef __ANDROID__
int32_t qmt_ffi_android_init(void *env, void *context);
#endif
int32_t qmt_ffi_configure_telemetry(int32_t enabled, const char *endpoint);
// Register exactly one ABI-v1 callback table before mobile agent initialization.
// The table is copied; user_data must remain valid until unregister succeeds.
// ABI version 1 requires struct_size == sizeof(qmt_ffi_storage_callbacks).
int32_t qmt_ffi_storage_register(const qmt_ffi_storage_callbacks *callbacks);
// Returns QMT_FFI_BUSY while agents, runtime, init reservations, or callbacks live.
int32_t qmt_ffi_storage_unregister(void);
int32_t qmt_ffi_init_agent(const char *config_toml, uint64_t *out_agent);
int32_t qmt_ffi_shutdown(uint64_t agent_handle);
int32_t qmt_ffi_shutdown_runtime_if_idle(void);
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
