//! Retrieval foundation for context compression.
//!
//! Provides SQLite FTS5-backed indexing and retrieval of tool outputs and other
//! content sources. Content is chunked before indexing, and retrieval returns
//! bounded snippets with source-scoped filtering.
//!
//! # Architecture
//!
//! - **Sources**: Each indexed payload is associated with a *source* label
//!   (e.g., tool call ID, file path, URL) and a session.
//! - **Chunks**: Content is split into overlapping chunks before insertion into
//!   the FTS5 virtual table. Two chunking strategies are provided:
//!   - Markdown-ish: heading-aware splitting (respects `#`/`##`/`###` boundaries).
//!   - Plain-text/log: fixed line-window chunks with configurable overlap.
//! - **Search**: Source-scoped full-text search with bounded result count.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

// ============================================================================
// Domain types
// ============================================================================

/// Metadata for an indexed source.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexedSource {
    /// Internal DB row ID.
    pub id: i64,
    /// Session that owns this source.
    pub session_id: String,
    /// Human-readable label (e.g., tool call ID, file path, URL).
    pub source_label: String,
    /// MIME-ish content type hint (e.g., "text/plain", "text/markdown", "application/json").
    pub content_type: String,
    /// Total byte length of the original content.
    pub original_bytes: usize,
    /// Number of chunks stored.
    pub chunk_count: usize,
    /// When this source was indexed.
    pub created_at: OffsetDateTime,
}

/// A single chunk stored in the FTS5 index.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexedChunk {
    /// Internal DB row ID.
    pub id: i64,
    /// FK to the parent source.
    pub source_id: i64,
    /// Zero-based index within the source.
    pub chunk_index: usize,
    /// Optional heading/title context for this chunk.
    pub title: String,
    /// The chunk text.
    pub body: String,
}

/// A search result snippet returned by source-scoped retrieval.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetrievalSnippet {
    /// Source label of the matching chunk.
    pub source_label: String,
    /// Chunk index within the source.
    pub chunk_index: usize,
    /// The title/heading context.
    pub title: String,
    /// The matching chunk body text.
    pub body: String,
    /// FTS5 rank score (lower = better match).
    pub rank: f64,
}

/// Summary of an indexed source (for listing).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceSummary {
    pub source_label: String,
    pub content_type: String,
    pub original_bytes: usize,
    pub chunk_count: usize,
}

// ============================================================================
// Chunking helpers
// ============================================================================

/// Configuration for chunking behaviour.
#[derive(Debug, Clone)]
pub struct ChunkConfig {
    /// Target maximum number of lines per chunk for plain-text splitting.
    pub max_lines: usize,
    /// Number of overlapping lines between consecutive chunks.
    pub overlap_lines: usize,
    /// Target maximum bytes per chunk for markdown splitting.
    pub max_chunk_bytes: usize,
}

impl Default for ChunkConfig {
    fn default() -> Self {
        Self {
            max_lines: 50,
            overlap_lines: 5,
            max_chunk_bytes: 4096,
        }
    }
}

/// A produced chunk with optional heading context.
#[derive(Debug, Clone)]
pub struct Chunk {
    /// Heading context (empty string if none).
    pub title: String,
    /// The chunk body text.
    pub body: String,
}

/// Split markdown-ish content into heading-aware chunks.
///
/// Headings (`# …`, `## …`, `### …`) are used as split boundaries.
/// Each chunk carries the most recent heading as its `title`.
/// If a section exceeds `max_chunk_bytes`, it is further split on
/// paragraph boundaries (blank lines).
pub fn chunk_markdown(content: &str, config: &ChunkConfig) -> Vec<Chunk> {
    let mut chunks = Vec::new();
    let mut current_heading = String::new();
    let mut current_body = String::new();

    for line in content.lines() {
        let trimmed = line.trim_start();
        let is_heading = trimmed.starts_with('#')
            && trimmed
                .chars()
                .skip_while(|c| *c == '#')
                .next()
                .is_none_or(|c| c == ' ');

        if is_heading {
            // Flush current body
            if !current_body.trim().is_empty() {
                flush_markdown_body(&current_heading, &current_body, config, &mut chunks);
            }
            current_heading = trimmed.to_string();
            current_body.clear();
        } else {
            if !current_body.is_empty() {
                current_body.push('\n');
            }
            current_body.push_str(line);
        }
    }

    // Flush remaining
    if !current_body.trim().is_empty() {
        flush_markdown_body(&current_heading, &current_body, config, &mut chunks);
    }

    // Edge case: if no chunks were produced (empty or whitespace-only input),
    // produce nothing rather than a single empty chunk.
    chunks
}

