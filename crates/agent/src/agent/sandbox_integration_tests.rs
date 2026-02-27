//! Integration tests for the sandbox subsystem.
//!
//! These tests validate the interaction between:
//! - `querymt-sandbox` policy library
//! - `WorkerManager` lifecycle
//! - Tool-level `is_read_only()` enforcement
//! - Supervisor socket IPC
//!
//! Requires `--features sandbox` to compile and run.

use crate::agent::core::AgentMode;
use crate::agent::worker_manager::{WorkerError, WorkerManager};
use crate::tools::Tool;
use crate::tools::context::ToolError;
use crate::tools::context_impl::AgentToolContext;
use std::path::PathBuf;

// ── Sandbox Policy Tests ─────────────────────────────────────────────────

/// Build mode: the static profile still only grants Read for CWD.
/// Write access is obtained at runtime via extension tokens.
/// Extensions must be enabled so the runtime token exchange can work.
#[test]
fn sandbox_policy_build_mode_uses_read_for_cwd_and_enables_extensions() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().canonicalize().unwrap();
    let policy = querymt_sandbox::SandboxPolicy {
        cwd: cwd.clone(),
        read_only: false, // Build mode
        allow_network: true,
        db_path: None,
        socket_dir: None,
    };
    let caps = policy.to_capability_set().unwrap();
    assert!(caps.path_covered(&cwd), "cwd should be covered");
    assert!(!caps.is_network_blocked(), "network should be allowed");
    // Extensions MUST be enabled so runtime write tokens can be consumed
    assert!(
        caps.extensions_enabled(),
        "extensions must be enabled for runtime write token exchange"
    );
    // Verify the static profile uses Read (not ReadWrite) for CWD.
    // Write access comes from extension tokens, not the static profile.
    let cwd_cap = caps
        .fs_capabilities()
        .iter()
        .find(|cap| !cap.is_file && cwd.starts_with(&cap.resolved));
    if let Some(cap) = cwd_cap {
        assert_eq!(
            cap.access,
            querymt_sandbox::AccessMode::Read,
            "CWD static profile must use Read; write access is via extension tokens"
        );
    }
}

#[test]
fn sandbox_policy_plan_mode_is_read_only() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().canonicalize().unwrap();
    let policy = querymt_sandbox::SandboxPolicy {
        cwd: cwd.clone(),
        read_only: true,
        allow_network: true,
        db_path: None,
        socket_dir: None,
    };
    let caps = policy.to_capability_set().unwrap();
    assert!(caps.path_covered(&cwd));
    // Note: nono CapabilitySet doesn't expose the access mode per path,
    // but the sandbox will enforce read-only at the OS level.
}

#[test]
fn sandbox_policy_network_can_be_blocked() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().canonicalize().unwrap();
    let policy = querymt_sandbox::SandboxPolicy {
        cwd,
        read_only: false,
        allow_network: false,
        db_path: None,
        socket_dir: None,
    };
    let caps = policy.to_capability_set().unwrap();
    assert!(caps.is_network_blocked());
}

// ── Supervisor Socket IPC Tests ──────────────────────────────────────────

#[test]
fn supervisor_socket_pair_can_be_created() {
    let (parent, child) = querymt_sandbox::create_supervisor_socket_pair().unwrap();
    assert_ne!(parent.as_raw_fd(), child.as_raw_fd());
}

#[test]
fn supervisor_socket_roundtrip() {
    use querymt_sandbox::*;

    let (mut parent, mut child) = create_supervisor_socket_pair().unwrap();

    // Child sends a capability request
    let request = CapabilityRequest {
        request_id: "req-001".to_string(),
        path: PathBuf::from("/tmp/test-file"),
        access: AccessMode::Read,
        reason: Some("test roundtrip".to_string()),
        child_pid: std::process::id(),
        session_id: "test-session".to_string(),
    };

    child
        .send_message(&SupervisorMessage::Request(request))
        .unwrap();

    // Parent receives the request
    let msg = parent.recv_message().unwrap();
    match msg {
        SupervisorMessage::Request(req) => {
            assert_eq!(req.request_id, "req-001");
            assert_eq!(req.path, PathBuf::from("/tmp/test-file"));

            // Parent sends a response
            let response = SupervisorResponse::Decision {
                request_id: req.request_id,
                decision: ApprovalDecision::Granted,
            };
            parent.send_response(&response).unwrap();
        }
    }

    // Child receives the response
    let resp = child.recv_response().unwrap();
    match resp {
        SupervisorResponse::Decision {
            request_id,
            decision,
        } => {
            assert_eq!(request_id, "req-001");
            assert!(decision.is_granted());
        }
    }
}

