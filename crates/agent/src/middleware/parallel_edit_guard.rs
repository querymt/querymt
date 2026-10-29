//! Parallel edit guard middleware
//!
//! Detects when a single model response contains two or more tool calls that
//! write the same file (for example two `edit` calls on `design.md`) and steers
//! the model toward one atomic `multiedit` call per file.
//!
//! Same-file writes are always serialized by the tool-batch scheduler, so no
//! edit is ever lost. This middleware only changes what the *model* is told:
//!
//! - `warn` (default): every call runs; a guidance message is injected before
//!   the next LLM request, after the tool results.
//! - `deny`: only the first writing call per file runs; later writers get an
//!   error result telling the model to combine them into one `multiedit`.
//!
//! The guard reviews *prepared* calls (`on_prepared_tool_calls`): the file
//! access it inspects was declared for the final effective arguments, after
//! normalisation and pre-tool hook updates, and is the same access the
//! scheduler plans from. Aliased, repaired or hook-redirected targets are
//! therefore judged by where the call will actually write. UI notices are
//! emitted with the session's ordered event stream, so a slow event journal
//! never delays tool execution.
//!
//! # Example (TOML config)
//!
//! ```toml
//! [[middleware]]
//! type = "parallel_edit_guard"
//! mode = "deny"   # "warn" (default) or "deny"
//! # enabled = false
//! ```

use crate::agent::core::SessionRuntime;
use crate::middleware::factory::MiddlewareFactory;
use crate::middleware::{
    ExecutionState, MiddlewareDriver, PreparedBatch, PreparedCallVerdict, Result, ToolCall,
    ToolResult,
};
use crate::tools::{FileAccess, ToolRegistry};
use anyhow::Result as AnyhowResult;
use arc_swap::{ArcSwap, ArcSwapOption};
use async_trait::async_trait;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::event_sink::EventSink;
use crate::events::AgentEventKind;

/// How the guard reacts to a flagged batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ParallelEditMode {
    /// Run every call and advise the model before its next request.
    #[default]
    Warn,
    /// Run only the first writer per file and reject the rest.
    Deny,
}

impl ParallelEditMode {
    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "warn" => Some(Self::Warn),
            "deny" => Some(Self::Deny),
            _ => None,
        }
    }
}

/// A file written by two or more calls of one batch.
#[derive(Debug)]
struct FlaggedFile {
    path: PathBuf,
    /// Number of distinct calls writing this path.
    writers: usize,
}

/// Middleware that detects and reacts to multiple same-file writes per response.
pub struct ParallelEditGuard {
    /// The agent's tool registry. `bind_agent` swaps in the very `Arc` the
    /// scheduler uses, so both always see the same tools.
    registry: ArcSwap<ToolRegistry>,
    /// Where warnings are published for UIs; set by `bind_agent`.
    events: ArcSwapOption<EventSink>,
    mode: ParallelEditMode,
    /// Guidance waiting to be injected, keyed by session id.
    pending: Mutex<HashMap<String, String>>,
}

impl ParallelEditGuard {
    pub fn new(registry: impl Into<Arc<ToolRegistry>>, mode: ParallelEditMode) -> Self {
        Self {
            registry: ArcSwap::new(registry.into()),
            events: ArcSwapOption::empty(),
            mode,
            pending: Mutex::new(HashMap::new()),
        }
    }

    pub fn mode(&self) -> ParallelEditMode {
        self.mode
    }

    /// The registry currently in use (shared, not copied).
    pub fn registry(&self) -> Arc<ToolRegistry> {
        self.registry.load_full()
    }

    /// Tell UIs about a flagged batch. Never waits for the event journal.
    fn notify(&self, session_id: &str, message: String) {
        if let Some(sink) = self.events.load_full() {
            sink.session_stream(session_id)
                .emit(AgentEventKind::HookNotice {
                    event_name: "parallel_edit_guard".to_string(),
                    message,
                    is_error: false,
                });
        }
    }

    /// Distinct written paths of one call, in declaration order.
    fn call_writes(access: &FileAccess) -> Vec<&PathBuf> {
        let mut seen = HashSet::new();
        access
            .written_paths()
            .iter()
            .filter(|path| seen.insert(*path))
            .collect()
    }

