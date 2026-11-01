//! Repository knowledge tools: `repo_knowledge_query`, `_get`, `_list`, `_stats`.
//!
//! Separate from the session-memory `knowledge_*` tools, whose schemas and
//! behavior are untouched. These tools take no scope, root or checkout
//! argument: every call resolves the session's *effective* working directory
//! through the host's registry, so an ID or path can only ever address the one
//! authorized checkout. They are registered only when the host enabled
//! repository knowledge.

use crate::knowledge::repository::{
    BindingError, CatalogKind, CatalogOrigin, CatalogRequest, DirectoryCatalog, DocumentCatalog,
    EntryKind, EvidenceState, Fetched, FileInfo, GetError, GetRequest, HitKind, Lane, ListRequest,
    MapError, MapRequest, Provenance, PurposeInfo, QueryError, QueryRequest, RepositoryService,
    SectionContent, SectionRef, StateSignals, SymbolInfo,
};
use crate::tools::{CapabilityRequirement, Tool as ToolTrait, ToolContext, ToolError};
use async_trait::async_trait;
use querymt::chat::{FunctionTool, Tool, ToolResultPart};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::sync::Arc;

/// Hard ceiling, mirroring the configuration cap.
const MAX_BUDGET: usize = 64 * 1024;
/// Output reserved for headers and guidance when sizing a content slice.
const FRAME_BYTES: usize = 1024;
const DISABLED: &str = "Repository knowledge is not enabled for this session. \
Use the normal source tools (glob, search_text, index, read_tool).";

/// The four repository tools, in registration order.
pub fn repository_knowledge_tools() -> Vec<Arc<dyn ToolTrait>> {
    vec![
        Arc::new(RepoKnowledgeQueryTool),
        Arc::new(RepoKnowledgeGetTool),
        Arc::new(RepoKnowledgeListTool),
        Arc::new(RepoKnowledgeStatsTool),
    ]
}

/// Names of the repository tools.
pub const REPOSITORY_TOOL_NAMES: [&str; 4] = [
    "repo_knowledge_query",
    "repo_knowledge_get",
    "repo_knowledge_list",
    "repo_knowledge_stats",
];

// ─── Shared plumbing ─────────────────────────────────────────────────────────

struct Bound {
    service: Arc<RepositoryService>,
    budget: usize,
    default_limit: usize,
}

/// Resolve the session's checkout. Fails with an actionable diagnostic when the
/// feature is off, there is no working directory, or policy denies the root.
fn bind(context: &dyn ToolContext) -> Result<Bound, ToolError> {
    let registry = context
        .repository_knowledge()
        .ok_or_else(|| ToolError::ProviderError(DISABLED.into()))?;
    let service = registry.service_for(context.cwd()).map_err(|error| {
        let message = format!(
            "Repository knowledge is unavailable ({}): {error}. \
             Use the normal source tools instead.",
            error.code()
        );
        match error {
            BindingError::Root(crate::knowledge::repository::RootError::Denied { .. }) => {
                ToolError::PermissionDenied(message)
            }
            _ => ToolError::ProviderError(message),
        }
    })?;
    let settings = registry.settings();
    Ok(Bound {
        service,
        budget: settings.effective_max_response_bytes().min(MAX_BUDGET),
        default_limit: settings.effective_default_result_limit(),
    })
}

/// Run a blocking repository operation off the async executor.
async fn blocking<T, F>(work: F) -> Result<T, ToolError>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| ToolError::ProviderError(format!("repository task failed: {error}")))
}

fn unavailable(error: impl std::fmt::Display, code: &str) -> ToolError {
    ToolError::ProviderError(format!(
        "Repository knowledge is unavailable ({code}): {error}. Use the normal source tools instead."
    ))
}

fn query_error(error: QueryError) -> ToolError {
    match error {
        QueryError::Service(inner) => unavailable(&inner, inner.code()),
        other => ToolError::InvalidRequest(format!("{} ({})", other, other.code())),
    }
}

fn map_error(error: MapError) -> ToolError {
    match error {
        MapError::Service(inner) => unavailable(&inner, inner.code()),
        other => ToolError::InvalidRequest(format!("{} ({})", other, other.code())),
    }
}

fn get_error(error: GetError) -> ToolError {
    match error {
        GetError::Service(inner) => unavailable(&inner, inner.code()),
        GetError::StaleSelector { .. } | GetError::Ambiguous { .. } => ToolError::InvalidRequest(
            format!("{} ({})\n{}", error, error.code(), sections_hint(&error)),
        ),
        other => ToolError::InvalidRequest(format!("{} ({})", other, other.code())),
    }
}

fn sections_hint(error: &GetError) -> String {
    let (label, sections): (&str, &[SectionRef]) = match error {
        GetError::StaleSelector { available, .. } => ("Sections that exist now:", available),
        GetError::Ambiguous { candidates, .. } => {
            ("Pick one by its id or add `~N` to the heading:", candidates)
        }
        _ => return String::new(),
    };
    let mut out = String::from(label);
    for section in sections.iter().take(20) {
        let _ = write!(out, "\n- {} (id: {})", selector_text(section), section.key);
    }
    out
}

fn selector_text(section: &SectionRef) -> String {
    section
        .selector
        .clone()
        .unwrap_or_else(|| "(preamble)".to_string())
}

/// Cut `text` to at most `max` bytes on a character boundary.
fn cut(text: &str, max: usize) -> &str {
    let mut end = max.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Keep `text` within `budget` bytes, appending a disclosure when it was cut.
fn bounded(mut text: String, budget: usize, hint: &str) -> String {
    if text.len() <= budget {
        return text;
    }
    let note = format!("\n[output truncated at {budget} bytes. {hint}]");
    let keep = cut(&text, budget.saturating_sub(note.len())).len();
    text.truncate(keep);
    text.push_str(&note);
    text
}

fn reply(text: String) -> Vec<ToolResultPart> {
    vec![ToolResultPart::text(text)]
}

fn str_arg<'a>(args: &'a Value, name: &str) -> Result<Option<&'a str>, ToolError> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.as_str())),
        Some(_) => Err(ToolError::InvalidRequest(format!(
            "`{name}` must be a string"
        ))),
    }
}

fn limit_arg(args: &Value, name: &str, max: u64) -> Result<Option<usize>, ToolError> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => match value.as_u64() {
            Some(n) if (1..=max).contains(&n) => Ok(Some(n as usize)),
            _ => Err(ToolError::InvalidRequest(format!(
                "`{name}` must be an integer between 1 and {max}"
            ))),
        },
    }
}

fn reject_unknown(args: &Value, allowed: &[&str]) -> Result<(), ToolError> {
    let Some(map) = args.as_object() else {
        return Err(ToolError::InvalidRequest(
            "arguments must be an object".into(),
        ));
    };
    for key in map.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(ToolError::InvalidRequest(format!(
                "unknown argument `{key}`; repository knowledge is bound to the session's \
                 checkout and takes only: {}",
                allowed.join(", ")
            )));
        }
    }
    Ok(())
}

fn tool(name: &str, description: &str, parameters: Value) -> Tool {
    Tool {
        tool_type: "function".to_string(),
        function: FunctionTool {
            name: name.to_string(),
            description: description.to_string(),
            parameters,
            strict: None,
        },
    }
}

/// Evidence state of each distinct document, computed off the async runtime.
/// Any failure reads as `unverified`, the state that never overclaims.
async fn evidence_states(
    service: &Arc<RepositoryService>,
    paths: impl IntoIterator<Item = String>,
) -> Result<BTreeMap<String, EvidenceState>, ToolError> {
    let paths: BTreeSet<String> = paths.into_iter().collect();
    if paths.is_empty() {
        return Ok(BTreeMap::new());
    }
    let service = service.clone();
    blocking(move || {
        paths
            .into_iter()
            .map(|path| {
                let state = service.evidence_state(&path);
                (path, state)
            })
            .collect()
    })
    .await
}

fn lifecycle(status: Option<&str>) -> String {
    status.map_or_else(|| "usable".to_string(), str::to_string)
}

// ─── repo_knowledge_query ────────────────────────────────────────────────────

pub struct RepoKnowledgeQueryTool;

