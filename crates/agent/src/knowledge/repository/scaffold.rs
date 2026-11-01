//! Optional, deterministic inventory scaffolding.
//!
//! A directory may carry a File/Purpose table so readers (and the index) can
//! see which files live there. Scaffolding proposes the *rows* for files that
//! are selected code but not yet listed. It never invents meaning:
//!
//! * new rows have an **empty purpose cell**; a person writes the purpose;
//! * rows are **sorted** by file name and rerunning proposes nothing new;
//! * existing rows, their purposes, **orphan rows** and all prose are preserved
//!   byte for byte; nothing is pruned or rewritten;
//! * there is no repository-root table: the directory must be a non-root path;
//! * no model is involved.
//!
//! # Where it may write
//!
//! Exactly two places, and nowhere else:
//!
//! 1. a **new** directory document (default `<dir>/AGENTS.md`) that consists of
//!    a title and one managed block; or
//! 2. inside the single managed block of an existing document:
//!
//! ```markdown
//! <!-- qmt:inventory:begin -->
//! | File | Purpose |
//! |------|---------|
//! | `queue.rs` | |
//! <!-- qmt:inventory:end -->
//! ```
//!
//! The markers are HTML comments, so tools that load the file as instructions
//! see an ordinary table. Blocks hold only that table, never instructions; a
//! block with anything else in it is refused. An existing file without exactly
//! one block is left **byte-for-byte unchanged** and a finding explains why.
//!
//! Like every maintenance write it is two steps: [`plan_scaffold`] previews and
//! records the target's preimage; [`apply_scaffold`] re-derives the plan from
//! the repository as it is now and refuses a stale one.
//!
//! [`plan_scaffold`]: RepositoryService::plan_scaffold
//! [`apply_scaffold`]: RepositoryService::apply_scaffold

use super::document::DiagnosticSeverity;
use super::export::{ExportAction, ExportError, confine_target, digest, read_target};
use super::maintenance::{Finding, MaintenanceError, PlannedWrite, apply_writes};
use super::purpose::{
    INVENTORY_BEGIN, INVENTORY_END, is_header, is_separator, parse_purpose_rows, unfenced_lines,
};
use super::query::normalize_relative_path;
use super::service::RepositoryService;
use super::store::SourceState;
use serde_json::{Value, json};
use std::collections::BTreeSet;

/// Directory document used when the request names none.
pub const DEFAULT_FILE_NAME: &str = "AGENTS.md";

pub mod codes {
    pub const UNMANAGED: &str = "scaffold_target_unmanaged";
    pub const AMBIGUOUS: &str = "scaffold_target_ambiguous";
    pub const NOT_A_TABLE: &str = "scaffold_block_not_a_table";
    pub const NAME_UNSUPPORTED: &str = "scaffold_name_unsupported";
}

/// Which directory to scaffold.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScaffoldRequest {
    /// Repository-relative, non-root directory.
    pub directory: String,
    /// Document name inside `directory`; defaults to [`DEFAULT_FILE_NAME`].
    pub file_name: Option<String>,
}

/// What applying a plan does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScaffoldAction {
    /// A new directory document is created.
    Create,
    /// Rows are added inside the existing managed block.
    Update,
    /// Every eligible file is already listed (or none is eligible).
    Unchanged,
    /// The existing target cannot be scaffolded safely; see the findings.
    Refused,
}

impl ScaffoldAction {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Update => "update",
            Self::Unchanged => "unchanged",
            Self::Refused => "refused",
        }
    }
}

/// A previewed scaffold. The repository is untouched until it is applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScaffoldPlan {
    pub directory: String,
    /// Normalized repository-relative target document.
    pub target: String,
    pub action: ScaffoldAction,
    /// Files that would gain a row, sorted.
    pub additions: Vec<String>,
    pub findings: Vec<Finding>,
    /// The write, bound to the target's current bytes. `None` when nothing
    /// would change.
    pub write: Option<PlannedWrite>,
}

impl ScaffoldPlan {
    pub fn to_json(&self) -> Value {
        json!({
            "directory": self.directory,
            "target": self.target,
            "action": self.action.as_str(),
            "additions": self.additions,
            "findings": self.findings.iter().map(Finding::to_json).collect::<Vec<_>>(),
        })
    }
}

/// The outcome of applying a plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScaffoldReceipt {
    pub target: String,
    pub written: bool,
    pub rows: usize,
}

