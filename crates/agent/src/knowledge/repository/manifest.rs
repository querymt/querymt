//! `.qmt/knowledge.toml` v1 source manifest.
//!
//! The manifest selects *what* is indexed. It cannot enable the feature, grant
//! tools, or widen filesystem access; those stay host-controlled. Unknown
//! versions and unknown tables or keys are rejected outright so a future format
//! is never half-interpreted.
//!
//! ```toml
//! version = 1
//!
//! [documents]
//! include = [".agents/memories/*.md", "docs/architecture/**/*.md"]
//! exclude = ["openspec/changes/archive/**"]
//!
//! [code]
//! include = ["crates/agent/src/**/*.rs"]
//!
//! [orientation]
//! entrypoints = ["docs/architecture/README.md#Repository map"]
//! ```
//!
//! Only `<root>/.qmt/knowledge.toml` is ever read. `.agents/knowledge.toml` is
//! never consulted: `.agents/` is the shared dotagents namespace and QueryMT
//! control files do not live there.

use glob::{MatchOptions, Pattern};
use serde::Deserialize;
use std::io;
use std::path::{Component, Path};
use thiserror::Error;

/// Manifest location relative to the knowledge root.
pub const MANIFEST_RELATIVE_PATH: &str = ".qmt/knowledge.toml";

/// The only manifest format version this build understands.
pub const MANIFEST_VERSION: u32 = 1;

/// Upper bound on manifest size. A manifest is a short selection list.
const MAX_MANIFEST_BYTES: u64 = 64 * 1024;

/// Top-level keys the v1 format defines.
const KNOWN_TOP_LEVEL: &[&str] = &["version", "documents", "code", "orientation"];

const MATCH_OPTIONS: MatchOptions = MatchOptions {
    case_sensitive: true,
    // `*` never crosses a directory separator; only `**` does.
    require_literal_separator: true,
    // Selected hidden documentation (`.agents/memories`) must be matchable.
    require_literal_leading_dot: false,
};

/// Why a manifest could not be used.
///
/// Every variant carries a stable [`code`](ManifestError::code) for
/// diagnostics. None of them modify the repository.
#[derive(Debug, Error)]
pub enum ManifestError {
    #[error(
        "unsupported knowledge manifest version {found} (this build supports {MANIFEST_VERSION})"
    )]
    UnsupportedVersion { found: i64 },
    #[error("knowledge manifest is missing the required integer `version` key")]
    MissingVersion,
    #[error("unknown top-level entry `{name}` in knowledge manifest")]
    UnknownTable { name: String },
    #[error("invalid knowledge manifest: {message}")]
    Invalid { message: String },
    #[error("invalid {section} pattern `{pattern}`: {reason}")]
    InvalidPattern {
        section: &'static str,
        pattern: String,
        reason: String,
    },
    #[error("knowledge manifest is not a confined regular file: {reason}")]
    Unsafe { reason: String },
    #[error("knowledge manifest exceeds {limit} bytes")]
    TooLarge { limit: u64 },
    #[error("cannot read knowledge manifest: {0}")]
    Unreadable(#[source] io::Error),
}

impl ManifestError {
    /// Stable machine-readable diagnostic code.
    pub fn code(&self) -> &'static str {
        match self {
            Self::UnsupportedVersion { .. } => "manifest_unsupported_version",
            Self::MissingVersion => "manifest_missing_version",
            Self::UnknownTable { .. } => "manifest_unknown_table",
            Self::Invalid { .. } => "manifest_invalid",
            Self::InvalidPattern { .. } => "manifest_invalid_pattern",
            Self::Unsafe { .. } => "manifest_unsafe",
            Self::TooLarge { .. } => "manifest_too_large",
            Self::Unreadable(_) => "manifest_unreadable",
        }
    }
}

/// Result of looking for a manifest under a knowledge root.
#[derive(Debug, Clone, PartialEq)]
pub enum ManifestOutcome {
    /// No `.qmt/knowledge.toml`. Repository knowledge is *not configured*;
    /// nothing is crawled and nothing is created.
    NotConfigured,
    Loaded(KnowledgeManifest),
}

impl ManifestOutcome {
    /// Diagnostic code for the not-configured state.
    pub const NOT_CONFIGURED_CODE: &'static str = "not_configured";
}

/// A parsed, validated v1 manifest.
#[derive(Debug, Clone, PartialEq)]
pub struct KnowledgeManifest {
    pub documents: SourceSelection,
    pub code: SourceSelection,
    pub entrypoints: Vec<Entrypoint>,
}

