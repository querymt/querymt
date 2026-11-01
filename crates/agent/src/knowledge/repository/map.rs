//! Bounded source maps.
//!
//! A *map* lists the selected code files under a directory with their
//! declarations (kind, qualified name, signature, line range), grouped by
//! directory in path order. It is rendered from the checkout-local index and is
//! a navigation aid: it never replaces reading the current source, and nothing
//! in it is inferred meaning.
//!
//! # Provenance
//!
//! * Files, declarations, signatures and ranges are **source-derived**.
//! * `purpose:` lines are **authored**: they quote a File/Purpose row a person
//!   wrote in a document, with a `document:line` citation. A row with no
//!   purpose text contributes no line; nothing is ever generated to fill it.
//! * A file that could not be read as symbols (unsupported language, parse
//!   failure) is listed as path-only and says so.
//!
//! # Determinism
//!
//! [`render_map`] is a pure function of the stored records: stable path and
//! source ordering, no timestamps, no generation numbers and no machine-local
//! paths. Equal sources therefore render byte-identical maps in any checkout,
//! which is what makes a committed export reproducible.
//!
//! # Bounds
//!
//! The output never exceeds its byte budget. A file entry is either listed
//! whole (its own declaration list is capped) or not at all; when entries are
//! left out the map ends with a footer that counts them and names narrower
//! paths to ask for instead.

use super::query::normalize_relative_path;
use super::reconcile::ReconcileStatus;
use super::service::{RepositoryService, ServiceError};
use super::store::{IndexDiagnostic, MapData, MapFile, PurposeMatch};
use crate::index::symbol_index::SymbolKind;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use thiserror::Error;

/// Map size when the caller gives no budget.
pub const DEFAULT_MAP_BYTES: usize = 16 * 1024;
/// Largest map budget a caller can request.
pub const MAX_MAP_BYTES: usize = 64 * 1024;
/// Smallest budget that still fits a header and a footer.
pub const MIN_MAP_BYTES: usize = 2048;

/// Room kept for the truncation footer: its fixed text plus at most
/// [`MAX_NARROWER`] paths of [`MAX_NARROWER_PATH_BYTES`] bytes each.
const FOOTER_RESERVE: usize = 640;
/// Declarations listed per file before the rest are summarized.
const MAX_DECLARATIONS_PER_FILE: usize = 60;
const SIGNATURE_BYTES: usize = 120;
const MAX_NARROWER: usize = 5;
const MAX_NARROWER_PATH_BYTES: usize = 80;

/// What to map.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MapRequest {
    /// Repository-relative directory or file to confine the map to.
    pub path: Option<String>,
    /// Byte budget; defaults to [`DEFAULT_MAP_BYTES`], clamped to
    /// [`MIN_MAP_BYTES`]..=[`MAX_MAP_BYTES`].
    pub max_bytes: Option<usize>,
}

/// A rendered map and how complete it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedMap {
    pub text: String,
    /// Code files in scope.
    pub files_total: usize,
    /// Files listed in `text`.
    pub files_shown: usize,
    /// Declarations in the files that were not listed.
    pub declarations_omitted: usize,
    pub truncated: bool,
    /// Directories to ask for instead when `truncated`.
    pub narrower: Vec<String>,
}

/// The outcome of [`RepositoryService::map`].
#[derive(Debug, Clone, PartialEq)]
pub struct MapOutput {
    /// Generation the map was read from (local; not part of `map.text`).
    pub generation: u64,
    pub status: ReconcileStatus,
    /// Normalized scope; empty for the whole repository.
    pub scope: String,
    pub map: RenderedMap,
    /// Why coverage of the selected corpus is incomplete, if it is.
    pub coverage: Vec<IndexDiagnostic>,
}

