//! Duplicate-code review through the real run loop: prompts go through a
//! session actor, tool calls change real files, and the review reads a real
//! workspace index (spec `duplicate-code-review`).

use crate::middleware::{
    DedupCheckMiddleware, EndVerdict, HookCx, LlmResponse, Middleware, ReplyVerdict, Result,
    TurnInfo, TurnMiddleware,
};
use crate::test_utils::{
    MockChatResponse, MockLlmProvider, SharedLlmProvider, UndoTestFixture, mock_querymt_tool_call,
};
use crate::tools::{Tool, builtins::WriteFileTool};
use async_trait::async_trait;
use querymt::chat::ChatMessage;
use querymt_agent_core::model::{MessagePart, NoticeSource};
use querymt_agent_core::session::backend::StorageBackend;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::Notify;

/// A function long enough for the index (`min_lines = 5`).
const INDEXED_BODY: &str = "fn compute_total(items: &[u32]) -> u32 {
    let mut total = 0;
    for item in items {
        if *item > 10 {
            total += item * 2;
        } else {
            total += item;
        }
    }
    total
}
";

/// The same body under another name: above the 0.8 threshold.
const DUPLICATE_BODY: &str = "fn sum_everything(items: &[u32]) -> u32 {
    let mut total = 0;
    for item in items {
        if *item > 10 {
            total += item * 2;
        } else {
            total += item;
        }
    }
    total
}
";

