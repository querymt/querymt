//! Protocol memory model and its document parser.

use super::diagnostics::{DotagentsDiagnostic, DotagentsDiagnosticCode};
use super::fields::{fingerprint, partition_fields};
use super::frontmatter::{field_bool, field_list, field_string, parse_frontmatter_markdown};
use super::layer::DotagentsSource;
use std::collections::BTreeMap;

/// A protocol memory from `.agents/memories/<id>.md`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DotagentsMemory {
    /// Stable normalized ID.
    pub id: String,
    /// Optional title.
    pub title: Option<String>,
    /// Topics/tags.
    pub tags: Vec<String>,
    /// Protocol importance value, when provided.
    pub importance: Option<String>,
    /// Explicit content override from metadata, when provided.
    pub content: Option<String>,
    /// The Markdown body.
    pub body: String,
    /// Whether the memory is enabled (defaults to true).
    pub enabled: bool,
    /// Unknown frontmatter fields retained for forward compatibility.
    pub extensions: BTreeMap<String, serde_json::Value>,
    /// Where the effective entry came from.
    pub source: DotagentsSource,
    /// Content fingerprint of the effective source.
    pub fingerprint: String,
}

// ---------------------------------------------------------------------------
// `memories/<id>.md`
// ---------------------------------------------------------------------------

/// Frontmatter keys recognized on a memory document.
const MEMORY_KEYS: &[&str] = &["id", "title", "tags", "importance", "content", "enabled"];

/// Parse a `memories/*.md` document.
///
/// `entry_id` is the file stem, used as the stable ID unless the frontmatter
/// provides an explicit `id`. The body may be empty when explicit `content`
/// metadata is provided, since the importer falls back to it.
pub fn parse_memory_document(
    text: &str,
    entry_id: &str,
    source: DotagentsSource,
) -> Result<DotagentsMemory, Box<DotagentsDiagnostic>> {
    let parsed =
        parse_frontmatter_markdown(text).map_err(|error| error.with_source(source.clone()))?;
    let (recognized, extensions) = partition_fields(&parsed.fields, MEMORY_KEYS);

    let id = field_string(&recognized, "id").unwrap_or_else(|| entry_id.to_string());
    let title = field_string(&recognized, "title");
    let tags = field_list(&recognized, "tags").unwrap_or_default();
    let importance = field_string(&recognized, "importance");
    let content = field_string(&recognized, "content");
    let enabled = field_bool(&recognized, "enabled").unwrap_or(true);

    let body = parsed.body;
    if body.trim().is_empty() && content.as_deref().map(str::trim).unwrap_or("").is_empty() {
        return Err(Box::new(
            DotagentsDiagnostic::error(
                DotagentsDiagnosticCode::MissingField,
                format!("memory `{id}` has neither a Markdown body nor `content` metadata"),
            )
            .with_source(source),
        ));
    }

    Ok(DotagentsMemory {
        id,
        title,
        tags,
        importance,
        content,
        body,
        enabled,
        extensions,
        source: source.clone(),
        fingerprint: fingerprint(text),
    })
}
