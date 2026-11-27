//! Delegation by worktree handoff: a delegate runs in an earlier delegation's
//! isolated worktree by ownership handoff. Scripted delegates over a real git
//! repository and a real managed-workspace service; the "coder" edits are
//! written into the worktree by the test, between delegations.

use crate::agent::session_mcp::{
    SessionMcpAttachment, SessionMcpAttachmentContext, SessionMcpAttachmentSource,
};
use crate::session::backend::StorageBackend;
use crate::session::domain::{Delegation, DelegationStatus, ForkOrigin};
use crate::test_utils::delegation::{DelegateBehavior, HarnessMember, TestHarness};
use crate::worktree::{DelegationIsolation, WorkspaceLifecycle, WorkspaceRecord};
use querymt_agent_core::events::AgentEventKind;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;

const WAIT: Duration = Duration::from_secs(30);

#[derive(Debug)]
enum Terminal {
    Completed(String),
    Failed(String),
    Cancelled,
}

impl Terminal {
    fn completed(self) -> String {
        match self {
            Self::Completed(text) => text,
            other => panic!("expected the delegation to complete, got {other:?}"),
        }
    }

    fn failed(self) -> String {
        match self {
            Self::Failed(error) => error,
            other => panic!("expected the delegation to fail, got {other:?}"),
        }
    }
}

fn member(id: &str, behavior: DelegateBehavior) -> HarnessMember {
    (id.to_string(), behavior, None)
}

/// MCP source whose resolution always fails: a member using it can create its
/// session row and take ownership, but never start its actor.
struct FailingAttachments;

#[async_trait::async_trait]
impl SessionMcpAttachmentSource for FailingAttachments {
    async fn attachments(
        &self,
        _context: &SessionMcpAttachmentContext,
    ) -> Result<Vec<SessionMcpAttachment>, crate::acp::protocol::Error> {
        Err(crate::acp::protocol::Error::internal_error().data("injected actor start failure"))
    }
}

/// Records a delegation of the harness's parent session and asks the
/// orchestrator to run it on behalf of `parent_session`.
async fn request(
    h: &TestHarness,
    parent_session: &str,
    target: &str,
    isolation: DelegationIsolation,
    handoff_from: Option<&str>,
) -> (
    Delegation,
    tokio::sync::broadcast::Receiver<Arc<querymt_agent_core::events::EventEnvelope>>,
) {
    let delegation = h
        .turn
        .state
        .record_delegation(
            target.into(),
            "do the work".into(),
            None,
            None,
            None,
            isolation,
            h.env.turn_authority().stored_origin(),
        )
        .await
        .expect("record delegation");
    let events = h.config.subscribe_events();
    h.config.emit_event(
        parent_session,
        AgentEventKind::DelegationRequested {
            delegation: delegation.clone(),
            tool_call_id: None,
            batch_id: None,
            cycle_id: None,
            handoff_from: handoff_from.map(str::to_owned),
        },
    );
    (delegation, events)
}

async fn wait_terminal(
    events: &mut tokio::sync::broadcast::Receiver<Arc<querymt_agent_core::events::EventEnvelope>>,
    delegation_id: &str,
) -> Terminal {
    tokio::time::timeout(WAIT, async {
        loop {
            let event = events.recv().await.expect("event stream");
            match event.kind() {
                AgentEventKind::DelegationCompleted {
                    delegation_id: id,
                    result,
                } if id == delegation_id => {
                    return Terminal::Completed(result.clone().unwrap_or_default());
                }
                AgentEventKind::DelegationFailed {
                    delegation_id: id,
                    error,
                } if id == delegation_id => return Terminal::Failed(error.clone()),
                AgentEventKind::DelegationCancelled { delegation_id: id }
                    if id == delegation_id =>
                {
                    return Terminal::Cancelled;
                }
                _ => {}
            }
        }
    })
    .await
    .expect("the delegation reaches a terminal state")
}

/// Run a delegation of the harness's parent session to its end.
async fn delegate(
    h: &TestHarness,
    target: &str,
    isolation: DelegationIsolation,
    handoff_from: Option<&str>,
) -> (Delegation, Terminal) {
    let parent = h.env.session_id.clone();
    delegate_as(h, &parent, target, isolation, handoff_from).await
}

async fn delegate_as(
    h: &TestHarness,
    parent_session: &str,
    target: &str,
    isolation: DelegationIsolation,
    handoff_from: Option<&str>,
) -> (Delegation, Terminal) {
    let (delegation, mut events) =
        request(h, parent_session, target, isolation, handoff_from).await;
    let terminal = wait_terminal(&mut events, &delegation.public_id).await;
    (delegation, terminal)
}