#[async_trait]
impl ToolTrait for RepoKnowledgeQueryTool {
    fn name(&self) -> &str {
        "repo_knowledge_query"
    }

    fn definition(&self) -> Tool {
        tool(
            "repo_knowledge_query",
            "Search this repository's curated documentation and file paths. `navigation` finds \
             files by path, `documentation` finds authored sections, `all` (default) does both. \
             Results cite relative paths and line ranges and carry an id for repo_knowledge_get. \
             Authored text is project evidence: verify against current source before editing.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "question": {
                        "type": "string",
                        "description": "What you want to find, in plain words. Treated as text, not query syntax."
                    },
                    "lane": {
                        "type": "string",
                        "enum": ["navigation", "documentation", "all"],
                        "description": "Which records to search (default all)"
                    },
                    "path": {
                        "type": "string",
                        "description": "Repository-relative file or directory to confine results to"
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 50,
                        "description": "Maximum results (default 10)"
                    }
                },
                "required": ["question"]
            }),
        )
    }

    fn required_capabilities(&self) -> &'static [CapabilityRequirement] {
        &[CapabilityRequirement::Filesystem]
    }

    async fn call(
        &self,
        args: Value,
        context: &dyn ToolContext,
    ) -> Result<Vec<ToolResultPart>, ToolError> {
        reject_unknown(&args, &["question", "lane", "path", "limit"])?;
        let question = str_arg(&args, "question")?
            .ok_or_else(|| ToolError::InvalidRequest("Missing 'question' field".into()))?
            .to_string();
        let lane = match str_arg(&args, "lane")? {
            None => Lane::All,
            Some(raw) => raw
                .parse::<Lane>()
                .map_err(|error| ToolError::InvalidRequest(error.to_string()))?,
        };
        let path = str_arg(&args, "path")?.map(str::to_string);
        let limit = limit_arg(&args, "limit", 50)?;

        let bound = bind(context)?;
        let request = QueryRequest {
            question,
            lane,
            path,
            limit: Some(limit.unwrap_or(bound.default_limit)),
        };
        let service = bound.service.clone();
        let response = blocking(move || service.query(&request))
            .await?
            .map_err(query_error)?;

        let evidence = evidence_states(
            &bound.service,
            response
                .hits
                .iter()
                .filter(|hit| matches!(hit.kind, HitKind::Section | HitKind::Purpose))
                .map(|hit| hit.path.clone()),
        )
        .await?;

        let mut out = String::new();
        let _ = writeln!(
            out,
            "Repository knowledge, {} lane, generation {}: {} result(s){}",
            response.lane.as_str(),
            response.generation,
            response.hits.len(),
            if response.truncated {
                format!(
                    " (more exist: showing {} of at least {})",
                    response.hits.len(),
                    response.total_matches
                )
            } else {
                String::new()
            }
        );
        if matches!(
            response.status,
            crate::knowledge::repository::ReconcileStatus::NotConfigured
        ) {
            out.push_str(
                "Not configured: this repository has no .qmt/knowledge.toml, so nothing is indexed.\n",
            );
        }
        if !response.coverage.is_empty() {
            let _ = writeln!(
                out,
                "Coverage is incomplete ({} issue(s)); results may omit selected files. \
                 See repo_knowledge_stats.",
                response.coverage.len()
            );
        }
        for (index, hit) in response.hits.iter().enumerate() {
            let _ = writeln!(out, "\n[{}] {}", index + 1, hit.title);
            let source = match hit.provenance {
                Provenance::Authored => "authored",
                Provenance::SourceDerived => "source-derived",
            };
            debug_assert_eq!(
                hit.provenance,
                match hit.kind {
                    HitKind::Section | HitKind::Purpose => Provenance::Authored,
                    HitKind::File | HitKind::Symbol => Provenance::SourceDerived,
                }
            );
            match (hit.start_line, hit.end_line) {
                (Some(start), Some(end)) => {
                    let _ = writeln!(out, "    {source} · {}:{start}-{end}", hit.path);
                }
                _ => {
                    let _ = writeln!(out, "    {source} · {}", hit.path);
                }
            }
            if let Some(selector) = &hit.selector {
                let label = match hit.kind {
                    HitKind::Section => "section",
                    HitKind::Symbol => "symbol",
                    HitKind::Purpose => "describes",
                    HitKind::File => "selector",
                };
                let _ = writeln!(out, "    {label}: {selector}");
            }
            let _ = writeln!(out, "    id: {}", hit.id);
            if matches!(hit.kind, HitKind::Section | HitKind::Purpose) {
                let evidence = evidence.get(&hit.path).copied().unwrap_or_default();
                let _ = writeln!(
                    out,
                    "    {}",
                    StateSignals::authored(hit.status.as_deref()).with_evidence(evidence)
                );
            }
            if !hit.excerpt.is_empty() {
                let _ = writeln!(out, "    {}", hit.excerpt);
            }
        }
        if response.hits.is_empty() {
            out.push_str(
                "\nNo matches. Try different words, search the other lane, or fall back to \
                 glob/search_text.\n",
            );
        } else {
            out.push_str(
                "\nFetch a section body, a symbol's record or a file's map with \
                 repo_knowledge_get {id}.\n",
            );
        }
        Ok(reply(bounded(
            out,
            bound.budget,
            "Narrow with `path`, a smaller `limit`, or a more specific question.",
        )))
    }
}

// ─── repo_knowledge_get ──────────────────────────────────────────────────────

pub struct RepoKnowledgeGetTool;

fn render_section(content: &SectionContent, evidence: EvidenceState, out: &mut String) {
    let _ = writeln!(
        out,
        "{} · {}:{}-{}",
        content.document_title, content.path, content.section.start_line, content.section.end_line
    );
    let _ = writeln!(out, "section: {}", selector_text(&content.section));
    let _ = writeln!(out, "id: {}", content.section.key);
    let _ = writeln!(
        out,
        "{}",
        StateSignals::authored(content.status.as_deref()).with_evidence(evidence)
    );
    let end = content.offset + content.content.len();
    let _ = writeln!(
        out,
        "bytes {}-{} of {}\n",
        content.offset, end, content.total_bytes
    );
    out.push_str(&content.content);
    if !content.content.ends_with('\n') {
        out.push('\n');
    }
    if let Some(cursor) = &content.next_cursor {
        let _ = writeln!(
            out,
            "\n[more content: call repo_knowledge_get with id `{}` and cursor `{cursor}`]",
            content.section.key
        );
    }
}

/// Declarations shown inline in a file record before pointing at the map.
const FILE_RECORD_DECLARATIONS: usize = 100;

fn render_file(file: &FileInfo, out: &mut String) {
    let _ = writeln!(
        out,
        "{} · source-derived file record · {} bytes · {}",
        file.path,
        file.size,
        file.language.as_deref().unwrap_or("unknown language")
    );
    match file.status {
        crate::knowledge::repository::CodeStatus::Extracted => {
            let _ = writeln!(
                out,
                "{} declaration(s) extracted from the source; syntax only, no inferred purpose.",
                file.symbols.len()
            );
        }
        crate::knowledge::repository::CodeStatus::Unsupported => {
            out.push_str("Path-level record only: no symbol extractor exists for this language.\n")
        }
        crate::knowledge::repository::CodeStatus::Failed => {
            out.push_str("Path-level record only: symbols could not be extracted from this file.\n")
        }
    }
    for doc in file.docs.iter().filter(|doc| doc.symbol_key.is_none()) {
        let _ = writeln!(
            out,
            "\nModule documentation (verbatim source comment, {}:{}-{}):\n{}",
            file.path, doc.start_line, doc.end_line, doc.text
        );
    }
    if !file.symbols.is_empty() {
        out.push_str("\nDeclarations (fetch one with repo_knowledge_get {id}):\n");
    }
    for symbol in file.symbols.iter().take(FILE_RECORD_DECLARATIONS) {
        let indent = "  ".repeat(symbol.depth.min(3) as usize);
        let _ = writeln!(
            out,
            "{indent}- {} {} · lines {}-{} · id: {}",
            symbol.kind.as_str(),
            symbol.qualified_name,
            symbol.start_line,
            symbol.end_line,
            symbol.key
        );
    }
    if file.symbols.len() > FILE_RECORD_DECLARATIONS {
        let _ = writeln!(
            out,
            "[{} more declaration(s); use repo_knowledge_list with view `map` and a narrower path]",
            file.symbols.len() - FILE_RECORD_DECLARATIONS
        );
    }
    out.push_str("\nThis is not file content. Read the current source with read_tool or index.\n");
}

