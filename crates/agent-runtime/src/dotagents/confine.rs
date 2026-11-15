//! Confined file resolution for `.agents` Protocol layers.
//!
//! Protocol entries can carry commands and credentials, and workspace
//! repositories may contain untrusted symlinks. Before any file is read, this
//! module verifies that:
//!
//! - the path is a regular file (not a directory, device, socket, ...);
//! - the canonicalized target remains under an allowed root;
//! - `..` traversal does not escape the owning root;
//! - symlink targets do not escape the owning root, unless the caller
//!   explicitly trusts that external root.
//!
//! Resolution never creates files or directories.

use super::diagnostics::{DotagentsDiagnostic, DotagentsDiagnosticCode};
use super::layer::{DotagentsLayer, DotagentsSource};
use std::path::{Component, Path, PathBuf};

/// The outcome of a confined resolution attempt.
pub type ConfineResult<T> = Result<T, Box<DotagentsDiagnostic>>;

/// A confinement policy: a set of allowed roots plus explicitly trusted
/// external roots.
#[derive(Debug, Clone)]
pub struct ConfinementPolicy {
    layer: DotagentsLayer,
    /// Canonicalized allowed root for the owning layer.
    allowed_root: PathBuf,
    /// Additional explicitly trusted canonical roots.
    trusted_roots: Vec<PathBuf>,
}

impl ConfinementPolicy {
    /// Build a policy for a layer root.
    ///
    /// Returns `None` when the root does not exist or cannot be canonicalized;
    /// callers should treat that as "no files can be resolved from this layer".
    pub fn for_layer(
        layer: DotagentsLayer,
        root: &Path,
        trusted_roots: &[PathBuf],
    ) -> Option<Self> {
        let allowed_root = std::fs::canonicalize(root).ok()?;
        let trusted = trusted_roots
            .iter()
            .filter_map(|root| std::fs::canonicalize(root).ok())
            .collect();
        Some(Self {
            layer,
            allowed_root,
            trusted_roots: trusted,
        })
    }

    /// The layer this policy governs.
    pub fn layer(&self) -> DotagentsLayer {
        self.layer
    }

    /// The canonicalized allowed root.
    pub fn allowed_root(&self) -> &Path {
        &self.allowed_root
    }

    /// Resolve a path relative to the layer root under confinement.
    ///
    /// `relative` must be a relative path built from protocol layout (for
    /// example `agents.md` or `skills/review/skill.md`).
    pub fn resolve_relative(&self, relative: &Path) -> ConfineResult<PathBuf> {
        let lexical = self.allowed_root.join(relative);
        let source = DotagentsSource::singleton(self.layer, lexical.clone());
        self.confine_lexical(relative, &lexical, &source)
    }

    /// Resolve an absolute or layer-relative path, attaching the given source.
    pub fn resolve(&self, candidate: &Path, source: &DotagentsSource) -> ConfineResult<PathBuf> {
        if candidate.is_absolute() {
            self.confine_absolute(candidate, source)
        } else {
            let lexical = self.allowed_root.join(candidate);
            self.confine_lexical(candidate, &lexical, source)
        }
    }

    /// Whether a canonical path is within the allowed root or a trusted root.
    pub fn is_within_allowed(&self, canonical: &Path) -> bool {
        canonical.starts_with(&self.allowed_root)
            || self
                .trusted_roots
                .iter()
                .any(|root| canonical.starts_with(root))
    }

    fn confine_lexical(
        &self,
        relative: &Path,
        lexical: &Path,
        source: &DotagentsSource,
    ) -> ConfineResult<PathBuf> {
        // Reject lexical traversal before touching the filesystem.
        if !is_lexically_confined(relative) {
            return Err(Box::new(
                DotagentsDiagnostic::error(
                    DotagentsDiagnosticCode::PathEscape,
                    format!(
                        "protocol reference `{}` escapes its layer root",
                        relative.display()
                    ),
                )
                .with_source(source.clone()),
            ));
        }
        self.confine_absolute(lexical, source)
    }