fn service(h: &TestHarness) -> Arc<crate::worktree::WorkspaceService> {
    h.workspace
        .as_ref()
        .expect("workspace harness")
        .service
        .clone()
}

async fn workspace_of(h: &TestHarness, delegation_id: &str) -> WorkspaceRecord {
    service(h)
        .registry()
        .get_workspace_by_delegation(delegation_id)
        .await
        .expect("registry")
        .unwrap_or_else(|| panic!("no worktree for delegation {delegation_id}"))
}

async fn worktrees(h: &TestHarness) -> Vec<WorkspaceRecord> {
    service(h)
        .registry()
        .list_workspaces()
        .await
        .expect("list workspaces")
}

fn owner(record: &WorkspaceRecord) -> String {
    record
        .ownership
        .owner_session_id
        .clone()
        .expect("the worktree has an owner")
}

fn write(record: &WorkspaceRecord, name: &str, content: &str) {
    std::fs::write(record.session_cwd().join(name), content).expect("write into the worktree");
}

async fn result_commits(h: &TestHarness, record: &WorkspaceRecord) -> Vec<String> {
    service(h)
        .registry()
        .list_results(&record.id)
        .await
        .expect("results")
        .into_iter()
        .filter_map(|result| result.result_commit)
        .collect()
}

/// The coder delegation of the standard scenario: D1 completed in worktree W.
async fn coder_done(h: &TestHarness) -> (Delegation, WorkspaceRecord) {
    let (d1, terminal) = delegate(h, "agent", DelegationIsolation::Isolated, None).await;
    terminal.completed();
    let w = workspace_of(h, &d1.public_id).await;
    write(&w, "coder.txt", "A");
    (d1, w)
}

#[tokio::test]
async fn reviewer_takes_over_a_coders_worktree() {
    let h = TestHarness::new_with_workspace(DelegateBehavior::AlwaysOk).await;
    let (d1, w1) = coder_done(&h).await;
    let coder_session = owner(&w1);
    let earlier_results = result_commits(&h, &w1).await;

    let (_d2, terminal) = delegate(
        &h,
        "agent1",
        DelegationIsolation::Isolated,
        Some(&d1.public_id),
    )
    .await;
    terminal.completed();

    // The same worktree: no new one, same branch and baseline, the coder's
    // edits are there.
    assert_eq!(worktrees(&h).await.len(), 1, "no new worktree");
    let w2 = workspace_of(&h, &d1.public_id).await;
    assert_eq!(w2.id, w1.id);
    assert_eq!(w2.branch.as_str(), w1.branch.as_str());
    assert_eq!(format!("{:?}", w2.baseline), format!("{:?}", w1.baseline));
    assert_eq!(w2.checkout_path, w1.checkout_path);
    assert_eq!(
        std::fs::read_to_string(w2.session_cwd().join("coder.txt")).unwrap(),
        "A"
    );
    // Earlier results are unchanged (the reviewer adds its own).
    let results = result_commits(&h, &w2).await;
    assert!(
        earlier_results
            .iter()
            .all(|commit| results.contains(commit))
    );

    // The reviewer owns it and runs in its execution cwd; the coder's session
    // no longer does and is told who does.
    let reviewer_session = owner(&w2);
    assert_ne!(reviewer_session, coder_session);
    let store = h.config.provider.history_store();
    let reviewer = store
        .get_session(&reviewer_session)
        .await
        .unwrap()
        .expect("reviewer session");
    assert_eq!(reviewer.cwd, Some(w2.session_cwd()));
    assert!(
        !member_holds(&h, "agent", &coder_session).await,
        "the previous owner's actor was unloaded"
    );
    let registry = service(&h).registry().clone();
    assert!(
        registry
            .get_workspace_by_session(&coder_session)
            .await
            .unwrap()
            .is_none(),
        "the coder's session owns nothing any more"
    );
    assert_eq!(
        registry.transferred_to(&coder_session).await.unwrap(),
        Some(reviewer_session)
    );
}

/// The member (not the root) holds a delegate's actor in a config-file run.
async fn member_holds(h: &TestHarness, member: &str, session_id: &str) -> bool {
    let member = h
        .config
        .roster()
        .expect("roster")
        .local_member(member)
        .expect("local member");
    member.registry.lock().await.get(session_id).is_some()
}

