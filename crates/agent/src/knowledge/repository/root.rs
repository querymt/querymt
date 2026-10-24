//! Effective-checkout root resolution and machine-local cache identity.
//!
//! Repository knowledge belongs to the *effective session checkout*, never to
//! whatever directory a builder happened to be constructed in. Root precedence:
//!
//! 1. a host-provided effective checkout/knowledge root (e.g. a workspace
//!    binding);
//! 2. the root configured in host settings;
//! 3. the Git worktree root containing the session working directory;
//! 4. otherwise [`RootError::RootRequired`] — non-Git workspaces must name a
//!    root explicitly.
//!
//! The nearest Cargo manifest is deliberately *not* repository identity:
//! starting in `crates/agent` finds the same root as starting at the repository
//! root. A nested Git repository selects its own worktree unless the host
//! explicitly supplies a containing authorized root.
//!
//! The derived index lives under the shared QueryMT config directory, keyed by
//! checkout. Linked worktrees have distinct canonical roots and therefore
//! distinct keys even though they share a Git common directory, so refreshing
//! one never overwrites another's active corpus. The key is machine-local and
//! must never appear in citations or portable IDs.

use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Bumped whenever the derived-index layout or key derivation changes, so an
/// incompatible cache is abandoned rather than reused.
pub const CACHE_KEY_VERSION: u32 = 1;

/// Number of digest bytes kept in the checkout key (32 hex characters).
const KEY_BYTES: usize = 16;

/// Why a repository knowledge root could not be resolved.
#[derive(Debug, Error)]
pub enum RootError {
    #[error("no session working directory or explicit root is available")]
    NoWorkingDirectory,
    #[error(
        "`{cwd}` is not inside a Git worktree; configure an explicit repository_knowledge root"
    )]
    RootRequired { cwd: String },
    #[error("knowledge root is unavailable: {reason}")]
    Unavailable { reason: String },
    #[error("working directory `{cwd}` is not inside knowledge root `{root}`")]
    CwdOutsideRoot { cwd: String, root: String },
    #[error("knowledge root `{root}` is not permitted by host policy")]
    Denied { root: String },
    #[error("the index cache directory would be inside the repository checkout")]
    CacheInsideRepository,
}

impl RootError {
    /// Stable machine-readable diagnostic code.
    pub fn code(&self) -> &'static str {
        match self {
            Self::NoWorkingDirectory => "root_no_working_directory",
            Self::RootRequired { .. } => "root_required",
            Self::Unavailable { .. } => "root_unavailable",
            Self::CwdOutsideRoot { .. } => "root_cwd_outside",
            Self::Denied { .. } => "root_denied",
            Self::CacheInsideRepository => "cache_inside_repository",
        }
    }
}

/// Where the selected root came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootSource {
    /// Supplied by the host as the effective checkout (e.g. a workspace binding).
    Host,
    /// `repository_knowledge.root` from host settings.
    Configured,
    /// The Git worktree containing the session working directory.
    GitWorktree,
}

/// Inputs to root selection. All paths are untrusted until resolved.
#[derive(Debug, Clone, Copy, Default)]
pub struct RootRequest<'a> {
    /// The effective session working directory, when one exists.
    pub cwd: Option<&'a Path>,
    /// Effective checkout/knowledge root from the host (highest precedence).
    pub host_root: Option<&'a Path>,
    /// `repository_knowledge.root` from settings.
    pub configured_root: Option<&'a Path>,
    /// `repository_knowledge.allowed_roots`; empty means no extra restriction.
    pub allowed_roots: &'a [PathBuf],
}

/// A canonical, authorized knowledge root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedRoot {
    root: PathBuf,
    checkout_root: PathBuf,
    source: RootSource,
}

impl ResolvedRoot {
    /// Canonical knowledge root: manifest, documents and code are read from here.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Canonical root of the Git worktree containing [`root`](Self::root), or
    /// the root itself when it is not inside a worktree. Used for identity only.
    pub fn checkout_root(&self) -> &Path {
        &self.checkout_root
    }

    pub fn source(&self) -> RootSource {
        self.source
    }

