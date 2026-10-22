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
//! File access is read from the agent's *live* tool registry (bound through
//! [`MiddlewareDriver::bind_agent`] when the config is assembled), so
//! the guard and the scheduler always agree on which tools exist and which
//! paths they write.
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
use crate::middleware::{ExecutionState, MiddlewareDriver, Result, ToolCall, ToolResult};
use crate::tools::{FileAccess, ToolRegistry};
use anyhow::Result as AnyhowResult;
use async_trait::async_trait;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

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
    /// The agent's tool registry; replaced by the live one in `bind_agent`.
    registry: RwLock<ToolRegistry>,
    /// Where warnings are published for UIs; set by `bind_agent`.
    events: RwLock<Option<Arc<EventSink>>>,
    mode: ParallelEditMode,
    /// Guidance waiting to be injected, keyed by session id.
    pending: Mutex<HashMap<String, String>>,
}

impl ParallelEditGuard {
    pub fn new(registry: ToolRegistry, mode: ParallelEditMode) -> Self {
        Self {
            registry: RwLock::new(registry),
            events: RwLock::new(None),
            mode,
            pending: Mutex::new(HashMap::new()),
        }
    }

    pub fn mode(&self) -> ParallelEditMode {
        self.mode
    }

    fn registry_snapshot(&self) -> ToolRegistry {
        self.registry
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Tell UIs about a flagged batch (`HookNotice`, like other middleware warnings).
    async fn notify(&self, session_id: &str, message: String) {
        let sink = self
            .events
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let Some(sink) = sink else { return };
        if let Err(error) = sink
            .emit_durable(
                session_id,
                AgentEventKind::HookNotice {
                    event_name: "parallel_edit_guard".to_string(),
                    message,
                    is_error: false,
                },
            )
            .await
        {
            log::warn!("parallel_edit_guard: failed to emit notice: {error}");
        }
    }

    /// Declared file access of every call (already path-normalised by the
    /// tools), evaluated once off the async executor.
    async fn accesses(&self, calls: &[ToolCall], cwd: Option<&Path>) -> Vec<FileAccess> {
        self.registry_snapshot()
            .file_access_batch(
                calls
                    .iter()
                    .map(|call| (call.function.name.clone(), call.function.arguments.clone()))
                    .collect(),
                cwd.map(Path::to_path_buf),
            )
            .await
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
    fn detect(accesses: &[FileAccess]) -> Vec<FlaggedFile> {
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

    fn deny_result(call: &ToolCall, text: String) -> ToolResult {
        ToolResult::new(
            call.id.clone(),
            vec![querymt::chat::ToolResultPart::Text { text }],
            true,
            Some(call.function.name.clone()),
            Some(call.function.arguments.clone()),
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

    async fn on_processing_tool_calls(
        &self,
        state: ExecutionState,
        runtime: Option<&Arc<SessionRuntime>>,
    ) -> Result<ExecutionState> {
        let ExecutionState::ProcessingToolCalls {
            remaining_calls,
            results,
            context,
        } = state
        else {
            return Ok(state);
        };

        // `results` may already hold outcomes for calls that never reach the
        // scheduler (e.g. calls gated for invalid arguments). They do not change
        // which of the remaining calls write the same file, so every batch is
        // inspected.
        let cwd = runtime.and_then(|r| r.cwd.as_deref());
        let accesses = self.accesses(&remaining_calls, cwd).await;
        let flagged = Self::detect(&accesses);
        if flagged.is_empty() {
            return Ok(ExecutionState::ProcessingToolCalls {
                remaining_calls,
                results,
                context,
            });
        }

        match self.mode {
            ParallelEditMode::Warn => {
                let note = Self::warn_note(&flagged, cwd);
                self.notify(&context.session_id, note.clone()).await;
                self.lock_pending()
                    .insert(context.session_id.to_string(), note);
                Ok(ExecutionState::ProcessingToolCalls {
                    remaining_calls,
                    results,
                    context,
                })
            }
            ParallelEditMode::Deny => {
                let flagged_by_path: HashMap<&PathBuf, &FlaggedFile> =
                    flagged.iter().map(|f| (&f.path, f)).collect();
                let mut claimed: HashSet<&PathBuf> = HashSet::new();
                let mut kept: Vec<ToolCall> = Vec::with_capacity(remaining_calls.len());
                let mut all_results: Vec<ToolResult> = results.to_vec();

                for (call, access) in remaining_calls.iter().zip(&accesses) {
                    let writes: Vec<&PathBuf> = Self::call_writes(access)
                        .into_iter()
                        .filter(|path| flagged_by_path.contains_key(path))
                        .collect();
                    if let Some(conflict) = writes.iter().find(|path| claimed.contains(**path)) {
                        all_results.push(Self::deny_result(
                            call,
                            Self::deny_message(flagged_by_path[*conflict], cwd),
                        ));
                    } else {
                        claimed.extend(writes);
                        kept.push(call.clone());
                    }
                }

                let summary = flagged
                    .iter()
                    .map(|f| {
                        format!(
                            "`{}` ({} writes)",
                            Self::display_path(&f.path, cwd),
                            f.writers
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                self.notify(
                    &context.session_id,
                    format!("Rejected redundant same-file writes in one response: {summary}"),
                )
                .await;

                Ok(ExecutionState::ProcessingToolCalls {
                    remaining_calls: Arc::from(kept.into_boxed_slice()),
                    results: Arc::from(all_results.into_boxed_slice()),
                    context,
                })
            }
        }
    }

    fn bind_agent(&self, registry: &ToolRegistry, events: &Arc<EventSink>) {
        *self
            .registry
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = registry.clone();
        *self
            .events
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(events.clone());
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
        }
    }

    fn edit(id: &str, path: &Path) -> ToolCall {
        call(
            id,
            "edit",
            json!({"filePath": path.to_string_lossy(), "oldString": "a", "newString": "b"}),
        )
    }

    fn batch(calls: Vec<ToolCall>) -> ExecutionState {
        ExecutionState::ProcessingToolCalls {
            remaining_calls: Arc::from(calls.into_boxed_slice()),
            results: Arc::from([]),
            context: context(),
        }
    }

    fn unpack(state: ExecutionState) -> (Vec<ToolCall>, Vec<ToolResult>) {
        match state {
            ExecutionState::ProcessingToolCalls {
                remaining_calls,
                results,
                ..
            } => (remaining_calls.to_vec(), results.to_vec()),
            other => panic!("unexpected state: {other:?}"),
        }
    }

    fn temp_root() -> (TempDir, PathBuf) {
        let dir = TempDir::new().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        (dir, root)
    }

    async fn detect_in(guard: &ParallelEditGuard, calls: &[ToolCall]) -> Vec<FlaggedFile> {
        let accesses = guard.accesses(calls, None).await;
        ParallelEditGuard::detect(&accesses)
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
        let state = batch(vec![
            edit("1", &root.join("design.md")),
            edit("2", &root.join("design.md")),
        ]);

        let out = guard.on_processing_tool_calls(state, None).await.unwrap();
        let (calls, results) = unpack(out);
        assert_eq!(calls.len(), 2);
        assert!(results.is_empty());

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
        let state = batch(vec![
            edit("1", &root.join("a.md")),
            edit("2", &root.join("b.md")),
        ]);
        guard.on_processing_tool_calls(state, None).await.unwrap();

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
        let state = batch(vec![
            edit("1", &root.join("a.md")),
            edit("2", &root.join("a.md")),
        ]);
        guard.on_processing_tool_calls(state, None).await.unwrap();
        guard.reset();

        let before = ExecutionState::BeforeLlmCall { context: context() };
        let ExecutionState::BeforeLlmCall { context } =
            guard.on_step_start(before, None).await.unwrap()
        else {
            panic!("expected BeforeLlmCall");
        };
        assert!(context.messages.is_empty());
    }

    #[tokio::test]
    async fn batches_with_prefilled_results_are_still_inspected_in_warn_mode() {
        // e.g. one call was gated for invalid arguments before the batch got here.
        let (_dir, root) = temp_root();
        let guard = ParallelEditGuard::new(registry(), ParallelEditMode::Warn);
        let gated = ToolResult::new("0".into(), vec![], true, None, None);
        let state = ExecutionState::ProcessingToolCalls {
            remaining_calls: Arc::from(vec![
                edit("1", &root.join("a.md")),
                edit("2", &root.join("a.md")),
            ]),
            results: Arc::from(vec![gated]),
            context: context(),
        };

        let (calls, results) = unpack(guard.on_processing_tool_calls(state, None).await.unwrap());
        assert_eq!(calls.len(), 2);
        assert_eq!(results.len(), 1);

        let before = ExecutionState::BeforeLlmCall { context: context() };
        let ExecutionState::BeforeLlmCall { context } =
            guard.on_step_start(before, None).await.unwrap()
        else {
            panic!("expected BeforeLlmCall");
        };
        assert_eq!(context.messages.len(), 1);
    }

    #[tokio::test]
    async fn deny_mode_keeps_prefilled_results_and_appends_rejections() {
        let (_dir, root) = temp_root();
        let guard = ParallelEditGuard::new(registry(), ParallelEditMode::Deny);
        let gated = ToolResult::new("0".into(), vec![], true, None, None);
        let state = ExecutionState::ProcessingToolCalls {
            remaining_calls: Arc::from(vec![
                edit("1", &root.join("a.md")),
                edit("2", &root.join("a.md")),
            ]),
            results: Arc::from(vec![gated]),
            context: context(),
        };

        let (calls, results) = unpack(guard.on_processing_tool_calls(state, None).await.unwrap());
        assert_eq!(
            calls.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            ["1"]
        );
        assert_eq!(
            results
                .iter()
                .map(|r| r.call_id.as_str())
                .collect::<Vec<_>>(),
            ["0", "2"]
        );
        assert!(results[1].text_content().contains("2 writes"));
    }

    async fn flag_same_file(guard: &ParallelEditGuard, root: &Path) {
        let state = batch(vec![
            edit("1", &root.join("a.md")),
            edit("2", &root.join("a.md")),
        ]);
        guard.on_processing_tool_calls(state, None).await.unwrap();
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
        guard.bind_agent(&registry(), &agent.config.event_sink);
        assert_eq!(detect_in(&guard, &calls).await.len(), 1);
    }

    #[tokio::test]
    async fn deny_mode_rejects_later_writers_and_keeps_unaffected_calls() {
        let (_dir, root) = temp_root();
        let guard = ParallelEditGuard::new(registry(), ParallelEditMode::Deny);
        let state = batch(vec![
            edit("1", &root.join("design.md")),
            edit("2", &root.join("design.md")),
            edit("3", &root.join("other.md")),
            call("4", "shell", json!({"command": "echo hi"})),
            edit("5", &root.join("design.md")),
        ]);

        let (calls, results) = unpack(guard.on_processing_tool_calls(state, None).await.unwrap());

        let kept: Vec<&str> = calls.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(kept, ["1", "3", "4"]);
        let rejected: Vec<&str> = results.iter().map(|r| r.call_id.as_str()).collect();
        assert_eq!(rejected, ["2", "5"]);
        for result in &results {
            assert!(result.is_error);
            let text = result.text_content();
            assert!(text.contains("design.md"), "{text}");
            assert!(text.contains("multiedit"), "{text}");
        }
        // Exactly one outcome (run or rejected) per original call.
        assert_eq!(calls.len() + results.len(), 5);
    }

    #[tokio::test]
    async fn deny_mode_without_conflict_is_a_no_op() {
        let (_dir, root) = temp_root();
        let guard = ParallelEditGuard::new(registry(), ParallelEditMode::Deny);
        let state = batch(vec![
            edit("1", &root.join("a.md")),
            edit("2", &root.join("b.md")),
        ]);

        let (calls, results) = unpack(guard.on_processing_tool_calls(state, None).await.unwrap());
        assert_eq!(calls.len(), 2);
        assert!(results.is_empty());
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
        let state = batch(vec![
            edit("1", &root.join("a.md")),
            edit("2", &root.join("a.md")),
        ]);
        let (calls, results) = unpack(driver.on_processing_tool_calls(state, None).await.unwrap());
        calls.len() == 1 && results.len() == 1
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