/// Helper: flush a markdown section body, splitting on paragraph boundaries
/// if it exceeds `max_chunk_bytes`.
fn flush_markdown_body(heading: &str, body: &str, config: &ChunkConfig, chunks: &mut Vec<Chunk>) {
    if body.len() <= config.max_chunk_bytes {
        chunks.push(Chunk {
            title: heading.to_string(),
            body: body.to_string(),
        });
        return;
    }

    // Split on blank lines (paragraph boundaries)
    let paragraphs: Vec<&str> = body.split("\n\n").collect();
    let mut current = String::new();

    for para in paragraphs {
        let tentative_len = if current.is_empty() {
            para.len()
        } else {
            current.len() + 2 + para.len() // +2 for "\n\n"
        };

        if tentative_len > config.max_chunk_bytes && !current.is_empty() {
            chunks.push(Chunk {
                title: heading.to_string(),
                body: current.clone(),
            });
            current.clear();
        }

        if !current.is_empty() {
            current.push_str("\n\n");
        }
        current.push_str(para);
    }

    if !current.trim().is_empty() {
        chunks.push(Chunk {
            title: heading.to_string(),
            body: current,
        });
    }
}

/// Split plain-text/log content into fixed line-window chunks with overlap.
///
/// Each chunk contains up to `max_lines` lines. Consecutive chunks share
/// `overlap_lines` lines to preserve context across boundaries.
pub fn chunk_plain_text(content: &str, config: &ChunkConfig) -> Vec<Chunk> {
    let lines: Vec<&str> = content.lines().collect();
    if lines.is_empty() {
        return Vec::new();
    }

    let mut chunks = Vec::new();
    let mut start = 0;

    while start < lines.len() {
        let end = (start + config.max_lines).min(lines.len());
        let body = lines[start..end].join("\n");
        chunks.push(Chunk {
            title: String::new(),
            body,
        });

        // Advance with overlap
        let step = if config.max_lines > config.overlap_lines {
            config.max_lines - config.overlap_lines
        } else {
            config.max_lines
        };
        start += step;

        // Don't create a tiny trailing chunk that's entirely overlap
        if start >= lines.len() {
            break;
        }
    }

    chunks
}

/// Auto-detect content type and chunk accordingly.
///
/// Returns `("text/markdown", chunks)` or `("text/plain", chunks)`.
pub fn chunk_auto(content: &str, config: &ChunkConfig) -> (&'static str, Vec<Chunk>) {
    // Simple heuristic: if content contains markdown headings, treat as markdown
    let has_headings = content.lines().any(|line| {
        let trimmed = line.trim_start();
        trimmed.starts_with('#')
            && trimmed
                .chars()
                .skip_while(|c| *c == '#')
                .next()
                .is_none_or(|c| c == ' ')
    });

    if has_headings {
        ("text/markdown", chunk_markdown(content, config))
    } else {
        ("text/plain", chunk_plain_text(content, config))
    }
}

// ============================================================================
// SQL schema helpers (called from migration)
// ============================================================================