    /// Machine-local key identifying this checkout's index: 32 lowercase hex
    /// characters derived from the checkout root, knowledge root, and
    /// [`CACHE_KEY_VERSION`]. It carries no recoverable path text.
    pub fn cache_key(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(b"qmt-repository-knowledge\0");
        hasher.update(CACHE_KEY_VERSION.to_le_bytes());
        hasher.update(self.checkout_root.as_os_str().as_encoded_bytes());
        hasher.update([0u8]);
        hasher.update(self.root.as_os_str().as_encoded_bytes());
        let digest = hasher.finalize();
        hex::encode(&digest[..KEY_BYTES])
    }

    /// `<config_dir>/cache/knowledge/<checkout-key>`.
    ///
    /// Computes the path only; nothing is created. Fails if the config
    /// directory sits inside the checkout, because the index must never be
    /// written into the repository.
    pub fn cache_dir(&self, config_dir: &Path) -> Result<PathBuf, RootError> {
        let config = absolute_best_effort(config_dir);
        if config.starts_with(&self.root) || config.starts_with(&self.checkout_root) {
            return Err(RootError::CacheInsideRepository);
        }
        Ok(config
            .join("cache")
            .join("knowledge")
            .join(self.cache_key()))
    }

    /// `<cache_dir>/index.sqlite`.
    pub fn index_path(&self, config_dir: &Path) -> Result<PathBuf, RootError> {
        Ok(self.cache_dir(config_dir)?.join("index.sqlite"))
    }
}

/// The shared QueryMT config directory (`QMT_CONFIG_DIR`, `QMT_HOME`, or
/// `~/.qmt`), so the index location follows host overrides.
pub fn default_config_dir() -> Result<PathBuf, RootError> {
    querymt_utils::providers::config_dir().map_err(|e| RootError::Unavailable {
        reason: format!("cannot resolve the QueryMT config directory: {e}"),
    })
}

/// Resolve and authorize the knowledge root. Reads metadata only; never creates
/// or modifies anything.
pub fn resolve_root(request: &RootRequest<'_>) -> Result<ResolvedRoot, RootError> {
    let cwd = request.cwd.map(canonical_dir).transpose()?;

    let explicit = request
        .host_root
        .map(|p| (p, RootSource::Host))
        .or_else(|| request.configured_root.map(|p| (p, RootSource::Configured)));

    let (root, source) = match explicit {
        Some((path, source)) => (canonical_dir(path)?, source),
        None => {
            let cwd = cwd.as_deref().ok_or(RootError::NoWorkingDirectory)?;
            let root = find_git_root(cwd).ok_or_else(|| RootError::RootRequired {
                cwd: cwd.display().to_string(),
            })?;
            (root, RootSource::GitWorktree)
        }
    };

    if let Some(cwd) = &cwd
        && !cwd.starts_with(&root)
    {
        return Err(RootError::CwdOutsideRoot {
            cwd: cwd.display().to_string(),
            root: root.display().to_string(),
        });
    }

    if !request.allowed_roots.is_empty() {
        let permitted = request
            .allowed_roots
            .iter()
            .filter_map(|allowed| allowed.canonicalize().ok())
            .any(|allowed| root.starts_with(allowed));
        if !permitted {
            return Err(RootError::Denied {
                root: root.display().to_string(),
            });
        }
    }

    let checkout_root = find_git_root(&root).unwrap_or_else(|| root.clone());
    Ok(ResolvedRoot {
        root,
        checkout_root,
        source,
    })
}

/// Nearest ancestor of `start` (inclusive) that is a Git worktree root: it
/// contains a `.git` directory, or a `.git` file beginning `gitdir:` (linked
/// worktrees and submodules). Cargo manifests play no part.
pub fn find_git_root(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|dir| is_git_worktree_root(dir))
        .map(Path::to_path_buf)
}

fn is_git_worktree_root(dir: &Path) -> bool {
    let dot_git = dir.join(".git");
    let Ok(metadata) = std::fs::symlink_metadata(&dot_git) else {
        return false;
    };
    if metadata.is_dir() {
        return true;
    }
    if !metadata.is_file() {
        return false;
    }
    let mut prefix = [0u8; 7];
    std::fs::File::open(&dot_git)
        .and_then(|mut f| f.read_exact(&mut prefix))
        .is_ok()
        && &prefix == b"gitdir:"
}