fn render_symbol(info: &SymbolInfo, out: &mut String) {
    let symbol = &info.symbol;
    let _ = writeln!(
        out,
        "{} {} · {}:{}-{}",
        symbol.kind.as_str(),
        symbol.qualified_name,
        info.path,
        symbol.start_line,
        symbol.end_line
    );
    let _ = writeln!(out, "source-derived · id: {}", symbol.key);
    let _ = writeln!(out, "signature: {}", symbol.signature);
    if let Some(parent) = &symbol.parent {
        let _ = writeln!(out, "parent: {parent}");
    }
    let _ = writeln!(
        out,
        "digest: {} ({} bytes, {} lines)",
        symbol.digest, symbol.byte_len, symbol.line_count
    );
    if let Some(doc) = &info.doc {
        let _ = writeln!(
            out,
            "\nSource documentation (verbatim comment, {}:{}-{}):\n{}",
            info.path, doc.start_line, doc.end_line, doc.text
        );
    }
    if !info.members.is_empty() {
        out.push_str("\nMembers:\n");
        for member in &info.members {
            let _ = writeln!(
                out,
                "- {} {} · lines {}-{} · id: {}",
                member.kind.as_str(),
                member.qualified_name,
                member.start_line,
                member.end_line,
                member.key
            );
        }
    }
    out.push_str(
        "\nThis is a navigation record from an index, not the declaration's text. Read the \
         current source before editing.\n",
    );
}

fn render_purpose(info: &PurposeInfo, evidence: EvidenceState, out: &mut String) {
    let row = &info.row;
    let _ = writeln!(
        out,
        "{} · described in {}:{}",
        row.target_path, row.doc_path, row.line
    );
    let _ = writeln!(out, "authored · id: {}", row.key());
    let _ = writeln!(
        out,
        "{}",
        StateSignals::authored(info.doc_status.as_deref()).with_evidence(evidence)
    );
    if row.purpose.is_empty() {
        out.push_str("purpose: (the author left this empty)\n");
    } else {
        let _ = writeln!(out, "purpose: {}", row.purpose);
    }
    if row.managed {
        out.push_str("row of a managed inventory block\n");
    }
}

fn render_catalog(catalog: &DocumentCatalog, evidence: EvidenceState, out: &mut String) {
    let _ = writeln!(out, "{} · {}", catalog.title, catalog.path);
    let _ = writeln!(out, "id: {}", catalog.document_id);
    let _ = writeln!(
        out,
        "{}",
        StateSignals::authored(catalog.status.as_deref()).with_evidence(evidence)
    );
    if let Some(description) = &catalog.description {
        let _ = writeln!(out, "description: {description}");
    }
    let _ = writeln!(
        out,
        "\nThis document has {} sections; this is its table of contents, not its content.",
        catalog.sections.len()
    );
    if let Some(preamble) = &catalog.preamble {
        let _ = writeln!(
            out,
            "\nPreamble{}:\n{preamble}",
            if catalog.preamble_truncated {
                " (truncated)"
            } else {
                ""
            }
        );
    }
    out.push_str("\nSections (fetch one with repo_knowledge_get {id}):\n");
    for section in &catalog.sections {
        let _ = writeln!(
            out,
            "- {} · lines {}-{} · {} bytes · id: {}",
            selector_text(section),
            section.start_line,
            section.end_line,
            section.bytes,
            section.key
        );
    }
}

#[async_trait]
impl ToolTrait for RepoKnowledgeGetTool {
    fn name(&self) -> &str {
        "repo_knowledge_get"
    }

    fn definition(&self) -> Tool {
        tool(
            "repo_knowledge_get",
            "Fetch current repository knowledge by id (from repo_knowledge_query/list) or by \
             repository-relative path with an optional heading `section`. A section returns its \
             body, paged with a cursor. A multi-section document returns its table of contents \
             and preamble. A renamed or removed section fails as stale instead of returning a \
             different one.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "id": {
                        "type": "string",
                        "description": "Section or document id returned by repo_knowledge_query/list"
                    },
                    "path": {
                        "type": "string",
                        "description": "Repository-relative document path (alternative to id)"
                    },
                    "section": {
                        "type": "string",
                        "description": "Heading selector such as `Architecture > Storage`; add `~1` for the second repeat. Requires path."
                    },
                    "cursor": {
                        "type": "string",
                        "description": "Continuation cursor from a previous response"
                    }
                },
                "required": []
            }),
        )
    }

    fn required_capabilities(&self) -> &'static [CapabilityRequirement] {
        &[CapabilityRequirement::Filesystem]
    }

    async fn call(
        &self,
        args: Value,
        context: &dyn ToolContext,
    ) -> Result<Vec<ToolResultPart>, ToolError> {
        reject_unknown(&args, &["id", "path", "section", "cursor"])?;
        let bound = bind(context)?;
        let request = GetRequest {
            id: str_arg(&args, "id")?.map(str::to_string),
            path: str_arg(&args, "path")?.map(str::to_string),
            section: str_arg(&args, "section")?.map(str::to_string),
            cursor: str_arg(&args, "cursor")?.map(str::to_string),
            max_bytes: bound.budget.saturating_sub(FRAME_BYTES).max(256),
        };
        let service = bound.service.clone();
        let fetched = blocking(move || service.get(&request))
            .await?
            .map_err(get_error)?;
        // Local observation only; independent of whether a card was delivered.
        if let Some(registry) = context.repository_knowledge() {
            registry.diagnostics().record_fetch();
        }

        let document = match &fetched {
            Fetched::Section(content) => Some(content.path.clone()),
            Fetched::Catalog(catalog) => Some(catalog.path.clone()),
            Fetched::Purpose(info) => Some(info.row.doc_path.clone()),
            Fetched::File(_) | Fetched::Symbol(_) => None,
        };
        let evidence = evidence_states(&bound.service, document.clone())
            .await?
            .into_values()
            .next()
            .unwrap_or_default();

        let mut out = String::new();
        match &fetched {
            Fetched::Section(content) => render_section(content, evidence, &mut out),
            Fetched::Catalog(catalog) => render_catalog(catalog, evidence, &mut out),
            Fetched::File(file) => render_file(file, &mut out),
            Fetched::Symbol(info) => render_symbol(info, &mut out),
            Fetched::Purpose(info) => render_purpose(info, evidence, &mut out),
        }
        Ok(reply(bounded(
            out,
            bound.budget,
            "Fetch a narrower section.",
        )))
    }
}

// ─── repo_knowledge_list ─────────────────────────────────────────────────────