/// In a config-file run a delegate's actor lives in its member's registry, not
/// the root's. Stopping that owner must unload it there, or a `/worktree
/// handoff` would move the worktree under a loaded actor.
#[tokio::test]
async fn handoff_unloads_an_idle_delegate_owner_in_a_config_runtime() {
    let h = TestHarness::new_with_workspace(DelegateBehavior::AlwaysOk).await;
    let (_d1, w1) = coder_done(&h).await;
    let coder_session = owner(&w1);
    assert!(
        member_holds(&h, "agent", &coder_session).await,
        "the idle coder is still loaded in its member"
    );
    assert!(
        h.delegator
            .registry
            .lock()
            .await
            .get(&coder_session)
            .is_none(),
        "the root's own registry does not hold it"
    );

    let handed = crate::worktree::WorktreeOps::new(service(&h))
        .handoff(
            &*h.delegator,
            &crate::worktree::WorkspaceTarget::Workspace(w1.id.to_string()),
            Default::default(),
        )
        .await
        .expect("handoff of an idle delegate's worktree");

    assert_eq!(handed.previous_session_id, coder_session);
    assert!(
        !member_holds(&h, "agent", &coder_session).await,
        "the member's actor was unloaded before the transfer"
    );
    assert_eq!(
        owner(
            &service(&h)
                .registry()
                .get_workspace(&w1.id)
                .await
                .unwrap()
                .unwrap()
        ),
        handed.session_id
    );
}

#[tokio::test]
async fn delegation_handoff_to_a_delegate_session() {
    let h = TestHarness::new_with_workspace(DelegateBehavior::AlwaysOk).await;
    let (d1, w1) = coder_done(&h).await;
    let coder_session = owner(&w1);

    delegate(
        &h,
        "agent1",
        DelegationIsolation::Isolated,
        Some(&d1.public_id),
    )
    .await
    .1
    .completed();

    let reviewer_session = owner(&workspace_of(&h, &d1.public_id).await);
    let journaled = h
        .storage
        .event_journal()
        .load_session_stream(&coder_session, None, None)
        .await
        .unwrap();
    assert!(
        journaled.iter().any(|event| matches!(
            &event.kind,
            AgentEventKind::WorktreeOwnershipTransferred {
                workspace_id,
                successor_session_id,
                summary_seeded: false,
                ..
            } if *workspace_id == w1.id.to_string() && *successor_session_id == reviewer_session
        )),
        "the previous owner's stream records the transfer to the delegate session"
    );
}

#[tokio::test]
async fn delegate_handoff_successor_keeps_delegation_lineage() {
    let h = TestHarness::new_with_workspace(DelegateBehavior::AlwaysOk).await;
    let parent = h.env.session_id.clone();
    let (d1, w1) = coder_done(&h).await;
    delegate(
        &h,
        "agent1",
        DelegationIsolation::Isolated,
        Some(&d1.public_id),
    )
    .await
    .1
    .completed();

    let reviewer_session = owner(&workspace_of(&h, &d1.public_id).await);
    let store = h.config.provider.history_store();
    let reviewer = store.get_session(&reviewer_session).await.unwrap().unwrap();
    assert_eq!(reviewer.fork_origin, Some(ForkOrigin::Delegation));
    let (parent_of_reviewer, _) = store
        .get_session_ref_by_internal_id(reviewer.parent_session_id.expect("a parent"))
        .await
        .unwrap()
        .expect("parent row");
    assert_eq!(
        parent_of_reviewer, parent,
        "the delegating session, not the previous owner"
    );
    assert_ne!(parent_of_reviewer, owner_before(&w1));
}

fn owner_before(record: &WorkspaceRecord) -> String {
    owner(record)
}

#[tokio::test]
async fn handoff_from_a_failed_or_cancelled_delegation() {
    // A failed delegation keeps its retained worktree and its edits.
    let h = TestHarness::new_with_workspace_members(vec![
        member("coder", DelegateBehavior::AlwaysFail),
        member("reviewer", DelegateBehavior::AlwaysOk),
    ])
    .await;
    let (d1, terminal) = delegate(&h, "coder", DelegationIsolation::Isolated, None).await;
    terminal.failed();
    let w = workspace_of(&h, &d1.public_id).await;
    write(&w, "partial.txt", "half done");
    let (_d2, terminal) = delegate(
        &h,
        "reviewer",
        DelegationIsolation::Isolated,
        Some(&d1.public_id),
    )
    .await;
    terminal.completed();
    let after = workspace_of(&h, &d1.public_id).await;
    assert_eq!(after.id, w.id);
    assert_ne!(owner(&after), owner(&w));
    assert_eq!(
        std::fs::read_to_string(after.session_cwd().join("partial.txt")).unwrap(),
        "half done"
    );
    assert_eq!(worktrees(&h).await.len(), 1);

    // A cancelled delegation likewise.
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let h = TestHarness::new_with_workspace_members(vec![
        member(
            "coder",
            DelegateBehavior::PauseBeforeLlmCall {
                entered: entered.clone(),
                release,
            },
        ),
        member("reviewer", DelegateBehavior::AlwaysOk),
    ])
    .await;
    let parent = h.env.session_id.clone();
    let (d1, mut events) = request(&h, &parent, "coder", DelegationIsolation::Isolated, None).await;
    tokio::time::timeout(WAIT, entered.notified())
        .await
        .expect("the coder starts");
    h.config.emit_event(
        &parent,
        AgentEventKind::DelegationCancelRequested {
            delegation_id: d1.public_id.clone(),
        },
    );
    assert!(matches!(
        wait_terminal(&mut events, &d1.public_id).await,
        Terminal::Cancelled
    ));
    let w = workspace_of(&h, &d1.public_id).await;
    write(&w, "partial.txt", "half done");
    // Let the cancelled coder's turn wind down before taking its worktree.
    tokio::time::sleep(Duration::from_millis(300)).await;
    delegate(
        &h,
        "reviewer",
        DelegationIsolation::Isolated,
        Some(&d1.public_id),
    )
    .await
    .1
    .completed();
    let after = workspace_of(&h, &d1.public_id).await;
    assert_eq!(after.id, w.id);
    assert_ne!(owner(&after), owner(&w));
    assert_eq!(
        std::fs::read_to_string(after.session_cwd().join("partial.txt")).unwrap(),
        "half done"
    );
}