    /// Files written by two or more calls, in first-seen order.
    fn detect(accesses: &[&FileAccess]) -> Vec<FlaggedFile> {
        let mut order: Vec<&PathBuf> = Vec::new();
        let mut counts: HashMap<&PathBuf, usize> = HashMap::new();
        for access in accesses {
            for path in Self::call_writes(access) {
                let count = counts.entry(path).or_insert(0);
                if *count == 0 {
                    order.push(path);
                }
                *count += 1;
            }
        }
        order
            .into_iter()
            .filter_map(|path| {
                let writers = counts[path];
                (writers >= 2).then(|| FlaggedFile {
                    path: path.clone(),
                    writers,
                })
            })
            .collect()
    }

    fn display_path(path: &Path, cwd: Option<&Path>) -> String {
        if let Some(cwd) = cwd {
            if let Ok(rel) = path.strip_prefix(cwd) {
                return rel.display().to_string();
            }
            if let Ok(canonical) = dunce::canonicalize(cwd)
                && let Ok(rel) = path.strip_prefix(canonical)
            {
                return rel.display().to_string();
            }
        }
        path.display().to_string()
    }

    /// Short `file (N writes)` list for UI notices.
    fn summary(flagged: &[FlaggedFile], cwd: Option<&Path>) -> String {
        flagged
            .iter()
            .map(|f| {
                format!(
                    "`{}` ({} writes)",
                    Self::display_path(&f.path, cwd),
                    f.writers
                )
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn warn_note(flagged: &[FlaggedFile], cwd: Option<&Path>) -> String {
        let files = flagged
            .iter()
            .map(|f| {
                format!(
                    "`{}` ({} separate calls)",
                    Self::display_path(&f.path, cwd),
                    f.writers
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "System note: your previous response contained several separate tool calls \
             that modify the same file: {files}. Calls on one file are executed one after \
             another in order, but it is more reliable and cheaper to combine all edits to \
             one file into a single `multiedit` call."
        )
    }

    fn deny_message(flagged: &FlaggedFile, cwd: Option<&Path>) -> String {
        format!(
            "Rejected: {} writes to `{}` in one response. This call was not executed. \
             Combine all edits to this file into a single `multiedit` call.",
            flagged.writers,
            Self::display_path(&flagged.path, cwd)
        )
    }

    fn deny_result(call: &ToolCall, effective_args: &Value, text: String) -> ToolResult {
        ToolResult::new(
            call.id.clone(),
            vec![querymt::chat::ToolResultPart::Text { text }],
            true,
            Some(call.function.name.clone()),
            Some(effective_args.to_string()),
        )
        .with_execution(true, "parallel_edit_guard")
    }

    fn lock_pending(&self) -> std::sync::MutexGuard<'_, HashMap<String, String>> {
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Drop a note that was never delivered (cancelled or ended turn).
    fn discard_pending(&self, state: &ExecutionState) {
        if let Some(context) = state.context() {
            self.lock_pending().remove(&*context.session_id);
        }
    }
}

#[async_trait]
impl MiddlewareDriver for ParallelEditGuard {
    async fn on_turn_start(
        &self,
        state: ExecutionState,
        _runtime: Option<&Arc<SessionRuntime>>,
    ) -> Result<ExecutionState> {
        // A note left over from a cancelled turn must not leak into this one.
        self.discard_pending(&state);
        Ok(state)
    }

    async fn on_turn_end(
        &self,
        state: ExecutionState,
        _runtime: Option<&Arc<SessionRuntime>>,
    ) -> Result<ExecutionState> {
        // Other middleware may turn `Complete` back into `BeforeLlmCall`; only
        // drop the note when the turn really ends.
        if matches!(
            state,
            ExecutionState::Complete { .. }
                | ExecutionState::Stopped { .. }
                | ExecutionState::Cancelled
        ) {
            self.discard_pending(&state);
        }
        Ok(state)
    }

    async fn on_step_start(
        &self,
        state: ExecutionState,
        _runtime: Option<&Arc<SessionRuntime>>,
    ) -> Result<ExecutionState> {
        match state {
            ExecutionState::BeforeLlmCall { context } => {
                let note = self.lock_pending().remove(&*context.session_id);
                match note {
                    Some(note) => Ok(ExecutionState::BeforeLlmCall {
                        context: Arc::new(context.inject_message(note)),
                    }),
                    None => Ok(ExecutionState::BeforeLlmCall { context }),
                }
            }
            other => Ok(other),
        }
    }

    async fn on_prepared_tool_calls(
        &self,
        batch: &PreparedBatch<'_>,
    ) -> Result<Vec<PreparedCallVerdict>> {
        let mut verdicts: Vec<PreparedCallVerdict> = batch
            .calls
            .iter()
            .map(|_| PreparedCallVerdict::Proceed)
            .collect();

        // The accesses were declared for each call's final effective arguments
        // (after normalisation and pre-tool hook updates), so aliased, repaired
        // and hook-redirected targets are judged by where they will write.
        let accesses: Vec<&FileAccess> = batch.calls.iter().map(|c| c.access).collect();
        let flagged = Self::detect(&accesses);
        if flagged.is_empty() {
            return Ok(verdicts);
        }
        let cwd = batch.cwd;

        match self.mode {
            ParallelEditMode::Warn => {
                let note = Self::warn_note(&flagged, cwd);
                self.notify(
                    batch.session_id,
                    format!(
                        "Several calls modified the same file in one response: {}. They ran in order; `multiedit` was recommended.",
                        Self::summary(&flagged, cwd)
                    ),
                );
                self.lock_pending()
                    .insert(batch.session_id.to_string(), note);
            }
            ParallelEditMode::Deny => {
                let flagged_by_path: HashMap<&PathBuf, &FlaggedFile> =
                    flagged.iter().map(|f| (&f.path, f)).collect();
                let mut claimed: HashSet<&PathBuf> = HashSet::new();

                for (prepared, verdict) in batch.calls.iter().zip(verdicts.iter_mut()) {
                    let writes: Vec<&PathBuf> = Self::call_writes(prepared.access)
                        .into_iter()
                        .filter(|path| flagged_by_path.contains_key(path))
                        .collect();
                    if let Some(conflict) = writes.iter().find(|path| claimed.contains(**path)) {
                        *verdict = PreparedCallVerdict::Reject(Box::new(Self::deny_result(
                            prepared.call,
                            prepared.args,
                            Self::deny_message(flagged_by_path[*conflict], cwd),
                        )));
                    } else {
                        claimed.extend(writes);
                    }
                }

                self.notify(
                    batch.session_id,
                    format!(
                        "Rejected redundant same-file writes in one response: {}. Only the first write per file ran.",
                        Self::summary(&flagged, cwd)
                    ),
                );
            }
        }
        Ok(verdicts)
    }

    fn bind_agent(&self, registry: &Arc<ToolRegistry>, events: &Arc<EventSink>) {
        self.registry.store(Arc::clone(registry));
        self.events.store(Some(Arc::clone(events)));
    }

    fn reset(&self) {
        self.lock_pending().clear();
    }

    fn name(&self) -> &'static str {
        "ParallelEditGuard"
    }
}

/// Factory for creating [`ParallelEditGuard`] from config.
pub struct ParallelEditGuardFactory;

impl MiddlewareFactory for ParallelEditGuardFactory {
    fn type_name(&self) -> &'static str {
        "parallel_edit_guard"
    }

    fn create(
        &self,
        config: &Value,
        agent_config: &crate::agent::agent_config::AgentConfig,
    ) -> AnyhowResult<Arc<dyn MiddlewareDriver>> {
        let enabled = config
            .get("enabled")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        if !enabled {
            return Err(anyhow::anyhow!("Middleware disabled"));
        }

        let mode = match config.get("mode") {
            None => ParallelEditMode::default(),
            Some(value) => value
                .as_str()
                .and_then(ParallelEditMode::parse)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "Invalid parallel_edit_guard mode {value}: expected \"warn\" or \"deny\""
                    )
                })?,
        };

        Ok(Arc::new(ParallelEditGuard::new(
            agent_config.tool_registry.clone(),
            mode,
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::middleware::{AgentStats, ConversationContext, ToolFunction};
    use crate::tools::builtins::{EditTool, MultiEditTool, ReadTool, ShellTool};
    use serde_json::json;
    use tempfile::TempDir;

    fn registry() -> ToolRegistry {
        let mut registry = ToolRegistry::new();
        registry.add(Arc::new(EditTool::new()));
        registry.add(Arc::new(MultiEditTool::new()));
        registry.add(Arc::new(ShellTool::new()));
        registry.add(Arc::new(ReadTool::new()));
        registry
    }

    fn context() -> Arc<ConversationContext> {
        Arc::new(ConversationContext::new(
            "s1".into(),
            Arc::from([]),
            Arc::new(AgentStats::default()),
            "mock".into(),
            "mock".into(),
        ))
    }

    fn call(id: &str, name: &str, args: Value) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            function: ToolFunction {
                name: name.to_string(),
                arguments: args.to_string(),
            },
            invalid_input: false,
        }
    }

    fn edit(id: &str, path: &Path) -> ToolCall {
        call(
            id,
            "edit",
            json!({"file_path": path.to_string_lossy(), "old_string": "a", "new_string": "b"}),
        )
    }

    /// Effective arguments of a test call (its arguments are already canonical).
    fn effective_args(call: &ToolCall) -> Value {
        serde_json::from_str(&call.function.arguments).unwrap()
    }

    /// Declare access for `calls` like the scheduler does (from effective
    /// arguments) and run the guard's prepared-call review for `session_id`.
    async fn review_in(
        guard: &ParallelEditGuard,
        session_id: &str,
        calls: &[ToolCall],
    ) -> Vec<PreparedCallVerdict> {
        let args: Vec<Value> = calls.iter().map(effective_args).collect();
        let accesses = guard
            .registry()
            .file_access_batch(
                calls
                    .iter()
                    .zip(&args)
                    .map(|(call, args)| (call.function.name.clone(), args.clone()))
                    .collect(),
                None,
            )
            .await;
        let views: Vec<crate::middleware::PreparedToolCall<'_>> = calls
            .iter()
            .zip(&args)
            .zip(&accesses)
            .map(
                |((call, args), access)| crate::middleware::PreparedToolCall { call, args, access },
            )
            .collect();
        guard
            .on_prepared_tool_calls(&PreparedBatch {
                session_id,
                cwd: None,
                calls: &views,
            })
            .await
            .unwrap()
    }

    async fn review(guard: &ParallelEditGuard, calls: &[ToolCall]) -> Vec<PreparedCallVerdict> {
        review_in(guard, "s1", calls).await
    }

    /// `(ids that proceed, ids rejected with their result)`.
    fn split<'a>(
        calls: &'a [ToolCall],
        verdicts: Vec<PreparedCallVerdict>,
    ) -> (Vec<&'a str>, Vec<(&'a str, ToolResult)>) {
        let mut proceed = Vec::new();
        let mut rejected = Vec::new();
        for (call, verdict) in calls.iter().zip(verdicts) {
            match verdict {
                PreparedCallVerdict::Proceed => proceed.push(call.id.as_str()),
                PreparedCallVerdict::Reject(result) => rejected.push((call.id.as_str(), *result)),
            }
        }
        (proceed, rejected)
    }