#[test]
fn mode_approval_backend_enforces_cwd_boundary() {
    use querymt_sandbox::*;

    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().canonicalize().unwrap();
    let backend = ModeApprovalBackend::new(cwd.clone(), true);

    // Request within cwd should be granted
    let within = CapabilityRequest {
        request_id: "in-cwd".to_string(),
        path: cwd.join("src/lib.rs"),
        access: AccessMode::ReadWrite,
        reason: None,
        child_pid: 1,
        session_id: "s1".to_string(),
    };
    assert!(backend.request_capability(&within).unwrap().is_granted());

    // Request outside cwd should be denied
    let outside = CapabilityRequest {
        request_id: "outside-cwd".to_string(),
        path: PathBuf::from("/etc/shadow"),
        access: AccessMode::Read,
        reason: None,
        child_pid: 1,
        session_id: "s1".to_string(),
    };
    assert!(backend.request_capability(&outside).unwrap().is_denied());
}

#[test]
fn mode_approval_backend_mode_transitions() {
    use querymt_sandbox::*;

    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().canonicalize().unwrap();
    let backend = ModeApprovalBackend::new(cwd.clone(), false); // Start in Plan mode

    let write_req = CapabilityRequest {
        request_id: "w1".to_string(),
        path: cwd.join("file.txt"),
        access: AccessMode::ReadWrite,
        reason: None,
        child_pid: 1,
        session_id: "s1".to_string(),
    };

    let read_req = CapabilityRequest {
        request_id: "r1".to_string(),
        path: cwd.join("file.txt"),
        access: AccessMode::Read,
        reason: None,
        child_pid: 1,
        session_id: "s1".to_string(),
    };

    // Plan mode: read OK, write denied
    assert!(backend.request_capability(&read_req).unwrap().is_granted());
    assert!(backend.request_capability(&write_req).unwrap().is_denied());

    // Switch to Build mode
    backend.set_allow_write(true);
    assert!(backend.request_capability(&write_req).unwrap().is_granted());

    // Switch back to Review mode
    backend.set_allow_write(false);
    assert!(backend.request_capability(&write_req).unwrap().is_denied());
    assert!(backend.request_capability(&read_req).unwrap().is_granted());
}

// ── WorkerManager Tests ──────────────────────────────────────────────────

#[test]
fn worker_manager_lifecycle() {
    let mgr = WorkerManager::new();
    assert!(mgr.is_empty());
    assert_eq!(mgr.len(), 0);
    assert!(!mgr.has_worker("sess-1"));
    assert!(mgr.session_ids().is_empty());
}

#[tokio::test]
async fn worker_manager_destroy_nonexistent_is_ok() {
    let mut mgr = WorkerManager::new();
    // Should not error
    mgr.destroy_worker("nonexistent").await.unwrap();
}

#[tokio::test]
async fn worker_manager_switch_mode_nonexistent_errors() {
    let mut mgr = WorkerManager::new();
    let result = mgr.switch_mode("nonexistent", AgentMode::Plan).await;
    assert!(matches!(result, Err(WorkerError::NotFound(_))));
}

// ── Tool Read-Only Enforcement Tests ─────────────────────────────────────

/// Helper to create a read-only tool context.
fn read_only_context() -> AgentToolContext {
    AgentToolContext::basic_read_only(
        "test-session".to_string(),
        Some(PathBuf::from("/tmp/test-project")),
    )
}

/// Helper to create a read-write tool context.
#[allow(dead_code)]
fn read_write_context() -> AgentToolContext {
    AgentToolContext::basic(
        "test-session".to_string(),
        Some(PathBuf::from("/tmp/test-project")),
    )
}

#[tokio::test]
async fn edit_tool_blocked_in_read_only_mode() {
    use crate::tools::builtins::edit::EditTool;
    let tool = EditTool::new();
    let ctx = read_only_context();
    let args = serde_json::json!({
        "filePath": "/tmp/test-project/test.txt",
        "oldString": "old",
        "newString": "new"
    });
    let result = tool.call(args, &ctx).await;
    assert!(matches!(result, Err(ToolError::PermissionDenied(_))));
}

#[tokio::test]
async fn write_file_tool_blocked_in_read_only_mode() {
    use crate::tools::builtins::write_file::WriteFileTool;
    let tool = WriteFileTool::new();
    let ctx = read_only_context();
    let args = serde_json::json!({
        "path": "/tmp/test-project/new-file.txt",
        "content": "hello"
    });
    let result = tool.call(args, &ctx).await;
    assert!(matches!(result, Err(ToolError::PermissionDenied(_))));
}