/// A reviewed orientation entrypoint: a stable document ID or a repository-
/// relative document path, with an optional section selector (`target#Section`).
///
/// Entrypoints are inert data, never executable rules. Whether the target
/// resolves to an indexed document is checked later against the index, so an
/// invalid entrypoint yields a diagnostic rather than failing the manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entrypoint {
    pub target: String,
    pub section: Option<String>,
}

/// Include/exclude glob selection relative to the knowledge root. Excludes win.
///
/// Selection is a predicate over a path, so overlapping or duplicated include
/// patterns can never select the same file twice.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SourceSelection {
    include: Vec<CompiledPattern>,
    exclude: Vec<CompiledPattern>,
}

#[derive(Debug, Clone, PartialEq)]
struct CompiledPattern {
    source: String,
    pattern: Pattern,
}

impl SourceSelection {
    /// True when `relative_path` (forward-slash, root-relative) is selected.
    pub fn is_selected(&self, relative_path: &str) -> bool {
        let included = self
            .include
            .iter()
            .any(|p| p.pattern.matches_with(relative_path, MATCH_OPTIONS));
        included
            && !self
                .exclude
                .iter()
                .any(|p| p.pattern.matches_with(relative_path, MATCH_OPTIONS))
    }

    /// True when `relative_path` matches an exclude pattern (excludes win over
    /// includes, regardless of whether the path is also included).
    pub fn is_excluded(&self, relative_path: &str) -> bool {
        self.exclude
            .iter()
            .any(|p| p.pattern.matches_with(relative_path, MATCH_OPTIONS))
    }

    /// Normalized include patterns in declaration order, duplicates removed.
    pub fn include_patterns(&self) -> impl Iterator<Item = &str> {
        self.include.iter().map(|p| p.source.as_str())
    }

    /// Normalized exclude patterns in declaration order, duplicates removed.
    pub fn exclude_patterns(&self) -> impl Iterator<Item = &str> {
        self.exclude.iter().map(|p| p.source.as_str())
    }

    /// True when nothing can be selected.
    pub fn is_empty(&self) -> bool {
        self.include.is_empty()
    }