    fn temp_root() -> (TempDir, PathBuf) {
        let dir = TempDir::new().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        (dir, root)
    }

    async fn detect_in(guard: &ParallelEditGuard, calls: &[ToolCall]) -> Vec<FlaggedFile> {
        let args: Vec<Value> = calls.iter().map(effective_args).collect();
        let accesses = guard
            .registry()
            .file_access_batch(
                calls
                    .iter()
                    .zip(&args)
                    .map(|(call, args)| (call.function.name.clone(), args.clone()))
                    .collect(),
                None,
            )
            .await;
        let refs: Vec<&FileAccess> = accesses.iter().collect();
        ParallelEditGuard::detect(&refs)
    }

    #[tokio::test]
    async fn detects_two_edits_to_same_file() {
        let (_dir, root) = temp_root();
        let guard = ParallelEditGuard::new(registry(), ParallelEditMode::Warn);
        let calls = vec![
            edit("1", &root.join("design.md")),
            edit("2", &root.join("./design.md")),
        ];

        let flagged = detect_in(&guard, &calls).await;

        assert_eq!(flagged.len(), 1);
        assert_eq!(flagged[0].path, root.join("design.md"));
        assert_eq!(flagged[0].writers, 2);
    }

    #[tokio::test]
    async fn does_not_flag_different_files() {
        let (_dir, root) = temp_root();
        let guard = ParallelEditGuard::new(registry(), ParallelEditMode::Warn);
        let calls = vec![edit("1", &root.join("a.md")), edit("2", &root.join("b.md"))];

        assert!(detect_in(&guard, &calls).await.is_empty());
    }