#[tokio::test]
async fn delete_file_tool_blocked_in_read_only_mode() {
    use crate::tools::builtins::delete_file::DeleteFileTool;
    let tool = DeleteFileTool::new();
    let ctx = read_only_context();
    let args = serde_json::json!({
        "path": "/tmp/test-project/file.txt"
    });
    let result = tool.call(args, &ctx).await;
    assert!(matches!(result, Err(ToolError::PermissionDenied(_))));
}

#[tokio::test]
async fn apply_patch_tool_blocked_in_read_only_mode() {
    use crate::tools::builtins::apply_patch::ApplyPatchTool;
    let tool = ApplyPatchTool::new();
    let ctx = read_only_context();
    let args = serde_json::json!({
        "patch": "--- a/file.txt\n+++ b/file.txt\n@@ -1 +1 @@\n-old\n+new"
    });
    let result = tool.call(args, &ctx).await;
    assert!(matches!(result, Err(ToolError::PermissionDenied(_))));
}

#[tokio::test]
async fn multiedit_tool_blocked_in_read_only_mode() {
    use crate::tools::builtins::multiedit::MultiEditTool;
    let tool = MultiEditTool::new();
    let ctx = read_only_context();
    let args = serde_json::json!({
        "filePath": "/tmp/test-project/test.txt",
        "edits": [{"oldString": "old", "newString": "new"}]
    });
    let result = tool.call(args, &ctx).await;
    assert!(matches!(result, Err(ToolError::PermissionDenied(_))));
}

/// Shell tool does NOT block in read-only mode at the application layer.
///
/// The shell tool defers write enforcement to the OS sandbox (nono/Seatbelt).
/// When the sandbox blocks a write, the tool detects the EPERM and annotates
/// the result with a `sandbox_note` field, but it does NOT return
/// `ToolError::PermissionDenied` for the shell tool (unlike write_file/edit).
///
/// Read-only shell commands (e.g. `echo`, `ls`, `cat`) must succeed even in
/// Plan/Review mode so the agent can still inspect the codebase.
#[tokio::test]
async fn shell_tool_passes_through_in_read_only_mode() {
    use crate::tools::builtins::shell::ShellTool;
    let dir = tempfile::tempdir().unwrap();
    let tool = ShellTool::new();
    let ctx = AgentToolContext::basic_read_only(
        "test-session".to_string(),
        Some(dir.path().to_path_buf()),
    );
    let args = serde_json::json!({ "command": "echo hello" });
    let result = tool.call(args, &ctx).await;
    // Shell tool must NOT return PermissionDenied -- it defers to the OS sandbox
    assert!(
        result.is_ok(),
        "shell tool must not block in read-only mode at the application layer, got: {:?}",
        result.err()
    );
    let parsed: serde_json::Value = serde_json::from_str(&result.unwrap()).unwrap();
    assert_eq!(
        parsed["exit_code"], 0,
        "read-only echo command should succeed"
    );
    // A successful read-only command should have no sandbox_note
    assert!(
        parsed.get("sandbox_note").is_none(),
        "read-only echo command should not trigger a sandbox_note, got: {:?}",
        parsed.get("sandbox_note")
    );
}

/// Verify that read tools still work in read-only mode.
#[tokio::test]
async fn read_tool_allowed_in_read_only_mode() {
    use crate::tools::builtins::read_tool::ReadTool;
    let dir = tempfile::tempdir().unwrap();
    let file_path = dir.path().join("test.txt");
    std::fs::write(&file_path, "hello world").unwrap();

    let tool = ReadTool::new();
    let ctx = AgentToolContext::basic_read_only(
        "test-session".to_string(),
        Some(dir.path().to_path_buf()),
    );
    let args = serde_json::json!({
        "path": file_path.to_str().unwrap()
    });
    let result = tool.call(args, &ctx).await;
    assert!(result.is_ok(), "Read tool should work in read-only mode");
}

/// Verify that glob tool still works in read-only mode.
#[tokio::test]
async fn glob_tool_allowed_in_read_only_mode() {
    use crate::tools::builtins::glob::GlobTool;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("test.txt"), "hello").unwrap();

    let tool = GlobTool::new();
    let ctx = AgentToolContext::basic_read_only(
        "test-session".to_string(),
        Some(dir.path().to_path_buf()),
    );
    let args = serde_json::json!({
        "pattern": "*.txt"
    });
    let result = tool.call(args, &ctx).await;
    assert!(result.is_ok(), "Glob tool should work in read-only mode");
}
