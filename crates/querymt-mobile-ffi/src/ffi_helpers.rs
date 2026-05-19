//! Thread-local error state, handle generation, call guards, and background state.
//!
//! Every non-OK FFI call stores a thread-local last error code and message.
//! Handles are simple incrementing counters. Active calls are tracked per-agent
//! to prevent shutdown while busy.

use crate::types::{AgentHandle, FfiErrorCode, SessionHandle};
use parking_lot::Mutex;
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};

// ─── Thread-Local Last Error ────────────────────────────────────────────────

std::thread_local! {
    static LAST_ERROR: Mutex<(FfiErrorCode, String)> = const { Mutex::new((FfiErrorCode::Ok, String::new())) };
}

/// Store a thread-local error.
pub fn set_last_error(code: FfiErrorCode, msg: String) {
    LAST_ERROR.with(|e| {
        *e.lock() = (code, msg);
    });
}

/// Store a thread-local error from an anyhow error.
pub fn set_last_error_from_anyhow(code: FfiErrorCode, err: anyhow::Error) {
    set_last_error(code, format!("{:#}", err));
}

/// Clear the thread-local error.
pub fn clear_last_error() {
    LAST_ERROR.with(|e| {
        *e.lock() = (FfiErrorCode::Ok, String::new());
    });
}

/// Read and clear the last error code.
pub fn take_last_error_code() -> FfiErrorCode {
    LAST_ERROR.with(|e| {
        let (code, _) = &*e.lock();
        *code
    })
}

/// Read the last error message (returns a clone, caller must free).
pub fn take_last_error_message() -> String {
    LAST_ERROR.with(|e| {
        let (_, msg) = &*e.lock();
        msg.clone()
    })
}

/// Helper: return code translation. Sets thread-local error on failure.
pub fn ffi_result<T, E: std::fmt::Display>(
    result: Result<T, E>,
    err_code: FfiErrorCode,
) -> (i32, Option<T>) {
    match result {
        Ok(val) => {
            clear_last_error();
            (FfiErrorCode::Ok as i32, Some(val))
        }
        Err(e) => {
            let msg = format!("{:#}", e);
            set_last_error(err_code, msg);
            (err_code as i32, None)
        }
    }
}

/// Helper: return code translation for void functions.
pub fn ffi_result_void<E: std::fmt::Display>(result: Result<(), E>, err_code: FfiErrorCode) -> i32 {
    match result {
        Ok(_) => {
            clear_last_error();
            FfiErrorCode::Ok as i32
        }
        Err(e) => {
            let msg = format!("{:#}", e);
            set_last_error(err_code, msg);
            err_code as i32
        }
    }
}

// ─── Handle Generation ──────────────────────────────────────────────────────

static NEXT_AGENT_HANDLE: AtomicU64 = AtomicU64::new(1);
static NEXT_SESSION_HANDLE: AtomicU64 = AtomicU64::new(1);

/// Allocate a new unique agent handle.
pub fn new_agent_handle() -> AgentHandle {
    NEXT_AGENT_HANDLE.fetch_add(1, Ordering::Relaxed)
}

/// Allocate a new unique session handle.
pub fn new_session_handle() -> SessionHandle {
    NEXT_SESSION_HANDLE.fetch_add(1, Ordering::Relaxed)
}

// ─── Active Call Guard ──────────────────────────────────────────────────────

/// Tracks in-flight FFI calls per agent to prevent shutdown while busy.
pub struct ActiveCallTracker {
    active: Mutex<HashSet<u64>>,
}

impl Default for ActiveCallTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl ActiveCallTracker {
    pub fn new() -> Self {
        Self {
            active: Mutex::new(HashSet::new()),
        }
    }

    /// Mark that an agent has an active FFI call.
    /// Returns a guard that will decrement on drop.
    pub fn begin_call(
        &self,
        agent_handle: AgentHandle,
    ) -> Result<ActiveCallGuard<'_>, FfiErrorCode> {
        self.active.lock().insert(agent_handle);
        Ok(ActiveCallGuard {
            tracker: self,
            agent_handle,
        })
    }

    /// Check if an agent has active calls.
    pub fn has_active_calls(&self, agent_handle: AgentHandle) -> bool {
        self.active.lock().contains(&agent_handle)
    }
}

/// RAII guard: decrements active call count on drop.
pub struct ActiveCallGuard<'a> {
    tracker: &'a ActiveCallTracker,
    agent_handle: AgentHandle,
}

impl Drop for ActiveCallGuard<'_> {
    fn drop(&mut self) {
        self.tracker.active.lock().remove(&self.agent_handle);
    }
}

// ─── Background State ───────────────────────────────────────────────────────

static BACKGROUNDED: AtomicU64 = AtomicU64::new(0);

/// Set the backgrounded state (1 = backgrounded, 0 = foreground).
pub fn set_backgrounded(yes: bool) {
    BACKGROUNDED.store(yes as u64, Ordering::Relaxed);
}

/// Check if the app is currently backgrounded.
pub fn is_backgrounded() -> bool {
    BACKGROUNDED.load(Ordering::Relaxed) != 0
}

/// Returns `QMT_MOBILE_INVALID_STATE` if backgrounded.
pub fn check_not_backgrounded() -> Result<(), FfiErrorCode> {
    if is_backgrounded() {
        Err(FfiErrorCode::InvalidState)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_error_round_trip_works() {
        clear_last_error();
        assert_eq!(take_last_error_code(), FfiErrorCode::Ok);
        assert_eq!(take_last_error_message(), "");

        set_last_error(FfiErrorCode::RuntimeError, "boom".to_string());
        assert_eq!(take_last_error_code(), FfiErrorCode::RuntimeError);
        assert_eq!(take_last_error_message(), "boom");
    }

    #[test]
    fn ffi_result_sets_error_on_failure() {
        let (code, value) = ffi_result::<u32, _>(Err("bad"), FfiErrorCode::InvalidArgument);
        assert_eq!(code, FfiErrorCode::InvalidArgument as i32);
        assert!(value.is_none());
        assert_eq!(take_last_error_code(), FfiErrorCode::InvalidArgument);
        assert!(take_last_error_message().contains("bad"));
    }

    #[test]
    fn handle_generation_monotonically_increases() {
        let a = new_agent_handle();
        let b = new_agent_handle();
        let s1 = new_session_handle();
        let s2 = new_session_handle();
        assert!(b > a);
        assert!(s2 > s1);
    }

    #[test]
    fn active_call_tracker_clears_on_drop() {
        let tracker = ActiveCallTracker::new();
        assert!(!tracker.has_active_calls(42));
        {
            let _guard = tracker.begin_call(42).unwrap();
            assert!(tracker.has_active_calls(42));
        }
        assert!(!tracker.has_active_calls(42));
    }

    #[test]
    fn background_state_is_tracked() {
        set_backgrounded(false);
        assert!(!is_backgrounded());
        assert_eq!(check_not_backgrounded(), Ok(()));

        set_backgrounded(true);
        assert!(is_backgrounded());
        assert_eq!(check_not_backgrounded(), Err(FfiErrorCode::InvalidState));

        set_backgrounded(false);
    }
}