fn render_directory(catalog: &DirectoryCatalog) -> String {
    let mut out = String::new();
    let name = if catalog.directory.is_empty() {
        "(repository root)"
    } else {
        catalog.directory.as_str()
    };
    let _ = writeln!(
        out,
        "Directory catalog for {name}, generation {}: entries {}-{} of {}",
        catalog.generation,
        if catalog.entries.is_empty() {
            0
        } else {
            catalog.offset + 1
        },
        catalog.offset + catalog.entries.len(),
        catalog.total
    );
    match &catalog.origin {
        CatalogOrigin::Authored { path, id } => {
            let _ = writeln!(
                out,
                "origin: authored listing {path} (fetch it with id: {id}); entries follow its link order"
            );
        }
        CatalogOrigin::Derived => out.push_str(
            "origin: derived from the titles and descriptions of selected documents; \
             no listing was authored or written\n",
        ),
    }
    if catalog.generation_changed {
        out.push_str(
            "The index changed since that cursor was issued, so this catalog restarts from \
             the beginning.\n",
        );
    }
    if let Some(path) = &catalog.ignored_listing {
        let _ = writeln!(
            out,
            "note: {path} links to no selected file, so it was not used as the listing"
        );
    }
    for entry in &catalog.entries {
        let label = match entry.kind {
            CatalogKind::Directory => format!(
                "{} · directory · {} document(s)",
                entry.path, entry.documents
            ),
            CatalogKind::Document => format!(
                "{} · {} · lifecycle: {}",
                entry.path,
                entry.title,
                lifecycle(entry.status.as_deref())
            ),
            CatalogKind::File => format!("{} · source file", entry.path),
        };
        let _ = write!(out, "- {label}");
        if let Some(id) = &entry.id {
            let _ = write!(out, " · id: {id}");
        }
        out.push('\n');
        if let Some(description) = &entry.description {
            let _ = writeln!(out, "    {description}");
        }
        if let Some(citation) = &entry.citation {
            let _ = writeln!(out, "    cited at {}:{}", citation.path, citation.line);
        }
    }
    if catalog.omitted_links > 0 {
        let _ = writeln!(
            out,
            "[{} listing link(s) did not resolve to selected sources and are not shown]",
            catalog.omitted_links
        );
    }
    if catalog.scan_truncated {
        out.push_str("[the listing has more links than were read]\n");
    }
    if catalog.code_files > 0 {
        let _ = writeln!(
            out,
            "[{} selected source file(s) below this directory; browse with view: flat and a path]",
            catalog.code_files
        );
    }
    if let Some(cursor) = &catalog.next_cursor {
        let _ = writeln!(
            out,
            "\n[more: call repo_knowledge_list with view `directory`, the same path, and cursor `{cursor}`]"
        );
    }
    out
}

pub struct RepoKnowledgeListTool;

#[async_trait]
impl ToolTrait for RepoKnowledgeListTool {
    fn name(&self) -> &str {
        "repo_knowledge_list"
    }

    fn definition(&self) -> Tool {
        tool(
            "repo_knowledge_list",
            "Browse the repository knowledge catalog: authored documents with titles and \
             descriptions, then selected source files, sorted by path and paginated. Confine \
             with `path` to descend into a directory. `view: directory` instead shows one \
             directory's children: its authored index.md/README.md listing when it links to \
             selected files, otherwise a listing derived from selected documents. `view: map` \
             renders source-derived declarations (kind, name, signature, line range) of selected \
             code files under `path`, quoting authored File/Purpose rows with citations.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Repository-relative directory or file to list"
                    },
                    "view": {
                        "type": "string",
                        "enum": ["flat", "directory", "map"],
                        "description": "`flat` (default) lists the whole corpus; `directory` \
                                        browses one directory (`path`, default the root); `map` \
                                        renders a bounded source map of declarations under `path`"
                    },
                    "cursor": {
                        "type": "string",
                        "description": "Continuation cursor from a previous page"
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 50,
                        "description": "Entries per page (default 20)"
                    }
                },
                "required": []
            }),
        )
    }

    fn required_capabilities(&self) -> &'static [CapabilityRequirement] {
        &[CapabilityRequirement::Filesystem]
    }

    async fn call(
        &self,
        args: Value,
        context: &dyn ToolContext,
    ) -> Result<Vec<ToolResultPart>, ToolError> {
        reject_unknown(&args, &["path", "cursor", "limit", "view"])?;
        let (directory_view, map_view) = match str_arg(&args, "view")? {
            None | Some("flat") => (false, false),
            Some("directory") => (true, false),
            Some("map") => (false, true),
            Some(other) => {
                return Err(ToolError::InvalidRequest(format!(
                    "`view` must be `flat`, `directory` or `map`, not `{other}`"
                )));
            }
        };
        if map_view {
            if args.get("cursor").is_some_and(|value| !value.is_null()) {
                return Err(ToolError::InvalidRequest(
                    "a map has no continuation; narrow it with `path`".into(),
                ));
            }
            let request = MapRequest {
                path: str_arg(&args, "path")?.map(str::to_string),
                max_bytes: None,
            };
            let bound = bind(context)?;
            let request = MapRequest {
                max_bytes: Some(bound.budget),
                ..request
            };
            let service = bound.service.clone();
            let output = blocking(move || service.map(&request))
                .await?
                .map_err(map_error)?;
            let mut out = output.map.text;
            if !output.coverage.is_empty() {
                let _ = write!(
                    out,
                    "\n> Coverage is incomplete ({} issue(s)); some selected files may be missing. \
                     See repo_knowledge_stats.\n",
                    output.coverage.len()
                );
            }
            return Ok(reply(bounded(out, bound.budget, "Use a narrower `path`.")));
        }
        if directory_view {
            let request = CatalogRequest {
                path: str_arg(&args, "path")?.map(str::to_string),
                cursor: str_arg(&args, "cursor")?.map(str::to_string),
                limit: limit_arg(&args, "limit", 50)?,
            };
            let bound = bind(context)?;
            let service = bound.service.clone();
            let catalog = blocking(move || service.catalog(&request))
                .await?
                .map_err(get_error)?;
            return Ok(reply(bounded(
                render_directory(&catalog),
                bound.budget,
                "Use a smaller `limit` or a narrower `path`.",
            )));
        }
        let request = ListRequest {
            path: str_arg(&args, "path")?.map(str::to_string),
            cursor: str_arg(&args, "cursor")?.map(str::to_string),
            limit: limit_arg(&args, "limit", 50)?,
        };
        let bound = bind(context)?;
        let service = bound.service.clone();
        let page = blocking(move || service.list(&request))
            .await?
            .map_err(get_error)?;

        let mut out = String::new();
        let _ = writeln!(
            out,
            "Repository knowledge catalog, generation {}: entries {}-{} of {}",
            page.generation,
            if page.entries.is_empty() {
                0
            } else {
                page.offset + 1
            },
            page.offset + page.entries.len(),
            page.total
        );
        if page.generation_changed {
            out.push_str(
                "The index changed since that cursor was issued, so this listing restarts from \
                 the beginning instead of mixing two corpora.\n",
            );
        }
        for entry in &page.entries {
            match entry.kind {
                EntryKind::Document => {
                    let _ = writeln!(
                        out,
                        "- {} · {} · {} section(s) · lifecycle: {} · id: {}",
                        entry.path,
                        entry.title,
                        entry.sections,
                        lifecycle(entry.status.as_deref()),
                        entry.id
                    );
                    if let Some(description) = &entry.description {
                        let _ = writeln!(out, "    {description}");
                    }
                }
                EntryKind::File => {
                    let _ = writeln!(
                        out,
                        "- {} · source file · {} bytes · id: {}",
                        entry.path,
                        entry.size.unwrap_or_default(),
                        entry.id
                    );
                }
            }
        }
        if let Some(cursor) = &page.next_cursor {
            let _ = writeln!(
                out,
                "\n[more: call repo_knowledge_list with cursor `{cursor}`]"
            );
        }
        Ok(reply(bounded(
            out,
            bound.budget,
            "Use a smaller `limit` or a narrower `path`.",
        )))
    }
}

// ─── repo_knowledge_stats ────────────────────────────────────────────────────

pub struct RepoKnowledgeStatsTool;

#[async_trait]
impl ToolTrait for RepoKnowledgeStatsTool {
    fn name(&self) -> &str {
        "repo_knowledge_stats"
    }