#[tokio::test]
async fn second_round_after_a_review() {
    let h = TestHarness::new_with_workspace(DelegateBehavior::AlwaysOk).await;
    let (d1, w1) = coder_done(&h).await;
    let coder_session = owner(&w1);

    delegate(
        &h,
        "agent1",
        DelegationIsolation::Isolated,
        Some(&d1.public_id),
    )
    .await
    .1
    .completed();
    let reviewer_session = owner(&workspace_of(&h, &d1.public_id).await);

    // Another round names the same (worktree's) delegation again.
    delegate(
        &h,
        "agent2",
        DelegationIsolation::Isolated,
        Some(&d1.public_id),
    )
    .await
    .1
    .completed();
    let third = owner(&workspace_of(&h, &d1.public_id).await);
    assert_ne!(third, reviewer_session);
    assert_ne!(third, coder_session);
    assert_eq!(
        service(&h)
            .registry()
            .transferred_to(&reviewer_session)
            .await
            .unwrap(),
        Some(third)
    );
    assert_eq!(worktrees(&h).await.len(), 1);
}

#[tokio::test]
async fn completion_names_the_worktrees_delegation() {
    let h = TestHarness::new_with_workspace(DelegateBehavior::AlwaysOk).await;
    let (d1, _w1) = coder_done(&h).await;

    let (d2, terminal) = delegate(
        &h,
        "agent1",
        DelegationIsolation::Isolated,
        Some(&d1.public_id),
    )
    .await;
    let text = terminal.completed();

    let w = workspace_of(&h, &d1.public_id).await;
    let latest = result_commits(&h, &w).await.pop().expect("a result");
    assert!(
        text.contains(&format!(
            "accept_delegation_result(delegation_id=\"{}\"",
            d1.public_id
        )),
        "{text}"
    );
    assert!(
        !text.contains(&format!("delegation_id=\"{}\"", d2.public_id)),
        "{text}"
    );
    assert!(
        text.contains(&latest) || text.contains(&latest[..7]),
        "{text}"
    );
}

/// Every refusal fails the delegation with its code, leaves the owner
/// unchanged and allocates no worktree.
async fn assert_refused(
    h: &TestHarness,
    terminal: Terminal,
    code: &str,
    expected_owner: Option<&str>,
    worktree_count: usize,
    source: &str,
) {
    let error = terminal.failed();
    assert!(error.contains(code), "expected {code} in: {error}");
    assert_eq!(worktrees(h).await.len(), worktree_count, "no new worktree");
    if let Some(expected_owner) = expected_owner {
        assert_eq!(
            owner(&workspace_of(h, source).await),
            expected_owner,
            "ownership unchanged"
        );
    }
}

#[tokio::test]
async fn handoff_from_another_sessions_delegation() {
    let h = TestHarness::new_with_workspace(DelegateBehavior::AlwaysOk).await;
    let (d1, w1) = coder_done(&h).await;
    let other = h
        .config
        .provider
        .create_session(
            Some(h.workspace.as_ref().unwrap().repo.path().to_path_buf()),
            None,
            &crate::session::store::SessionExecutionConfig::default(),
        )
        .await
        .expect("second session")
        .session()
        .public_id
        .clone();

    let (_d2, terminal) = delegate_as(
        &h,
        &other,
        "agent1",
        DelegationIsolation::Isolated,
        Some(&d1.public_id),
    )
    .await;
    assert_refused(
        &h,
        terminal,
        "not_parent_session",
        Some(&owner(&w1)),
        1,
        &d1.public_id,
    )
    .await;
}