/// Why a map could not be produced.
#[derive(Debug, Error)]
pub enum MapError {
    #[error("invalid path `{path}`: {reason}")]
    InvalidPath { path: String, reason: &'static str },
    #[error(transparent)]
    Service(#[from] ServiceError),
}

impl MapError {
    /// Stable machine-readable diagnostic code.
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidPath { .. } => "path_invalid",
            Self::Service(error) => error.code(),
        }
    }
}

/// Declarations worth listing: the ones a reader navigates by. Imports are
/// counted, and fields, variants and tests stay reachable through their parent.
fn is_listed(kind: SymbolKind) -> bool {
    !matches!(
        kind,
        SymbolKind::Import | SymbolKind::Field | SymbolKind::EnumVariant | SymbolKind::Test
    )
}

fn directory_of(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(directory, _)| directory)
}

fn name_of(path: &str) -> &str {
    path.rsplit_once('/').map_or(path, |(_, name)| name)
}

/// Collapse whitespace, neutralize code-span delimiters and cut on a character
/// boundary.
fn clip(text: &str, max: usize) -> String {
    let mut out = String::new();
    for word in text.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
        if out.len() > max {
            break;
        }
    }
    let out = out.replace('`', "'");
    if out.len() <= max {
        return out;
    }
    let mut end = max;
    while !out.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &out[..end])
}

fn purpose_lines(rows: &[&PurposeMatch], indent: &str, out: &mut String) {
    for found in rows {
        let row = &found.row;
        if row.purpose.is_empty() {
            continue;
        }
        let _ = writeln!(
            out,
            "{indent}purpose (authored): {} [{}:{}]",
            clip(&row.purpose, 240),
            row.doc_path,
            row.line
        );
    }
}

fn file_entry(file: &MapFile, purposes: &[&PurposeMatch]) -> String {
    let mut out = String::new();
    let name = name_of(&file.path);
    let listed: Vec<_> = file
        .symbols
        .iter()
        .filter(|symbol| is_listed(symbol.kind))
        .collect();
    let imports = file
        .symbols
        .iter()
        .filter(|symbol| symbol.kind == SymbolKind::Import)
        .count();
    match (file.status, file.language.as_deref()) {
        (super::code::CodeStatus::Extracted, language) => {
            let _ = writeln!(
                out,
                "- `{name}` · {} · {} declaration(s){}",
                language.unwrap_or("unknown"),
                listed.len(),
                if imports > 0 {
                    format!(" · {imports} import(s)")
                } else {
                    String::new()
                }
            );
        }
        (super::code::CodeStatus::Unsupported, _) => {
            let _ = writeln!(out, "- `{name}` · path only (unsupported language)");
        }
        (super::code::CodeStatus::Failed, language) => {
            let _ = writeln!(
                out,
                "- `{name}` · {} · path only (symbols could not be extracted)",
                language.unwrap_or("unknown")
            );
        }
    }
    purpose_lines(purposes, "  ", &mut out);
    for symbol in listed.iter().take(MAX_DECLARATIONS_PER_FILE) {
        let indent = "  ".repeat(usize::try_from(symbol.depth).unwrap_or(0).min(3) + 1);
        let _ = writeln!(
            out,
            "{indent}- {} `{}` ({}-{}): `{}`",
            symbol.kind.as_str(),
            clip(&symbol.qualified_name, SIGNATURE_BYTES),
            symbol.start_line,
            symbol.end_line,
            clip(&symbol.signature, SIGNATURE_BYTES)
        );
    }
    if listed.len() > MAX_DECLARATIONS_PER_FILE {
        let _ = writeln!(
            out,
            "  - … {} more declaration(s); fetch the file's record for all of them",
            listed.len() - MAX_DECLARATIONS_PER_FILE
        );
    }
    out
}

fn heading_entry(directory: &str, purposes: &[&PurposeMatch]) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "\n## {}",
        if directory.is_empty() {
            "(repository root)"
        } else {
            directory
        }
    );
    purpose_lines(purposes, "", &mut out);
    out
}