fn canonical_dir(path: &Path) -> Result<PathBuf, RootError> {
    let canonical = path.canonicalize().map_err(|e| RootError::Unavailable {
        reason: format!("`{}`: {e}", path.display()),
    })?;
    if !canonical.is_dir() {
        return Err(RootError::Unavailable {
            reason: format!("`{}` is not a directory", path.display()),
        });
    }
    Ok(canonical)
}

/// Canonicalize a path that may not exist yet: resolve symlinks in the deepest
/// existing ancestor, then re-append the missing tail. A purely lexical result
/// would miss symlinked prefixes (e.g. `/var` -> `/private/var`) and let a
/// not-yet-created cache directory slip inside the repository.
fn absolute_best_effort(path: &Path) -> PathBuf {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let mut tail: Vec<&std::ffi::OsStr> = Vec::new();
    let mut current = absolute.as_path();
    loop {
        if let Ok(canonical) = current.canonicalize() {
            return tail
                .iter()
                .rev()
                .fold(canonical, |acc, segment| acc.join(segment));
        }
        match (current.parent(), current.file_name()) {
            (Some(parent), Some(name)) => {
                tail.push(name);
                current = parent;
            }
            _ => return absolute,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn mkdir(root: &Path, rel: &str) -> PathBuf {
        let path = root.join(rel);
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn git_repo(root: &Path, rel: &str) -> PathBuf {
        let repo = mkdir(root, rel);
        fs::create_dir_all(repo.join(".git")).unwrap();
        repo
    }

    fn resolve_cwd(cwd: &Path) -> Result<ResolvedRoot, RootError> {
        resolve_root(&RootRequest {
            cwd: Some(cwd),
            ..Default::default()
        })
    }

    fn canon(path: &Path) -> PathBuf {
        path.canonicalize().unwrap()
    }

    #[test]
    fn repository_root_and_crate_cwd_resolve_identically() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = git_repo(tmp.path(), "repo");
        // Both the root and the crate carry a Cargo manifest; the nearest one
        // must not become the repository identity.
        fs::write(repo.join("Cargo.toml"), "[workspace]\n").unwrap();
        let krate = mkdir(&repo, "crates/agent");
        fs::write(krate.join("Cargo.toml"), "[package]\nname=\"a\"\n").unwrap();
        let deep = mkdir(&krate, "src/tools");

        let at_root = resolve_cwd(&repo).unwrap();
        let at_crate = resolve_cwd(&krate).unwrap();
        let at_deep = resolve_cwd(&deep).unwrap();

        assert_eq!(at_root.root(), canon(&repo));
        assert_eq!(at_root, at_crate);
        assert_eq!(at_root, at_deep);
        assert_eq!(at_root.cache_key(), at_crate.cache_key());
        assert_eq!(at_root.source(), RootSource::GitWorktree);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_cwd_resolves_to_the_same_checkout() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = git_repo(tmp.path(), "repo");
        let link = tmp.path().join("alias");
        std::os::unix::fs::symlink(&repo, &link).unwrap();
        assert_eq!(resolve_cwd(&link).unwrap(), resolve_cwd(&repo).unwrap());
    }

    #[test]
    fn non_git_workspace_requires_an_explicit_root() {
        let tmp = tempfile::tempdir().unwrap();
        let plain = mkdir(tmp.path(), "plain/sub");
        let err = resolve_cwd(&plain).unwrap_err();
        assert_eq!(err.code(), "root_required");

        // Configured root makes the same cwd usable.
        let root = tmp.path().join("plain");
        let resolved = resolve_root(&RootRequest {
            cwd: Some(&plain),
            configured_root: Some(&root),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(resolved.root(), canon(&root));
        assert_eq!(resolved.checkout_root(), canon(&root));
        assert_eq!(resolved.source(), RootSource::Configured);
    }

    #[test]
    fn no_cwd_and_no_root_is_reported() {
        let err = resolve_root(&RootRequest::default()).unwrap_err();
        assert_eq!(err.code(), "root_no_working_directory");
    }

    #[test]
    fn host_root_works_without_a_cwd_and_wins_over_configured() {
        let tmp = tempfile::tempdir().unwrap();
        let host = mkdir(tmp.path(), "host");
        let configured = mkdir(tmp.path(), "configured");
        let resolved = resolve_root(&RootRequest {
            host_root: Some(&host),
            configured_root: Some(&configured),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(resolved.root(), canon(&host));
        assert_eq!(resolved.source(), RootSource::Host);
    }

    #[test]
    fn explicit_root_must_contain_the_working_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let root = mkdir(tmp.path(), "root");
        let elsewhere = mkdir(tmp.path(), "elsewhere");
        let err = resolve_root(&RootRequest {
            cwd: Some(&elsewhere),
            configured_root: Some(&root),
            ..Default::default()
        })
        .unwrap_err();
        assert_eq!(err.code(), "root_cwd_outside");

        // A sibling whose name merely shares a prefix is not "inside".
        let sibling = mkdir(tmp.path(), "root-other");
        let err = resolve_root(&RootRequest {
            cwd: Some(&sibling),
            configured_root: Some(&root),
            ..Default::default()
        })
        .unwrap_err();
        assert_eq!(err.code(), "root_cwd_outside");
    }

    #[test]
    fn nested_repository_selects_its_own_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        let outer = git_repo(tmp.path(), "outer");
        let inner = git_repo(&outer, "vendor/inner");
        let cwd = mkdir(&inner, "src");

        let own = resolve_cwd(&cwd).unwrap();
        assert_eq!(own.root(), canon(&inner));
        assert_ne!(own.cache_key(), resolve_cwd(&outer).unwrap().cache_key());

        // The host may explicitly supply a containing authorized root.
        let contained = resolve_root(&RootRequest {
            cwd: Some(&cwd),
            host_root: Some(&outer),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(contained.root(), canon(&outer));
        assert_eq!(contained.source(), RootSource::Host);
    }

    #[test]
    fn linked_worktrees_are_independent_checkouts() {
        let tmp = tempfile::tempdir().unwrap();
        let common = mkdir(tmp.path(), "main/.git/worktrees/a");
        let main = tmp.path().join("main");
        let wt_a = mkdir(tmp.path(), "wt-a");
        let wt_b = mkdir(tmp.path(), "wt-b");
        for wt in [&wt_a, &wt_b] {
            fs::write(wt.join(".git"), format!("gitdir: {}\n", common.display())).unwrap();
        }

        let a = resolve_cwd(&wt_a).unwrap();
        let b = resolve_cwd(&wt_b).unwrap();
        let m = resolve_cwd(&main).unwrap();
        assert_eq!(a.root(), canon(&wt_a));
        assert_eq!(b.root(), canon(&wt_b));
        // Sharing a Git common directory must not share an active corpus.
        let keys = [a.cache_key(), b.cache_key(), m.cache_key()];
        assert_eq!(
            keys.iter().collect::<std::collections::BTreeSet<_>>().len(),
            3,
            "{keys:?}"
        );
    }

    #[test]
    fn dot_git_file_without_gitdir_marker_is_not_a_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = mkdir(tmp.path(), "not-git");
        fs::write(dir.join(".git"), "just some notes\n").unwrap();
        assert_eq!(resolve_cwd(&dir).unwrap_err().code(), "root_required");
        // Short files must not panic either.
        fs::write(dir.join(".git"), "x").unwrap();
        assert!(find_git_root(&dir).is_none());
    }

    #[test]
    fn unavailable_roots_are_reported_not_guessed() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("not-mounted/repo");

        // Working directory that does not exist on this executor.
        let err = resolve_cwd(&missing).unwrap_err();
        assert_eq!(err.code(), "root_unavailable");

        // Explicit root that does not exist.
        let err = resolve_root(&RootRequest {
            host_root: Some(&missing),
            ..Default::default()
        })
        .unwrap_err();
        assert_eq!(err.code(), "root_unavailable");

        // Explicit root that is a file.
        let file = tmp.path().join("file.txt");
        fs::write(&file, "x").unwrap();
        let err = resolve_root(&RootRequest {
            configured_root: Some(&file),
            ..Default::default()
        })
        .unwrap_err();
        assert_eq!(err.code(), "root_unavailable");
    }

    #[test]
    fn host_allowlist_denies_roots_outside_it() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = git_repo(tmp.path(), "work/repo");
        let allowed = vec![tmp.path().join("work")];
        let other = vec![tmp.path().join("elsewhere")];
        let missing_only = vec![tmp.path().join("does-not-exist")];

        let ok = resolve_root(&RootRequest {
            cwd: Some(&repo),
            allowed_roots: &allowed,
            ..Default::default()
        })
        .unwrap();
        assert_eq!(ok.root(), canon(&repo));

        for list in [&other, &missing_only] {
            let err = resolve_root(&RootRequest {
                cwd: Some(&repo),
                allowed_roots: list,
                ..Default::default()
            })
            .unwrap_err();
            assert_eq!(err.code(), "root_denied");
        }

        // Explicit roots are subject to the same policy.
        let err = resolve_root(&RootRequest {
            host_root: Some(&repo),
            allowed_roots: &other,
            ..Default::default()
        })
        .unwrap_err();
        assert_eq!(err.code(), "root_denied");

        // Prefix siblings of an allowed directory are not allowed.
        let sibling = git_repo(tmp.path(), "work-evil/repo");
        let err = resolve_root(&RootRequest {
            cwd: Some(&sibling),
            allowed_roots: &allowed,
            ..Default::default()
        })
        .unwrap_err();
        assert_eq!(err.code(), "root_denied");
    }

    #[test]
    fn cache_key_is_stable_portable_text_and_distinguishes_checkouts() {
        let tmp = tempfile::tempdir().unwrap();
        let a = git_repo(tmp.path(), "alpha-checkout");
        let b = git_repo(tmp.path(), "beta-checkout");
        let ra = resolve_cwd(&a).unwrap();
        let rb = resolve_cwd(&b).unwrap();

        let key = ra.cache_key();
        assert_eq!(key.len(), 32);
        assert!(
            key.bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        );
        assert_eq!(key, resolve_cwd(&a).unwrap().cache_key());
        assert_ne!(key, rb.cache_key());
        assert!(!key.contains("alpha"));

        // Same checkout with a different selected knowledge root is a
        // different corpus and must not share an index.
        let sub = mkdir(&a, "docs");
        let narrowed = resolve_root(&RootRequest {
            cwd: Some(&sub),
            host_root: Some(&sub),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(narrowed.checkout_root(), ra.checkout_root());
        assert_ne!(narrowed.cache_key(), key);
    }

    #[test]
    fn index_lives_under_the_supplied_config_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = git_repo(tmp.path(), "repo");
        let resolved = resolve_cwd(&repo).unwrap();

        let default_like = tmp.path().join("home/.qmt");
        let overridden = tmp.path().join("custom-config");
        let p1 = resolved.index_path(&default_like).unwrap();
        let p2 = resolved.index_path(&overridden).unwrap();

        // The index path is canonicalized (e.g. /var -> /private/var on macOS).
        let base = canon(tmp.path());
        assert!(
            p1.starts_with(base.join("home/.qmt/cache/knowledge")),
            "{p1:?}"
        );
        assert!(
            p2.starts_with(base.join("custom-config/cache/knowledge")),
            "{p2:?}"
        );
        assert_eq!(p2.file_name().unwrap(), "index.sqlite");
        assert_eq!(
            p2.parent().unwrap().file_name().unwrap().to_str().unwrap(),
            resolved.cache_key()
        );
        // Path computation creates nothing.
        assert!(!overridden.exists());
        assert!(!repo.join(".qmt").exists());
    }

    #[test]
    fn index_may_not_be_placed_inside_the_repository() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = git_repo(tmp.path(), "repo");
        let resolved = resolve_cwd(&repo).unwrap();
        for inside in [repo.join(".qmt-cache"), repo.clone(), repo.join("a/b")] {
            let err = resolved.cache_dir(&inside).unwrap_err();
            assert_eq!(err.code(), "cache_inside_repository");
        }
    }

    #[test]
    fn resolution_does_not_modify_the_filesystem() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = git_repo(tmp.path(), "repo");
        let before: Vec<_> = walk(&repo);
        let _ = resolve_cwd(&repo).unwrap();
        assert_eq!(walk(&repo), before);
    }

    fn walk(dir: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            out.push(path.clone());
            if path.is_dir() {
                out.extend(walk(&path));
            }
        }
        out.sort();
        out
    }
}
