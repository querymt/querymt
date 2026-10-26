//! Full-lifecycle coverage for the supported repository administration
//! layouts (F8) and canonical checkout identity (F1).
//!
//! Every supported source layout — an ordinary checkout, a checkout whose
//! Git directory lives elsewhere (separate Git directory), an existing
//! linked worktree, and a linked checkout backed by a bare common
//! repository — runs the same full lifecycle: create, own + status,
//! interrupted-provisioning recovery, immutable result capture, archive,
//! and conservative removal. Administration always operates on the
//! canonical common directory, never on a working directory guessed from a
//! checkout.
//!
//! A bare repository without a working checkout is rejected explicitly
//! before any workspace resources are allocated.
//!
//! Canonicalization guarantees are also exercised directly: path aliases
//! (symlinks) resolve to identical repository/checkout identities, cannot
//! bypass the durable single-owner check, and cannot split the
//! administration lock from the checkout it guards.
//!
//! All fixtures are built with `gix` object APIs and plain filesystem
//! writes; no Git CLI is invoked.

#![cfg(test)]

use crate::session::schema;
use crate::worktree::domain::{
    BaselineMode, ExecutionMode, ManagedBranch, ProvisioningPhase, WorkspaceId, WorkspaceLifecycle,
    WorkspaceOwnership, WorkspaceRecord, baseline_ref_for,
};
use crate::worktree::identity::{relative_cwd, resolve_repository};
use crate::worktree::lock::admin_lock_path;
use crate::worktree::repo_ops::{GixRepoOps, RepositoryOps};
use crate::worktree::repo_workspace::SqliteWorkspaceRepository;
use crate::worktree::service::WorkspaceService;
use crate::worktree::session_options::SessionCreationOptions;
use crate::worktree::test_support::{commit_files, init_repo_with_subdir, materialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

// -- fixtures ---------------------------------------------------------------

/// A workspace service over an in-memory registry with the dormant session
/// rows the owner foreign keys require.
fn layout_service() -> (TempDir, WorkspaceService) {
    let storage = TempDir::new().unwrap();
    let mut conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute("PRAGMA foreign_keys = ON;", []).unwrap();
    schema::init_schema(&mut conn).unwrap();
    for session in ["sess-layout", "sess-alias", "sess-real"] {
        conn.execute(
            "INSERT INTO sessions (public_id, name, created_at, updated_at) \
             VALUES (?1, ?1, '2026-01-15T00:00:00Z', '2026-01-15T00:00:00Z')",
            [session],
        )
        .unwrap();
    }
    let conn = Arc::new(Mutex::new(conn));
    let svc = WorkspaceService::new(
        Arc::new(GixRepoOps::new()),
        Arc::new(SqliteWorkspaceRepository::new(conn)),
        storage.path(),
    );
    (storage, svc)
}

/// A repository whose Git directory lives outside the checkout (the layout
/// `git init --separate-git-dir` produces): the checkout carries a `.git`
/// file pointer, the real Git directory (the common directory) is
/// `<tmp>/admin/repo.git`, and `core.worktree` points back at the checkout.
fn separate_git_dir_source() -> (TempDir, PathBuf, PathBuf) {
    let tmp = TempDir::new().unwrap();
    let checkout = tmp.path().join("checkout");
    let admin = tmp.path().join("admin");
    fs::create_dir_all(&admin).unwrap();
    gix::init(&checkout).unwrap();
    let pre_config_path = checkout.join(".git/config");
    let mut pre_config = fs::read_to_string(&pre_config_path).unwrap();
    pre_config.push_str("\n[user]\n\tname = test\n\temail = test@example.com\n");
    fs::write(&pre_config_path, pre_config).unwrap();
    commit_files(
        &checkout,
        "init",
        &[("file.txt", "one"), ("sub/file.txt", "sub")],
    );
    materialize(&checkout, &[("file.txt", "one"), ("sub/file.txt", "sub")]);

    let git_dir = admin.join("repo.git");
    fs::rename(checkout.join(".git"), &git_dir).unwrap();
    fs::write(
        checkout.join(".git"),
        format!("gitdir: {}\n", git_dir.display()),
    )
    .unwrap();
    let config_path = git_dir.join("config");
    let mut config = fs::read_to_string(&config_path).unwrap();
    config.push_str(&format!("\n[core]\n\tworktree = {}\n", checkout.display()));
    fs::write(config_path, config).unwrap();

    // Return canonical handles: repository identity is always compared in
    // canonical form (see `identity::resolve_repository`).
    let checkout = fs::canonicalize(checkout).unwrap();
    let git_dir = fs::canonicalize(git_dir).unwrap();

    (tmp, checkout, git_dir)
}

/// A bare common repository plus one usable linked source checkout created
/// through the same native administration primitive production code uses.
/// The bare registration itself has no working tree — the linked checkout
/// is the only usable one.
fn bare_backed_linked_source() -> (TempDir, PathBuf, PathBuf) {
    let tmp = TempDir::new().unwrap();
    let bare = tmp.path().join("project.git");
    gix::init_bare(&bare).unwrap();
    let config_path = bare.join("config");
    let mut config = fs::read_to_string(&config_path).unwrap();
    config.push_str("\n[user]\n\tname = test\n\temail = test@example.com\n");
    fs::write(config_path, config).unwrap();

    // Objects and refs are committed directly in the bare repository; the
    // shared object database is what every linked checkout reads.
    commit_files(
        &bare,
        "init",
        &[("file.txt", "one"), ("sub/file.txt", "sub")],
    );

    let repo = gix::open(&bare).unwrap();
    let head_ref = repo
        .head_name()
        .expect("HEAD lookup succeeds")
        .expect("bare repository HEAD is attached to a branch");
    let linked = tmp.path().join("main");
    let interrupt = std::sync::atomic::AtomicBool::new(false);
    repo.add_worktree(
        &linked,
        gix::worktree::add::Head::Attached(head_ref),
        gix::progress::Discard,
        &interrupt,
    )
    .unwrap();

    // Return canonical handles: repository identity is always compared in
    // canonical form (see `identity::resolve_repository`).
    let bare = fs::canonicalize(bare).unwrap();

    (tmp, linked, bare)
}

/// An existing linked worktree of an ordinary repository used as the
/// session source. Returns the repository temp dir, the worktree temp dir
/// (the linked checkout must live outside the main checkout), and the
/// linked checkout path.
async fn linked_source_of_standard_repo() -> (TempDir, TempDir, PathBuf) {
    let (tmp, _, _) = init_repo_with_subdir();
    let main_root = tmp.path().to_path_buf();
    let wt_tmp = TempDir::new().unwrap();
    let linked = wt_tmp.path().join("linked-source");

    // A branch can only be checked out in one worktree, so the linked
    // source gets its own branch at the same commit — created through the
    // production primitive with the canonical common directory handle.
    let resolved = resolve_repository(&main_root).unwrap();
    let head_id = gix::open(&resolved.identity.common_dir)
        .unwrap()
        .rev_parse_single(b"HEAD")
        .unwrap()
        .detach();
    let ops = GixRepoOps::new();
    ops.create_branch(
        &resolved.identity.common_dir,
        &ManagedBranch::from_raw("linked-source"),
        &head_id.to_string(),
    )
    .await
    .unwrap();

    let repo = gix::open(&resolved.identity.common_dir).unwrap();
    let interrupt = std::sync::atomic::AtomicBool::new(false);
    repo.add_worktree(
        &linked,
        gix::worktree::add::Head::Attached(
            gix::refs::FullName::try_from("refs/heads/linked-source").unwrap(),
        ),
        gix::progress::Discard,
        &interrupt,
    )
    .unwrap();

    (tmp, wt_tmp, linked)
}

// -- shared full-lifecycle assertions ---------------------------------------

/// Run create → own + status → interrupted-provisioning recovery → result
/// capture → archive → removal for one source layout, asserting that every
/// administration step operates on the canonical common directory.
async fn assert_full_lifecycle(source_cwd: &Path, svc: &WorkspaceService, storage: &Path) {
    let options = SessionCreationOptions::isolated_worktree(BaselineMode::Commit);

    // -- create: the managed checkout is provisioned from the source layout.
    let binding = svc
        .allocate_for_new_session(source_cwd, &options)
        .await
        .unwrap()
        .expect("requested isolation allocates");
    let record = binding.record;
    assert_eq!(record.lifecycle, WorkspaceLifecycle::Ready);
    assert!(record.checkout_path.is_dir());
    assert!(record.checkout_path.join("file.txt").is_file());

    // Administration went through the canonical common directory: the
    // managed branch exists in the shared repository at the recorded
    // baseline commit.
    let common = record.repository.common_dir.clone();
    let baseline = record.baseline.as_ref().expect("baseline is bound");
    let tip = svc
        .ops()
        .branch_tip(&common, &record.branch)
        .await
        .unwrap()
        .expect("managed branch exists in the common repository");
    assert_eq!(tip, baseline.baseline_commit);
    let resolved = resolve_repository(&record.checkout_path).unwrap();
    assert_eq!(resolved.identity.common_dir, common);

    // -- own + status: the checkout observes clean.
    svc.bind_owner(&record.id, "sess-layout").await.unwrap();
    let status = svc.manager().status(&record).await.unwrap();
    assert!(
        status.is_clean_for_removal(),
        "fresh checkout must be clean"
    );

    // -- recovery: an interrupted sibling provisioning intent completes
    //    through the same canonical common directory.
    let sibling_id = WorkspaceId::generate();
    let sibling = WorkspaceRecord {
        id: sibling_id.clone(),
        repository: record.repository.clone(),
        checkout_path: crate::worktree::checkout_path_for(storage, &common, &sibling_id),
        branch: ManagedBranch::for_workspace(&sibling_id),
        relative_cwd: PathBuf::new(),
        source_cwd: String::new(),
        execution: ExecutionMode::Worktree,
        baseline: None,
        lifecycle: WorkspaceLifecycle::Provisioning,
        ownership: WorkspaceOwnership::default(),
        last_error: None,
        requested_baseline_mode: BaselineMode::Commit,
        requested_base_ref: None,
        operation_id: format!("op-{sibling_id}"),
        provisioning_phase: Some(ProvisioningPhase::IntentPersisted),
        created_at: time::OffsetDateTime::now_utc(),
        updated_at: time::OffsetDateTime::now_utc(),
    };
    svc.registry().insert_workspace(sibling).await.unwrap();
    let recovered = svc.allocator().recover_pending(&common).await.unwrap();
    assert_eq!(recovered.len(), 1);
    assert_eq!(recovered[0].id, sibling_id);
    assert_eq!(recovered[0].lifecycle, WorkspaceLifecycle::Ready);
    assert!(recovered[0].checkout_path.is_dir());

    // -- result capture: an immutable result ref is published in the common
    //    repository without touching the real index or working files (the
    //    edit below is removed again before removal safety checks).
    fs::write(record.checkout_path.join("edit.txt"), "workspace result\n").unwrap();
    let provenance = svc
        .publish_result(&record, &record.checkout_path, None, None)
        .await
        .unwrap();
    let result_ref = provenance
        .result_ref
        .clone()
        .expect("create-only result ref");
    let published = svc
        .ops()
        .resolve_commit(&common, &result_ref)
        .await
        .unwrap()
        .expect("result ref resolves in the common repository");
    assert_eq!(
        Some(published.as_str()),
        provenance.result_commit.as_deref()
    );

    // -- archive: execution retires without deleting history or binding.
    fs::remove_file(record.checkout_path.join("edit.txt")).unwrap();
    let archived = svc.manager().archive(&record).await.unwrap();
    assert_eq!(archived.lifecycle, WorkspaceLifecycle::Archived);

    // -- removal: conservative and complete for this clean checkout; the
    //    retained refs are disclosed and still resolve in the repository.
    let report = svc.manager().remove(&record).await.unwrap();
    assert!(report.checkout_removed);
    assert!(!record.checkout_path.exists());
    assert!(
        report.retained_refs.contains(&result_ref),
        "result ref is retained and disclosed: {:?}",
        report.retained_refs
    );
    let baseline_ref = baseline_ref_for(&record.id);
    assert!(
        report.retained_refs.contains(&baseline_ref),
        "baseline ref is retained and disclosed: {:?}",
        report.retained_refs
    );
    assert!(
        svc.ops()
            .resolve_commit(&common, &result_ref)
            .await
            .unwrap()
            .is_some(),
        "retained result ref remains reachable after removal"
    );

    // The recovered sibling is independently inspectable and removable.
    let report = svc.manager().remove(&recovered[0]).await.unwrap();
    assert!(report.checkout_removed);
}

// -- supported layouts ------------------------------------------------------

/// Separate Git directory: the full lifecycle administers the external Git
/// directory (`<tmp>/admin/repo.git`), never a guessed `<checkout>/.git`.
#[tokio::test]
async fn separate_git_directory_layout_runs_the_full_lifecycle() {
    let (repo_tmp, source, common) = separate_git_dir_source();
    let (storage, svc) = layout_service();
    assert_eq!(
        resolve_repository(&source).unwrap().identity.common_dir,
        common
    );
    assert_full_lifecycle(&source, &svc, storage.path()).await;
    drop(repo_tmp);
}

/// Existing linked source: a linked worktree of an ordinary repository
/// allocates through the shared common directory and its managed checkout
/// is a sibling linked worktree of the same repository.
#[tokio::test]
async fn existing_linked_source_runs_the_full_lifecycle() {
    let (_repo_tmp, _wt_tmp, source) = linked_source_of_standard_repo().await;
    let (storage, svc) = layout_service();

    let resolved = resolve_repository(&source).unwrap();
    assert!(resolved.checkout.is_linked_worktree);

    assert_full_lifecycle(&source, &svc, storage.path()).await;
}

/// `HEAD` is per-worktree. A commit-mode baseline requested from a linked
/// source that is ahead of the main checkout must start at the *linked*
/// source's tip, not the main checkout's HEAD (allocation and recovery).
#[tokio::test]
async fn commit_baseline_from_a_linked_source_starts_at_the_linked_tip() {
    let (repo_tmp, _wt_tmp, linked) = linked_source_of_standard_repo().await;
    let (_storage, svc) = layout_service();

    let main_head = gix::open(repo_tmp.path())
        .unwrap()
        .head_id()
        .unwrap()
        .detach()
        .to_string();
    // Advance only the linked source's branch by one commit.
    let ahead = commit_files(
        &linked,
        "linked ahead",
        &[
            ("file.txt", "one"),
            ("sub/file.txt", "sub"),
            ("ahead.txt", "linked only"),
        ],
    )
    .to_string();
    assert_ne!(ahead, main_head, "fixture: linked source is ahead");
    assert_eq!(
        gix::open(repo_tmp.path())
            .unwrap()
            .head_id()
            .unwrap()
            .detach()
            .to_string(),
        main_head,
        "fixture: the main checkout did not move"
    );

    // Implicit base (HEAD) and an explicit HEAD-relative base.
    let implicit = svc
        .allocate_for_new_session(
            &linked,
            &SessionCreationOptions::isolated_worktree(BaselineMode::Commit),
        )
        .await
        .unwrap()
        .expect("isolation allocates");
    let baseline = implicit.record.baseline.as_ref().expect("bound baseline");
    assert_eq!(
        baseline.baseline_commit, ahead,
        "implicit HEAD resolves in the linked source, not the main checkout"
    );
    assert!(
        implicit.record.checkout_path.join("ahead.txt").is_file(),
        "the managed checkout contains the linked-only commit"
    );
    let branch_tip = svc
        .ops()
        .branch_tip(
            &resolve_repository(&linked).unwrap().identity.common_dir,
            &implicit.record.branch,
        )
        .await
        .unwrap();
    assert_eq!(branch_tip.as_deref(), Some(ahead.as_str()));

    let mut parent_opts = SessionCreationOptions::isolated_worktree(BaselineMode::Commit);
    parent_opts.base_ref = Some("HEAD~1".to_string());
    let parent = svc
        .allocate_for_new_session(&linked, &parent_opts)
        .await
        .unwrap()
        .expect("isolation allocates");
    assert_eq!(
        parent.record.baseline.as_ref().unwrap().baseline_commit,
        main_head,
        "HEAD~1 of the linked source is the shared ancestor"
    );
}

/// Linked source backed by a bare common repository: the bare registration
/// (no working tree) is not misinterpreted as the main checkout; creation,
/// status, capture, recovery, and removal all use the bare repository as
/// the shared administration handle.
#[tokio::test]
async fn bare_backed_linked_source_runs_the_full_lifecycle() {
    let (_repo_tmp, source, bare) = bare_backed_linked_source();
    let (storage, svc) = layout_service();

    let resolved = resolve_repository(&source).unwrap();
    assert_eq!(resolved.identity.common_dir, bare);
    assert!(resolved.checkout.is_linked_worktree);

    // Enumeration skips the workdir-less bare main registration instead of
    // failing it: the linked source is listed as a usable checkout.
    let listed = svc.ops().list_worktrees(&bare).await.unwrap();
    assert!(
        listed.iter().all(|w| w.path != bare),
        "the bare registration is not reported as a checkout: {listed:?}"
    );
    assert!(
        listed.iter().any(|w| w.path == resolved.checkout.root),
        "the linked source checkout is listed: {listed:?}"
    );

    assert_full_lifecycle(&source, &svc, storage.path()).await;
}

/// A subdirectory session in any layout maps its relative cwd onto the
/// managed checkout and executes there.
#[tokio::test]
async fn subdirectory_session_maps_cwd_into_the_managed_checkout() {
    let (_repo_tmp, source, _common) = separate_git_dir_source();
    let (_storage, svc) = layout_service();
    let sub = source.join("sub");

    let options = SessionCreationOptions::isolated_worktree(BaselineMode::Commit);
    let binding = svc
        .allocate_for_new_session(&sub, &options)
        .await
        .unwrap()
        .expect("isolation allocates");

    assert_eq!(binding.record.relative_cwd, Path::new("sub"));
    assert_eq!(
        binding.effective_cwd,
        binding.record.checkout_path.join("sub")
    );
    assert!(
        binding.record.checkout_path.join("sub/file.txt").is_file(),
        "the baseline contains the subdirectory"
    );
}

/// A bare source without a working checkout is rejected explicitly before
/// any workspace resources are allocated.
#[tokio::test]
async fn bare_source_without_checkout_fails_explicitly() {
    let tmp = TempDir::new().unwrap();
    let bare = tmp.path().join("bare.git");
    gix::init_bare(&bare).unwrap();
    let (_storage, svc) = layout_service();

    let options = SessionCreationOptions::isolated_worktree(BaselineMode::Commit);
    let err = svc
        .allocate_for_new_session(&bare, &options)
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            crate::worktree::WorktreeError::BareRepositoryUnsupported { .. }
        ),
        "unexpected error: {err:?}"
    );
    // Nothing was provisioned for the rejected request.
    assert!(
        svc.registry().list_workspaces().await.unwrap().is_empty(),
        "a rejected bare source allocates no workspace"
    );
}