    fn compile(section: &'static str, raw: RawSelection) -> Result<Self, ManifestError> {
        Ok(Self {
            include: compile_patterns(section, raw.include)?,
            exclude: compile_patterns(section, raw.exclude)?,
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSelection {
    #[serde(default)]
    include: Vec<String>,
    #[serde(default)]
    exclude: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawOrientation {
    #[serde(default)]
    entrypoints: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManifest {
    #[allow(dead_code)]
    version: u32,
    documents: Option<RawSelection>,
    code: Option<RawSelection>,
    orientation: Option<RawOrientation>,
}

fn compile_patterns(
    section: &'static str,
    raw: Vec<String>,
) -> Result<Vec<CompiledPattern>, ManifestError> {
    let mut out: Vec<CompiledPattern> = Vec::with_capacity(raw.len());
    for entry in raw {
        let source = normalize_pattern(&entry).map_err(|reason| ManifestError::InvalidPattern {
            section,
            pattern: entry.clone(),
            reason: reason.to_string(),
        })?;
        if out.iter().any(|p| p.source == source) {
            continue;
        }
        let pattern = Pattern::new(&source).map_err(|e| ManifestError::InvalidPattern {
            section,
            pattern: entry.clone(),
            reason: e.to_string(),
        })?;
        out.push(CompiledPattern { source, pattern });
    }
    Ok(out)
}

/// Reject anything that is not a plain root-relative, forward-slash pattern.
fn normalize_pattern(raw: &str) -> Result<String, &'static str> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("pattern is empty");
    }
    if trimmed.contains('\0') || trimmed.contains('\\') {
        return Err("pattern must use `/` separators and no control characters");
    }
    let stripped = trimmed.strip_prefix("./").unwrap_or(trimmed);
    if stripped.is_empty() {
        return Err("pattern is empty");
    }
    if stripped.starts_with('/') || stripped.starts_with('~') {
        return Err("pattern must be relative to the knowledge root");
    }
    // Split manually: `Path::components()` silently drops interior `.` and
    // empty segments, which would let `docs/./a.md` or `docs//a.md` through.
    for segment in stripped.split('/') {
        match segment {
            "" => return Err("pattern must not contain empty segments"),
            "." => return Err("pattern must not contain `.` segments"),
            ".." => return Err("pattern must not contain `..` segments"),
            _ => {}
        }
    }
    if Path::new(stripped)
        .components()
        .any(|c| matches!(c, Component::RootDir | Component::Prefix(_)))
    {
        return Err("pattern must be relative to the knowledge root");
    }
    Ok(stripped.to_string())
}

fn parse_entrypoint(raw: &str) -> Result<Entrypoint, ManifestError> {
    let trimmed = raw.trim();
    let (target, section) = match trimmed.split_once('#') {
        Some((target, section)) => (target.trim(), Some(section.trim())),
        None => (trimmed, None),
    };
    if target.is_empty() {
        return Err(ManifestError::Invalid {
            message: format!("orientation entrypoint `{raw}` has no document target"),
        });
    }
    Ok(Entrypoint {
        target: target.to_string(),
        section: section.filter(|s| !s.is_empty()).map(str::to_string),
    })
}

impl KnowledgeManifest {
    /// Parse manifest text. Does no filesystem access.
    pub fn parse(content: &str) -> Result<Self, ManifestError> {
        let table: toml::Table = toml::from_str(content).map_err(|e| ManifestError::Invalid {
            message: e.message().to_string(),
        })?;

        // Version first, so a future format with a different shape is reported
        // as unsupported rather than as a confusing schema error.
        match table.get("version") {
            Some(toml::Value::Integer(v)) if *v == i64::from(MANIFEST_VERSION) => {}
            Some(toml::Value::Integer(v)) => {
                return Err(ManifestError::UnsupportedVersion { found: *v });
            }
            _ => return Err(ManifestError::MissingVersion),
        }

        if let Some(unknown) = table
            .keys()
            .find(|k| !KNOWN_TOP_LEVEL.contains(&k.as_str()))
        {
            return Err(ManifestError::UnknownTable {
                name: unknown.clone(),
            });
        }

        let raw: RawManifest =
            table
                .try_into()
                .map_err(|e: toml::de::Error| ManifestError::Invalid {
                    message: e.message().to_string(),
                })?;

        let empty = || RawSelection {
            include: Vec::new(),
            exclude: Vec::new(),
        };
        let documents = SourceSelection::compile("documents", raw.documents.unwrap_or_else(empty))?;
        let code = SourceSelection::compile("code", raw.code.unwrap_or_else(empty))?;

        let mut entrypoints: Vec<Entrypoint> = Vec::new();
        for entry in raw.orientation.map(|o| o.entrypoints).unwrap_or_default() {
            let parsed = parse_entrypoint(&entry)?;
            if !entrypoints.contains(&parsed) {
                entrypoints.push(parsed);
            }
        }

        Ok(Self {
            documents,
            code,
            entrypoints,
        })
    }
}

/// Load `<root>/.qmt/knowledge.toml`.
///
/// Reads exactly one file. The resolved path must stay inside the canonical
/// root and be a regular file no larger than 64 KiB. A missing manifest is
/// [`ManifestOutcome::NotConfigured`]; this function never creates files and
/// never looks at `.agents/`.
pub fn load_manifest(root: &Path) -> Result<ManifestOutcome, ManifestError> {
    let canonical_root = root.canonicalize().map_err(ManifestError::Unreadable)?;
    let candidate = canonical_root.join(MANIFEST_RELATIVE_PATH);

    let resolved = match candidate.canonicalize() {
        Ok(path) => path,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(ManifestOutcome::NotConfigured),
        Err(e) => return Err(ManifestError::Unreadable(e)),
    };
    if !resolved.starts_with(&canonical_root) {
        return Err(ManifestError::Unsafe {
            reason: "resolves outside the knowledge root".to_string(),
        });
    }
    let metadata = std::fs::metadata(&resolved).map_err(ManifestError::Unreadable)?;
    if !metadata.is_file() {
        return Err(ManifestError::Unsafe {
            reason: "not a regular file".to_string(),
        });
    }
    if metadata.len() > MAX_MANIFEST_BYTES {
        return Err(ManifestError::TooLarge {
            limit: MAX_MANIFEST_BYTES,
        });
    }
    let content = std::fs::read_to_string(&resolved).map_err(|e| {
        if e.kind() == io::ErrorKind::InvalidData {
            ManifestError::Invalid {
                message: "manifest is not valid UTF-8".to_string(),
            }
        } else {
            ManifestError::Unreadable(e)
        }
    })?;
    KnowledgeManifest::parse(&content).map(ManifestOutcome::Loaded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(root: &Path, rel: &str, content: &str) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn parse(content: &str) -> KnowledgeManifest {
        KnowledgeManifest::parse(content).unwrap()
    }

    fn err_code(content: &str) -> &'static str {
        KnowledgeManifest::parse(content).unwrap_err().code()
    }

    #[test]
    fn parses_full_v1_manifest() {
        let m = parse(
            r#"
version = 1
[documents]
include = [".agents/memories/*.md", "docs/architecture/**/*.md", "crates/agent/**/AGENTS.md"]
exclude = ["openspec/changes/archive/**"]
[code]
include = ["crates/agent/src/**/*.rs"]
[orientation]
entrypoints = ["docs/architecture/README.md#Repository map", "runtime-overview"]
"#,
        );
        assert_eq!(m.documents.include_patterns().count(), 3);
        assert_eq!(m.code.include_patterns().count(), 1);
        assert_eq!(
            m.entrypoints,
            vec![
                Entrypoint {
                    target: "docs/architecture/README.md".into(),
                    section: Some("Repository map".into()),
                },
                Entrypoint {
                    target: "runtime-overview".into(),
                    section: None,
                },
            ]
        );
    }

    #[test]
    fn version_only_manifest_selects_nothing() {
        let m = parse("version = 1\n");
        assert!(m.documents.is_empty() && m.code.is_empty());
        assert!(!m.documents.is_selected("README.md"));
        assert!(m.entrypoints.is_empty());
    }

    #[test]
    fn includes_match_with_literal_separators_and_hidden_dirs() {
        let m = parse(
            "version = 1\n[documents]\ninclude = [\".agents/memories/*.md\", \"docs/**/*.md\", \"crates/agent/**/AGENTS.md\"]\n",
        );
        let d = &m.documents;
        assert!(d.is_selected(".agents/memories/runtime.md"));
        // `*` must not cross a directory boundary.
        assert!(!d.is_selected(".agents/memories/nested/runtime.md"));
        assert!(d.is_selected("docs/a.md"));
        assert!(d.is_selected("docs/x/y/z.md"));
        assert!(!d.is_selected("docs/a.txt"));
        assert!(d.is_selected("crates/agent/AGENTS.md"));
        assert!(d.is_selected("crates/agent/src/tools/AGENTS.md"));
        assert!(!d.is_selected("crates/other/AGENTS.md"));
        // Matching is case sensitive.
        assert!(!d.is_selected("DOCS/a.md"));
    }

    #[test]
    fn excludes_win_over_includes() {
        let m = parse(
            "version = 1\n[documents]\ninclude = [\"**/*.md\"]\nexclude = [\"openspec/changes/archive/**\", \"docs/private.md\"]\n",
        );
        let d = &m.documents;
        assert!(d.is_selected("docs/public.md"));
        assert!(!d.is_selected("docs/private.md"));
        assert!(!d.is_selected("openspec/changes/archive/2026/old.md"));
        assert!(d.is_selected("openspec/changes/active/new.md"));
    }

    #[test]
    fn overlapping_and_duplicate_patterns_select_a_path_once() {
        let m = parse(
            "version = 1\n[documents]\ninclude = [\"docs/**/*.md\", \"./docs/**/*.md\", \"docs/a.md\", \"docs/**/*.md\"]\n",
        );
        // Duplicates (including `./` spellings) collapse; order is preserved.
        assert_eq!(
            m.documents.include_patterns().collect::<Vec<_>>(),
            vec!["docs/**/*.md", "docs/a.md"]
        );
        // Selection is a predicate: an overlapping match is still one file.
        assert!(m.documents.is_selected("docs/a.md"));
    }

    #[test]
    fn code_and_documents_are_independent_selections() {
        let m = parse(
            "version = 1\n[documents]\ninclude = [\"docs/**/*.md\"]\n[code]\ninclude = [\"src/**/*.rs\"]\n",
        );
        assert!(m.documents.is_selected("docs/a.md"));
        assert!(!m.documents.is_selected("src/lib.rs"));
        assert!(m.code.is_selected("src/lib.rs"));
        assert!(!m.code.is_selected("docs/a.md"));
    }

    #[test]
    fn rejects_unsupported_and_missing_versions() {
        assert_eq!(err_code("version = 2\n"), "manifest_unsupported_version");
        assert_eq!(err_code("version = 0\n"), "manifest_unsupported_version");
        // A future-version manifest with a different shape is still "unsupported".
        assert_eq!(
            err_code("version = 2\n[bundles]\nx = 1\n"),
            "manifest_unsupported_version"
        );
        assert_eq!(
            err_code("[documents]\ninclude = []\n"),
            "manifest_missing_version"
        );
        assert_eq!(err_code("version = \"1\"\n"), "manifest_missing_version");
    }

    #[test]
    fn rejects_unknown_top_level_tables_and_keys() {
        let err = KnowledgeManifest::parse("version = 1\n[[bundles]]\nroot = \"x\"\n").unwrap_err();
        assert_eq!(err.code(), "manifest_unknown_table");
        assert!(err.to_string().contains("bundles"), "{err}");
        assert_eq!(
            err_code("version = 1\nenabled = true\n"),
            "manifest_unknown_table"
        );
    }

    #[test]
    fn rejects_unknown_keys_inside_known_tables() {
        assert_eq!(
            err_code("version = 1\n[documents]\ninclude = []\ntitle = \"x\"\n"),
            "manifest_invalid"
        );
        assert_eq!(
            err_code("version = 1\n[orientation]\nrules = []\n"),
            "manifest_invalid"
        );
    }

    #[test]
    fn repository_content_cannot_enable_or_widen_host_controls() {
        // Host-only controls must not be settable from the repository.
        for key in [
            "enabled = true",
            "allow_ignored = true",
            "ignore = false",
            "root = \"/\"",
            "tools = [\"shell\"]",
            "hooks = []",
        ] {
            let top = KnowledgeManifest::parse(&format!("version = 1\n{key}\n"));
            assert_eq!(top.unwrap_err().code(), "manifest_unknown_table", "{key}");
            let inner = KnowledgeManifest::parse(&format!("version = 1\n[documents]\n{key}\n"));
            assert_eq!(inner.unwrap_err().code(), "manifest_invalid", "{key}");
        }
    }

    #[test]
    fn rejects_unsafe_and_malformed_patterns() {
        for bad in [
            "/etc/passwd",
            "~/notes.md",
            "../outside/*.md",
            "docs/../../outside.md",
            "docs/./a.md",
            "docs\\\\a.md",
            "",
            "   ",
            "docs/[unclosed",
        ] {
            let toml = format!("version = 1\n[documents]\ninclude = [\"{bad}\"]\n");
            assert_eq!(err_code(&toml), "manifest_invalid_pattern", "{bad:?}");
            let toml = format!("version = 1\n[code]\nexclude = [\"{bad}\"]\n");
            assert_eq!(err_code(&toml), "manifest_invalid_pattern", "{bad:?}");
        }
    }

    #[test]
    fn rejects_empty_entrypoint_target_and_non_string_values() {
        assert_eq!(
            err_code("version = 1\n[orientation]\nentrypoints = [\"#Section\"]\n"),
            "manifest_invalid"
        );
        assert_eq!(
            err_code("version = 1\n[documents]\ninclude = [1]\n"),
            "manifest_invalid"
        );
        assert_eq!(err_code("version = 1\n[documents"), "manifest_invalid");
    }

    #[test]
    fn duplicate_entrypoints_collapse() {
        let m =
            parse("version = 1\n[orientation]\nentrypoints = [\"a.md#X\", \"a.md#X\", \"a.md\"]\n");
        assert_eq!(m.entrypoints.len(), 2);
    }

    #[test]
    fn missing_manifest_is_not_configured_and_creates_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            load_manifest(dir.path()).unwrap(),
            ManifestOutcome::NotConfigured
        );
        assert!(!dir.path().join(".qmt").exists(), "must not create .qmt");
        assert_eq!(ManifestOutcome::NOT_CONFIGURED_CODE, "not_configured");
    }

    #[test]
    fn loads_manifest_from_dot_qmt() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            ".qmt/knowledge.toml",
            "version = 1\n[documents]\ninclude = [\"docs/**/*.md\"]\n",
        );
        match load_manifest(dir.path()).unwrap() {
            ManifestOutcome::Loaded(m) => assert!(m.documents.is_selected("docs/a.md")),
            other => panic!("expected loaded manifest, got {other:?}"),
        }
    }

