//! Binding repository knowledge to the effective session checkout.
//!
//! A [`RepositoryKnowledgeRegistry`] is created once from host settings (and
//! only when the feature is enabled). It does **nothing** at construction: no
//! root is resolved, no directory is created and no index is opened. The first
//! tool call for a session resolves the root from that session's *effective*
//! working directory (the bound worktree for isolated sessions), authorizes it
//! against the host allowlist, and opens or reuses the checkout's service.
//!
//! * Sessions whose cwd resolves to the same knowledge root and cache key share
//!   one [`RepositoryService`], so refreshes are serialized per checkout.
//! * Different checkouts (including linked worktrees of one repository) have
//!   different cache keys and therefore separate indexes and services.
//! * Callers cannot choose a checkout: the only input is the session cwd, and
//!   the root is re-resolved and re-authorized on every call, so a session that
//!   resumes in another directory is bound to that directory's checkout.

use super::reconcile::ReconcileOptions;
use super::root::{ResolvedRoot, RootError, RootRequest, default_config_dir, resolve_root};
use super::service::{RefreshPolicy, RepositoryService, ServiceError};
use crate::config::RepositoryKnowledgeSettings;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use thiserror::Error;

/// Why a session could not be bound to repository knowledge.
#[derive(Debug, Error)]
pub enum BindingError {
    #[error(transparent)]
    Root(#[from] RootError),
    #[error(transparent)]
    Service(#[from] ServiceError),
}

impl BindingError {
    /// Stable machine-readable diagnostic code.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Root(error) => error.code(),
            Self::Service(error) => error.code(),
        }
    }
}

/// Lazily resolves and shares one [`RepositoryService`] per checkout.
#[derive(Debug)]
pub struct RepositoryKnowledgeRegistry {
    settings: RepositoryKnowledgeSettings,
    config_dir: Option<PathBuf>,
    policy: RefreshPolicy,
    services: Mutex<HashMap<String, Arc<RepositoryService>>>,
    /// Serializes opening, so two sessions reaching a new checkout together
    /// do not race to create the same index file.
    open_lock: Mutex<()>,
    /// Local, in-memory delivery counters for this registry only.
    diagnostics: super::diagnostics::DeliveryDiagnostics,
}

impl RepositoryKnowledgeRegistry {
    /// `Some` only when the host enabled repository knowledge. Disabled
    /// settings produce no registry, so nothing downstream can read or write
    /// anything. Performs no filesystem access.
    pub fn from_settings(settings: &RepositoryKnowledgeSettings) -> Option<Arc<Self>> {
        settings.if_enabled().map(|settings| {
            Arc::new(Self {
                settings: settings.clone(),
                config_dir: None,
                policy: RefreshPolicy::default(),
                services: Mutex::new(HashMap::new()),
                open_lock: Mutex::new(()),
                diagnostics: Default::default(),
            })
        })
    }

    /// As [`from_settings`](Self::from_settings), placing indexes under
    /// `config_dir` instead of the shared QueryMT config directory (hosts with
    /// their own layout, and tests).
    pub fn from_settings_in(
        settings: &RepositoryKnowledgeSettings,
        config_dir: PathBuf,
    ) -> Option<Arc<Self>> {
        settings.if_enabled().map(|settings| {
            Arc::new(Self {
                settings: settings.clone(),
                config_dir: Some(config_dir),
                policy: RefreshPolicy::default(),
                services: Mutex::new(HashMap::new()),
                open_lock: Mutex::new(()),
                diagnostics: Default::default(),
            })
        })
    }

    pub fn settings(&self) -> &RepositoryKnowledgeSettings {
        &self.settings
    }

    /// Local matched/delivered/suppressed/fetched counters.
    pub fn diagnostics(&self) -> &super::diagnostics::DeliveryDiagnostics {
        &self.diagnostics
    }

    /// Resolve and authorize the knowledge root for a session working
    /// directory without opening anything.
    pub fn resolve(&self, cwd: Option<&Path>) -> Result<ResolvedRoot, RootError> {
        resolve_root(&RootRequest {
            cwd,
            host_root: None,
            configured_root: self.settings.root.as_deref(),
            allowed_roots: &self.settings.allowed_roots,
        })
    }

    /// The service for the checkout containing `cwd`, opened on first use and
    /// shared afterwards.
    pub fn service_for(&self, cwd: Option<&Path>) -> Result<Arc<RepositoryService>, BindingError> {
        let root = self.resolve(cwd)?;
        let key = root.cache_key();
        if let Some(existing) = self.lock().get(&key) {
            return Ok(existing.clone());
        }

        let _opening = self
            .open_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(existing) = self.lock().get(&key) {
            return Ok(existing.clone());
        }
        let config_dir = match &self.config_dir {
            Some(dir) => dir.clone(),
            None => default_config_dir()?,
        };
        let index_path = root.index_path(&config_dir)?;
        // Opened outside the map lock so lookups for open checkouts never wait
        // on a slow open.
        let opened = Arc::new(RepositoryService::open(
            root,
            &index_path,
            ReconcileOptions::from(&self.settings),
            self.policy,
        )?);
        self.lock().insert(key, opened.clone());
        Ok(opened)
    }