#[tokio::test]
async fn handoff_from_a_shared_delegation() {
    let h = TestHarness::new_with_workspace(DelegateBehavior::AlwaysOk).await;
    let (d1, terminal) = delegate(&h, "agent", DelegationIsolation::Shared, None).await;
    terminal.completed();

    let (_d2, terminal) = delegate(
        &h,
        "agent1",
        DelegationIsolation::Isolated,
        Some(&d1.public_id),
    )
    .await;
    let error = terminal.failed();
    assert!(error.contains("no_isolated_workspace"), "{error}");
    assert!(worktrees(&h).await.is_empty(), "nothing was allocated");
}

#[tokio::test]
async fn owner_still_running() {
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let h = TestHarness::new_with_workspace_members(vec![
        member(
            "coder",
            DelegateBehavior::PauseBeforeLlmCall {
                entered: entered.clone(),
                release: release.clone(),
            },
        ),
        member("reviewer", DelegateBehavior::AlwaysOk),
    ])
    .await;
    let parent = h.env.session_id.clone();
    let (d1, mut d1_events) =
        request(&h, &parent, "coder", DelegationIsolation::Isolated, None).await;
    tokio::time::timeout(WAIT, entered.notified())
        .await
        .expect("the coder is mid-turn");
    let w = workspace_of(&h, &d1.public_id).await;
    let coder_session = owner(&w);

    let (_d2, terminal) = delegate(
        &h,
        "reviewer",
        DelegationIsolation::Isolated,
        Some(&d1.public_id),
    )
    .await;
    assert_refused(
        &h,
        terminal,
        "owner_active",
        Some(&coder_session),
        1,
        &d1.public_id,
    )
    .await;

    // The running owner was not interrupted: it still finishes its turn.
    release.notify_one();
    wait_terminal(&mut d1_events, &d1.public_id)
        .await
        .completed();
}

#[tokio::test]
async fn unknown_delegation_handoff() {
    let h = TestHarness::new_with_workspace(DelegateBehavior::AlwaysOk).await;
    let (_d, terminal) = delegate(
        &h,
        "agent",
        DelegationIsolation::Isolated,
        Some("no-such-delegation"),
    )
    .await;
    let error = terminal.failed();
    assert!(error.contains("no-such-delegation"), "{error}");
    assert!(worktrees(&h).await.is_empty(), "nothing was allocated");
}

#[tokio::test]
async fn delegate_successor_fails_to_start() {
    let h = TestHarness::new_with_workspace_members(vec![
        member("coder", DelegateBehavior::AlwaysOk),
        (
            "reviewer".to_string(),
            DelegateBehavior::AlwaysOk,
            Some(Arc::new(FailingAttachments) as Arc<dyn SessionMcpAttachmentSource>),
        ),
    ])
    .await;
    let (d1, w1) = {
        let (d1, terminal) = delegate(&h, "coder", DelegationIsolation::Isolated, None).await;
        terminal.completed();
        let w = workspace_of(&h, &d1.public_id).await;
        (d1, w)
    };
    let coder_session = owner(&w1);

    let (_d2, terminal) = delegate(
        &h,
        "reviewer",
        DelegationIsolation::Isolated,
        Some(&d1.public_id),
    )
    .await;
    let error = terminal.failed();
    assert!(error.contains("injected actor start failure"), "{error}");

    // Never failed, never ownerless: it is back with the previous owner.
    let after = workspace_of(&h, &d1.public_id).await;
    assert_eq!(after.lifecycle, WorkspaceLifecycle::Ready, "{after:?}");
    assert_eq!(owner(&after), coder_session);
    assert_eq!(worktrees(&h).await.len(), 1);
    let store = h.config.provider.history_store();
    assert_eq!(
        store
            .list_child_sessions(&h.env.session_id)
            .await
            .unwrap()
            .len(),
        1,
        "the failed successor's row was discarded"
    );
    assert_eq!(
        service(&h)
            .registry()
            .transferred_to(&coder_session)
            .await
            .unwrap(),
        None,
        "the previous owner is writable again, not a handed-off session"
    );
    assert_eq!(
        store
            .get_delegation(&d1.public_id)
            .await
            .unwrap()
            .map(|delegation| delegation.status),
        Some(DelegationStatus::Complete)
    );
}