/// Why `block` (the text between the markers) cannot take new rows.
enum SpliceError {
    Unmanaged,
    Ambiguous(&'static str),
    NotATable,
}

impl SpliceError {
    fn finding(&self, target: &str) -> Finding {
        let (code, message) = match self {
            Self::Unmanaged => (
                codes::UNMANAGED,
                format!(
                    "`{target}` exists without a `{INVENTORY_BEGIN}` / `{INVENTORY_END}` block; \
                     it was left unchanged. Add the two marker lines where the table belongs, or \
                     scaffold into a new file with `file_name`"
                ),
            ),
            Self::Ambiguous(reason) => (
                codes::AMBIGUOUS,
                format!(
                    "`{target}` has {reason}; it was left unchanged. Keep exactly one \
                     `{INVENTORY_BEGIN}` / `{INVENTORY_END}` pair"
                ),
            ),
            Self::NotATable => (
                codes::NOT_A_TABLE,
                format!(
                    "the managed block in `{target}` holds more than one contiguous File/Purpose \
                     table; it was left unchanged. Managed blocks contain only that table"
                ),
            ),
        };
        Finding::new(Some(target), code, DiagnosticSeverity::Error, message)
    }
}

/// A file name that is safe to write as a table cell and to resolve back as a
/// relative reference.
fn supported_name(name: &str) -> bool {
    !name.is_empty()
        && !name.chars().any(|c| {
            c.is_control()
                || c.is_whitespace()
                || matches!(c, '|' | '`' | '\\' | '%' | '#' | '?' | '[' | ']')
        })
}

fn row(name: &str, eol: &str) -> String {
    format!("| `{name}` | |{eol}")
}

const TABLE_HEAD: [&str; 2] = ["| File | Purpose |", "| --- | --- |"];

/// A new directory document holding only a title and one managed block.
fn new_document(directory: &str, names: &[String]) -> String {
    let title = directory.rsplit('/').next().unwrap_or(directory);
    let mut text = format!("# {title}\n\n{INVENTORY_BEGIN}\n");
    for line in TABLE_HEAD {
        text.push_str(line);
        text.push('\n');
    }
    for name in names {
        text.push_str(&row(name, "\n"));
    }
    text.push_str(INVENTORY_END);
    text.push('\n');
    text
}

/// Insert `names` into the single managed block of `text`, touching no other
/// byte.
fn add_rows(text: &str, names: &[String]) -> Result<String, SpliceError> {
    let (mut begins, mut ends) = (Vec::new(), Vec::new());
    for (index, line) in unfenced_lines(text) {
        match line.trim() {
            INVENTORY_BEGIN => begins.push(index),
            INVENTORY_END => ends.push(index),
            _ => {}
        }
    }
    let ([begin], [end]) = (begins.as_slice(), ends.as_slice()) else {
        return Err(if begins.is_empty() && ends.is_empty() {
            SpliceError::Unmanaged
        } else {
            SpliceError::Ambiguous("missing, repeated or unpaired markers")
        });
    };
    if begin > end {
        return Err(SpliceError::Ambiguous(
            "its end marker before its begin marker",
        ));
    }

    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let eol = if lines[*begin].ends_with("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let block = &lines[begin + 1..*end];
    let filled: Vec<usize> = block
        .iter()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(index, _)| index)
        .collect();

    // The block is empty, or exactly one contiguous header/separator/rows table.
    let (after, head) = match filled.first() {
        None => (*begin, true),
        Some(&first) => {
            let contiguous = filled
                .iter()
                .enumerate()
                .all(|(n, &index)| index == first + n);
            let table = contiguous
                && filled.len() >= 2
                && is_header(block[first].trim())
                && is_separator(block[first + 1].trim())
                && filled[2..]
                    .iter()
                    .all(|&index| block[index].trim_start().starts_with('|'));
            if !table {
                return Err(SpliceError::NotATable);
            }
            (begin + 1 + filled[filled.len() - 1], false)
        }
    };

    let mut insert = String::new();
    if head {
        for line in TABLE_HEAD {
            insert.push_str(line);
            insert.push_str(eol);
        }
    }
    for name in names {
        insert.push_str(&row(name, eol));
    }
    let offset: usize = lines[..=after].iter().map(|line| line.len()).sum();
    Ok([&text[..offset], &insert, &text[offset..]].concat())
}

impl RepositoryService {
    /// Preview the inventory rows missing from one directory's document.
    ///
    /// Reconciles the index to learn the selected code files, then reads the
    /// target document directly. Writes nothing.
    ///
    /// # Errors
    ///
    /// Fails if the directory or file name is unsafe, the target is not a plain
    /// UTF-8 file, or the index is unavailable. An existing target that cannot
    /// be scaffolded is a [`ScaffoldAction::Refused`] plan, not an error.
    pub fn plan_scaffold(
        &self,
        request: &ScaffoldRequest,
    ) -> Result<ScaffoldPlan, MaintenanceError> {
        let directory = normalize_relative_path(&request.directory).map_err(|reason| {
            MaintenanceError::invalid(format!("directory `{}`: {reason}", request.directory))
        })?;
        let name = request.file_name.as_deref().unwrap_or(DEFAULT_FILE_NAME);
        if name.contains('/')
            || normalize_relative_path(name).as_deref() != Ok(name)
            || !name.ends_with(".md")
        {
            return Err(MaintenanceError::invalid(format!(
                "file name `{name}` must be a plain `*.md` name inside the directory"
            )));
        }
        let target = format!("{directory}/{name}");
        let (target, absolute) = confine_target(self.root().root(), &target)?;
        let existing = read_target(&absolute, &target)?;

        self.refresh()?;
        let (_, sources) = self.store().sources()?;
        let mut findings = Vec::new();
        let mut eligible = BTreeSet::new();
        for source in sources
            .iter()
            .filter(|source| source.is_code && source.state == SourceState::Code)
        {
            let Some((parent, file)) = source.path.rsplit_once('/') else {
                continue;
            };
            if parent != directory || source.path == target {
                continue;
            }
            if supported_name(file) {
                eligible.insert(file.to_owned());
            } else {
                findings.push(Finding::new(
                    Some(&source.path),
                    codes::NAME_UNSUPPORTED,
                    DiagnosticSeverity::Info,
                    "the file name cannot be written safely as a table cell and was skipped",
                ));
            }
        }

        let text =
            existing
                .map(String::from_utf8)
                .transpose()
                .map_err(|_| ExportError::NotUtf8 {
                    path: target.clone(),
                })?;
        let covered: BTreeSet<String> = text
            .as_deref()
            .map(|text| parse_purpose_rows(&target, text, 1))
            .unwrap_or_default()
            .into_iter()
            .map(|row| row.target_path)
            .collect();
        let additions: Vec<String> = eligible
            .into_iter()
            .filter(|file| !covered.contains(&format!("{directory}/{file}")))
            .collect();

        let plan = |action, additions: Vec<String>, findings, write| ScaffoldPlan {
            directory: directory.clone(),
            target: target.clone(),
            action,
            additions,
            findings,
            write,
        };
        if additions.is_empty() {
            return Ok(plan(ScaffoldAction::Unchanged, additions, findings, None));
        }
        let (action, content) = match text.as_deref() {
            None => (ScaffoldAction::Create, new_document(&directory, &additions)),
            Some("") => (ScaffoldAction::Update, new_document(&directory, &additions)),
            Some(text) => match add_rows(text, &additions) {
                Ok(content) => (ScaffoldAction::Update, content),
                Err(error) => {
                    findings.push(error.finding(&target));
                    return Ok(plan(ScaffoldAction::Refused, Vec::new(), findings, None));
                }
            },
        };
        let write = PlannedWrite {
            path: target.clone(),
            preimage: text.as_deref().map(|text| digest(text.as_bytes())),
            content,
        };
        Ok(plan(action, additions, findings, Some(write)))
    }