    /// The already-open service for the checkout containing `cwd`. Never
    /// opens, creates or reads anything: a checkout nobody has queried yet has
    /// nothing to keep warm.
    pub fn open_service_for(&self, cwd: &Path) -> Option<Arc<RepositoryService>> {
        let key = self.resolve(Some(cwd)).ok()?.cache_key();
        self.lock().get(&key).cloned()
    }

    /// Forward workspace change events for `cwd` as coalesced refresh hints.
    ///
    /// Hints only pre-warm the index of a checkout that is already open.
    /// Retrieval still reconciles with the files itself, so events the watcher
    /// missed, writes made outside the process and branch switches are
    /// observed on the next explicit retrieval regardless of this task.
    /// The task ends when the watcher's channel closes.
    pub fn spawn_change_hints(
        self: &Arc<Self>,
        cwd: PathBuf,
        mut changes: tokio::sync::broadcast::Receiver<crate::index::FileChangeSet>,
    ) -> tokio::task::JoinHandle<()> {
        let registry = Arc::clone(self);
        tokio::spawn(async move {
            use tokio::sync::broadcast::error::RecvError;
            loop {
                match changes.recv().await {
                    // A lagged receiver lost events, which is still a change.
                    Ok(_) | Err(RecvError::Lagged(_)) => {}
                    Err(RecvError::Closed) => break,
                }
                let Some(service) = registry.open_service_for(&cwd) else {
                    continue;
                };
                if service.hint_changed() {
                    tokio::task::spawn_blocking(move || {
                        service.run_hinted_refresh();
                    });
                }
            }
        })
    }