/// Ownership has moved to the delegate and its actor started, then the
/// session's finalization fails: the workspace goes back to the previous
/// owner, as for a failed actor start.
#[tokio::test]
async fn delegate_successor_fails_after_its_actor_started() {
    let h = TestHarness::new_with_workspace_members(vec![
        member("coder", DelegateBehavior::AlwaysOk),
        member("reviewer", DelegateBehavior::SessionStartHookFails),
    ])
    .await;
    let (d1, terminal) = delegate(&h, "coder", DelegationIsolation::Isolated, None).await;
    terminal.completed();
    let coder_session = owner(&workspace_of(&h, &d1.public_id).await);

    let (_d2, terminal) = delegate(
        &h,
        "reviewer",
        DelegationIsolation::Isolated,
        Some(&d1.public_id),
    )
    .await;
    terminal.failed();

    let after = workspace_of(&h, &d1.public_id).await;
    assert_eq!(after.lifecycle, WorkspaceLifecycle::Ready, "{after:?}");
    assert_eq!(owner(&after), coder_session, "back with the previous owner");
    assert_eq!(worktrees(&h).await.len(), 1);
    assert_eq!(
        h.config
            .provider
            .history_store()
            .list_child_sessions(&h.env.session_id)
            .await
            .unwrap()
            .len(),
        1,
        "the failed successor's row was discarded"
    );
    assert_eq!(
        service(&h)
            .registry()
            .transferred_to(&coder_session)
            .await
            .unwrap(),
        None,
        "the previous owner is writable again"
    );

    // The worktree can be handed on again.
    let (_d3, terminal) = delegate(
        &h,
        "coder",
        DelegationIsolation::Isolated,
        Some(&d1.public_id),
    )
    .await;
    terminal.completed();
}

/// A post-delegation hook that holds the delegation in its finishing phase
/// (after the delegate's turn, before the result is published) until
/// `release` exists; it creates `entered` when it starts.
fn finishing_gate(entered: &std::path::Path, release: &std::path::Path) -> crate::hooks::Hooks {
    use crate::hooks::config::{HookHandlerConfig, HooksConfig, MatcherGroupConfig};
    crate::hooks::Hooks::new(HooksConfig {
        enabled: true,
        bypass_trust: true,
        post_delegation: vec![MatcherGroupConfig {
            matcher: None,
            hooks: vec![HookHandlerConfig::Command(
                crate::hooks::HookCommandConfig {
                    id: None,
                    command: format!(
                        "touch '{}'; while [ ! -f '{}' ]; do sleep 0.05; done; printf '{{}}'",
                        entered.display(),
                        release.display()
                    ),
                    timeout_sec: Some(60),
                    status_message: None,
                    additional_context_limit: None,
                    env: Default::default(),
                },
            )],
        }],
        ..HooksConfig::default()
    })
    .expect("post-delegation hook")
}