/// Render the map of `data` for `scope` within `budget` bytes.
///
/// Pure: the same records always render the same bytes.
pub fn render_map(data: &MapData, scope: Option<&str>, budget: usize) -> RenderedMap {
    let mut header = format!(
        "# Source map: {}\n\nSource-derived structure of selected code files: declarations, \
         signatures and line ranges as written. Lines marked `purpose (authored)` quote a \
         File/Purpose row from the cited document. Nothing here is inferred meaning; read the \
         current source before relying on it.\n",
        scope.unwrap_or("repository root")
    );
    let total = data.files.len();
    if total == 0 {
        header.push_str("\nNo selected code files under this path.\n");
        return RenderedMap {
            text: header,
            files_total: 0,
            files_shown: 0,
            declarations_omitted: 0,
            truncated: false,
            narrower: Vec::new(),
        };
    }

    let mut by_target: BTreeMap<&str, Vec<&PurposeMatch>> = BTreeMap::new();
    for found in &data.purposes {
        by_target
            .entry(found.row.target_path.as_str())
            .or_default()
            .push(found);
    }
    let rows_for = |path: &str| by_target.get(path).map_or(&[][..], Vec::as_slice);

    let limit = budget.saturating_sub(FOOTER_RESERVE);
    let mut text = header;
    let mut shown = 0;
    let mut previous_dir: Option<&str> = None;
    for file in &data.files {
        let directory = directory_of(&file.path);
        let mut entry = String::new();
        if previous_dir != Some(directory) {
            entry.push_str(&heading_entry(directory, rows_for(directory)));
        }
        entry.push_str(&file_entry(file, rows_for(&file.path)));
        if text.len() + entry.len() > limit {
            break;
        }
        text.push_str(&entry);
        previous_dir = Some(directory);
        shown += 1;
    }

    let omitted = &data.files[shown..];
    let truncated = !omitted.is_empty();
    let declarations_omitted = omitted
        .iter()
        .flat_map(|file| &file.symbols)
        .filter(|symbol| is_listed(symbol.kind))
        .count();
    let mut narrower: Vec<String> = Vec::new();
    for file in omitted {
        let directory = directory_of(&file.path);
        let candidate = if directory.is_empty() {
            file.path.as_str()
        } else {
            directory
        };
        if candidate.len() <= MAX_NARROWER_PATH_BYTES
            && !narrower.iter().any(|seen| seen == candidate)
        {
            narrower.push(candidate.to_string());
            if narrower.len() == MAX_NARROWER {
                break;
            }
        }
    }
    if truncated {
        let _ = write!(
            text,
            "\n> Truncated: {shown} of {total} files listed; {declarations_omitted} declaration(s) \
             in the remaining files are not shown."
        );
        if narrower.is_empty() {
            text.push_str(" Ask for a narrower path.\n");
        } else {
            let hints: Vec<String> = narrower.iter().map(|path| format!("`{path}`")).collect();
            let _ = writeln!(
                text,
                " Narrow with a path, for example {}.",
                hints.join(", ")
            );
        }
    }
    RenderedMap {
        text,
        files_total: total,
        files_shown: shown,
        declarations_omitted,
        truncated,
        narrower,
    }
}