    /// Number of checkouts with an open service (diagnostics and tests).
    pub fn open_services(&self) -> usize {
        self.lock().len()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<RepositoryService>>> {
        self.services.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct Fx {
        tmp: tempfile::TempDir,
        registry: Arc<RepositoryKnowledgeRegistry>,
    }

    impl Fx {
        fn new(settings: RepositoryKnowledgeSettings) -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let registry =
                RepositoryKnowledgeRegistry::from_settings_in(&settings, tmp.path().join("config"))
                    .unwrap();
            Self { tmp, registry }
        }

        fn enabled() -> Self {
            Self::new(RepositoryKnowledgeSettings::enabled())
        }

        fn repo(&self, name: &str) -> PathBuf {
            let repo = self.tmp.path().join(name);
            fs::create_dir_all(repo.join(".git")).unwrap();
            fs::create_dir_all(repo.join("crates/inner")).unwrap();
            repo
        }

        fn config_exists(&self) -> bool {
            self.tmp.path().join("config").exists()
        }
    }

    async fn wait_for(mut done: impl FnMut() -> bool) -> bool {
        for _ in 0..200 {
            if done() {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        false
    }

    #[tokio::test]
    async fn workspace_changes_prewarm_only_checkouts_that_are_already_open() {
        let fx = Fx::enabled();
        let repo = fx.repo("repo");
        fs::create_dir_all(repo.join(".qmt")).unwrap();
        fs::create_dir_all(repo.join("docs")).unwrap();
        fs::write(
            repo.join(".qmt/knowledge.toml"),
            "version = 1\n[documents]\ninclude = [\"docs/**/*.md\"]\n",
        )
        .unwrap();
        fs::write(repo.join("docs/a.md"), "# A\nbody\n").unwrap();

        let (tx, rx) = tokio::sync::broadcast::channel(8);
        let task = fx.registry.spawn_change_hints(repo.clone(), rx);

        // Nothing has queried this checkout: a hint must not open or create anything.
        tx.send(crate::index::FileChangeSet::default()).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(fx.registry.open_services(), 0);
        assert!(!fx.config_exists());

        // Once a session has opened the checkout, hints refresh it.
        let service = fx.registry.service_for(Some(&repo)).unwrap();
        assert_eq!(service.store().counts().unwrap().documents, 0);
        tx.send(crate::index::FileChangeSet::default()).unwrap();
        let svc = service.clone();
        assert!(wait_for(move || svc.store().counts().unwrap().documents == 1).await);

        // A lagged/closed channel never panics; closing ends the task.
        drop(tx);
        task.await.unwrap();
    }

    #[test]
    fn disabled_settings_produce_no_registry() {
        assert!(
            RepositoryKnowledgeRegistry::from_settings(&RepositoryKnowledgeSettings::default())
                .is_none()
        );
    }

    #[test]
    fn construction_and_resolution_touch_nothing() {
        let fx = Fx::enabled();
        let repo = fx.repo("repo");
        assert!(!fx.config_exists());
        assert_eq!(fx.registry.open_services(), 0);
        fx.registry.resolve(Some(&repo)).unwrap();
        assert!(!fx.config_exists(), "resolving must not create the cache");
        assert_eq!(fx.registry.open_services(), 0);
    }

    #[test]
    fn sessions_in_one_checkout_share_a_service() {
        let fx = Fx::enabled();
        let repo = fx.repo("repo");
        let a = fx.registry.service_for(Some(&repo)).unwrap();
        let b = fx.registry.service_for(Some(&repo)).unwrap();
        let nested = fx
            .registry
            .service_for(Some(&repo.join("crates/inner")))
            .unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        assert!(
            Arc::ptr_eq(&a, &nested),
            "crate cwd and repo root are one checkout"
        );
        assert_eq!(fx.registry.open_services(), 1);
    }

    #[test]
    fn different_checkouts_are_isolated() {
        let fx = Fx::enabled();
        let one = fx.repo("one");
        let two = fx.repo("two");
        let a = fx.registry.service_for(Some(&one)).unwrap();
        let b = fx.registry.service_for(Some(&two)).unwrap();
        assert!(!Arc::ptr_eq(&a, &b));
        assert_ne!(a.store().path(), b.store().path());
        assert_eq!(fx.registry.open_services(), 2);
        // Each index lives under the config dir, never in the repository.
        // (Compared canonically: the cache path is resolved through symlinks,
        // e.g. /var -> /private/var for temp dirs on macOS.)
        let config = fx.tmp.path().canonicalize().unwrap().join("config");
        for service in [&a, &b] {
            assert!(service.store().path().starts_with(&config));
        }
        assert!(!one.join(".qmt").exists() && !two.join(".qmt").exists());
    }

    #[test]
    fn a_resumed_session_binds_to_the_checkout_of_its_current_cwd() {
        let fx = Fx::enabled();
        let one = fx.repo("one");
        let two = fx.repo("two");
        let first = fx.registry.service_for(Some(&one)).unwrap();
        // Same session, resumed in a different (e.g. relocated worktree) cwd.
        let resumed = fx.registry.service_for(Some(&two)).unwrap();
        assert!(!Arc::ptr_eq(&first, &resumed));
        // Resuming back finds the original service again.
        assert!(Arc::ptr_eq(
            &first,
            &fx.registry.service_for(Some(&one)).unwrap()
        ));
    }

    #[test]
    fn missing_non_git_and_denied_roots_are_disclosed_not_guessed() {
        let fx = Fx::enabled();
        let plain = fx.tmp.path().join("plain");
        fs::create_dir_all(&plain).unwrap();
        assert_eq!(
            fx.registry.service_for(None).unwrap_err().code(),
            "root_no_working_directory"
        );
        assert_eq!(
            fx.registry.service_for(Some(&plain)).unwrap_err().code(),
            "root_required"
        );

        let allowed = Fx::new(RepositoryKnowledgeSettings {
            allowed_roots: vec![PathBuf::from("/nonexistent-allowlist")],
            ..RepositoryKnowledgeSettings::enabled()
        });
        let repo = allowed.repo("repo");
        assert_eq!(
            allowed
                .registry
                .service_for(Some(&repo))
                .unwrap_err()
                .code(),
            "root_denied"
        );
        assert_eq!(allowed.registry.open_services(), 0);
        assert!(!allowed.config_exists());
    }

    #[test]
    fn an_explicit_root_serves_non_git_workspaces() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("notes");
        fs::create_dir_all(&root).unwrap();
        let registry = RepositoryKnowledgeRegistry::from_settings_in(
            &RepositoryKnowledgeSettings {
                root: Some(root.clone()),
                ..RepositoryKnowledgeSettings::enabled()
            },
            tmp.path().join("config"),
        )
        .unwrap();
        let service = registry.service_for(Some(&root)).unwrap();
        assert_eq!(service.root().root(), root.canonicalize().unwrap());
    }

    #[test]
    fn the_index_cannot_be_placed_inside_the_checkout() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir_all(repo.join(".git")).unwrap();
        let registry = RepositoryKnowledgeRegistry::from_settings_in(
            &RepositoryKnowledgeSettings::enabled(),
            repo.join("cfg"),
        )
        .unwrap();
        assert_eq!(
            registry.service_for(Some(&repo)).unwrap_err().code(),
            "cache_inside_repository"
        );
        assert!(!repo.join("cfg").exists());
    }

    #[test]
    fn concurrent_first_use_converges_on_one_service() {
        let fx = Fx::enabled();
        let repo = fx.repo("repo");
        let services: Vec<_> = std::thread::scope(|scope| {
            (0..8)
                .map(|_| scope.spawn(|| fx.registry.service_for(Some(&repo)).unwrap()))
                .collect::<Vec<_>>()
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect()
        });
        assert!(
            services
                .windows(2)
                .all(|pair| Arc::ptr_eq(&pair[0], &pair[1]))
        );
        assert_eq!(fx.registry.open_services(), 1);
    }
}