    #[tokio::test]
    async fn shell_and_reads_are_not_counted() {
        let (_dir, root) = temp_root();
        let guard = ParallelEditGuard::new(registry(), ParallelEditMode::Warn);
        let calls = vec![
            edit("1", &root.join("a.md")),
            call("2", "shell", json!({"command": "echo hi"})),
            call(
                "3",
                "read_tool",
                json!({"path": root.join("a.md").to_string_lossy()}),
            ),
        ];

        assert!(detect_in(&guard, &calls).await.is_empty());
    }

    #[tokio::test]
    async fn warn_mode_passes_state_through_and_injects_once_after_results() {
        let (_dir, root) = temp_root();
        let guard = ParallelEditGuard::new(registry(), ParallelEditMode::Warn);
        let calls = vec![
            edit("1", &root.join("design.md")),
            edit("2", &root.join("design.md")),
        ];

        let (proceed, rejected) = split(&calls, review(&guard, &calls).await);
        assert_eq!(proceed, ["1", "2"], "warn mode runs every call");
        assert!(rejected.is_empty());

        // The step after the tool results gets the note appended as the last message.
        let before = ExecutionState::BeforeLlmCall { context: context() };
        let out = guard.on_step_start(before, None).await.unwrap();
        let ExecutionState::BeforeLlmCall { context } = out else {
            panic!("expected BeforeLlmCall");
        };
        assert_eq!(context.messages.len(), 1);
        let text = context.messages.last().unwrap().text();
        assert!(text.contains("design.md"), "{text}");
        assert!(text.contains("multiedit"), "{text}");

        // Second request: nothing more is injected.
        let before = ExecutionState::BeforeLlmCall {
            context: context.clone(),
        };
        let ExecutionState::BeforeLlmCall { context: again } =
            guard.on_step_start(before, None).await.unwrap()
        else {
            panic!("expected BeforeLlmCall");
        };
        assert_eq!(again.messages.len(), 1);
    }

