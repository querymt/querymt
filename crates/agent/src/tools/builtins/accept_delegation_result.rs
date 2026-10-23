//! Explicit acceptance of a delegate's retained workspace result.
//!
//! The model only decides *whether* to accept a reviewed result. Applying it
//! is a deterministic runtime operation
//! ([`WorkspaceService::accept_delegation_result`]): the model supplies two
//! identifiers and never a path, a command or a commit message.

use crate::tools::{
    CapabilityRequirement, FileAccess, Tool as ToolTrait, ToolContext, ToolError,
    ToolExecutionClass,
};
use crate::worktree::{AcceptRequest, IntegrationOutcome};
use async_trait::async_trait;
use querymt::chat::{FunctionTool, Tool, ToolResultPart};
use serde_json::{Value, json};
use std::path::Path;

pub struct AcceptDelegationResultTool;

impl Default for AcceptDelegationResultTool {
    fn default() -> Self {
        Self::new()
    }
}

impl AcceptDelegationResultTool {
    pub fn new() -> Self {
        Self
    }
}

fn describe(outcome: &IntegrationOutcome) -> (String, bool) {
    match outcome {
        IntegrationOutcome::Integrated(done) => {
            let paths = done
                .changed_paths
                .iter()
                .map(|p| format!("  {} ({})", p.path, p.kind))
                .collect::<Vec<_>>()
                .join("\n");
            (
                format!(
                    "Integrated: commit {} was created on branch `{}` (parent {}). The \
                     repository's pre-commit, prepare-commit-msg and commit-msg hooks ran \
                     natively and could not be bypassed.\nChanged paths:\n{paths}",
                    done.commit, done.branch, done.parent
                ),
                false,
            )
        }
        IntegrationOutcome::AlreadyIntegrated { commit } => (
            format!("Already integrated earlier as commit {commit}; nothing was changed."),
            false,
        ),
        IntegrationOutcome::NoChanges => (
            "Nothing to integrate: the current branch already contains this result's whole \
             delta, or the delegate made no changes. Nothing was changed."
                .to_string(),
            true,
        ),
        IntegrationOutcome::Conflicted { paths } => (
            format!(
                "Conflict: the result overlaps edits already on the current branch in: {}. \
                 Nothing was changed. Resolve by delegating a new task based on the current \
                 state that re-does the conflicting part; do not patch the checkout by hand.",
                paths.join(", ")
            ),
            true,
        ),
        IntegrationOutcome::Refused(refusal) => {
            let paths = if refusal.paths.is_empty() {
                String::new()
            } else {
                format!(" Paths: {}.", refusal.paths.join(", "))
            };
            // A hook that staged outside the delta keeps its own changes in
            // place; everything QueryMT wrote was still rolled back.
            let closing = if refusal.code == "hook_changed_paths_outside_delta" {
                "Nothing was committed and QueryMT's own file changes were rolled back; the \
                 hook's staging of the listed paths is left for you to inspect."
            } else {
                "Nothing was changed."
            };
            (
                format!(
                    "Refused ({}): {}{paths} {closing}",
                    refusal.code, refusal.message
                ),
                true,
            )
        }
    }
}

#[async_trait]
impl ToolTrait for AcceptDelegationResultTool {
    fn name(&self) -> &str {
        "accept_delegation_result"
    }

    fn definition(&self) -> Tool {
        Tool {
            tool_type: "function".to_string(),
            function: FunctionTool {
                name: "accept_delegation_result".to_string(),
                description: "Accept a reviewed result of an isolated delegate and integrate it \
                    into this session's checkout. QueryMT applies exactly the delegate's \
                    baseline-relative change as one commit on the checkout's current branch. \
                    The repository's pre-commit, prepare-commit-msg and commit-msg hooks run \
                    natively (there is no way to skip them) and a failing hook rejects the \
                    acceptance and restores the files. Commit signing is not supported. It \
                    refuses (changing nothing) when the touched paths carry local changes, and \
                    reports a conflict when an earlier integrated result overlaps. Use this instead of delegating work to \
                    apply a patch or merge a branch. Review the result first (the delegation \
                    summary lists the result commit and changed paths)."
                    .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "delegation_id": {
                            "type": "string",
                            "description": "The delegation id reported in the delegation result."
                        },
                        "result_commit": {
                            "type": "string",
                            "description": "The result commit you reviewed, as reported in the delegation summary (full id or at least 7 hex digits). Pins acceptance to exactly what was reviewed."
                        }
                    },
                    "required": ["delegation_id", "result_commit"]
                }),
                strict: None,
            },
        }
    }

    fn required_capabilities(&self) -> &'static [CapabilityRequirement] {
        &[CapabilityRequirement::Filesystem]
    }

    fn execution_class(&self) -> ToolExecutionClass {
        ToolExecutionClass::SerialStateful
    }

    fn file_access(&self, _args: &Value, _cwd: Option<&Path>) -> FileAccess {
        // Rewrites tracked files and moves the branch.
        FileAccess::Any
    }

    async fn call(
        &self,
        args: Value,
        context: &dyn ToolContext,
    ) -> Result<Vec<ToolResultPart>, ToolError> {
        let delegation_id = args["delegation_id"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidRequest("Missing delegation_id".into()))?;
        let result_commit = args["result_commit"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidRequest("Missing result_commit".into()))?;
        let cwd = context.cwd().ok_or_else(|| {
            ToolError::InvalidRequest("no working directory is set for this session".into())
        })?;
        let service = context.workspace_service().ok_or_else(|| {
            ToolError::InvalidRequest(
                "managed worktree workspaces are not enabled; delegates did not work in isolated \
                 checkouts, so there is nothing to integrate"
                    .into(),
            )
        })?;
        let store = context
            .session_store()
            .ok_or_else(|| ToolError::SessionError("session store is unavailable".into()))?;

        let delegation = store
            .get_delegation(delegation_id)
            .await
            .map_err(|e| ToolError::SessionError(e.to_string()))?
            .ok_or_else(|| {
                ToolError::InvalidRequest(format!("Unknown delegation '{delegation_id}'"))
            })?;

        // Serialize on the session's checkout gate; the service falls back
        // to the checkout-keyed gate when the context provides none.
        let gate = context.checkout_mutation_gate();
        let outcome = service
            .accept_delegation_result(&AcceptRequest {
                delegation_id,
                result_commit,
                requesting_session_id: context.session_id(),
                target_cwd: cwd,
                objective: &delegation.objective,
                delegation_status: delegation.status,
                mutation_gate: gate.as_deref(),
            })
            .await
            .map_err(|e| ToolError::Other(anyhow::anyhow!("integration failed: {e}")))?;

        let (text, needs_attention) = describe(&outcome);
        let text = if needs_attention {
            format!("ACTION NEEDED: {text}")
        } else {
            text
        };
        Ok(vec![ToolResultPart::text(text)])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worktree::Refusal;

    #[test]
    fn schema_requires_both_identifiers_and_no_paths() {
        let definition = AcceptDelegationResultTool::new().definition();
        let params = definition.function.parameters;
        assert_eq!(
            params["required"],
            json!(["delegation_id", "result_commit"])
        );
        assert_eq!(params["properties"].as_object().unwrap().len(), 2);
    }

    #[test]
    fn refusals_state_that_nothing_changed() {
        let (text, attention) = describe(&IntegrationOutcome::Refused(Refusal {
            code: "touches_local_changes",
            message: "blocked".into(),
            paths: vec!["a.txt".into()],
        }));
        assert!(attention);
        assert!(text.contains("touches_local_changes") && text.contains("a.txt"));
        assert!(text.contains("Nothing was changed"));
    }
}