/// A body that resembles nothing indexed.
const UNRELATED_BODY: &str = "fn render_banner(title: &str) -> String {
    let mut out = String::new();
    out.push_str(\"==== \");
    out.push_str(&title.to_uppercase());
    out.push_str(\" ====\");
    out.push('\\n');
    out
}
";

/// Every request the provider received, in order.
type RequestLog = Arc<Mutex<Vec<Vec<ChatMessage>>>>;

/// A provider that answers a request whose last message contains `marker`
/// with one `write_file` call, and every other request with plain text.
fn writing_provider(script: &[(&str, &str, &str)]) -> (SharedLlmProvider, RequestLog) {
    let script: Vec<(String, String, String)> = script
        .iter()
        .map(|(marker, path, content)| (marker.to_string(), path.to_string(), content.to_string()))
        .collect();
    let log: RequestLog = Arc::default();
    let respond = {
        let log = log.clone();
        Arc::new(move |messages: &[ChatMessage]| {
            log.lock().unwrap().push(messages.to_vec());
            let last = messages.last().map(ChatMessage::text).unwrap_or_default();
            match script.iter().find(|(marker, ..)| last.contains(marker)) {
                Some((_, path, content)) => MockChatResponse::with_tools(
                    "writing",
                    vec![mock_querymt_tool_call(
                        &format!("write-{path}"),
                        "write_file",
                        &serde_json::json!({ "path": path, "content": content }).to_string(),
                    )],
                ),
                None => MockChatResponse::text_only("done"),
            }
        })
    };
    let mut mock = MockLlmProvider::new();
    let with_tools = respond.clone();
    mock.expect_chat_with_tools()
        .returning(move |messages, _| Ok(with_tools(messages).into()));
    mock.expect_chat()
        .returning(move |messages| Ok(respond(messages).into()));
    (
        SharedLlmProvider::new(mock, vec![WriteFileTool::new().definition()]),
        log,
    )
}

async fn fixture_with_review(script: &[(&str, &str, &str)]) -> (UndoTestFixture, RequestLog) {
    fixture_with(script, DedupCheckMiddleware::new(), true).await
}

/// A fixture whose agent runs `review`; `indexed` says whether the
/// workspace index is ready when sessions start.
async fn fixture_with(
    script: &[(&str, &str, &str)],
    review: DedupCheckMiddleware,
    indexed: bool,
) -> (UndoTestFixture, RequestLog) {
    let (provider, log) = writing_provider(script);
    let mut fixture =
        UndoTestFixture::with_provider_and_middleware(provider, vec![Arc::new(review)])
            .await
            .expect("fixture");
    fixture
        .write_file("indexed.rs", INDEXED_BODY)
        .expect("seed file");
    if indexed {
        fixture.index_workspace().await.expect("workspace index");
    }
    (fixture, log)
}

async fn new_session(fixture: &UndoTestFixture) -> String {
    new_session_in(fixture, fixture.worktree.path()).await
}

async fn new_session_in(fixture: &UndoTestFixture, cwd: &std::path::Path) -> String {
    let session = fixture.create_session_with_cwd(cwd).await.expect("session");
    bind_llm(fixture, &session).await;
    session
}

/// Binds the mock model to `session` so a prompt can run.
async fn bind_llm(fixture: &UndoTestFixture, session: &str) {
    let store = fixture.storage.session_store();
    let llm_config = store
        .create_or_get_llm_config(
            &querymt::LLMParams::new()
                .provider("mock")
                .model("mock-model"),
        )
        .await
        .expect("llm config");
    store
        .set_session_llm_config(session, llm_config.id)
        .await
        .expect("bind llm config");
}

/// Contents of the `duplicate_code` notices stored in `session`.
async fn duplicate_notices(fixture: &UndoTestFixture, session: &str) -> Vec<String> {
    fixture
        .storage
        .session_store()
        .get_history(session)
        .await
        .expect("history")
        .into_iter()
        .flat_map(|message| message.parts)
        .filter_map(|part| match part {
            MessagePart::Notice {
                source: NoticeSource::DuplicateCode,
                content,
            } => Some(content),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn second_run_of_a_session_is_reviewed() {
    let (fixture, _log) = fixture_with_review(&[
        ("PROMPT_FIRST", "first.rs", DUPLICATE_BODY),
        ("PROMPT_SECOND", "second.rs", DUPLICATE_BODY),
    ])
    .await;
    let session = new_session(&fixture).await;

    fixture
        .prompt_to_completion(&session, "PROMPT_FIRST")
        .await
        .expect("first run");
    assert_eq!(
        duplicate_notices(&fixture, &session).await.len(),
        1,
        "the first run is reviewed"
    );

    fixture
        .prompt_to_completion(&session, "PROMPT_SECOND")
        .await
        .expect("second run");

    assert_eq!(
        duplicate_notices(&fixture, &session).await.len(),
        2,
        "the second run changed files too and is reviewed before it completes"
    );
}

/// Runs `prompt` as a session's only run and returns the stored notices
/// and the number of model requests it took.
async fn single_run(
    fixture: &UndoTestFixture,
    log: &RequestLog,
    prompt: &str,
) -> (Vec<String>, usize) {
    let session = new_session(fixture).await;
    fixture
        .prompt_to_completion(&session, prompt)
        .await
        .expect("run completes without an error");
    let requests = log.lock().unwrap().len();
    (duplicate_notices(fixture, &session).await, requests)
}

#[tokio::test]
async fn run_without_file_changes_is_not_reviewed() {
    let (fixture, log) = fixture_with_review(&[("PROMPT_WRITE", "w.rs", DUPLICATE_BODY)]).await;

    let (notices, requests) = single_run(&fixture, &log, "PROMPT_NOTHING").await;

    assert!(notices.is_empty());
    assert_eq!(requests, 1, "the run ends after its first reply");
}

#[tokio::test]
async fn review_runs_once_per_run() {
    let (fixture, log) = fixture_with_review(&[("PROMPT_WRITE", "w.rs", DUPLICATE_BODY)]).await;

    let (notices, requests) = single_run(&fixture, &log, "PROMPT_WRITE").await;

    // Prompt, tool result, then the continuation; the model ends its turn
    // again and no second review follows.
    assert_eq!(notices.len(), 1);
    assert_eq!(requests, 3);
}

#[tokio::test]
async fn duplicates_found() {
    let (fixture, log) = fixture_with_review(&[("PROMPT_WRITE", "w.rs", DUPLICATE_BODY)]).await;

    let (notices, _) = single_run(&fixture, &log, "PROMPT_WRITE").await;

    assert_eq!(notices.len(), 1);
    assert!(
        notices[0].contains("compute_total") || notices[0].contains("sum_everything"),
        "the notice names the duplicate: {}",
        notices[0]
    );
    let log = log.lock().unwrap();
    let last = log.last().expect("a request");
    assert!(
        last.last().is_some_and(|m| m.text().contains(&notices[0])),
        "the run continued with a request that carries the notice"
    );
}

#[tokio::test]
async fn no_duplicates_found() {
    let (fixture, log) = fixture_with_review(&[("PROMPT_WRITE", "w.rs", UNRELATED_BODY)]).await;

    let (notices, requests) = single_run(&fixture, &log, "PROMPT_WRITE").await;

    assert!(notices.is_empty());
    assert_eq!(requests, 2, "prompt and tool result; no continuation");
}

#[tokio::test]
async fn workspace_index_not_ready() {
    let (fixture, log) = fixture_with(
        &[("PROMPT_WRITE", "w.rs", DUPLICATE_BODY)],
        DedupCheckMiddleware::new(),
        false,
    )
    .await;

    let (notices, requests) = single_run(&fixture, &log, "PROMPT_WRITE").await;

    assert!(notices.is_empty());
    assert_eq!(requests, 2);
}

#[tokio::test]
async fn review_disabled() {
    let (fixture, log) = fixture_with(
        &[("PROMPT_WRITE", "w.rs", DUPLICATE_BODY)],
        DedupCheckMiddleware::new().enabled(false),
        true,
    )
    .await;

    let (notices, requests) = single_run(&fixture, &log, "PROMPT_WRITE").await;

    assert!(notices.is_empty());
    assert_eq!(requests, 2);
}

#[tokio::test]
async fn changes_of_an_earlier_unreviewed_run_are_not_included() {
    let (fixture, _log) = fixture_with(
        &[
            ("PROMPT_EARLY", "early.rs", DUPLICATE_BODY),
            ("PROMPT_LATER", "later.rs", UNRELATED_BODY),
        ],
        DedupCheckMiddleware::new(),
        false,
    )
    .await;
    let (session, index_cell) = fixture
        .create_session_with_pending_index()
        .await
        .expect("session");
    bind_llm(&fixture, &session).await;

    // The review is unavailable: the index is not ready when the run ends.
    fixture
        .prompt_to_completion(&session, "PROMPT_EARLY")
        .await
        .expect("first run");
    assert!(duplicate_notices(&fixture, &session).await.is_empty());

    // The index becomes ready; the next run changes only an unrelated file.
    let index = fixture.build_workspace_index().await.expect("index");
    assert!(index_cell.set(index).is_ok());
    fixture
        .prompt_to_completion(&session, "PROMPT_LATER")
        .await
        .expect("second run");

    assert!(
        duplicate_notices(&fixture, &session).await.is_empty(),
        "early.rs belongs to the first run and is not reviewed with the second"
    );
}

#[tokio::test]
async fn sessions_changes_are_reviewed_separately() {
    let (fixture, _log) = fixture_with_review(&[
        ("PROMPT_ONE", "one_dup.rs", DUPLICATE_BODY),
        ("PROMPT_TWO", "two_dup.rs", DUPLICATE_BODY),
    ])
    .await;
    let dir_one = tempfile::TempDir::new().unwrap();
    let dir_two = tempfile::TempDir::new().unwrap();
    let one = new_session_in(&fixture, dir_one.path()).await;
    let two = new_session_in(&fixture, dir_two.path()).await;

    let (run_one, run_two) = tokio::join!(
        fixture.prompt_to_completion(&one, "PROMPT_ONE"),
        fixture.prompt_to_completion(&two, "PROMPT_TWO"),
    );
    run_one.expect("first session");
    run_two.expect("second session");

    let notices_one = duplicate_notices(&fixture, &one).await;
    let notices_two = duplicate_notices(&fixture, &two).await;
    assert_eq!(notices_one.len(), 1);
    assert_eq!(notices_two.len(), 1);
    // The reviewed files are the ones listed with their function count; the
    // other session's file may still appear as a match from the shared index.
    for (notice, own, other) in [
        (&notices_one[0], "one_dup.rs", "two_dup.rs"),
        (&notices_two[0], "two_dup.rs", "one_dup.rs"),
    ] {
        assert!(notice.contains("across 1 file(s)"), "{notice}");
        assert!(
            notice.contains(&format!("{own}  (1 functions)")),
            "{notice}"
        );
        assert!(!notice.contains(&format!("{other}  (")), "{notice}");
    }
}

#[tokio::test]
async fn changes_made_after_the_review_are_dropped() {
    let (fixture, _log) = fixture_with_review(&[
        ("PROMPT_FIRST", "first.rs", DUPLICATE_BODY),
        // The model reacts to the review notice by writing another duplicate.
        ("CODE REVIEW", "after_review.rs", DUPLICATE_BODY),
        ("PROMPT_NEXT", "next.rs", UNRELATED_BODY),
    ])
    .await;
    let session = new_session(&fixture).await;

    fixture
        .prompt_to_completion(&session, "PROMPT_FIRST")
        .await
        .expect("first run");
    assert_eq!(
        duplicate_notices(&fixture, &session).await.len(),
        1,
        "only the change made before the review is reviewed in this run"
    );

    fixture
        .prompt_to_completion(&session, "PROMPT_NEXT")
        .await
        .expect("next run");

    assert_eq!(
        duplicate_notices(&fixture, &session).await.len(),
        1,
        "after_review.rs is not carried into the next run's review"
    );
}

/// Holds the two sessions of the concurrent test in the order that exposes
/// state shared between them. `role` places it before or after the review in
/// the middleware order. Session A writes a duplicate, session B does not.
#[derive(Clone)]
struct Interleave {
    role: Role,
    a: Arc<OnceLock<String>>,
    b: Arc<OnceLock<String>>,
    signals: Arc<Signals>,
    runs: Arc<Mutex<HashMap<usize, Who>>>,
    replies: Arc<Mutex<HashMap<usize, usize>>>,
}

#[derive(Clone, Copy, PartialEq)]
enum Role {
    BeforeReview,
    AfterReview,
}

/// A one-way flag that stays set once raised, for any number of waiters.
#[derive(Default)]
struct Latch {
    raised: AtomicBool,
    changed: Notify,
}

impl Latch {
    fn raise(&self) {
        self.raised.store(true, Ordering::SeqCst);
        self.changed.notify_waiters();
    }

    async fn wait(&self) {
        loop {
            let changed = self.changed.notified();
            if self.raised.load(Ordering::SeqCst) {
                return;
            }
            changed.await;
        }
    }
}

#[derive(Default)]
struct Signals {
    a_replied: Latch,
    b_replied: Latch,
    a_ended: Latch,
}

#[derive(Clone, Copy, PartialEq)]
enum Who {
    A,
    B,
}

type Runtime = Arc<crate::agent::core::SessionRuntime>;

impl Interleave {
    /// Which session a run belongs to. A reply's context names its session;
    /// a context built later may be another session's (that is the defect), so
    /// the run is recognised by its runtime, learned at its first reply.
    fn who(&self, runtime: &Runtime, replied_in: Option<&str>) -> Who {
        let key = Arc::as_ptr(runtime) as usize;
        let mut known = self.runs.lock().unwrap();
        if let Some(session) = replied_in {
            let who = if self.a.get().map(String::as_str) == Some(session) {
                Who::A
            } else {
                assert_eq!(self.b.get().map(String::as_str), Some(session));
                Who::B
            };
            known.insert(key, who);
        }
        known[&key]
    }

    /// Number of replies of this run seen so far, this one included.
    fn count_reply(&self, runtime: &Runtime) -> usize {
        let mut replies = self.replies.lock().unwrap();
        let n = replies.entry(Arc::as_ptr(runtime) as usize).or_default();
        *n += 1;
        *n
    }
}

impl Middleware for Interleave {
    fn name(&self) -> &'static str {
        "Interleave"
    }

    fn begin_turn(&self, _turn: &TurnInfo<'_>) -> Box<dyn TurnMiddleware> {
        Box::new(self.clone())
    }
}

#[async_trait]
impl TurnMiddleware for Interleave {
    async fn on_after_llm(
        &mut self,
        cx: &mut HookCx<'_>,
        _reply: &LlmResponse,
    ) -> Result<ReplyVerdict> {
        let session = cx.session_id().to_string();
        let runtime = cx.runtime().clone();
        let who = self.who(&runtime, Some(&session));
        // Reply 1 is the tool call, reply 2 the one that ends the run.
        if self.count_reply(&runtime) == 2 {
            match (self.role, who) {
                // B's reply reaches the review after A's has.
                (Role::BeforeReview, Who::B) => self.signals.a_replied.wait().await,
                (Role::AfterReview, Who::A) => self.signals.a_replied.raise(),
                (Role::AfterReview, Who::B) => self.signals.b_replied.raise(),
                (Role::BeforeReview, Who::A) => {}
            }
        }
        Ok(ReplyVerdict::Proceed)
    }

    async fn on_turn_end(&mut self, cx: &mut HookCx<'_>) -> Result<EndVerdict> {
        match (self.role, self.who(cx.runtime(), None)) {
            // A ends its run after both replies were seen; B after A's review.
            (Role::BeforeReview, Who::A) => self.signals.b_replied.wait().await,
            (Role::BeforeReview, Who::B) => self.signals.a_ended.wait().await,
            (Role::AfterReview, Who::A) => self.signals.a_ended.raise(),
            (Role::AfterReview, Who::B) => {}
        }
        Ok(EndVerdict::Finish)
    }
}

#[tokio::test]
async fn concurrent_sessions_each_continue_with_their_own_conversation() {
    let a = Arc::new(OnceLock::new());
    let b = Arc::new(OnceLock::new());
    let signals = Arc::new(Signals::default());
    let gate = |role| -> Arc<dyn Middleware> {
        Arc::new(Interleave {
            role,
            a: a.clone(),
            b: b.clone(),
            signals: signals.clone(),
            runs: Arc::default(),
            replies: Arc::default(),
        })
    };
    let (provider, log) = writing_provider(&[
        ("PROMPT_A", "a.rs", DUPLICATE_BODY),
        ("PROMPT_B", "b.rs", UNRELATED_BODY),
    ]);
    let mut fixture = UndoTestFixture::with_provider_and_middleware(
        provider,
        vec![
            gate(Role::BeforeReview),
            Arc::new(DedupCheckMiddleware::new()),
            gate(Role::AfterReview),
        ],
    )
    .await
    .expect("fixture");
    fixture
        .write_file("indexed.rs", INDEXED_BODY)
        .expect("seed file");
    fixture.index_workspace().await.expect("workspace index");
    // Own directories: a snapshot diff covers a whole directory, so sessions
    // sharing one would see each other's files in their changes.
    let dir_a = tempfile::TempDir::new().unwrap();
    let dir_b = tempfile::TempDir::new().unwrap();
    let session_a = new_session_in(&fixture, dir_a.path()).await;
    let session_b = new_session_in(&fixture, dir_b.path()).await;
    a.set(session_a.clone()).unwrap();
    b.set(session_b.clone()).unwrap();

    let both = async {
        tokio::join!(
            fixture.prompt_to_completion(&session_a, "PROMPT_A"),
            fixture.prompt_to_completion(&session_b, "PROMPT_B"),
        )
    };
    let (run_a, run_b) = tokio::time::timeout(std::time::Duration::from_secs(30), both)
        .await
        .expect("both runs finish");
    run_a.expect("run of session A");
    run_b.expect("run of session B");

    let notices_a = duplicate_notices(&fixture, &session_a).await;
    assert_eq!(notices_a.len(), 1, "session A is reviewed and continued");
    assert!(
        duplicate_notices(&fixture, &session_b).await.is_empty(),
        "session B changed nothing similar and is not continued"
    );

    // The request that carries the notice holds session A's conversation only.
    let log = log.lock().unwrap();
    let continuation: Vec<&Vec<ChatMessage>> = log
        .iter()
        .filter(|messages| {
            messages
                .last()
                .is_some_and(|last| last.text().contains(&notices_a[0]))
        })
        .collect();
    assert_eq!(continuation.len(), 1, "one continuation request");
    let text: String = continuation[0]
        .iter()
        .map(ChatMessage::text)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !text.contains("PROMPT_B"),
        "B's conversation must not leak into A's continuation: {text}"
    );
    assert!(text.contains("PROMPT_A"), "A's prompt is in its request");
}