    fn confine_absolute(&self, lexical: &Path, source: &DotagentsSource) -> ConfineResult<PathBuf> {
        // Inspect the lexical path first so a missing path yields an actionable
        // IO diagnostic (and so a symlink is not confused with its target).
        let link_metadata = std::fs::symlink_metadata(lexical).map_err(|error| {
            Box::new(
                DotagentsDiagnostic::error(
                    DotagentsDiagnosticCode::IoError,
                    format!("cannot access `{}`: {error}", lexical.display()),
                )
                .with_source(source.clone()),
            )
        })?;

        // Reject directories, devices, sockets, and other non-regular lexical
        // entries outright.
        if link_metadata.is_dir() {
            return Err(Box::new(
                DotagentsDiagnostic::error(
                    DotagentsDiagnosticCode::NotRegularFile,
                    format!("`{}` is not a regular file", lexical.display()),
                )
                .with_source(source.clone()),
            ));
        }

        // Resolve the path. This follows symlinks so we can confine the
        // *target*, not the link itself.
        let canonical = std::fs::canonicalize(lexical).map_err(|error| {
            Box::new(
                DotagentsDiagnostic::error(
                    DotagentsDiagnosticCode::IoError,
                    format!("cannot canonicalize `{}`: {error}", lexical.display()),
                )
                .with_source(source.clone()),
            )
        })?;

        // Confinement is checked against the canonical target before any read.
        if !self.is_within_allowed(&canonical) {
            return Err(Box::new(
                DotagentsDiagnostic::error(
                    DotagentsDiagnosticCode::PathEscape,
                    format!(
                        "`{}` resolves outside its allowed protocol root",
                        lexical.display()
                    ),
                )
                .with_source(source.clone().with_canonical(canonical)),
            ));
        }

        // The canonical target must be a regular file (rejects a symlink that
        // points at a directory or special file).
        let target_metadata = std::fs::metadata(&canonical).map_err(|error| {
            Box::new(
                DotagentsDiagnostic::error(
                    DotagentsDiagnosticCode::IoError,
                    format!("cannot stat `{}`: {error}", canonical.display()),
                )
                .with_source(source.clone().with_canonical(canonical.clone())),
            )
        })?;
        if !target_metadata.is_file() {
            return Err(Box::new(
                DotagentsDiagnostic::error(
                    DotagentsDiagnosticCode::NotRegularFile,
                    format!("`{}` is not a regular file", lexical.display()),
                )
                .with_source(source.clone().with_canonical(canonical)),
            ));
        }

        Ok(canonical)
    }
}

/// Whether a relative path stays lexically within its root.
///
/// Rejects `..` components, absolute prefixes, and root prefixes. `.` and
/// normal components are permitted.
fn is_lexically_confined(relative: &Path) -> bool {
    if relative.is_absolute() {
        return false;
    }
    for component in relative.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return false,
        }
    }
    true
}