/// After its delegate's turn the delegation still works in the checkout
/// (verification, the history read, the result capture). Its idle delegate
/// is neither unloaded nor handed off until the delegation has ended.
#[tokio::test]
async fn handoff_waits_for_a_finishing_delegation() {
    let gate = tempfile::TempDir::new().unwrap();
    let entered = gate.path().join("entered");
    let release = gate.path().join("release");
    let h = TestHarness::new_with_workspace_members_and_hooks(
        vec![
            member("coder", DelegateBehavior::AlwaysOk),
            member("reviewer", DelegateBehavior::AlwaysOk),
        ],
        finishing_gate(&entered, &release),
    )
    .await;
    let parent = h.env.session_id.clone();
    let (d1, mut d1_events) =
        request(&h, &parent, "coder", DelegationIsolation::Isolated, None).await;
    tokio::time::timeout(WAIT, async {
        while !entered.exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the coder's delegation is finishing");
    let w = workspace_of(&h, &d1.public_id).await;
    let coder_session = owner(&w);

    // A delegation by handoff is refused and changes nothing.
    let (_d2, terminal) = delegate(
        &h,
        "reviewer",
        DelegationIsolation::Isolated,
        Some(&d1.public_id),
    )
    .await;
    assert_refused(
        &h,
        terminal,
        "owner_active",
        Some(&coder_session),
        1,
        &d1.public_id,
    )
    .await;
    // So is `/worktree handoff`.
    let err = crate::worktree::WorktreeOps::new(service(&h))
        .handoff(
            &*h.delegator,
            &crate::worktree::WorkspaceTarget::Workspace(w.id.to_string()),
            Default::default(),
        )
        .await
        .expect_err("the finishing delegation holds the worktree");
    assert_eq!(err.code(), "owner_active", "{err}");
    assert!(
        member_holds(&h, "coder", &coder_session).await,
        "the finishing delegate was not unloaded"
    );

    // Once the delegation ends it publishes its own result, and the worktree
    // can be handed on.
    std::fs::write(&release, b"").unwrap();
    let text = wait_terminal(&mut d1_events, &d1.public_id)
        .await
        .completed();
    assert!(text.contains("Workspace result"), "{text}");
    delegate(
        &h,
        "reviewer",
        DelegationIsolation::Isolated,
        Some(&d1.public_id),
    )
    .await
    .1
    .completed();
    assert_ne!(owner(&workspace_of(&h, &d1.public_id).await), coder_session);
}

#[tokio::test]
async fn accept_coder_work_plus_reviewer_fixes() {
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let h = TestHarness::new_with_workspace_members(vec![
        member("coder", DelegateBehavior::AlwaysOk),
        member(
            "reviewer",
            DelegateBehavior::PauseBeforeLlmCall {
                entered: entered.clone(),
                release: release.clone(),
            },
        ),
    ])
    .await;
    let repo = fixture_repo_with_index(&h);

    let (d1, terminal) = delegate(&h, "coder", DelegationIsolation::Isolated, None).await;
    terminal.completed();
    let w = workspace_of(&h, &d1.public_id).await;
    write(&w, "coder.txt", "A"); // the coder changed file A
    let r1 = result_commits(&h, &w).await;

    // The reviewer fixes file B during its own run, so the result its
    // completion publishes holds A and B.
    let parent = h.env.session_id.clone();
    let (d2, mut events) = request(
        &h,
        &parent,
        "reviewer",
        DelegationIsolation::Isolated,
        Some(&d1.public_id),
    )
    .await;
    tokio::time::timeout(WAIT, entered.notified())
        .await
        .expect("the reviewer is mid-turn");
    write(&w, "reviewer.txt", "B");
    release.notify_one();
    let text = wait_terminal(&mut events, &d2.public_id).await.completed();
    let r2 = suggested_commit(&text, &d1.public_id);
    assert!(!r1.iter().any(|commit| commit.starts_with(&r2)));

    // Exactly the call the completion text suggests, through the tool.
    let outcome = accept_via_tool(&h, &d1.public_id, &r2).await;
    assert!(outcome.starts_with("Integrated"), "{outcome}");

    let changed = git(&repo, &["show", "--name-only", "--format=", "HEAD"]);
    let mut files: Vec<&str> = changed.lines().collect();
    files.sort_unstable();
    assert_eq!(
        files,
        ["coder.txt", "reviewer.txt"],
        "one commit with A and B"
    );
    assert_eq!(
        git(&repo, &["rev-list", "--count", "HEAD"]).trim(),
        "2",
        "exactly one new commit on the parent's branch"
    );
}

/// The worktree keeps the failed coder's delegation id, but the result was
/// produced by the completed reviewer: accepting it as the completion text
/// says is judged on the reviewer's run.
#[tokio::test]
async fn accept_after_a_failed_coder() {
    let h = TestHarness::new_with_workspace_members(vec![
        member("coder", DelegateBehavior::AlwaysFail),
        member("reviewer", DelegateBehavior::AlwaysOk),
    ])
    .await;
    let repo = fixture_repo_with_index(&h);

    let (d1, terminal) = delegate(&h, "coder", DelegationIsolation::Isolated, None).await;
    terminal.failed();
    let w = workspace_of(&h, &d1.public_id).await;
    write(&w, "coder.txt", "A");

    let (_d2, terminal) = delegate(
        &h,
        "reviewer",
        DelegationIsolation::Isolated,
        Some(&d1.public_id),
    )
    .await;
    let text = terminal.completed();
    let r2 = suggested_commit(&text, &d1.public_id);

    let outcome = accept_via_tool(&h, &d1.public_id, &r2).await;
    assert!(outcome.starts_with("Integrated"), "{outcome}");
    let changed = git(&repo, &["show", "--name-only", "--format=", "HEAD"]);
    assert_eq!(changed.trim(), "coder.txt");

    // A result of the failed run itself is still refused.
    let store = h.config.provider.history_store();
    assert_eq!(
        store
            .get_delegation(&d1.public_id)
            .await
            .unwrap()
            .map(|delegation| delegation.status),
        Some(DelegationStatus::Failed)
    );
    write(&w, "late.txt", "C");
    let published = crate::worktree::WorktreeOps::new(service(&h))
        .publish(&crate::worktree::WorkspaceTarget::Workspace(
            w.id.to_string(),
        ))
        .await
        .expect("publish without a delegation");
    let unowned = published.result.result_commit.expect("result commit");
    let outcome = accept_via_tool(&h, &d1.public_id, &unowned).await;
    assert!(outcome.contains("delegation_not_complete"), "{outcome}");
}

/// The fixture repository is built without an index; integration needs one.
fn fixture_repo_with_index(h: &TestHarness) -> std::path::PathBuf {
    let repo = h.workspace.as_ref().unwrap().repo.path().to_path_buf();
    git(&repo, &["reset", "-q"]);
    repo
}

/// The `result_commit` a completion text tells the orchestrator to accept
/// under `delegation_id`.
fn suggested_commit(text: &str, delegation_id: &str) -> String {
    let marker =
        format!("accept_delegation_result(delegation_id=\"{delegation_id}\", result_commit=\"");
    let start = text
        .find(&marker)
        .unwrap_or_else(|| panic!("no acceptance hint for {delegation_id}: {text}"))
        + marker.len();
    text[start..]
        .split('"')
        .next()
        .expect("a commit")
        .to_string()
}

/// Calls `accept_delegation_result` as the orchestrator's model does: the
/// real tool, in the delegating session, with the store's statuses.
async fn accept_via_tool(h: &TestHarness, delegation_id: &str, result_commit: &str) -> String {
    use crate::tools::Tool as _;
    let repo = h.workspace.as_ref().unwrap().repo.path().to_path_buf();
    let context = crate::tools::context_impl::AgentToolContext::new(
        h.env.session_id.clone(),
        Some(repo),
        None,
    )
    .with_workspace_service(Some(service(h)))
    .with_session_store(h.config.provider.history_store());
    let parts = crate::tools::builtins::AcceptDelegationResultTool::new()
        .call(
            serde_json::json!({
                "delegation_id": delegation_id,
                "result_commit": result_commit,
            }),
            &context,
        )
        .await
        .expect("the tool runs");
    parts
        .iter()
        .filter_map(|part| part.as_text())
        .collect::<Vec<_>>()
        .join("\n")
}

fn git(dir: &std::path::Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[tokio::test]
async fn handoff_is_disclosed_to_the_caller_and_hooks() {
    use crate::hooks::config::{HookHandlerConfig, HooksConfig, MatcherGroupConfig};
    use crate::hooks::{HookCommandConfig, Hooks};
    use crate::test_utils::{MockChatResponse, mock_querymt_tool_call};
    use mockall::Sequence;

    let capture = tempfile::TempDir::new().unwrap();
    let input_path = capture.path().join("pre-delegation.json");
    let hooks = Hooks::new(HooksConfig {
        enabled: true,
        pre_delegation: vec![MatcherGroupConfig {
            matcher: None,
            hooks: vec![HookHandlerConfig::Command(HookCommandConfig {
                id: None,
                command: format!("cat > {}; printf '{{}}'", input_path.display()),
                timeout_sec: Some(5),
                status_message: None,
                additional_context_limit: None,
                env: Default::default(),
            })],
        }],
        ..HooksConfig::default()
    })
    .unwrap();
    let mut h = TestHarness::new_with_hooks(DelegateBehavior::AlwaysOk, hooks).await;
    let parent = h.env.session_id.clone();
    let mut events = h.config.subscribe_events();

    let call = mock_querymt_tool_call(
        "call-1",
        "delegate",
        r#"{"target_agent_id":"agent","objective":"review it","handoff_from":"del-coder"}"#,
    );
    let mut seq = Sequence::new();
    h.provider_mut()
        .await
        .expect_chat()
        .times(1)
        .in_sequence(&mut seq)
        .returning(move |_| {
            Ok(MockChatResponse::with_tools("Delegating", vec![call.clone()]).into())
        });
    h.provider_mut()
        .await
        .expect_chat()
        .times(1)
        .in_sequence(&mut seq)
        .returning(|_| Ok(MockChatResponse::text_only("Done").into()));
    h.provider_mut()
        .await
        .expect_call_tool()
        .returning(|_, _| {
            Ok(vec![querymt::chat::ToolResultPart::Text {
                text: "queued".to_string(),
            }])
        })
        .times(1);
    h.provider_mut()
        .await
        .expect_tools()
        .return_const(None)
        .times(0..);
    h.run().await;

    // The hook input carries the request, read-only.
    let input: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&input_path).expect("hook ran")).unwrap();
    assert_eq!(input["handoff_from"], "del-coder");

    // The event carries it.
    let requested = tokio::time::timeout(WAIT, async {
        loop {
            let event = events.recv().await.unwrap();
            if let AgentEventKind::DelegationRequested { handoff_from, .. } = event.kind() {
                break handoff_from.clone();
            }
        }
    })
    .await
    .expect("delegation requested");
    assert_eq!(requested.as_deref(), Some("del-coder"));

    // The tool result tells the caller it is neither a new nor a shared checkout.
    let history = h
        .config
        .provider
        .history_store()
        .get_history(&parent)
        .await
        .unwrap();
    let rendered = format!("{history:?}");
    assert!(
        rendered.contains("existing worktree of delegation del-coder"),
        "{rendered}"
    );
}