/// Create the indexed_sources and indexed_chunks tables + FTS5 virtual table.
///
/// This is called from migration_0003 in sqlite_storage.rs.
pub fn create_retrieval_tables(conn: &rusqlite::Connection) -> Result<(), rusqlite::Error> {
    conn.execute_batch(
        r#"
        -- Sources: metadata for each indexed payload
        CREATE TABLE IF NOT EXISTS indexed_sources (
            id INTEGER PRIMARY KEY,
            session_id TEXT NOT NULL,
            source_label TEXT NOT NULL,
            content_type TEXT NOT NULL DEFAULT 'text/plain',
            original_bytes INTEGER NOT NULL DEFAULT 0,
            chunk_count INTEGER NOT NULL DEFAULT 0,
            created_at TEXT NOT NULL DEFAULT (datetime('now'))
        );

        CREATE INDEX IF NOT EXISTS idx_indexed_sources_session
            ON indexed_sources(session_id);
        CREATE INDEX IF NOT EXISTS idx_indexed_sources_label
            ON indexed_sources(session_id, source_label);

        -- Chunks: the actual text fragments
        CREATE TABLE IF NOT EXISTS indexed_chunks (
            id INTEGER PRIMARY KEY,
            source_id INTEGER NOT NULL,
            chunk_index INTEGER NOT NULL,
            title TEXT NOT NULL DEFAULT '',
            body TEXT NOT NULL,
            FOREIGN KEY(source_id) REFERENCES indexed_sources(id) ON DELETE CASCADE
        );

        CREATE INDEX IF NOT EXISTS idx_indexed_chunks_source
            ON indexed_chunks(source_id);

        -- FTS5 virtual table for full-text search on chunks
        -- content= points to indexed_chunks for external-content mode
        -- but we use a regular (internal-content) FTS5 table for simplicity
        CREATE VIRTUAL TABLE IF NOT EXISTS indexed_chunks_fts USING fts5(
            title,
            body,
            content='indexed_chunks',
            content_rowid='id'
        );

        -- Triggers to keep FTS5 in sync with indexed_chunks
        CREATE TRIGGER IF NOT EXISTS indexed_chunks_ai AFTER INSERT ON indexed_chunks BEGIN
            INSERT INTO indexed_chunks_fts(rowid, title, body) VALUES (new.id, new.title, new.body);
        END;

        CREATE TRIGGER IF NOT EXISTS indexed_chunks_ad AFTER DELETE ON indexed_chunks BEGIN
            INSERT INTO indexed_chunks_fts(indexed_chunks_fts, rowid, title, body) VALUES('delete', old.id, old.title, old.body);
        END;

        CREATE TRIGGER IF NOT EXISTS indexed_chunks_au AFTER UPDATE ON indexed_chunks BEGIN
            INSERT INTO indexed_chunks_fts(indexed_chunks_fts, rowid, title, body) VALUES('delete', old.id, old.title, old.body);
            INSERT INTO indexed_chunks_fts(rowid, title, body) VALUES (new.id, new.title, new.body);
        END;
        "#,
    )?;

    Ok(())
}

// ============================================================================
// Store operations (sync, called inside spawn_blocking)
// ============================================================================

/// Index content: create a source record, chunk the content, and insert chunks.
///
/// Returns the source ID.
pub fn index_content(
    conn: &rusqlite::Connection,
    session_id: &str,
    source_label: &str,
    content: &str,
    config: &ChunkConfig,
) -> Result<i64, rusqlite::Error> {
    let (content_type, chunks) = chunk_auto(content, config);
    let original_bytes = content.len();
    let chunk_count = chunks.len();

    let now = OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default();

    conn.execute(
        "INSERT INTO indexed_sources (session_id, source_label, content_type, original_bytes, chunk_count, created_at) VALUES (?, ?, ?, ?, ?, ?)",
        rusqlite::params![session_id, source_label, content_type, original_bytes, chunk_count, now],
    )?;
    let source_id = conn.last_insert_rowid();

    let mut stmt = conn.prepare(
        "INSERT INTO indexed_chunks (source_id, chunk_index, title, body) VALUES (?, ?, ?, ?)",
    )?;

    for (idx, chunk) in chunks.iter().enumerate() {
        stmt.execute(rusqlite::params![source_id, idx, chunk.title, chunk.body])?;
    }

    Ok(source_id)
}

