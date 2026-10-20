//! Typed session creation options for local versus isolated execution.
//!
//! These options are the entry-point surface (configuration, agent APIs,
//! and the worktree command surface) for requesting an isolated session.
//! Absent/default options always resolve to the existing local behavior —
//! isolation is strictly opt-in and fails closed when it cannot be
//! honored, never silently falling back to shared execution.

use crate::worktree::domain::{BaselineMode, ExecutionMode, WorkspaceCreationOptions};

/// Typed options for creating a session.
///
/// Defaults preserve compatibility: [`SessionCreationOptions::default`]
/// describes today's local session exactly (no managed worktree, commit
/// baseline irrelevant without a worktree, no explicit ref).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionCreationOptions {
    /// Local or worktree execution. Defaults to `Local`.
    pub execution: ExecutionMode,
    /// Baseline for worktree creation. Defaults to `Commit` (current HEAD;
    /// uncommitted local edits are excluded and disclosed).
    pub baseline: BaselineMode,
    /// Explicit base ref (branch, tag, or commit). Absent = source HEAD.
    pub base_ref: Option<String>,
}

impl Default for SessionCreationOptions {
    fn default() -> Self {
        Self {
            execution: ExecutionMode::Local,
            baseline: BaselineMode::Commit,
            base_ref: None,
        }
    }
}

impl SessionCreationOptions {
    /// Options preserving existing local execution unchanged.
    pub fn local() -> Self {
        Self::default()
    }

    /// Options requesting an isolated worktree from `baseline`.
    pub fn isolated_worktree(baseline: BaselineMode) -> Self {
        Self {
            execution: ExecutionMode::Worktree,
            baseline,
            base_ref: None,
        }
    }

    /// Whether these options request an isolated worktree.
    pub fn requests_isolation(&self) -> bool {
        self.execution == ExecutionMode::Worktree
    }

    /// Whether local changes are included in the worktree baseline.
    pub fn include_local_changes(&self) -> bool {
        self.baseline == BaselineMode::CurrentWorkingTree
    }

    /// Set the base ref for commit-mode isolation.
    pub fn with_base_ref(mut self, base_ref: impl Into<String>) -> Self {
        self.base_ref = Some(base_ref.into());
        self
    }

    /// Validate option combinations before any workspace is created.
    ///
    /// Rejections are explicit (fail closed); there is no silent downgrade
    /// to local or shared execution.
    pub fn validate(&self) -> Result<(), String> {
        if self.execution == ExecutionMode::Local {
            if self.baseline == BaselineMode::CurrentWorkingTree {
                return Err(
                    "current-working-tree baseline capture requires worktree execution; \
                     local sessions do not capture baselines"
                        .to_string(),
                );
            }
            if self.base_ref.is_some() {
                return Err(
                    "an explicit base ref requires worktree execution; local sessions \
                     always run in the current checkout"
                        .to_string(),
                );
            }
            return Ok(());
        }
        if self.baseline == BaselineMode::CurrentWorkingTree && self.base_ref.is_some() {
            return Err(
                "an explicit base ref cannot be combined with current-working-tree \
                 baseline capture; that baseline always reflects the checkout's \
                 current working state"
                    .to_string(),
            );
        }
        Ok(())
    }

    /// Project onto the durable workspace creation options.
    pub fn to_workspace_options(&self) -> WorkspaceCreationOptions {
        WorkspaceCreationOptions {
            execution: self.execution,
            baseline: self.baseline,
            base_ref: self.base_ref.clone(),
        }
    }

    /// Reconstruct from durable workspace creation options, e.g. a persisted
    /// fork-isolation intent replayed by explicit recovery.
    pub fn from_workspace_options(options: &WorkspaceCreationOptions) -> Self {
        Self {
            execution: options.execution,
            baseline: options.baseline,
            base_ref: options.base_ref.clone(),
        }
    }
}

/// `_meta.querymt.sessionWorkspace` key carrying typed creation options on
/// ACP session-creation requests (additive extension; upstream protocol
/// types are not modified).
pub const SESSION_WORKSPACE_META_KEY: &str = "sessionWorkspace";

fn parse_execution_mode(value: &serde_json::Value) -> Result<ExecutionMode, String> {
    let raw = value.as_str().ok_or_else(|| {
        "sessionWorkspace.execution must be a string (\"local\" or \"worktree\")".to_string()
    })?;
    raw.parse::<ExecutionMode>()
        .map_err(|err| format!("sessionWorkspace.execution: {err}"))
}