impl RepositoryService {
    /// The map of `request.path` (default: the whole checkout), read from the
    /// generation the refresh just reconciled.
    pub fn map(&self, request: &MapRequest) -> Result<MapOutput, MapError> {
        let scope = request
            .path
            .as_deref()
            .map(|raw| {
                normalize_relative_path(raw).map_err(|reason| MapError::InvalidPath {
                    path: raw.to_string(),
                    reason,
                })
            })
            .transpose()?;
        let budget = request
            .max_bytes
            .unwrap_or(DEFAULT_MAP_BYTES)
            .clamp(MIN_MAP_BYTES, MAX_MAP_BYTES);
        let report = self.refresh()?;
        let data = self
            .store()
            .map_data(scope.as_deref())
            .map_err(ServiceError::unavailable)?;
        Ok(MapOutput {
            generation: data.generation,
            status: report.status,
            map: render_map(&data, scope.as_deref(), budget),
            scope: scope.unwrap_or_default(),
            coverage: report.diagnostics,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::knowledge::repository::browse::tests::Fx;

    const QUEUE: &str = "use std::fmt;\npub struct Queue { items: Vec<u32> }\nimpl Queue {\n    pub fn push(&mut self, item: u32) {}\n}\npub fn helper() {}\n";

    fn map(fx: &Fx, path: Option<&str>, bytes: Option<usize>) -> MapOutput {
        fx.service
            .map(&MapRequest {
                path: path.map(str::to_string),
                max_bytes: bytes,
            })
            .unwrap()
    }

    #[test]
    fn declarations_are_listed_with_kind_signature_and_range() {
        let fx = Fx::new();
        fx.put("src/queue.rs", QUEUE);
        let out = map(&fx, None, None);
        let text = &out.map.text;
        assert!(text.contains("## src\n"), "{text}");
        assert!(text.contains("- `queue.rs` · rust · "), "{text}");
        assert!(
            text.contains("struct `Queue` (2-2): `pub struct Queue {"),
            "{text}"
        );
        assert!(text.contains("method `Queue::push` (4-4)"), "{text}");
        assert!(text.contains("1 import(s)"));
        // Fields stay reachable through their parent but are not listed.
        assert!(!text.contains("Queue::items"));
        assert_eq!((out.map.files_total, out.map.files_shown), (1, 1));
        assert!(!out.map.truncated);
    }

    #[test]
    fn authored_purposes_are_cited_and_empty_ones_stay_silent() {
        let fx = Fx::new();
        fx.put("src/queue.rs", QUEUE);
        fx.put("src/quiet.rs", "pub fn quiet() {}\n");
        fx.put(
            "docs/map.md",
            "# Map\n\n| File | Purpose |\n|--|--|\n| `../src/queue.rs` | Holds pending work. |\n| `../src/quiet.rs` | |\n| `../src` | The runtime. |\n",
        );
        let text = map(&fx, None, None).map.text;
        assert!(
            text.contains("purpose (authored): Holds pending work. [docs/map.md:5]"),
            "{text}"
        );
        assert!(text.contains("purpose (authored): The runtime. [docs/map.md:7]"));
        let quiet = text.split("`quiet.rs`").nth(1).unwrap();
        let first_line = quiet.lines().nth(1).unwrap_or_default();
        assert!(
            !first_line.contains("purpose"),
            "an empty purpose must stay empty: {first_line}"
        );
    }

    #[test]
    fn partial_extraction_is_stated_not_hidden() {
        let fx = Fx::new();
        fx.put(
            ".qmt/knowledge.toml",
            "version = 1\n[code]\ninclude = [\"src/**/*\"]\n",
        );
        fx.put("src/ok.rs", "pub fn ok() {}\n");
        fx.put("src/blob.xyz", "opaque");
        let text = map(&fx, None, None).map.text;
        assert!(text.contains("- `blob.xyz` · path only (unsupported language)"));
        assert!(text.contains("- `ok.rs` · rust · 1 declaration(s)"));
    }

    #[test]
    fn scope_confines_the_map_and_rejects_unsafe_paths() {
        let fx = Fx::new();
        fx.put("src/a/one.rs", "pub fn one() {}\n");
        fx.put("src/b/two.rs", "pub fn two() {}\n");
        let text = map(&fx, Some("src/a"), None).map.text;
        assert!(text.contains("# Source map: src/a"));
        assert!(text.contains("one.rs") && !text.contains("two.rs"));

        let empty = map(&fx, Some("docs"), None).map;
        assert_eq!(empty.files_total, 0);
        assert!(empty.text.contains("No selected code files"));

        for bad in ["../x", "/etc", "a/../../b"] {
            let err = fx
                .service
                .map(&MapRequest {
                    path: Some(bad.into()),
                    max_bytes: None,
                })
                .unwrap_err();
            assert_eq!(err.code(), "path_invalid", "{bad}");
        }
    }

    #[test]
    fn over_budget_maps_are_bounded_and_say_how_to_narrow() {
        let fx = Fx::new();
        for dir in ["alpha", "beta", "gamma", "delta"] {
            for n in 0..12 {
                fx.put(
                    &format!("src/{dir}/file{n}.rs"),
                    "pub fn a() {}\npub fn b() {}\npub fn c() {}\n",
                );
            }
        }
        let out = map(&fx, None, Some(MIN_MAP_BYTES));
        let map = &out.map;
        assert!(map.text.len() <= MIN_MAP_BYTES, "{}", map.text.len());
        assert!(map.truncated);
        assert_eq!(map.files_total, 48);
        assert!(map.files_shown > 0 && map.files_shown < 48);
        assert!(map.declarations_omitted > 0);
        assert!(map.text.contains("> Truncated:"), "{}", map.text);
        assert!(map.narrower.iter().all(|path| path.starts_with("src/")));
        assert!(
            map.text.contains(&format!("`{}`", map.narrower[0])),
            "narrowing paths are named in the text"
        );
        // Entries are whole or absent: the last listed file is complete.
        let listed = map.text.matches("- `file").count();
        assert_eq!(listed, map.files_shown);
        // Narrowing to one directory fits.
        let narrow = self::map(&fx, Some(&map.narrower[0]), Some(MAX_MAP_BYTES)).map;
        assert!(!narrow.truncated);
    }

    #[test]
    fn a_very_large_file_lists_a_capped_number_of_declarations() {
        let fx = Fx::new();
        let source: String = (0..150).map(|n| format!("pub fn f{n}() {{}}\n")).collect();
        fx.put("src/big.rs", &source);
        let text = map(&fx, None, None).map.text;
        assert_eq!(
            text.matches("- function `f").count(),
            MAX_DECLARATIONS_PER_FILE
        );
        assert!(text.contains("… 90 more declaration(s)"));
    }

    #[test]
    fn maps_follow_edits_without_a_watcher_and_drop_deleted_symbols() {
        let fx = Fx::new();
        fx.put("src/a.rs", "pub fn old_name() {}\n");
        assert!(map(&fx, None, None).map.text.contains("old_name"));
        fx.put("src/a.rs", "pub fn new_name() {}\n");
        let text = map(&fx, None, None).map.text;
        assert!(text.contains("new_name") && !text.contains("old_name"));
        std::fs::remove_file(fx.repo.join("src/a.rs")).unwrap();
        assert_eq!(map(&fx, None, None).map.files_total, 0);
    }

    #[test]
    fn identical_sources_render_identical_maps_in_different_checkouts() {
        let render = || {
            let fx = Fx::new();
            fx.put("src/queue.rs", QUEUE);
            fx.put("src/util/mod.rs", "pub mod x {}\n");
            fx.put(
                "docs/map.md",
                "# Map\n| File | Purpose |\n|--|--|\n| `../src/queue.rs` | Work. |\n",
            );
            map(&fx, None, None).map.text
        };
        let (a, b) = (render(), render());
        assert_eq!(a, b);
        assert!(!a.contains("/tmp") && !a.contains("/var/") && !a.contains("generation"));
    }

    #[test]
    fn code_spans_survive_hostile_signatures() {
        assert_eq!(clip("fn `x`()", 50), "fn 'x'()");
        assert_eq!(clip("  a \n b\t c ", 50), "a b c");
        let long = "é".repeat(100);
        let clipped = clip(&long, 11);
        assert!(clipped.ends_with('…') && clipped.len() <= 14);
    }
}