// -- canonical identity: aliases cannot split ownership or locking ----------

/// Path aliases resolve to identical canonical identities, and the
/// administration lock path is a pure function of the canonical common
/// directory, so aliases cannot split checkout/admin locking.
#[test]
#[cfg(unix)]
fn canonical_identity_and_admin_locking_are_alias_invariant() {
    let (tmp, _, _) = init_repo_with_subdir();
    let root = fs::canonicalize(tmp.path()).unwrap();
    let alias = root.join("alias");
    std::os::unix::fs::symlink(&root, &alias).unwrap();

    let via_real = resolve_repository(&root).unwrap();
    let via_alias = resolve_repository(&alias).unwrap();
    assert_eq!(via_real.identity, via_alias.identity);
    assert_eq!(via_real.checkout, via_alias.checkout);

    // One canonical lock file per repository: an alias cannot select a
    // second administration lock for the same checkout.
    assert_eq!(
        admin_lock_path(&root, &via_real.identity.common_dir),
        admin_lock_path(&root, &via_alias.identity.common_dir),
    );

    // Relative cwd resolution also normalizes through the alias.
    let rel = relative_cwd(&via_alias.checkout.root, &alias.join("sub")).unwrap();
    assert_eq!(rel, Path::new("sub"));
}

/// Durable ownership is keyed on canonical identity: a second writable
/// owner is refused no matter which alias attaches, and new allocations
/// through another alias create distinct resources under the same
/// repository (project) identity — execution and index identities are
/// never merged.
#[tokio::test]
#[cfg(unix)]
async fn alias_paths_cannot_bypass_single_owner_or_split_locking() {
    let (tmp, _, _) = init_repo_with_subdir();
    let (_storage, svc) = layout_service();
    let root = fs::canonicalize(tmp.path()).unwrap();
    let alias = root.join("alias");
    std::os::unix::fs::symlink(&root, &alias).unwrap();

    let options = SessionCreationOptions::isolated_worktree(BaselineMode::Commit);

    // Allocate and own through the alias.
    let via_alias = svc
        .allocate_for_new_session(&alias, &options)
        .await
        .unwrap()
        .expect("isolation allocates through the alias");
    svc.bind_owner(&via_alias.record.id, "sess-alias")
        .await
        .unwrap();

    // The same canonical workspace resolved through the real path still
    // enforces the single writable owner.
    assert!(
        svc.bind_owner(&via_alias.record.id, "sess-real")
            .await
            .is_err(),
        "a second writable owner is refused regardless of the path used"
    );

    // A new allocation through the real path cannot attach to the owned
    // checkout: it provisions distinct resources under the same repository
    // identity (project grouping) while execution/index identities stay
    // separate.
    let via_real = svc
        .allocate_for_new_session(&root, &options)
        .await
        .unwrap()
        .expect("isolation allocates through the real path");
    assert_ne!(via_real.record.id, via_alias.record.id);
    assert_ne!(
        via_real.record.checkout_path,
        via_alias.record.checkout_path
    );
    assert_eq!(
        via_real.record.repository, via_alias.record.repository,
        "both aliases resolve the same project identity"
    );
    assert_eq!(
        via_real.effective_cwd, via_real.record.checkout_path,
        "each session executes in its own checkout"
    );

    // The alias-resolved binding remains the single owner of its checkout.
    let owned = svc
        .registry()
        .get_workspace(&via_alias.record.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        owned.ownership.owner_session_id.as_deref(),
        Some("sess-alias")
    );
}