/// Accept both the snake-case domain spelling and its camelCase JSON
/// equivalent so clients can use either style. Explicit `null` counts as
/// "not specified".
fn parse_baseline_mode(value: &serde_json::Value) -> Result<Option<BaselineMode>, String> {
    if value.is_null() {
        return Ok(None);
    }
    let raw = value.as_str().ok_or_else(|| {
        "sessionWorkspace.baseline must be a string (\"commit\" or \
         \"current_working_tree\")"
            .to_string()
    })?;
    match raw {
        "commit" => Ok(Some(BaselineMode::Commit)),
        "current_working_tree" | "currentWorkingTree" => Ok(Some(BaselineMode::CurrentWorkingTree)),
        other => Err(format!(
            "sessionWorkspace.baseline: unknown baseline mode: {other}"
        )),
    }
}

/// Parse typed [`SessionCreationOptions`] from an ACP `_meta.querymt`
/// object.
///
/// Accepted shape:
///
/// ```json
/// {
///   "execution": "worktree",
///   "baseline": "commit",
///   "includeLocalChanges": false,
///   "baseRef": "v1.2"
/// }
/// ```
///
/// - `execution`: `"local"` (default) or `"worktree"`
/// - `baseline`: `"commit"` (default) or `"current_working_tree"`;
///   `includeLocalChanges: true` is an equivalent shorthand
/// - `baseRef`/`base_ref`: optional explicit ref for commit-mode isolation
///
/// Returns `Ok(None)` when the key is absent so omitted options preserve
/// local behavior. Malformed values return `Err` with an actionable
/// message so requested isolation fails closed instead of guessing.
pub fn session_creation_options_from_meta(
    meta: Option<&serde_json::Map<String, serde_json::Value>>,
) -> Result<Option<SessionCreationOptions>, String> {
    let Some(value) = meta
        .and_then(|meta| meta.get(crate::acp::shared::QUERYMT_META_NAMESPACE))
        .and_then(|querymt| querymt.get(SESSION_WORKSPACE_META_KEY))
    else {
        return Ok(None);
    };

    let object = value.as_object().ok_or_else(|| {
        "_meta.querymt.sessionWorkspace must be an object of creation options".to_string()
    })?;

    let mut options = SessionCreationOptions::default();
    if let Some(execution) = object.get("execution") {
        options.execution = parse_execution_mode(execution)?;
    }
    if let Some(baseline) = object.get("baseline")
        && let Some(mode) = parse_baseline_mode(baseline)?
    {
        options.baseline = mode;
    }
    for key in ["includeLocalChanges", "include_local_changes"] {
        match object.get(key) {
            None | Some(serde_json::Value::Null) | Some(serde_json::Value::Bool(false)) => {}
            Some(serde_json::Value::Bool(true)) => {
                options.baseline = BaselineMode::CurrentWorkingTree;
            }
            Some(other) => {
                return Err(format!(
                    "sessionWorkspace.{key} must be a boolean, got {other}"
                ));
            }
        }
    }
    for key in ["baseRef", "base_ref"] {
        match object.get(key) {
            None | Some(serde_json::Value::Null) => {}
            Some(serde_json::Value::String(raw)) if raw.is_empty() => {}
            Some(serde_json::Value::String(raw)) => {
                options.base_ref = Some(raw.clone());
            }
            Some(other) => {
                return Err(format!(
                    "sessionWorkspace.{key} must be a string ref, got {other}"
                ));
            }
        }
    }

    options.validate()?;
    Ok(Some(options))
}