    #[tokio::test]
    async fn warn_mode_without_conflict_injects_nothing() {
        let (_dir, root) = temp_root();
        let guard = ParallelEditGuard::new(registry(), ParallelEditMode::Warn);
        let calls = vec![edit("1", &root.join("a.md")), edit("2", &root.join("b.md"))];
        review(&guard, &calls).await;

        let before = ExecutionState::BeforeLlmCall { context: context() };
        let ExecutionState::BeforeLlmCall { context } =
            guard.on_step_start(before, None).await.unwrap()
        else {
            panic!("expected BeforeLlmCall");
        };
        assert!(context.messages.is_empty());
    }

    #[tokio::test]
    async fn reset_clears_pending_note() {
        let (_dir, root) = temp_root();
        let guard = ParallelEditGuard::new(registry(), ParallelEditMode::Warn);
        flag_same_file(&guard, &root).await;
        guard.reset();

        let before = ExecutionState::BeforeLlmCall { context: context() };
        let ExecutionState::BeforeLlmCall { context } =
            guard.on_step_start(before, None).await.unwrap()
        else {
            panic!("expected BeforeLlmCall");
        };
        assert!(context.messages.is_empty());
    }

    /// Review a pair whose *raw* arguments and *effective* access disagree.
    async fn review_with_access(
        guard: &ParallelEditGuard,
        raw_paths: [&Path; 2],
        effective_paths: [&Path; 2],
    ) -> Vec<PreparedCallVerdict> {
        let calls = [edit("1", raw_paths[0]), edit("2", raw_paths[1])];
        let args: Vec<Value> = calls.iter().map(effective_args).collect();
        let accesses: Vec<FileAccess> = effective_paths
            .iter()
            .map(|path| FileAccess::writes([path.to_path_buf()]))
            .collect();
        let views: Vec<crate::middleware::PreparedToolCall<'_>> = calls
            .iter()
            .zip(&args)
            .zip(&accesses)
            .map(
                |((call, args), access)| crate::middleware::PreparedToolCall { call, args, access },
            )
            .collect();
        guard
            .on_prepared_tool_calls(&PreparedBatch {
                session_id: "s1",
                cwd: None,
                calls: &views,
            })
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn conflicts_are_judged_by_effective_access_not_raw_arguments() {
        let (_dir, root) = temp_root();
        let a = root.join("a.md");
        let b = root.join("b.md");

        // Raw arguments differ, but a hook redirected both calls to `a.md`.
        let guard = ParallelEditGuard::new(registry(), ParallelEditMode::Deny);
        let verdicts = review_with_access(&guard, [&a, &b], [&a, &a]).await;
        assert!(matches!(verdicts[0], PreparedCallVerdict::Proceed));
        let PreparedCallVerdict::Reject(rejected) = &verdicts[1] else {
            panic!("redirected same-file writer must be rejected");
        };
        assert!(rejected.is_error);
        assert!(rejected.text_content().contains("a.md"));
        // The rejection records the effective arguments of the rejected call.
        assert_eq!(
            serde_json::from_str::<Value>(rejected.tool_arguments.as_deref().unwrap()).unwrap()["file_path"],
            json!(b.to_string_lossy())
        );

        // Raw arguments name the same file, but the effective targets differ.
        let guard = ParallelEditGuard::new(registry(), ParallelEditMode::Deny);
        let verdicts = review_with_access(&guard, [&a, &a], [&a, &b]).await;
        assert!(
            verdicts
                .iter()
                .all(|v| matches!(v, PreparedCallVerdict::Proceed))
        );
    }

    #[tokio::test]
    async fn warn_mode_notes_use_the_session_of_the_batch() {
        let (_dir, root) = temp_root();
        let guard = ParallelEditGuard::new(registry(), ParallelEditMode::Warn);
        let calls = vec![edit("1", &root.join("a.md")), edit("2", &root.join("a.md"))];
        review_in(&guard, "other-session", &calls).await;

        // The note is delivered to the batch's session only.
        assert_eq!(injected_messages(&guard).await, 0);
        let before = ExecutionState::BeforeLlmCall {
            context: Arc::new(ConversationContext::new(
                "other-session".into(),
                Arc::from([]),
                Arc::new(AgentStats::default()),
                "mock".into(),
                "mock".into(),
            )),
        };
        let ExecutionState::BeforeLlmCall { context } =
            guard.on_step_start(before, None).await.unwrap()
        else {
            panic!("expected BeforeLlmCall");
        };
        assert_eq!(context.messages.len(), 1);
    }

    async fn flag_same_file(guard: &ParallelEditGuard, root: &Path) {
        let calls = vec![edit("1", &root.join("a.md")), edit("2", &root.join("a.md"))];
        review(guard, &calls).await;
    }

    async fn injected_messages(guard: &ParallelEditGuard) -> usize {
        let before = ExecutionState::BeforeLlmCall { context: context() };
        let ExecutionState::BeforeLlmCall { context } =
            guard.on_step_start(before, None).await.unwrap()
        else {
            panic!("expected BeforeLlmCall");
        };
        context.messages.len()
    }

    #[tokio::test]
    async fn pending_note_is_dropped_at_turn_start_and_turn_end() {
        let (_dir, root) = temp_root();
        let guard = ParallelEditGuard::new(registry(), ParallelEditMode::Warn);

        // Turn start drops a note left by a cancelled turn.
        flag_same_file(&guard, &root).await;
        let start = ExecutionState::BeforeLlmCall { context: context() };
        guard.on_turn_start(start, None).await.unwrap();
        assert_eq!(injected_messages(&guard).await, 0);

        // Turn end (Complete) drops an undelivered note.
        flag_same_file(&guard, &root).await;
        let end = ExecutionState::Complete { context: context() };
        guard.on_turn_end(end, None).await.unwrap();
        assert_eq!(injected_messages(&guard).await, 0);

        // A BeforeLlmCall at turn end (another middleware asked to continue) keeps it.
        flag_same_file(&guard, &root).await;
        let cont = ExecutionState::BeforeLlmCall { context: context() };
        guard.on_turn_end(cont, None).await.unwrap();
        assert_eq!(injected_messages(&guard).await, 1);
    }

    #[tokio::test]
    async fn bind_agent_replaces_the_registry() {
        let (_dir, root) = temp_root();
        // Starts with no tools: nothing is detected.
        let guard = ParallelEditGuard::new(ToolRegistry::new(), ParallelEditMode::Warn);
        let calls = vec![edit("1", &root.join("a.md")), edit("2", &root.join("a.md"))];
        assert!(detect_in(&guard, &calls).await.is_empty());

        let agent = crate::test_utils::TestAgent::new().await;
        guard.bind_agent(&Arc::new(registry()), &agent.config.event_sink);
        assert_eq!(detect_in(&guard, &calls).await.len(), 1);
    }

    #[tokio::test]
    async fn bind_agent_shares_the_configs_registry_arc() {
        let agent = crate::test_utils::TestAgent::new().await;
        let guard = ParallelEditGuard::new(ToolRegistry::new(), ParallelEditMode::Warn);
        guard.bind_agent(&agent.config.tool_registry, &agent.config.event_sink);
        assert!(Arc::ptr_eq(&guard.registry(), &agent.config.tool_registry));
    }

    #[tokio::test]
    async fn builder_binds_the_same_registry_arc_into_the_guard() {
        let agent = crate::test_utils::TestAgent::new().await;
        let guard = Arc::new(ParallelEditGuard::new(
            ToolRegistry::new(),
            ParallelEditMode::Warn,
        ));
        let config = (*agent.config)
            .clone()
            .with_middleware(vec![guard.clone() as Arc<dyn MiddlewareDriver>]);
        assert!(Arc::ptr_eq(&guard.registry(), &config.tool_registry));
    }

    /// Run a flagged batch through a guard bound to a real event sink and
    /// return the `parallel_edit_guard` notice published for the session.
    async fn flagged_notice(mode: ParallelEditMode) -> String {
        let (_dir, root) = temp_root();
        let agent = crate::test_utils::TestAgent::new().await;
        let session_id = agent.create_session().await;
        let mut rx = agent.config.event_sink.fanout().subscribe();

        let guard = ParallelEditGuard::new(registry(), mode);
        guard.bind_agent(&Arc::new(registry()), &agent.config.event_sink);
        let calls = vec![edit("1", &root.join("a.md")), edit("2", &root.join("a.md"))];
        // Returns before the event is persisted: emission is detached.
        review_in(&guard, session_id.as_str(), &calls).await;

        let deadline = std::time::Duration::from_secs(5);
        loop {
            let envelope = tokio::time::timeout(deadline, rx.recv())
                .await
                .expect("notice published")
                .expect("fanout open");
            if let AgentEventKind::HookNotice {
                event_name,
                message,
                is_error,
            } = envelope.kind()
                && event_name == "parallel_edit_guard"
            {
                assert_eq!(envelope.session_id(), session_id);
                assert!(envelope.is_durable());
                assert!(!is_error);
                return message.clone();
            }
        }
    }

    #[tokio::test]
    async fn warn_mode_publishes_a_durable_ui_notice() {
        let message = flagged_notice(ParallelEditMode::Warn).await;
        assert!(message.contains("a.md"), "{message}");
        assert!(message.contains("2 writes"), "{message}");
        assert!(!message.starts_with("System note"), "{message}");
    }

    #[tokio::test]
    async fn deny_mode_publishes_a_durable_ui_notice() {
        let message = flagged_notice(ParallelEditMode::Deny).await;
        assert!(message.contains("Rejected"), "{message}");
        assert!(message.contains("a.md"), "{message}");
    }

    #[tokio::test]
    async fn deny_mode_rejects_later_writers_and_keeps_unaffected_calls() {
        let (_dir, root) = temp_root();
        let guard = ParallelEditGuard::new(registry(), ParallelEditMode::Deny);
        let calls = vec![
            edit("1", &root.join("design.md")),
            edit("2", &root.join("design.md")),
            edit("3", &root.join("other.md")),
            call("4", "shell", json!({"command": "echo hi"})),
            edit("5", &root.join("design.md")),
        ];

        let (kept, rejected) = split(&calls, review(&guard, &calls).await);

        assert_eq!(kept, ["1", "3", "4"]);
        let rejected_ids: Vec<&str> = rejected.iter().map(|(id, _)| *id).collect();
        assert_eq!(rejected_ids, ["2", "5"]);
        for (id, result) in &rejected {
            assert_eq!(&result.call_id, id);
            assert!(result.is_error);
            let text = result.text_content();
            assert!(text.contains("design.md"), "{text}");
            assert!(text.contains("multiedit"), "{text}");
        }
        // Exactly one verdict (proceed or reject) per original call.
        assert_eq!(kept.len() + rejected.len(), 5);
    }

    #[tokio::test]
    async fn deny_mode_without_conflict_is_a_no_op() {
        let (_dir, root) = temp_root();
        let guard = ParallelEditGuard::new(registry(), ParallelEditMode::Deny);
        let calls = vec![edit("1", &root.join("a.md")), edit("2", &root.join("b.md"))];

        let (kept, rejected) = split(&calls, review(&guard, &calls).await);
        assert_eq!(kept, ["1", "2"]);
        assert!(rejected.is_empty());
    }

    #[test]
    fn mode_parsing() {
        assert_eq!(
            ParallelEditMode::parse("warn"),
            Some(ParallelEditMode::Warn)
        );
        assert_eq!(
            ParallelEditMode::parse("deny"),
            Some(ParallelEditMode::Deny)
        );
        assert_eq!(ParallelEditMode::parse("block"), None);
    }

    async fn run_factory(config: Value) -> AnyhowResult<Arc<dyn MiddlewareDriver>> {
        let agent = crate::test_utils::TestAgent::new().await;
        ParallelEditGuardFactory.create(&config, &agent.config)
    }

    /// Whether the driver rejects the second same-file edit (deny) or not (warn).
    async fn denies_second_edit(driver: &Arc<dyn MiddlewareDriver>) -> bool {
        let (_dir, root) = temp_root();
        let calls = vec![edit("1", &root.join("a.md")), edit("2", &root.join("a.md"))];
        let args: Vec<Value> = calls.iter().map(effective_args).collect();
        let accesses = Arc::new(registry())
            .file_access_batch(
                calls
                    .iter()
                    .zip(&args)
                    .map(|(call, args)| (call.function.name.clone(), args.clone()))
                    .collect(),
                None,
            )
            .await;
        let views: Vec<crate::middleware::PreparedToolCall<'_>> = calls
            .iter()
            .zip(&args)
            .zip(&accesses)
            .map(
                |((call, args), access)| crate::middleware::PreparedToolCall { call, args, access },
            )
            .collect();
        let verdicts = driver
            .on_prepared_tool_calls(&PreparedBatch {
                session_id: "s1",
                cwd: None,
                calls: &views,
            })
            .await
            .unwrap();
        matches!(verdicts[0], PreparedCallVerdict::Proceed)
            && matches!(verdicts[1], PreparedCallVerdict::Reject(_))
    }

    #[tokio::test]
    async fn factory_defaults_to_warn() {
        let driver = run_factory(json!({})).await.unwrap();
        assert_eq!(driver.name(), "ParallelEditGuard");
        assert!(!denies_second_edit(&driver).await);
    }

    #[tokio::test]
    async fn factory_explicit_deny() {
        let driver = run_factory(json!({"mode": "deny"})).await.unwrap();
        assert!(denies_second_edit(&driver).await);
    }

    #[tokio::test]
    async fn factory_explicit_warn() {
        let driver = run_factory(json!({"mode": "warn"})).await.unwrap();
        assert!(!denies_second_edit(&driver).await);
    }

    #[tokio::test]
    async fn factory_rejects_invalid_mode() {
        for bad in [json!({"mode": "block"}), json!({"mode": 3})] {
            let err = run_factory(bad)
                .await
                .err()
                .expect("invalid mode must fail");
            let msg = err.to_string();
            assert!(msg.contains("mode"), "{msg}");
            assert!(!msg.contains("disabled"), "{msg}");
        }
    }

    #[tokio::test]
    async fn factory_disabled_returns_disabled_error() {
        let err = run_factory(json!({"enabled": false}))
            .await
            .err()
            .expect("disabled must fail");
        assert!(err.to_string().contains("disabled"));
    }

    #[test]
    fn registry_has_parallel_edit_guard() {
        let types = crate::middleware::MIDDLEWARE_REGISTRY.type_names();
        assert!(types.contains(&"parallel_edit_guard"));
    }
}