    /// Apply a previewed scaffold if the directory and target are unchanged.
    ///
    /// # Errors
    ///
    /// [`MaintenanceError::Stale`] if re-deriving the plan from the repository
    /// as it is now gives a different result; nothing is written then.
    pub fn apply_scaffold(&self, plan: &ScaffoldPlan) -> Result<ScaffoldReceipt, MaintenanceError> {
        let name = plan.target.rsplit('/').next().map(str::to_owned);
        let fresh = self.plan_scaffold(&ScaffoldRequest {
            directory: plan.directory.clone(),
            file_name: name,
        })?;
        if fresh != *plan {
            return Err(MaintenanceError::Stale {
                path: plan.target.clone(),
            });
        }
        let written = match &fresh.write {
            Some(write) => {
                !apply_writes(self.root().root(), std::slice::from_ref(write))?.is_empty()
            }
            None => false,
        };
        Ok(ScaffoldReceipt {
            target: fresh.target,
            written,
            rows: fresh.additions.len(),
        })
    }
}

impl ScaffoldPlan {
    /// The action a write plan performs, in export terms.
    pub fn export_action(&self) -> ExportAction {
        match self.action {
            ScaffoldAction::Create => ExportAction::Create,
            ScaffoldAction::Update => ExportAction::Replace,
            ScaffoldAction::Unchanged | ScaffoldAction::Refused => ExportAction::Unchanged,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::knowledge::repository::browse::tests::Fx;
    use std::fs;

    fn fixture() -> Fx {
        let fx = Fx::new();
        for name in ["zeta.rs", "alpha.rs", "mid.rs"] {
            fx.put(&format!("src/sched/{name}"), "pub fn f() {}\n");
        }
        fx.put("src/other/o.rs", "pub fn o() {}\n");
        fx
    }

    fn request(directory: &str) -> ScaffoldRequest {
        ScaffoldRequest {
            directory: directory.into(),
            file_name: None,
        }
    }

    fn plan(fx: &Fx, directory: &str) -> ScaffoldPlan {
        fx.service.plan_scaffold(&request(directory)).unwrap()
    }

    fn read(fx: &Fx, rel: &str) -> String {
        fs::read_to_string(fx.repo.join(rel)).unwrap()
    }

    #[test]
    fn a_new_directory_document_lists_sorted_files_with_empty_purposes() {
        let fx = fixture();
        let plan = plan(&fx, "src/sched");
        assert_eq!(plan.action, ScaffoldAction::Create);
        assert_eq!(plan.target, "src/sched/AGENTS.md");
        assert_eq!(plan.additions, ["alpha.rs", "mid.rs", "zeta.rs"]);
        assert!(
            !fx.repo.join("src/sched/AGENTS.md").exists(),
            "preview writes nothing"
        );

        let receipt = fx.service.apply_scaffold(&plan).unwrap();
        assert!(receipt.written);
        assert_eq!(
            read(&fx, "src/sched/AGENTS.md"),
            "# sched\n\n<!-- qmt:inventory:begin -->\n| File | Purpose |\n| --- | --- |\n\
             | `alpha.rs` | |\n| `mid.rs` | |\n| `zeta.rs` | |\n<!-- qmt:inventory:end -->\n"
        );
        assert!(
            !fx.repo.join("src/other/AGENTS.md").exists(),
            "other directories untouched"
        );
    }

    #[test]
    fn rerunning_proposes_nothing_and_never_duplicates_rows() {
        let fx = fixture();
        fx.service.apply_scaffold(&plan(&fx, "src/sched")).unwrap();
        let before = read(&fx, "src/sched/AGENTS.md");
        let again = plan(&fx, "src/sched");
        assert_eq!(again.action, ScaffoldAction::Unchanged);
        assert!(again.additions.is_empty() && again.write.is_none());
        assert!(!fx.service.apply_scaffold(&again).unwrap().written);
        assert_eq!(read(&fx, "src/sched/AGENTS.md"), before);
    }

    #[test]
    fn authored_purposes_prose_and_orphan_rows_are_preserved_byte_for_byte() {
        let fx = fixture();
        let authored = "# Scheduler\n\nHand-written intro.\n\n<!-- qmt:inventory:begin -->\n\
            | File | Purpose |\n|---|---|\n| `alpha.rs` | Dispatches work. |\n| `gone.rs` | Removed long ago. |\n\
            <!-- qmt:inventory:end -->\n\n## Notes\nKeep me.\n";
        fx.put("src/sched/AGENTS.md", authored);
        let plan = plan(&fx, "src/sched");
        assert_eq!(plan.action, ScaffoldAction::Update);
        assert_eq!(plan.additions, ["mid.rs", "zeta.rs"]);
        fx.service.apply_scaffold(&plan).unwrap();

        let after = read(&fx, "src/sched/AGENTS.md");
        let expected = authored.replace(
            "| `gone.rs` | Removed long ago. |\n",
            "| `gone.rs` | Removed long ago. |\n| `mid.rs` | |\n| `zeta.rs` | |\n",
        );
        assert_eq!(after, expected);
        assert!(after.contains("| `alpha.rs` | Dispatches work. |"));
        assert!(
            after.contains("| `gone.rs` | Removed long ago. |"),
            "orphan rows are never pruned"
        );
    }

    #[test]
    fn files_listed_in_an_authored_table_outside_the_block_count_as_covered() {
        let fx = fixture();
        fx.put(
            "src/sched/AGENTS.md",
            "| File | Purpose |\n|---|---|\n| `alpha.rs` | Authored. |\n\n<!-- qmt:inventory:begin -->\n<!-- qmt:inventory:end -->\n",
        );
        assert_eq!(plan(&fx, "src/sched").additions, ["mid.rs", "zeta.rs"]);
    }

    #[test]
    fn an_empty_block_gains_a_header_and_rows() {
        let fx = fixture();
        fx.put(
            "src/sched/AGENTS.md",
            "Intro\n<!-- qmt:inventory:begin -->\n<!-- qmt:inventory:end -->\nTail\n",
        );
        fx.service.apply_scaffold(&plan(&fx, "src/sched")).unwrap();
        assert_eq!(
            read(&fx, "src/sched/AGENTS.md"),
            "Intro\n<!-- qmt:inventory:begin -->\n| File | Purpose |\n| --- | --- |\n| `alpha.rs` | |\n| `mid.rs` | |\n| `zeta.rs` | |\n<!-- qmt:inventory:end -->\nTail\n"
        );
    }

    #[test]
    fn crlf_documents_keep_their_line_endings() {
        let fx = fixture();
        fx.put(
            "src/sched/AGENTS.md",
            "# S\r\n<!-- qmt:inventory:begin -->\r\n| File | Purpose |\r\n|---|---|\r\n<!-- qmt:inventory:end -->\r\n",
        );
        fx.service.apply_scaffold(&plan(&fx, "src/sched")).unwrap();
        let after = read(&fx, "src/sched/AGENTS.md");
        assert!(after.contains("| `alpha.rs` | |\r\n"));
        assert!(
            !after.replace("\r\n", "").contains('\n'),
            "no bare LF introduced"
        );
    }

    #[test]
    fn unmarked_and_multi_block_files_are_refused_and_left_untouched() {
        let fx = fixture();
        for (content, code) in [
            ("# Authored instructions\nDo things.\n", codes::UNMANAGED),
            (
                "<!-- qmt:inventory:begin -->\n<!-- qmt:inventory:end -->\n<!-- qmt:inventory:begin -->\n<!-- qmt:inventory:end -->\n",
                codes::AMBIGUOUS,
            ),
            ("<!-- qmt:inventory:begin -->\n", codes::AMBIGUOUS),
            (
                "<!-- qmt:inventory:end -->\n<!-- qmt:inventory:begin -->\n",
                codes::AMBIGUOUS,
            ),
        ] {
            fx.put("src/sched/AGENTS.md", content);
            let plan = plan(&fx, "src/sched");
            assert_eq!(plan.action, ScaffoldAction::Refused, "{content}");
            assert_eq!(plan.findings[0].code, code);
            assert!(plan.write.is_none() && plan.additions.is_empty());
            assert!(!fx.service.apply_scaffold(&plan).unwrap().written);
            assert_eq!(
                read(&fx, "src/sched/AGENTS.md"),
                content,
                "byte-for-byte unchanged"
            );
        }
    }

    #[test]
    fn markers_inside_code_fences_are_examples_not_blocks() {
        let fx = fixture();
        let content =
            "Example:\n```\n<!-- qmt:inventory:begin -->\n<!-- qmt:inventory:end -->\n```\n";
        fx.put("src/sched/AGENTS.md", content);
        assert_eq!(plan(&fx, "src/sched").action, ScaffoldAction::Refused);
    }

    #[test]
    fn blocks_holding_instructions_or_other_content_are_refused() {
        let fx = fixture();
        for body in [
            "Always run the tests.\n",
            "| File | Purpose |\n|---|---|\n| `alpha.rs` | |\nExtra prose.\n",
            "| File | Purpose |\n|---|---|\n\n| `alpha.rs` | |\n",
            "| Name | Notes |\n|---|---|\n",
        ] {
            let content =
                format!("<!-- qmt:inventory:begin -->\n{body}<!-- qmt:inventory:end -->\n");
            fx.put("src/sched/AGENTS.md", &content);
            let plan = plan(&fx, "src/sched");
            assert_eq!(plan.action, ScaffoldAction::Refused, "{body}");
            assert_eq!(plan.findings[0].code, codes::NOT_A_TABLE);
            assert_eq!(read(&fx, "src/sched/AGENTS.md"), content);
        }
    }

    #[test]
    fn generated_blocks_contain_only_the_table() {
        let fx = fixture();
        fx.service.apply_scaffold(&plan(&fx, "src/sched")).unwrap();
        let text = read(&fx, "src/sched/AGENTS.md");
        let block = text
            .split(INVENTORY_BEGIN)
            .nth(1)
            .and_then(|rest| rest.split(INVENTORY_END).next())
            .unwrap();
        assert!(
            block
                .lines()
                .filter(|line| !line.trim().is_empty())
                .all(|line| line.starts_with('|'))
        );
    }

    #[test]
    fn a_target_edit_between_preview_and_apply_is_rejected() {
        let fx = fixture();
        fx.put(
            "src/sched/AGENTS.md",
            "<!-- qmt:inventory:begin -->\n<!-- qmt:inventory:end -->\n",
        );
        let stale = plan(&fx, "src/sched");
        fx.put(
            "src/sched/AGENTS.md",
            "# Edited meanwhile\n<!-- qmt:inventory:begin -->\n<!-- qmt:inventory:end -->\n",
        );
        let error = fx.service.apply_scaffold(&stale).unwrap_err();
        assert_eq!(error.code(), "maintenance_stale");
        assert!(read(&fx, "src/sched/AGENTS.md").starts_with("# Edited meanwhile"));
        assert!(!read(&fx, "src/sched/AGENTS.md").contains("alpha.rs"));
    }

    #[test]
    fn a_new_source_file_between_preview_and_apply_is_rejected_not_silently_dropped() {
        let fx = fixture();
        let stale = plan(&fx, "src/sched");
        fx.put("src/sched/late.rs", "pub fn late() {}\n");
        assert_eq!(
            fx.service.apply_scaffold(&stale).unwrap_err().code(),
            "maintenance_stale"
        );
        assert!(!fx.repo.join("src/sched/AGENTS.md").exists());
    }

    #[test]
    fn a_created_target_between_preview_and_apply_is_rejected() {
        let fx = fixture();
        let stale = plan(&fx, "src/sched");
        fx.put("src/sched/AGENTS.md", "someone else's file\n");
        assert!(fx.service.apply_scaffold(&stale).is_err());
        assert_eq!(read(&fx, "src/sched/AGENTS.md"), "someone else's file\n");
    }

    #[test]
    fn root_and_unsafe_directories_and_names_are_rejected() {
        let fx = fixture();
        for bad in ["", ".", "/", "../x", "/abs"] {
            assert_eq!(
                fx.service.plan_scaffold(&request(bad)).unwrap_err().code(),
                "request_invalid",
                "{bad:?}"
            );
        }
        for name in ["a/b.md", "../x.md", "notes.txt", ".."] {
            let error = fx
                .service
                .plan_scaffold(&ScaffoldRequest {
                    directory: "src/sched".into(),
                    file_name: Some(name.into()),
                })
                .unwrap_err();
            assert_eq!(error.code(), "request_invalid", "{name}");
        }
    }

    #[test]
    fn agents_and_git_directories_are_never_written() {
        let fx = fixture();
        fx.put(".agents/memories/m.rs", "fn m() {}\n");
        fx.put(
            ".qmt/knowledge.toml",
            "version = 1\n[code]\ninclude = [\"**/*.rs\"]\n",
        );
        for directory in [".agents/memories", ".git/hooks"] {
            let error = fx.service.plan_scaffold(&request(directory)).unwrap_err();
            assert_eq!(error.code(), "export_target_forbidden", "{directory}");
        }
    }

    #[test]
    fn a_directory_with_no_selected_code_proposes_nothing() {
        let fx = fixture();
        let plan = plan(&fx, "src/empty");
        assert_eq!(plan.action, ScaffoldAction::Unchanged);
        assert!(!fx.repo.join("src/empty").exists());
    }

    #[test]
    fn unsafe_file_names_are_skipped_with_a_finding_not_written() {
        let fx = fixture();
        fx.put("src/sched/we ird.rs", "fn w() {}\n");
        let plan = plan(&fx, "src/sched");
        assert_eq!(plan.additions, ["alpha.rs", "mid.rs", "zeta.rs"]);
        assert_eq!(plan.findings[0].code, codes::NAME_UNSUPPORTED);
    }

    #[test]
    fn scaffold_rows_are_read_back_as_citable_purpose_rows_with_empty_purposes() {
        let fx = fixture();
        fx.service.apply_scaffold(&plan(&fx, "src/sched")).unwrap();
        let rows = parse_purpose_rows("src/sched/AGENTS.md", &read(&fx, "src/sched/AGENTS.md"), 1);
        assert_eq!(rows.len(), 3);
        assert!(rows.iter().all(|row| row.purpose.is_empty() && row.managed));
        assert_eq!(rows[0].target_path, "src/sched/alpha.rs");
    }
}