    #[test]
    fn agents_directory_manifest_is_never_read() {
        let dir = tempfile::tempdir().unwrap();
        // Valid-looking manifest in the shared `.agents` namespace.
        write(
            dir.path(),
            ".agents/knowledge.toml",
            "version = 1\n[documents]\ninclude = [\"**/*.md\"]\n",
        );
        assert_eq!(
            load_manifest(dir.path()).unwrap(),
            ManifestOutcome::NotConfigured
        );

        // Even a syntactically invalid one must not surface an error.
        write(
            dir.path(),
            ".agents/knowledge.toml",
            "version = 99\n[[nope\n",
        );
        assert_eq!(
            load_manifest(dir.path()).unwrap(),
            ManifestOutcome::NotConfigured
        );

        // And it never overrides or merges into the real manifest.
        write(
            dir.path(),
            ".qmt/knowledge.toml",
            "version = 1\n[documents]\ninclude = [\"docs/*.md\"]\n",
        );
        let ManifestOutcome::Loaded(m) = load_manifest(dir.path()).unwrap() else {
            panic!("expected loaded manifest");
        };
        assert!(!m.documents.is_selected("README.md"));
        assert!(m.documents.is_selected("docs/a.md"));
        // Nothing was written under .agents/.
        let entries: Vec<_> = fs::read_dir(dir.path().join(".agents"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(entries, vec![std::ffi::OsString::from("knowledge.toml")]);
    }

    #[test]
    fn invalid_manifest_files_report_stable_codes() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), ".qmt/knowledge.toml", "version = 2\n");
        assert_eq!(
            load_manifest(dir.path()).unwrap_err().code(),
            "manifest_unsupported_version"
        );