/// Parse creation options for an isolated conversation fork.
///
/// Identical to [`session_creation_options_from_meta`] except that an
/// isolated fork whose caller did not pin a baseline or ref defaults to
/// current-working-tree capture: the fork's checkout reflects the source's
/// current contents rather than the historical filesystem state at the
/// message fork point. `Ok(None)` (no workspace meta) keeps a local fork a
/// pure message copy with no new workspace.
pub fn fork_creation_options_from_meta(
    meta: Option<&serde_json::Map<String, serde_json::Value>>,
) -> Result<Option<SessionCreationOptions>, String> {
    let Some(options) = session_creation_options_from_meta(meta)? else {
        return Ok(None);
    };
    // A baseline is pinned by a non-null `baseline` or by an explicit
    // boolean `includeLocalChanges` (including `false`, which requests a
    // commit baseline and must not be upgraded to a working-tree capture).
    let baseline_pinned = meta
        .and_then(|meta| meta.get(crate::acp::shared::QUERYMT_META_NAMESPACE))
        .and_then(|querymt| querymt.get(SESSION_WORKSPACE_META_KEY))
        .is_some_and(|workspace| {
            workspace.get("baseline").is_some_and(|v| !v.is_null())
                || ["includeLocalChanges", "include_local_changes"]
                    .iter()
                    .any(|key| {
                        workspace
                            .get(*key)
                            .is_some_and(serde_json::Value::is_boolean)
                    })
        });
    let mut options = options;
    if options.requests_isolation() && !baseline_pinned && options.base_ref.is_none() {
        options.baseline = BaselineMode::CurrentWorkingTree;
    }
    Ok(Some(options))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_preserve_local_behavior() {
        let opts = SessionCreationOptions::default();
        assert!(!opts.requests_isolation());
        assert!(!opts.include_local_changes());
        assert_eq!(opts, SessionCreationOptions::local());
        opts.validate().expect("defaults must always be valid");
    }

    #[test]
    fn isolated_commit_mode_with_ref_is_valid() {
        let opts = SessionCreationOptions::isolated_worktree(BaselineMode::Commit)
            .with_base_ref("feature-x");
        assert!(opts.requests_isolation());
        assert!(!opts.include_local_changes());
        opts.validate().expect("commit + ref is the happy path");
    }

    #[test]
    fn isolated_current_working_tree_without_ref_is_valid() {
        let opts = SessionCreationOptions::isolated_worktree(BaselineMode::CurrentWorkingTree);
        assert!(opts.include_local_changes());
        opts.validate().expect("CWT without ref is valid");
    }

    #[test]
    fn explicit_ref_with_current_working_tree_capture_is_invalid() {
        let opts = SessionCreationOptions::isolated_worktree(BaselineMode::CurrentWorkingTree)
            .with_base_ref("v1.2");
        let err = opts.validate().unwrap_err();
        assert!(err.contains("cannot be combined"), "error: {err}");
    }

    #[test]
    fn local_sessions_reject_baseline_and_ref_options() {
        let err = SessionCreationOptions {
            execution: ExecutionMode::Local,
            baseline: BaselineMode::CurrentWorkingTree,
            base_ref: None,
        }
        .validate()
        .unwrap_err();
        assert!(err.contains("requires worktree execution"), "error: {err}");

        let err = SessionCreationOptions {
            execution: ExecutionMode::Local,
            baseline: BaselineMode::Commit,
            base_ref: Some("main".into()),
        }
        .validate()
        .unwrap_err();
        assert!(err.contains("requires worktree execution"), "error: {err}");
    }

    #[test]
    fn projects_onto_durable_workspace_options() {
        let local = SessionCreationOptions::local().to_workspace_options();
        assert_eq!(local, WorkspaceCreationOptions::default());

        let isolated = SessionCreationOptions::isolated_worktree(BaselineMode::CurrentWorkingTree)
            .to_workspace_options();
        assert!(isolated.requests_isolation());
        assert_eq!(isolated.baseline, BaselineMode::CurrentWorkingTree);
    }

    fn meta_with_workspace(value: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
        let mut meta = serde_json::Map::new();
        meta.insert(
            crate::acp::shared::QUERYMT_META_NAMESPACE.to_string(),
            serde_json::json!({ SESSION_WORKSPACE_META_KEY: value }),
        );
        meta
    }

    #[test]
    fn absent_meta_preserves_local_behavior() {
        assert_eq!(session_creation_options_from_meta(None).unwrap(), None);
        let empty = serde_json::Map::new();
        assert_eq!(
            session_creation_options_from_meta(Some(&empty)).unwrap(),
            None
        );
        let mut without_workspace = serde_json::Map::new();
        without_workspace.insert(
            crate::acp::shared::QUERYMT_META_NAMESPACE.to_string(),
            serde_json::json!({ "otherKey": 1 }),
        );
        assert_eq!(
            session_creation_options_from_meta(Some(&without_workspace)).unwrap(),
            None
        );
    }

    #[test]
    fn worktree_meta_requests_isolation_from_head_by_default() {
        let meta = meta_with_workspace(serde_json::json!({ "execution": "worktree" }));
        let opts = session_creation_options_from_meta(Some(&meta))
            .expect("valid options")
            .expect("isolation requested");
        assert!(opts.requests_isolation());
        assert_eq!(opts.baseline, BaselineMode::Commit);
        assert_eq!(opts.base_ref, None);
    }

    #[test]
    fn include_local_changes_shorthand_and_camel_case_ref() {
        let meta = meta_with_workspace(serde_json::json!({
            "execution": "worktree",
            "includeLocalChanges": true,
        }));
        let opts = session_creation_options_from_meta(Some(&meta))
            .expect("valid options")
            .expect("isolation requested");
        assert_eq!(opts.baseline, BaselineMode::CurrentWorkingTree);

        let snake = meta_with_workspace(serde_json::json!({
            "execution": "worktree",
            "baseline": "current_working_tree",
            "base_ref": "v1.2",
        }));
        let err = session_creation_options_from_meta(Some(&snake)).unwrap_err();
        assert!(err.contains("cannot be combined"), "error: {err}");

        let camel_ref = meta_with_workspace(serde_json::json!({
            "execution": "worktree",
            "baseRef": "v1.2",
        }));
        let opts = session_creation_options_from_meta(Some(&camel_ref))
            .expect("valid options")
            .expect("isolation requested");
        assert_eq!(opts.base_ref.as_deref(), Some("v1.2"));
    }

    #[test]
    fn malformed_meta_fails_closed_with_actionable_errors() {
        let not_object = meta_with_workspace(serde_json::json!("worktree"));
        let err = session_creation_options_from_meta(Some(&not_object)).unwrap_err();
        assert!(err.contains("must be an object"), "error: {err}");

        let bad_mode = meta_with_workspace(serde_json::json!({ "execution": "sandbox" }));
        let err = session_creation_options_from_meta(Some(&bad_mode)).unwrap_err();
        assert!(err.contains("unknown execution mode"), "error: {err}");

        let bad_baseline = meta_with_workspace(serde_json::json!({ "baseline": 3 }));
        let err = session_creation_options_from_meta(Some(&bad_baseline)).unwrap_err();
        assert!(err.contains("must be a string"), "error: {err}");

        let bad_ref = meta_with_workspace(serde_json::json!({ "baseRef": 7 }));
        let err = session_creation_options_from_meta(Some(&bad_ref)).unwrap_err();
        assert!(err.contains("must be a string ref"), "error: {err}");

        let bad_flag = meta_with_workspace(serde_json::json!({ "includeLocalChanges": "yes" }));
        let err = session_creation_options_from_meta(Some(&bad_flag)).unwrap_err();
        assert!(err.contains("must be a boolean"), "error: {err}");
    }

    #[test]
    fn local_meta_with_ref_options_is_rejected() {
        let meta = meta_with_workspace(serde_json::json!({
            "execution": "local",
            "baseRef": "main",
        }));
        let err = session_creation_options_from_meta(Some(&meta)).unwrap_err();
        assert!(err.contains("requires worktree execution"), "error: {err}");
    }

    #[test]
    fn fork_options_default_to_current_working_state() {
        let meta = meta_with_workspace(serde_json::json!({ "execution": "worktree" }));
        let opts = fork_creation_options_from_meta(Some(&meta))
            .expect("valid options")
            .expect("isolation requested");
        assert_eq!(
            opts.baseline,
            BaselineMode::CurrentWorkingTree,
            "a fork discloses the source's current contents by default"
        );

        // An explicitly pinned baseline is honored.
        let pinned = meta_with_workspace(serde_json::json!({
            "execution": "worktree",
            "baseline": "commit",
        }));
        let opts = fork_creation_options_from_meta(Some(&pinned))
            .expect("valid options")
            .expect("isolation requested");
        assert_eq!(opts.baseline, BaselineMode::Commit);

        // An explicit ref pins commit mode from that ref.
        let ref_pinned = meta_with_workspace(serde_json::json!({
            "execution": "worktree",
            "baseRef": "v1.2",
        }));
        let opts = fork_creation_options_from_meta(Some(&ref_pinned))
            .expect("valid options")
            .expect("isolation requested");
        assert_eq!(opts.baseline, BaselineMode::Commit);
        assert_eq!(opts.base_ref.as_deref(), Some("v1.2"));
    }

    /// An explicit boolean `includeLocalChanges` pins the baseline, including
    /// `false` (commit baseline); a null `baseline` does not pin.
    #[test]
    fn fork_explicit_include_local_changes_pins_the_baseline() {
        for key in ["includeLocalChanges", "include_local_changes"] {
            let off = meta_with_workspace(serde_json::json!({
                "execution": "worktree",
                key: false,
            }));
            let opts = fork_creation_options_from_meta(Some(&off))
                .expect("valid options")
                .expect("isolation requested");
            assert_eq!(
                opts.baseline,
                BaselineMode::Commit,
                "{key}: false must not be upgraded to a working-tree capture"
            );

            let on = meta_with_workspace(serde_json::json!({
                "execution": "worktree",
                key: true,
            }));
            let opts = fork_creation_options_from_meta(Some(&on))
                .expect("valid options")
                .expect("isolation requested");
            assert_eq!(opts.baseline, BaselineMode::CurrentWorkingTree);
        }

        let null_baseline = meta_with_workspace(serde_json::json!({
            "execution": "worktree",
            "baseline": null,
        }));
        let opts = fork_creation_options_from_meta(Some(&null_baseline))
            .expect("valid options")
            .expect("isolation requested");
        assert_eq!(
            opts.baseline,
            BaselineMode::CurrentWorkingTree,
            "a null baseline is not an explicit pin"
        );
    }

    #[test]
    fn fork_without_workspace_meta_stays_local() {
        assert_eq!(fork_creation_options_from_meta(None).unwrap(), None);
        let local = meta_with_workspace(serde_json::json!({ "execution": "local" }));
        let opts = fork_creation_options_from_meta(Some(&local))
            .expect("valid options")
            .expect("explicit local");
        assert!(!opts.requests_isolation());
    }
}