/// Search indexed chunks with optional source-label scoping.
///
/// Returns up to `max_results` snippets ordered by FTS5 rank (best first).
pub fn search_chunks(
    conn: &rusqlite::Connection,
    session_id: &str,
    query: &str,
    source_label_filter: Option<&str>,
    max_results: usize,
) -> Result<Vec<RetrievalSnippet>, rusqlite::Error> {
    // Build query with optional source filter
    let sql = if source_label_filter.is_some() {
        r#"
        SELECT s.source_label, c.chunk_index, c.title, c.body, fts.rank
        FROM indexed_chunks_fts fts
        JOIN indexed_chunks c ON c.id = fts.rowid
        JOIN indexed_sources s ON s.id = c.source_id
        WHERE indexed_chunks_fts MATCH ?
          AND s.session_id = ?
          AND s.source_label = ?
        ORDER BY fts.rank
        LIMIT ?
        "#
    } else {
        r#"
        SELECT s.source_label, c.chunk_index, c.title, c.body, fts.rank
        FROM indexed_chunks_fts fts
        JOIN indexed_chunks c ON c.id = fts.rowid
        JOIN indexed_sources s ON s.id = c.source_id
        WHERE indexed_chunks_fts MATCH ?
          AND s.session_id = ?
        ORDER BY fts.rank
        LIMIT ?
        "#
    };

    let mut stmt = conn.prepare(sql)?;

    let rows: Vec<RetrievalSnippet> = if let Some(label) = source_label_filter {
        stmt.query_map(
            rusqlite::params![query, session_id, label, max_results],
            parse_snippet_row,
        )?
        .collect::<Result<Vec<_>, _>>()?
    } else {
        stmt.query_map(
            rusqlite::params![query, session_id, max_results],
            parse_snippet_row,
        )?
        .collect::<Result<Vec<_>, _>>()?
    };

    Ok(rows)
}

fn parse_snippet_row(row: &rusqlite::Row<'_>) -> Result<RetrievalSnippet, rusqlite::Error> {
    Ok(RetrievalSnippet {
        source_label: row.get(0)?,
        chunk_index: row.get::<_, usize>(1)?,
        title: row.get(2)?,
        body: row.get(3)?,
        rank: row.get(4)?,
    })
}