        write(
            dir.path(),
            ".qmt/knowledge.toml",
            "version = 1\n[bundles]\n",
        );
        assert_eq!(
            load_manifest(dir.path()).unwrap_err().code(),
            "manifest_unknown_table"
        );

        fs::write(dir.path().join(".qmt/knowledge.toml"), [0xff, 0xfe, 0x00]).unwrap();
        assert_eq!(
            load_manifest(dir.path()).unwrap_err().code(),
            "manifest_invalid"
        );
    }

    #[test]
    fn oversized_manifest_is_rejected_without_parsing() {
        let dir = tempfile::tempdir().unwrap();
        let big = format!("version = 1\n# {}\n", "x".repeat(70 * 1024));
        write(dir.path(), ".qmt/knowledge.toml", &big);
        assert_eq!(
            load_manifest(dir.path()).unwrap_err().code(),
            "manifest_too_large"
        );
    }

    #[test]
    fn manifest_that_is_a_directory_is_unsafe() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".qmt/knowledge.toml")).unwrap();
        assert_eq!(
            load_manifest(dir.path()).unwrap_err().code(),
            "manifest_unsafe"
        );
    }

    #[cfg(unix)]
    #[test]
    fn manifest_symlinked_outside_root_is_rejected() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        write(
            outside.path(),
            "evil.toml",
            "version = 1\n[documents]\ninclude = [\"**/*.md\"]\n",
        );
        fs::create_dir_all(root.path().join(".qmt")).unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("evil.toml"),
            root.path().join(".qmt/knowledge.toml"),
        )
        .unwrap();
        assert_eq!(
            load_manifest(root.path()).unwrap_err().code(),
            "manifest_unsafe"
        );

        // A symlinked `.qmt` directory escaping the root is rejected as well.
        let root2 = tempfile::tempdir().unwrap();
        let outside_qmt = tempfile::tempdir().unwrap();
        write(outside_qmt.path(), "knowledge.toml", "version = 1\n");
        std::os::unix::fs::symlink(outside_qmt.path(), root2.path().join(".qmt")).unwrap();
        assert_eq!(
            load_manifest(root2.path()).unwrap_err().code(),
            "manifest_unsafe"
        );
    }

    #[cfg(unix)]
    #[test]
    fn manifest_symlink_inside_root_is_allowed() {
        let root = tempfile::tempdir().unwrap();
        write(root.path(), "config/knowledge.toml", "version = 1\n");
        fs::create_dir_all(root.path().join(".qmt")).unwrap();
        std::os::unix::fs::symlink(
            root.path().join("config/knowledge.toml"),
            root.path().join(".qmt/knowledge.toml"),
        )
        .unwrap();
        assert!(matches!(
            load_manifest(root.path()).unwrap(),
            ManifestOutcome::Loaded(_)
        ));
    }
}