    fn definition(&self) -> Tool {
        tool(
            "repo_knowledge_stats",
            "Describe the repository knowledge corpus: indexed documents, sections and source \
             files, index format and generation, and whether coverage is complete.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {},
                "required": []
            }),
        )
    }

    fn required_capabilities(&self) -> &'static [CapabilityRequirement] {
        &[CapabilityRequirement::Filesystem]
    }

    async fn call(
        &self,
        args: Value,
        context: &dyn ToolContext,
    ) -> Result<Vec<ToolResultPart>, ToolError> {
        reject_unknown(&args, &[])?;
        let bound = bind(context)?;
        let service = bound.service.clone();
        let stats = blocking(move || service.stats())
            .await?
            .map_err(|error| unavailable(&error, error.code()))?;

        let mut out = String::new();
        let _ = writeln!(out, "Repository knowledge statistics");
        let _ = writeln!(out, "index format: {}", stats.index_format);
        let _ = writeln!(out, "generation: {}", stats.generation);
        let _ = writeln!(out, "selected sources: {}", stats.sources);
        let _ = writeln!(out, "indexed documents: {}", stats.documents);
        let _ = writeln!(out, "indexed sections: {}", stats.sections);
        let _ = writeln!(out, "source files (path-level): {}", stats.code_files);
        let _ = writeln!(out, "declarations extracted: {}", stats.symbols);
        let _ = writeln!(out, "file/purpose rows: {}", stats.purposes);
        let _ = writeln!(
            out,
            "source files without symbols (unsupported or failed): {}",
            stats.partial_code_files
        );
        let _ = writeln!(out, "excluded documents: {}", stats.excluded_sources);
        let _ = writeln!(
            out,
            "coverage: {}",
            if stats.coverage_complete {
                "complete"
            } else {
                "INCOMPLETE; some selected files were skipped"
            }
        );
        if matches!(
            stats.status,
            crate::knowledge::repository::ReconcileStatus::NotConfigured
        ) {
            out.push_str("not configured: no .qmt/knowledge.toml in this repository\n");
        }
        if !stats.diagnostics.is_empty() {
            out.push_str("diagnostics by code:\n");
            for (code, count) in &stats.diagnostics {
                let _ = writeln!(out, "- {code}: {count}");
            }
        }
        for issue in &stats.coverage {
            let path = issue.path.as_deref().unwrap_or("-");
            let _ = writeln!(
                out,
                "coverage issue [{}] {path}: {}",
                issue.code, issue.message
            );
        }
        Ok(reply(bounded(
            out,
            bound.budget,
            "Repository knowledge diagnostics were shortened.",
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RepositoryKnowledgeSettings;
    use crate::knowledge::repository::RepositoryKnowledgeRegistry;
    use std::any::Any;
    use std::fs;
    use std::path::{Path, PathBuf};

    struct Ctx {
        cwd: Option<PathBuf>,
        registry: Option<Arc<RepositoryKnowledgeRegistry>>,
        legacy: Option<Arc<dyn crate::knowledge::KnowledgeStore>>,
    }

    #[async_trait]
    impl ToolContext for Ctx {
        fn session_id(&self) -> &str {
            "s"
        }
        fn cwd(&self) -> Option<&Path> {
            self.cwd.as_deref()
        }
        fn repository_knowledge(&self) -> Option<Arc<RepositoryKnowledgeRegistry>> {
            self.registry.clone()
        }
        fn knowledge_store(&self) -> Option<Arc<dyn crate::knowledge::KnowledgeStore>> {
            self.legacy.clone()
        }
        async fn record_progress(
            &self,
            _kind: &str,
            _content: String,
            _metadata: Option<Value>,
        ) -> Result<String, ToolError> {
            Ok(String::new())
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    struct Fx {
        tmp: tempfile::TempDir,
        repo: PathBuf,
        ctx: Ctx,
    }

    const MANIFEST: &str = "version = 1\n[documents]\ninclude = [\"docs/**/*.md\"]\n[code]\ninclude = [\"src/**/*.rs\"]\n";
    const ARCH: &str = "# Architecture\n## Scheduling\nThe scheduler dispatches queued tasks.\n## Storage\nRows live in SQLite.\n";

    impl Fx {
        fn with(settings: RepositoryKnowledgeSettings) -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let repo = tmp.path().join("repo");
            fs::create_dir_all(repo.join(".git")).unwrap();
            let registry =
                RepositoryKnowledgeRegistry::from_settings_in(&settings, tmp.path().join("cfg"));
            let ctx = Ctx {
                cwd: Some(repo.clone()),
                registry,
                legacy: None,
            };
            let fx = Self { tmp, repo, ctx };
            fx.put(".qmt/knowledge.toml", MANIFEST);
            fx
        }

        fn new() -> Self {
            Self::with(RepositoryKnowledgeSettings::enabled())
        }

        fn put(&self, rel: &str, content: &str) {
            let path = self.repo.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
        }

        async fn run(&self, tool: &dyn ToolTrait, args: Value) -> Result<String, ToolError> {
            let parts = tool.call(args, &self.ctx).await?;
            Ok(parts
                .into_iter()
                .map(|part| match part {
                    ToolResultPart::Text { text } => text,
                    _ => String::new(),
                })
                .collect())
        }
    }

    fn id_of(output: &str) -> String {
        output
            .lines()
            .find_map(|line| line.trim().strip_prefix("id: "))
            .expect("an id line")
            .to_string()
    }

    #[tokio::test]
    async fn query_then_get_round_trips_through_the_tools() {
        let fx = Fx::new();
        fx.put("docs/arch.md", ARCH);
        let found = fx
            .run(
                &RepoKnowledgeQueryTool,
                json!({"question": "how does the scheduler dispatch tasks?"}),
            )
            .await
            .unwrap();
        assert!(found.contains("docs/arch.md:2-3"), "{found}");
        assert!(found.contains("authored"));
        assert!(found.contains("index: current · evidence: unverified · lifecycle: usable"));
        assert!(
            !found.contains(fx.tmp.path().to_str().unwrap()),
            "no absolute paths"
        );

        let body = fx
            .run(&RepoKnowledgeGetTool, json!({"id": id_of(&found)}))
            .await
            .unwrap();
        assert!(
            body.contains("The scheduler dispatches queued tasks."),
            "{body}"
        );
        assert!(!body.contains("SQLite"));
    }

    #[tokio::test]
    async fn successful_get_is_counted_as_a_fetch_and_nothing_else() {
        use crate::knowledge::repository::DeliveryStage;
        let fx = Fx::new();
        fx.put("docs/arch.md", ARCH);
        let registry = fx.ctx.registry.clone().unwrap();
        let found = fx
            .run(
                &RepoKnowledgeQueryTool,
                json!({"question": "how does the scheduler dispatch tasks?"}),
            )
            .await
            .unwrap();
        // Querying is not a fetch.
        assert_eq!(registry.diagnostics().snapshot().fetched(), 0);
        fx.run(&RepoKnowledgeGetTool, json!({"id": id_of(&found)}))
            .await
            .unwrap();
        // A failed get is not counted.
        assert!(
            fx.run(&RepoKnowledgeGetTool, json!({"id": "no-such-id"}))
                .await
                .is_err()
        );
        let snap = registry.diagnostics().snapshot();
        assert_eq!(snap.fetched(), 1);
        assert_eq!(snap.delivered(DeliveryStage::Orientation), 0);
        assert_eq!(snap.delivered(DeliveryStage::IndexContext), 0);
    }

    #[tokio::test]
    async fn get_by_path_lists_sections_instead_of_one_arbitrary_section() {
        let fx = Fx::new();
        fx.put("docs/arch.md", ARCH);
        let catalog = fx
            .run(&RepoKnowledgeGetTool, json!({"path": "docs/arch.md"}))
            .await
            .unwrap();
        assert!(catalog.contains("table of contents, not its content"));
        assert!(catalog.contains("Architecture > Storage"));
        assert!(!catalog.contains("Rows live in SQLite"));
        let section = fx
            .run(
                &RepoKnowledgeGetTool,
                json!({"path": "docs/arch.md", "section": "Architecture > Storage"}),
            )
            .await
            .unwrap();
        assert!(section.contains("Rows live in SQLite"));
    }

    #[tokio::test]
    async fn stale_selectors_list_the_sections_that_exist_now() {
        let fx = Fx::new();
        fx.put("docs/arch.md", ARCH);
        let error = fx
            .run(
                &RepoKnowledgeGetTool,
                json!({"path": "docs/arch.md", "section": "Architecture > Gone"}),
            )
            .await
            .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("selector_stale"), "{message}");
        assert!(message.contains("Architecture > Scheduling"));
    }

    #[tokio::test]
    async fn list_and_stats_describe_the_corpus() {
        let fx = Fx::new();
        fx.put("docs/a.md", "---\ndescription: about a\n---\n# A\nx\n");
        fx.put("src/lib.rs", "");
        let listing = fx.run(&RepoKnowledgeListTool, json!({})).await.unwrap();
        assert!(listing.contains("docs/a.md"));
        assert!(listing.contains("about a"));
        assert!(listing.contains("src/lib.rs · source file"));
        let paged = fx
            .run(&RepoKnowledgeListTool, json!({"limit": 1}))
            .await
            .unwrap();
        assert!(paged.contains("[more: call repo_knowledge_list with cursor"));
        let stats = fx.run(&RepoKnowledgeStatsTool, json!({})).await.unwrap();
        assert!(stats.contains("indexed documents: 1"));
        assert!(stats.contains("source files (path-level): 1"));
        assert!(stats.contains("coverage: complete"));
        assert!(!stats.to_lowercase().contains("session"));
    }

    #[tokio::test]
    async fn directory_view_labels_authored_and_derived_catalogs_and_the_default_is_unchanged() {
        let fx = Fx::new();
        fx.put(
            "docs/index.md",
            "# Docs\n\n- [a](a.md)\n- [gone](missing.md)\n",
        );
        fx.put("docs/a.md", "---\ndescription: about a\n---\n# A\nx\n");
        fx.put("docs/sub/b.md", "# B\nx\n");

        let authored = fx
            .run(
                &RepoKnowledgeListTool,
                json!({"view": "directory", "path": "docs"}),
            )
            .await
            .unwrap();
        assert!(
            authored.contains("origin: authored listing docs/index.md"),
            "{authored}"
        );
        assert!(authored.contains("cited at docs/index.md:3"));
        assert!(authored.contains("about a"));
        assert!(authored.contains("1 listing link(s) did not resolve"));

        let derived = fx
            .run(
                &RepoKnowledgeListTool,
                json!({"view": "directory", "path": "docs/sub"}),
            )
            .await
            .unwrap();
        assert!(derived.contains("origin: derived"), "{derived}");
        assert!(derived.contains("cited at docs/sub/b.md:1"));

        let flat = fx.run(&RepoKnowledgeListTool, json!({})).await.unwrap();
        assert!(flat.starts_with("Repository knowledge catalog"));
        let error = fx
            .run(&RepoKnowledgeListTool, json!({"view": "tree"}))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("`view` must be"));
    }

    #[tokio::test]
    async fn symbols_maps_and_purpose_rows_are_retrievable_and_labelled() {
        let fx = Fx::new();
        fx.put(
            "src/queue.rs",
            "//! Queue module.\n\n/// A queue of tasks.\npub struct Queue { items: Vec<u32> }\nimpl Queue {\n    pub fn push(&mut self, item: u32) {}\n}\n",
        );
        fx.put(
            "docs/map.md",
            "# Map\n\n| File | Purpose |\n|--|--|\n| `../src/queue.rs` | Holds pending work. |\n",
        );

        let found = fx
            .run(&RepoKnowledgeQueryTool, json!({"question": "Queue::push"}))
            .await
            .unwrap();
        assert!(
            found.contains("source-derived · src/queue.rs:6-6"),
            "{found}"
        );
        assert!(found.contains("symbol: Queue::push"));
        let symbol = fx
            .run(&RepoKnowledgeGetTool, json!({"id": id_of(&found)}))
            .await
            .unwrap();
        assert!(
            symbol.contains("signature: ") && symbol.contains("fn push"),
            "{symbol}"
        );
        assert!(symbol.contains("navigation record from an index"));

        let queue = fx
            .run(
                &RepoKnowledgeGetTool,
                json!({"id": "symbol:src/queue.rs#struct:Queue"}),
            )
            .await
            .unwrap();
        assert!(queue.contains("A queue of tasks."), "{queue}");
        assert!(queue.contains("Members:") && queue.contains("Queue::items"));

        let file = fx
            .run(&RepoKnowledgeGetTool, json!({"path": "src/queue.rs"}))
            .await
            .unwrap();
        assert!(file.contains("Module documentation") && file.contains("Queue module."));
        assert!(
            file.contains("id: symbol:src/queue.rs#struct:Queue")
                || file.contains("· id: symbol:src/queue.rs#struct:Queue")
        );

        let purpose = fx
            .run(
                &RepoKnowledgeGetTool,
                json!({"id": "purpose:docs/map.md#src/queue.rs"}),
            )
            .await
            .unwrap();
        assert!(
            purpose.contains("purpose: Holds pending work."),
            "{purpose}"
        );
        assert!(purpose.contains("described in docs/map.md:5"));

        let map = fx
            .run(&RepoKnowledgeListTool, json!({"view": "map"}))
            .await
            .unwrap();
        assert!(map.contains("struct `Queue` (4-4)"), "{map}");
        assert!(map.contains("purpose (authored): Holds pending work. [docs/map.md:5]"));
        assert!(!map.contains(fx.tmp.path().to_str().unwrap()));
        let scoped = fx
            .run(
                &RepoKnowledgeListTool,
                json!({"view": "map", "path": "docs"}),
            )
            .await
            .unwrap();
        assert!(scoped.contains("No selected code files"));
        assert!(
            fx.run(
                &RepoKnowledgeListTool,
                json!({"view": "map", "path": "../x"})
            )
            .await
            .is_err()
        );

        // A renamed declaration's id is not found; it never resolves to another symbol.
        fx.put("src/queue.rs", "pub struct Renamed {}\n");
        let gone = fx
            .run(
                &RepoKnowledgeGetTool,
                json!({"id": "symbol:src/queue.rs#struct:Queue"}),
            )
            .await
            .unwrap_err();
        assert!(gone.to_string().contains("not_found"), "{gone}");

        let stats = fx.run(&RepoKnowledgeStatsTool, json!({})).await.unwrap();
        assert!(stats.contains("declarations extracted: 1"), "{stats}");
    }

    #[tokio::test]
    async fn arguments_cannot_select_another_checkout() {
        let fx = Fx::new();
        for (tool, args) in [
            (
                &RepoKnowledgeQueryTool as &dyn ToolTrait,
                json!({"question": "x", "scope": "global"}),
            ),
            (
                &RepoKnowledgeQueryTool,
                json!({"question": "x", "root": "/"}),
            ),
            (
                &RepoKnowledgeGetTool,
                json!({"path": "a.md", "checkout": "/other"}),
            ),
            (&RepoKnowledgeListTool, json!({"scope": "x"})),
            (&RepoKnowledgeStatsTool, json!({"root": "/"})),
        ] {
            let error = fx.run(tool, args).await.unwrap_err();
            assert!(matches!(error, ToolError::InvalidRequest(_)), "{error}");
        }
        for definition in repository_knowledge_tools().iter().map(|t| t.definition()) {
            let props = definition.function.parameters["properties"]
                .as_object()
                .unwrap();
            for forbidden in ["scope", "root", "checkout", "cwd", "workspace"] {
                assert!(
                    !props.contains_key(forbidden),
                    "{}",
                    definition.function.name
                );
            }
            assert_eq!(
                definition.function.parameters["additionalProperties"],
                json!(false)
            );
        }
    }

    #[tokio::test]
    async fn unavailable_states_give_actionable_diagnostics() {
        let disabled = Ctx {
            cwd: Some(PathBuf::from("/tmp")),
            registry: None,
            legacy: None,
        };
        let error = RepoKnowledgeQueryTool
            .call(json!({"question": "x"}), &disabled)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("not enabled"), "{error}");

        let fx = Fx::new();
        let no_cwd = Ctx {
            cwd: None,
            registry: fx.ctx.registry.clone(),
            legacy: None,
        };
        let error = RepoKnowledgeStatsTool
            .call(json!({}), &no_cwd)
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("root_no_working_directory"),
            "{error}"
        );
        assert!(error.to_string().contains("normal source tools"));

        let outside = tempfile::tempdir().unwrap();
        fs::create_dir_all(outside.path().join(".git")).unwrap();
        let mut settings = RepositoryKnowledgeSettings::enabled();
        settings.allowed_roots = vec![fx.repo.clone()];
        let registry =
            RepositoryKnowledgeRegistry::from_settings_in(&settings, fx.tmp.path().join("cfg2"));
        let denied = Ctx {
            cwd: Some(outside.path().to_path_buf()),
            registry,
            legacy: None,
        };
        let error = RepoKnowledgeListTool
            .call(json!({}), &denied)
            .await
            .unwrap_err();
        assert!(matches!(error, ToolError::PermissionDenied(_)), "{error}");
    }

    #[tokio::test]
    async fn invalid_requests_are_rejected_clearly() {
        let fx = Fx::new();
        let empty = fx
            .run(&RepoKnowledgeQueryTool, json!({"question": "  "}))
            .await
            .unwrap_err();
        assert!(empty.to_string().contains("question_empty"), "{empty}");
        assert!(fx.run(&RepoKnowledgeQueryTool, json!({})).await.is_err());
        assert!(
            fx.run(
                &RepoKnowledgeQueryTool,
                json!({"question": "x", "lane": "weird"})
            )
            .await
            .is_err()
        );
        assert!(
            fx.run(
                &RepoKnowledgeQueryTool,
                json!({"question": "x", "limit": 51})
            )
            .await
            .is_err()
        );
        assert!(
            fx.run(
                &RepoKnowledgeQueryTool,
                json!({"question": "x", "limit": 0})
            )
            .await
            .is_err()
        );
        assert!(
            fx.run(
                &RepoKnowledgeQueryTool,
                json!({"question": "x", "path": "../up"})
            )
            .await
            .is_err()
        );
        assert!(fx.run(&RepoKnowledgeGetTool, json!({})).await.is_err());
        assert!(
            fx.run(&RepoKnowledgeGetTool, json!({"path": "../etc/passwd"}))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn output_obeys_the_configured_budget_and_discloses_truncation() {
        let mut settings = RepositoryKnowledgeSettings::enabled();
        settings.max_response_bytes = 700;
        let fx = Fx::with(settings);
        for n in 0..30 {
            fx.put(
                &format!("docs/d{n:02}.md"),
                &format!("# Doc {n:02} é\n{}\n", "widget ".repeat(40)),
            );
        }
        let out = fx
            .run(
                &RepoKnowledgeQueryTool,
                json!({"question": "widget", "limit": 50}),
            )
            .await
            .unwrap();
        assert!(out.len() <= 700, "{} bytes", out.len());
        assert!(out.contains("[output truncated at 700 bytes"));

        fx.put("docs/big.md", &format!("# Big\n{}\n", "é".repeat(2000)));
        let page = fx
            .run(&RepoKnowledgeGetTool, json!({"path": "docs/big.md"}))
            .await
            .unwrap();
        assert!(page.len() <= 700, "{} bytes", page.len());
        assert!(page.contains("[more content: call repo_knowledge_get"));
    }

    #[tokio::test]
    async fn defaults_are_ten_results_and_sixteen_kib_and_the_ceilings_hold() {
        let fx = Fx::new();
        for n in 0..60 {
            fx.put(
                &format!("docs/d{n:02}.md"),
                &format!("# Doc {n:02}\n{}\n", "widget é ".repeat(300)),
            );
        }
        let out = fx
            .run(&RepoKnowledgeQueryTool, json!({"question": "widget"}))
            .await
            .unwrap();
        assert!(out.contains("10 result(s)"), "{}", &out[..200]);
        assert!(out.len() <= 16 * 1024, "{} bytes", out.len());
        assert!(out.contains("more exist"));

        // The per-call limit is capped at 50 and rejected beyond it.
        let error = fx
            .run(
                &RepoKnowledgeQueryTool,
                json!({"question": "widget", "limit": 51}),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("between 1 and 50"));

        // A host asking for more than 64 KiB is held at 64 KiB, on a char boundary.
        let mut settings = RepositoryKnowledgeSettings::enabled();
        settings.max_response_bytes = usize::MAX;
        let big = Fx::with(settings);
        big.put("docs/big.md", &format!("# Big\n{}\n", "é".repeat(60_000)));
        let page = big
            .run(&RepoKnowledgeGetTool, json!({"path": "docs/big.md"}))
            .await
            .unwrap();
        assert!(page.len() <= 64 * 1024, "{} bytes", page.len());
        assert!(page.contains("[more content"));
    }

    #[tokio::test]
    async fn state_signals_are_separate_fields_and_lifecycle_never_clears_evidence() {
        let fx = Fx::new();
        fx.put(
            "docs/old.md",
            "---\nstatus: deprecated\n---\n# Old\nlegacy gadget notes\n",
        );
        fx.put(
            "docs/trusted.md",
            "---\nstatus: verified\n---\n# Trusted\nlegacy gadget notes\n",
        );
        let found = fx
            .run(
                &RepoKnowledgeQueryTool,
                json!({"question": "legacy gadget"}),
            )
            .await
            .unwrap();
        assert!(
            found.contains("index: current · evidence: unverified · lifecycle: deprecated"),
            "{found}"
        );
        // A reassuring label does not turn unverified evidence into anything else.
        assert!(
            found.contains("index: current · evidence: unverified · lifecycle: verified"),
            "{found}"
        );
        let got = fx
            .run(&RepoKnowledgeGetTool, json!({"path": "docs/old.md"}))
            .await
            .unwrap();
        assert!(got.contains("evidence: unverified · lifecycle: deprecated"));
        // Deprecated documents stay explicitly retrievable.
        assert!(got.contains("legacy gadget notes"));
    }

    #[tokio::test]
    async fn retrieval_reports_the_real_evidence_state_of_acknowledged_notes() {
        use crate::knowledge::repository::{AcknowledgeRequest, AcknowledgeSelection};

        let fx = Fx::new();
        fx.put("src/lib.rs", "pub fn run() {}\n");
        fx.put(
            "docs/trusted.md",
            "---\nstatus: draft\nknowledge_sources: src/lib.rs\n---\n# Trusted\nlegacy gadget notes\n",
        );
        let service = fx
            .ctx
            .registry
            .as_ref()
            .unwrap()
            .service_for(Some(fx.repo.as_path()))
            .unwrap();
        service.refresh().unwrap();
        let plan = service
            .plan_acknowledge(&AcknowledgeRequest {
                document: "docs/trusted.md".into(),
                selection: AcknowledgeSelection::WholeDocument,
            })
            .unwrap();
        service.apply_acknowledge(&plan).unwrap();

        let ask = || json!({"question": "legacy gadget"});
        let found = fx.run(&RepoKnowledgeQueryTool, ask()).await.unwrap();
        assert!(
            found.contains("index: current · evidence: unchanged · lifecycle: draft"),
            "{found}"
        );
        let got = fx
            .run(&RepoKnowledgeGetTool, json!({"path": "docs/trusted.md"}))
            .await
            .unwrap();
        assert!(got.contains("evidence: unchanged"), "{got}");

        // Dependency drift is surfaced; the lifecycle label stays as declared.
        fx.put("src/lib.rs", "pub fn run() { work(); }\n");
        let found = fx.run(&RepoKnowledgeQueryTool, ask()).await.unwrap();
        assert!(
            found.contains("evidence: changed · lifecycle: draft"),
            "{found}"
        );
        fs::remove_file(fx.repo.join("src/lib.rs")).unwrap();
        let found = fx.run(&RepoKnowledgeQueryTool, ask()).await.unwrap();
        assert!(found.contains("evidence: missing"), "{found}");
    }

    #[tokio::test]
    async fn ids_and_paths_from_another_checkout_are_not_found_and_disclose_nothing() {
        let fx = Fx::new();
        fx.put("docs/mine.md", "# Mine\nshared widget notes\n");
        let other_repo = fx.tmp.path().join("other");
        fs::create_dir_all(other_repo.join(".git")).unwrap();
        fs::create_dir_all(other_repo.join("docs")).unwrap();
        fs::write(
            other_repo
                .join(".qmt/knowledge.toml")
                .parent()
                .map(|p| {
                    fs::create_dir_all(p).unwrap();
                    p.join("knowledge.toml")
                })
                .unwrap(),
            MANIFEST,
        )
        .unwrap();
        fs::write(
            other_repo.join("docs/theirs.md"),
            "# Theirs\nOTHER CHECKOUT SECRET widget\n",
        )
        .unwrap();
        let other = Ctx {
            cwd: Some(other_repo),
            registry: fx.ctx.registry.clone(),
            legacy: None,
        };

        // Each checkout sees only its own corpus.
        let theirs = RepoKnowledgeQueryTool
            .call(json!({"question": "widget"}), &other)
            .await
            .unwrap();
        let theirs = format!("{theirs:?}");
        assert!(theirs.contains("docs/theirs.md") && !theirs.contains("docs/mine.md"));
        let mine = fx
            .run(&RepoKnowledgeQueryTool, json!({"question": "widget"}))
            .await
            .unwrap();
        assert!(mine.contains("docs/mine.md") && !mine.contains("theirs"));

        // The other checkout's id and path resolve to nothing here.
        for args in [
            json!({"id": "path:docs/theirs.md"}),
            json!({"path": "docs/theirs.md"}),
            json!({"id": "path:docs/theirs.md#Theirs"}),
        ] {
            let error = fx.run(&RepoKnowledgeGetTool, args).await.unwrap_err();
            assert!(
                !error.to_string().contains("OTHER CHECKOUT SECRET"),
                "{error}"
            );
            assert!(error.to_string().contains("not_found"), "{error}");
        }
    }

    #[tokio::test]
    async fn private_session_entries_and_consolidations_never_cross_the_boundary() {
        use crate::knowledge::sqlite::SqliteKnowledgeStore;
        use crate::knowledge::{ConsolidateRequest, IngestRequest, KnowledgeStore};

        let mut fx = Fx::new();
        fx.put(
            "docs/arch.md",
            "# Arch\nThe zebra scheduler dispatches work.\n",
        );
        let store = Arc::new(SqliteKnowledgeStore::new(
            crate::test_utils::sqlite_conn_with_schema(),
        ));
        let entry = store
            .ingest(
                "s",
                IngestRequest {
                    source: "session".into(),
                    raw_text: "PRIVATE zebra scheduler remark".into(),
                    summary: "PRIVATE zebra scheduler summary".into(),
                    entities: vec![],
                    topics: vec!["zebra".into()],
                    connections: vec![],
                    importance: 0.9,
                },
            )
            .await
            .unwrap();
        store
            .consolidate(
                "s",
                ConsolidateRequest {
                    source_entry_public_ids: vec![entry.public_id.clone()],
                    summary: "PRIVATE zebra consolidation".into(),
                    insight: "PRIVATE zebra insight".into(),
                    connections: vec![],
                },
            )
            .await
            .unwrap();
        fx.ctx.legacy = Some(store.clone());

        // Repository retrieval returns only repository sources.
        let found = fx
            .run(
                &RepoKnowledgeQueryTool,
                json!({"question": "zebra scheduler"}),
            )
            .await
            .unwrap();
        assert!(found.contains("docs/arch.md"), "{found}");
        assert!(!found.contains("PRIVATE"), "{found}");
        for tool in [
            &RepoKnowledgeListTool as &dyn ToolTrait,
            &RepoKnowledgeStatsTool,
        ] {
            let out = fx.run(tool, json!({})).await.unwrap();
            assert!(!out.contains("PRIVATE"), "{out}");
        }

        // The legacy tool still sees only its own memory, never repository text.
        let legacy = crate::tools::builtins::KnowledgeQueryTool::new()
            .call(json!({"question": "zebra scheduler"}), &fx.ctx)
            .await
            .unwrap();
        let legacy = format!("{legacy:?}");
        assert!(legacy.contains("PRIVATE"), "{legacy}");
        assert!(!legacy.contains("docs/arch.md") && !legacy.contains("dispatches work"));

        // Deleting the repository cache leaves the private entry untouched.
        drop(found);
        fs::remove_dir_all(fx.tmp.path().join("cfg")).ok();
        let stats = store.stats("s").await.unwrap();
        assert_eq!(stats.total_entries, 1);
        let again = fx
            .run(
                &RepoKnowledgeQueryTool,
                json!({"question": "zebra scheduler"}),
            )
            .await
            .unwrap();
        assert!(again.contains("docs/arch.md") && !again.contains("PRIVATE"));
    }

    #[tokio::test]
    async fn overlapping_patterns_and_protocol_memories_yield_one_projection_per_source() {
        let fx = Fx::new();
        fx.put(
            ".qmt/knowledge.toml",
            "version = 1\n[documents]\ninclude = [\".agents/memories/*.md\", \"**/*.md\", \".agents/**/*.md\"]\n",
        );
        fx.put(
            ".agents/memories/runtime.md",
            "---\nid: runtime\ntitle: Runtime\n---\n# Runtime\nquokka lifecycle details\n",
        );
        fx.put("docs/arch.md", "# Arch\nquokka also here\n");
        let before = fs::read_dir(fx.repo.join(".agents")).unwrap().count();

        let found = fx
            .run(&RepoKnowledgeQueryTool, json!({"question": "quokka"}))
            .await
            .unwrap();
        assert_eq!(
            found.matches(".agents/memories/runtime.md:").count(),
            1,
            "{found}"
        );
        let listing = fx.run(&RepoKnowledgeListTool, json!({})).await.unwrap();
        assert_eq!(
            listing.matches(".agents/memories/runtime.md").count(),
            1,
            "{listing}"
        );
        let stats = fx.run(&RepoKnowledgeStatsTool, json!({})).await.unwrap();
        assert!(stats.contains("indexed documents: 2"), "{stats}");

        // Reading a protocol memory never writes under `.agents/`.
        assert_eq!(
            fs::read_dir(fx.repo.join(".agents")).unwrap().count(),
            before
        );
        assert!(!fx.repo.join(".agents/knowledge.toml").exists());
    }

    #[tokio::test]
    async fn repository_tools_never_read_session_memory_or_reply_for_a_missing_manifest() {
        let fx = Fx::new();
        fs::remove_file(fx.repo.join(".qmt/knowledge.toml")).unwrap();
        let out = fx
            .run(&RepoKnowledgeQueryTool, json!({"question": "anything"}))
            .await
            .unwrap();
        assert!(out.contains("Not configured"), "{out}");
        assert!(out.contains("0 result(s)"));
    }

    #[test]
    fn legacy_knowledge_tool_schemas_are_unchanged() {
        use crate::tools::builtins::{
            KnowledgeConsolidateTool, KnowledgeIngestTool, KnowledgeListTool, KnowledgeQueryTool,
            KnowledgeStatsTool,
        };
        let legacy: [(Box<dyn ToolTrait>, &str, &[&str]); 5] = [
            (
                Box::new(KnowledgeQueryTool::new()),
                "knowledge_query",
                &[
                    "include_consolidations",
                    "limit",
                    "question",
                    "retrieval_mode",
                    "scope",
                ],
            ),
            (
                Box::new(KnowledgeListTool::new()),
                "knowledge_list_unconsolidated",
                &["limit", "scope"],
            ),
            (
                Box::new(KnowledgeStatsTool::new()),
                "knowledge_stats",
                &["scope"],
            ),
            (
                Box::new(KnowledgeIngestTool::new()),
                "knowledge_ingest",
                &[],
            ),
            (
                Box::new(KnowledgeConsolidateTool::new()),
                "knowledge_consolidate",
                &[],
            ),
        ];
        for (tool, name, properties) in legacy {
            assert_eq!(tool.name(), name);
            let definition = tool.definition();
            assert!(
                !definition
                    .function
                    .description
                    .to_lowercase()
                    .contains("repository"),
                "{name}"
            );
            let schema = &definition.function.parameters;
            let mut found: Vec<&str> = schema["properties"]
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect();
            found.sort_unstable();
            if !properties.is_empty() {
                assert_eq!(found, properties, "{name}");
            }
            assert!(
                !found
                    .iter()
                    .any(|p| p.contains("repo") || *p == "lane" || *p == "path"),
                "{name}"
            );
        }
    }

    #[tokio::test]
    async fn legacy_knowledge_tools_ignore_the_repository_collection() {
        let fx = Fx::new();
        fx.put("docs/arch.md", ARCH);
        // Without a session knowledge store the legacy tool has nothing to
        // search, repository knowledge being enabled notwithstanding.
        let error = crate::tools::builtins::KnowledgeQueryTool::new()
            .call(json!({"question": "scheduler"}), &fx.ctx)
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("Knowledge store not available"),
            "{error}"
        );
    }
}