/// List source summaries for a session.
pub fn list_sources(
    conn: &rusqlite::Connection,
    session_id: &str,
) -> Result<Vec<SourceSummary>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT source_label, content_type, original_bytes, chunk_count FROM indexed_sources WHERE session_id = ? ORDER BY created_at DESC",
    )?;

    let rows = stmt.query_map(rusqlite::params![session_id], |row| {
        Ok(SourceSummary {
            source_label: row.get(0)?,
            content_type: row.get(1)?,
            original_bytes: row.get(2)?,
            chunk_count: row.get(3)?,
        })
    })?;

    rows.collect()
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn setup_db() -> Connection {
        let conn = Connection::open_in_memory().expect("in-memory db");
        conn.execute("PRAGMA foreign_keys = ON;", []).unwrap();
        create_retrieval_tables(&conn).unwrap();
        conn
    }

    // ── Chunking tests ──────────────────────────────────────────────────

    #[test]
    fn chunk_plain_text_basic() {
        let content = (0..100)
            .map(|i| format!("line {}", i))
            .collect::<Vec<_>>()
            .join("\n");
        let config = ChunkConfig {
            max_lines: 20,
            overlap_lines: 5,
            ..Default::default()
        };
        let chunks = chunk_plain_text(&content, &config);
        assert!(chunks.len() > 1, "should produce multiple chunks");

        // First chunk should have 20 lines
        let first_lines: Vec<&str> = chunks[0].body.lines().collect();
        assert_eq!(first_lines.len(), 20);

        // Verify overlap: last 5 lines of chunk 0 == first 5 lines of chunk 1
        let chunk0_lines: Vec<&str> = chunks[0].body.lines().collect();
        let chunk1_lines: Vec<&str> = chunks[1].body.lines().collect();
        assert_eq!(chunk0_lines[15..20], chunk1_lines[0..5]);
    }

    #[test]
    fn chunk_plain_text_empty() {
        let chunks = chunk_plain_text("", &ChunkConfig::default());
        assert!(chunks.is_empty());
    }

    #[test]
    fn chunk_plain_text_small() {
        let content = "line 1\nline 2\nline 3";
        let config = ChunkConfig {
            max_lines: 50,
            ..Default::default()
        };
        let chunks = chunk_plain_text(content, &config);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].body, content);
    }

    #[test]
    fn chunk_markdown_heading_aware() {
        let content = "\
# Introduction
This is the intro.

# Methods
We used method A.

## Sub-method
Details here.

# Conclusion
The end.
";
        let config = ChunkConfig {
            max_chunk_bytes: 4096,
            ..Default::default()
        };
        let chunks = chunk_markdown(content, &config);
        assert!(chunks.len() >= 3, "got {} chunks", chunks.len());

        // First chunk should have heading context
        assert!(chunks[0].title.contains("Introduction"));
        // Last chunk should have "Conclusion" heading
        assert!(chunks.last().unwrap().title.contains("Conclusion"));
    }

    #[test]
    fn chunk_markdown_empty() {
        let chunks = chunk_markdown("", &ChunkConfig::default());
        assert!(chunks.is_empty());
    }

    #[test]
    fn chunk_markdown_no_headings() {
        let content = "Just some text.\nAnother line.";
        let chunks = chunk_markdown(content, &ChunkConfig::default());
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].title.is_empty());
    }

    #[test]
    fn chunk_markdown_large_section_splits() {
        // Create a section larger than max_chunk_bytes
        let big_section = (0..100)
            .map(|i| format!("Paragraph {} with enough text to fill some bytes.", i))
            .collect::<Vec<_>>()
            .join("\n\n");
        let content = format!("# Big Section\n{}", big_section);
        let config = ChunkConfig {
            max_chunk_bytes: 256,
            ..Default::default()
        };
        let chunks = chunk_markdown(&content, &config);
        assert!(
            chunks.len() > 1,
            "large section should be split into multiple chunks"
        );
        // All should have the same heading
        for chunk in &chunks {
            assert_eq!(chunk.title, "# Big Section");
        }
    }

    #[test]
    fn chunk_auto_detects_markdown() {
        let markdown = "# Title\nSome content\n## Section\nMore content";
        let (ct, chunks) = chunk_auto(markdown, &ChunkConfig::default());
        assert_eq!(ct, "text/markdown");
        assert!(!chunks.is_empty());
    }

    #[test]
    fn chunk_auto_detects_plain_text() {
        let plain = "line 1\nline 2\nline 3";
        let (ct, chunks) = chunk_auto(plain, &ChunkConfig::default());
        assert_eq!(ct, "text/plain");
        assert!(!chunks.is_empty());
    }

    // ── Schema tests ────────────────────────────────────────────────────

    #[test]
    fn create_retrieval_tables_idempotent() {
        let conn = setup_db();
        // Second call should succeed (IF NOT EXISTS)
        create_retrieval_tables(&conn).unwrap();
    }

    // ── Index + search tests ────────────────────────────────────────────

    #[test]
    fn index_and_search_basic() {
        let conn = setup_db();
        let session_id = "test-session-1";

        let source_id = index_content(
            &conn,
            session_id,
            "tool_call_123",
            "The quick brown fox jumps over the lazy dog.\nRust is a systems programming language.",
            &ChunkConfig::default(),
        )
        .unwrap();
        assert!(source_id > 0);

        let results = search_chunks(&conn, session_id, "Rust programming", None, 10).unwrap();
        assert!(
            !results.is_empty(),
            "should find results for 'Rust programming'"
        );
        assert_eq!(results[0].source_label, "tool_call_123");
    }

    #[test]
    fn search_no_match() {
        let conn = setup_db();
        let session_id = "test-session-2";

        index_content(
            &conn,
            session_id,
            "src1",
            "Hello world",
            &ChunkConfig::default(),
        )
        .unwrap();

        let results = search_chunks(&conn, session_id, "quantum entanglement", None, 10).unwrap();
        assert!(
            results.is_empty(),
            "should find no results for unrelated query"
        );
    }

    #[test]
    fn search_source_filter() {
        let conn = setup_db();
        let session_id = "test-session-3";

        index_content(
            &conn,
            session_id,
            "source_a",
            "Rust error handling with Result types",
            &ChunkConfig::default(),
        )
        .unwrap();

        index_content(
            &conn,
            session_id,
            "source_b",
            "Rust error handling with anyhow crate",
            &ChunkConfig::default(),
        )
        .unwrap();

        // Search with source filter
        let results_a =
            search_chunks(&conn, session_id, "error handling", Some("source_a"), 10).unwrap();
        assert!(!results_a.is_empty());
        assert!(results_a.iter().all(|r| r.source_label == "source_a"));

        let results_b =
            search_chunks(&conn, session_id, "error handling", Some("source_b"), 10).unwrap();
        assert!(!results_b.is_empty());
        assert!(results_b.iter().all(|r| r.source_label == "source_b"));
    }

    #[test]
    fn search_session_isolation() {
        let conn = setup_db();

        index_content(
            &conn,
            "session_x",
            "src",
            "Unique content for session X",
            &ChunkConfig::default(),
        )
        .unwrap();

        index_content(
            &conn,
            "session_y",
            "src",
            "Different content for session Y",
            &ChunkConfig::default(),
        )
        .unwrap();

        // Searching session_x should not find session_y content
        let results = search_chunks(&conn, "session_x", "session Y", None, 10).unwrap();
        assert!(
            results.is_empty(),
            "session isolation: should not find cross-session content"
        );

        let results = search_chunks(&conn, "session_x", "session X", None, 10).unwrap();
        assert!(
            !results.is_empty(),
            "should find content within same session"
        );
    }

    #[test]
    fn search_respects_max_results() {
        let conn = setup_db();
        let session_id = "test-session-limit";

        // Index many chunks
        for i in 0..10 {
            index_content(
                &conn,
                session_id,
                &format!("src_{}", i),
                &format!("Rust programming language variant {}", i),
                &ChunkConfig::default(),
            )
            .unwrap();
        }

        let results = search_chunks(&conn, session_id, "Rust", None, 3).unwrap();
        assert!(
            results.len() <= 3,
            "should return at most 3 results, got {}",
            results.len()
        );
    }

    #[test]
    fn list_sources_basic() {
        let conn = setup_db();
        let session_id = "test-session-list";

        index_content(
            &conn,
            session_id,
            "tool_call_1",
            "Some content",
            &ChunkConfig::default(),
        )
        .unwrap();

        index_content(
            &conn,
            session_id,
            "tool_call_2",
            "Other content",
            &ChunkConfig::default(),
        )
        .unwrap();

        let sources = list_sources(&conn, session_id).unwrap();
        assert_eq!(sources.len(), 2);

        let labels: Vec<&str> = sources.iter().map(|s| s.source_label.as_str()).collect();
        assert!(labels.contains(&"tool_call_1"));
        assert!(labels.contains(&"tool_call_2"));
    }

    #[test]
    fn list_sources_empty_session() {
        let conn = setup_db();
        let sources = list_sources(&conn, "nonexistent-session").unwrap();
        assert!(sources.is_empty());
    }

    #[test]
    fn index_markdown_content_preserves_headings() {
        let conn = setup_db();
        let session_id = "test-session-md";

        let markdown =
            "# API Reference\nGet endpoint returns data.\n\n# Error Codes\n404 means not found.";
        index_content(
            &conn,
            session_id,
            "api_docs",
            markdown,
            &ChunkConfig::default(),
        )
        .unwrap();

        // Search for error-related content
        let results = search_chunks(&conn, session_id, "404 not found", None, 10).unwrap();
        assert!(!results.is_empty());
        // The matching chunk should have "Error Codes" as title
        assert!(
            results[0].title.contains("Error Codes"),
            "title should contain heading, got: '{}'",
            results[0].title
        );
    }

    #[test]
    fn index_content_records_metadata() {
        let conn = setup_db();
        let session_id = "test-session-meta";
        let content = "Some test content that is not very long.";

        index_content(
            &conn,
            session_id,
            "meta_source",
            content,
            &ChunkConfig::default(),
        )
        .unwrap();

        let sources = list_sources(&conn, session_id).unwrap();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].source_label, "meta_source");
        assert_eq!(sources[0].original_bytes, content.len());
        assert!(sources[0].chunk_count > 0);
    }

    #[test]
    fn cascade_delete_source_removes_chunks() {
        let conn = setup_db();
        let session_id = "test-session-cascade";

        let source_id = index_content(
            &conn,
            session_id,
            "deleteme",
            "Content to be deleted",
            &ChunkConfig::default(),
        )
        .unwrap();

        // Verify chunks exist
        let chunk_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM indexed_chunks WHERE source_id = ?",
                rusqlite::params![source_id],
                |row| row.get(0),
            )
            .unwrap();
        assert!(chunk_count > 0);

        // Delete source
        conn.execute(
            "DELETE FROM indexed_sources WHERE id = ?",
            rusqlite::params![source_id],
        )
        .unwrap();

        // Chunks should be cascade-deleted
        let chunk_count_after: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM indexed_chunks WHERE source_id = ?",
                rusqlite::params![source_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(chunk_count_after, 0);
    }
}