/// Read a confined file, returning its contents as UTF-8 text.
pub fn read_confined_text(policy: &ConfinementPolicy, relative: &Path) -> ConfineResult<String> {
    let source = DotagentsSource::singleton(policy.layer(), policy.allowed_root().join(relative));
    let canonical = policy.resolve_relative(relative)?;
    std::fs::read_to_string(&canonical).map_err(|error| {
        Box::new(
            DotagentsDiagnostic::error(
                DotagentsDiagnosticCode::IoError,
                format!("cannot read `{}`: {error}", relative.display()),
            )
            .with_source(source.with_canonical(canonical)),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn policy_for(root: &Path) -> ConfinementPolicy {
        ConfinementPolicy::for_layer(DotagentsLayer::Workspace, root, &[]).unwrap()
    }

    #[test]
    fn resolves_regular_relative_file() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("agents.md"), "hello").unwrap();
        let policy = policy_for(tmp.path());

        let resolved = policy.resolve_relative(Path::new("agents.md")).unwrap();
        assert!(resolved.ends_with("agents.md"));
        assert_eq!(
            read_confined_text(&policy, Path::new("agents.md")).unwrap(),
            "hello"
        );
    }

    #[test]
    fn rejects_parent_traversal_without_reading() {
        let tmp = TempDir::new().unwrap();
        let outside = tmp.path().parent().unwrap().join("outside-secret.md");
        fs::write(&outside, "TOP SECRET").unwrap();
        let root = tmp.path().join("layer");
        fs::create_dir_all(&root).unwrap();
        let policy = policy_for(&root);

        let err = policy
            .resolve_relative(Path::new("../outside-secret.md"))
            .unwrap_err();
        assert_eq!(err.code, DotagentsDiagnosticCode::PathEscape);
        // The escape must be rejected on the lexical path, before reading.
        assert!(err.message.contains("escapes its layer root"));
        let _ = fs::remove_file(&outside);
    }

    #[test]
    fn rejects_absolute_paths_in_is_lexically_confined() {
        assert!(!is_lexically_confined(Path::new("/etc/passwd")));
        assert!(is_lexically_confined(Path::new("a/b/c.md")));
        assert!(is_lexically_confined(Path::new("./a.md")));
    }

    #[test]
    fn rejects_directory_where_file_required() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("adir")).unwrap();
        let policy = policy_for(tmp.path());

        let err = policy.resolve_relative(Path::new("adir")).unwrap_err();
        assert_eq!(err.code, DotagentsDiagnosticCode::NotRegularFile);
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_escape() {
        let tmp = TempDir::new().unwrap();
        let outside_dir = TempDir::new().unwrap();
        let secret = outside_dir.path().join("secret.md");
        fs::write(&secret, "SECRET").unwrap();

        let root = tmp.path().join("layer");
        fs::create_dir_all(&root).unwrap();
        std::os::unix::fs::symlink(&secret, root.join("link.md")).unwrap();

        let policy = policy_for(&root);
        let err = policy.resolve_relative(Path::new("link.md")).unwrap_err();
        assert_eq!(err.code, DotagentsDiagnosticCode::PathEscape);
        // The escaped content must not be surfaced in the diagnostic.
        assert!(!err.message.contains("SECRET"));
    }

    #[cfg(unix)]
    #[test]
    fn allows_symlink_when_target_root_is_trusted() {
        let tmp = TempDir::new().unwrap();
        let outside_dir = TempDir::new().unwrap();
        let target = outside_dir.path().join("shared.md");
        fs::write(&target, "shared content").unwrap();

        let root = tmp.path().join("layer");
        fs::create_dir_all(&root).unwrap();
        std::os::unix::fs::symlink(&target, root.join("link.md")).unwrap();

        let policy = ConfinementPolicy::for_layer(
            DotagentsLayer::Workspace,
            &root,
            &[outside_dir.path().to_path_buf()],
        )
        .unwrap();

        let resolved = policy.resolve_relative(Path::new("link.md")).unwrap();
        assert!(resolved.ends_with("shared.md"));
    }

    #[cfg(unix)]
    #[test]
    fn allows_symlink_staying_inside_root() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("layer");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("real.md"), "content").unwrap();
        std::os::unix::fs::symlink(root.join("real.md"), root.join("link.md")).unwrap();

        let policy = policy_for(&root);
        assert!(policy.resolve_relative(Path::new("link.md")).is_ok());
    }

    #[test]
    fn missing_file_reports_io_error() {
        let tmp = TempDir::new().unwrap();
        let policy = policy_for(tmp.path());
        let err = policy.resolve_relative(Path::new("nope.md")).unwrap_err();
        assert_eq!(err.code, DotagentsDiagnosticCode::IoError);
    }

    #[test]
    fn for_layer_returns_none_for_missing_root() {
        assert!(
            ConfinementPolicy::for_layer(
                DotagentsLayer::Global,
                Path::new("/definitely/not/here"),
                &[]
            )
            .is_none()
        );
    }

    #[test]
    fn is_within_allowed_accepts_root_and_children() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("f.md"), "x").unwrap();
        let policy = policy_for(tmp.path());
        let canonical_root = policy.allowed_root().to_path_buf();
        assert!(policy.is_within_allowed(&canonical_root));
        assert!(policy.is_within_allowed(&canonical_root.join("f.md")));
        assert!(!policy.is_within_allowed(Path::new("/etc")));
    }
}
