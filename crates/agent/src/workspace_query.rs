//! Types for workspace language queries (agent ↔ client).
//!
//! These types are used by the `language_query` builtin tool to request
//! language intelligence (diagnostics, references, definitions, symbols,
//! hover info) from the connected client (e.g., VS Code extension).
//!
//! The query is serialized as an [`ExtRequest`] with method `"workspace/query"`,
//! which the ACP SDK sends as `"_workspace/query"` on the wire. The client
//! handles it by calling VS Code language APIs and returns the result.
//!
//! ## Data flow
//!
//! ```text
//! LLM calls language_query tool
//!   → tool builds WorkspaceQueryRequest
//!   → ClientBridgeSender::workspace_query(req)
//!   → bridge task receives WorkspaceQuery message
//!   → connection.ext_method(ExtRequest::new("workspace/query", ...))
//!   → client (VS Code) handles _workspace/query
//!   → client returns JSON-RPC response
//!   → bridge task parses WorkspaceQueryResponse
//!   → oneshot channel delivers result to tool
//!   → tool formats result as string for LLM
//! ```

use serde::{Deserialize, Serialize};

// ══════════════════════════════════════════════════════════════════════════
//  Request types
// ══════════════════════════════════════════════════════════════════════════

/// Request sent from agent to client for workspace language queries.
///
/// Tagged by `action` field so the JSON looks like:
/// ```json
/// { "action": "references", "uri": "file:///...", "line": 10, "character": 5 }
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum WorkspaceQueryRequest {
    /// Get diagnostics (errors, warnings) for a file.
    Diagnostics { uri: String },

    /// Find all references to the symbol at the given position.
    References {
        uri: String,
        line: u32,
        character: u32,
    },

    /// Go to the definition of the symbol at the given position.
    Definition {
        uri: String,
        line: u32,
        character: u32,
    },

    /// List all symbols in a document.
    DocumentSymbols { uri: String },

    /// Search for symbols across the workspace.
    WorkspaceSymbols { query: String },

    /// Get hover information for the symbol at the given position.
    Hover {
        uri: String,
        line: u32,
        character: u32,
    },

    /// Go to the type definition of the symbol at the given position.
    TypeDefinition {
        uri: String,
        line: u32,
        character: u32,
    },
}

// ══════════════════════════════════════════════════════════════════════════
//  Response types
// ══════════════════════════════════════════════════════════════════════════

/// A source location returned from the client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocationInfo {
    pub uri: String,
    pub range: Range,
}

/// A range within a document, defined by start and end positions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

/// A position within a document (0-based line and character).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

/// A diagnostic entry (error, warning, etc.).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticInfo {
    pub uri: String,
    pub range: Range,
    /// Severity level: `"error"`, `"warning"`, `"info"`, or `"hint"`.
    pub severity: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<serde_json::Value>,
}

/// A symbol entry (for document or workspace symbols).
///
/// For document symbols, `children` may contain nested symbols.
/// For workspace symbols, `uri` is present to indicate the file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolInfo {
    pub name: String,
    /// Symbol kind: `"function"`, `"class"`, `"variable"`, `"module"`, etc.
    pub kind: String,
    pub range: Range,
    /// Present for workspace symbols to indicate the file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
    /// Nested child symbols (document symbols only).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<SymbolInfo>,
}

/// Response from the client for workspace queries.
///
/// Uses `#[serde(untagged)]` so the JSON shape depends on which fields are present:
/// - `{ "diagnostics": [...] }` for diagnostics
/// - `{ "locations": [...] }` for references, definition, type_definition
/// - `{ "symbols": [...] }` for document_symbols, workspace_symbols
/// - `{ "contents": "..." }` for hover
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum WorkspaceQueryResponse {
    Diagnostics { diagnostics: Vec<DiagnosticInfo> },
    Locations { locations: Vec<LocationInfo> },
    Symbols { symbols: Vec<SymbolInfo> },
    Hover { contents: String },
}

// ══════════════════════════════════════════════════════════════════════════
//  Display formatting (for tool output to LLM)
// ══════════════════════════════════════════════════════════════════════════

impl std::fmt::Display for WorkspaceQueryResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WorkspaceQueryResponse::Diagnostics { diagnostics } => {
                if diagnostics.is_empty() {
                    write!(f, "No diagnostics found.")?;
                } else {
                    writeln!(f, "Found {} diagnostic(s):", diagnostics.len())?;
                    for d in diagnostics {
                        write!(
                            f,
                            "\n[{}] {}:{}:{}: {}",
                            d.severity,
                            d.uri,
                            d.range.start.line + 1,
                            d.range.start.character + 1,
                            d.message
                        )?;
                        if let Some(source) = &d.source {
                            write!(f, " ({})", source)?;
                        }
                    }
                }
                Ok(())
            }
            WorkspaceQueryResponse::Locations { locations } => {
                if locations.is_empty() {
                    write!(f, "No locations found.")?;
                } else {
                    writeln!(f, "Found {} location(s):", locations.len())?;
                    for loc in locations {
                        write!(
                            f,
                            "\n  {}:{}:{}",
                            loc.uri,
                            loc.range.start.line + 1,
                            loc.range.start.character + 1,
                        )?;
                    }
                }
                Ok(())
            }
            WorkspaceQueryResponse::Symbols { symbols } => {
                if symbols.is_empty() {
                    write!(f, "No symbols found.")?;
                } else {
                    writeln!(f, "Found {} symbol(s):", symbols.len())?;
                    for sym in symbols {
                        format_symbol(f, sym, 0)?;
                    }
                }
                Ok(())
            }
            WorkspaceQueryResponse::Hover { contents } => {
                if contents.is_empty() {
                    write!(f, "No hover information available.")?;
                } else {
                    write!(f, "{}", contents)?;
                }
                Ok(())
            }
        }
    }
}

fn format_symbol(
    f: &mut std::fmt::Formatter<'_>,
    sym: &SymbolInfo,
    indent: usize,
) -> std::fmt::Result {
    let pad = "  ".repeat(indent + 1);
    write!(
        f,
        "\n{}{} ({}) at L{}",
        pad,
        sym.name,
        sym.kind,
        sym.range.start.line + 1,
    )?;
    if let Some(uri) = &sym.uri {
        write!(f, " in {}", uri)?;
    }
    for child in &sym.children {
        format_symbol(f, child, indent + 1)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_serializes_with_action_tag() {
        let req = WorkspaceQueryRequest::References {
            uri: "file:///src/main.rs".to_string(),
            line: 10,
            character: 5,
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["action"], "references");
        assert_eq!(json["uri"], "file:///src/main.rs");
        assert_eq!(json["line"], 10);
        assert_eq!(json["character"], 5);
    }

    #[test]
    fn request_deserializes_from_tagged_json() {
        let json = r#"{"action":"diagnostics","uri":"file:///foo.rs"}"#;
        let req: WorkspaceQueryRequest = serde_json::from_str(json).unwrap();
        assert!(
            matches!(req, WorkspaceQueryRequest::Diagnostics { uri } if uri == "file:///foo.rs")
        );
    }

    #[test]
    fn response_diagnostics_round_trip() {
        let resp = WorkspaceQueryResponse::Diagnostics {
            diagnostics: vec![DiagnosticInfo {
                uri: "file:///src/main.rs".to_string(),
                range: Range {
                    start: Position {
                        line: 5,
                        character: 0,
                    },
                    end: Position {
                        line: 5,
                        character: 10,
                    },
                },
                severity: "error".to_string(),
                message: "mismatched types".to_string(),
                source: Some("rust-analyzer".to_string()),
                code: Some(serde_json::Value::String("E0308".to_string())),
            }],
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: WorkspaceQueryResponse = serde_json::from_str(&json).unwrap();
        assert!(
            matches!(parsed, WorkspaceQueryResponse::Diagnostics { diagnostics } if diagnostics.len() == 1)
        );
    }

    #[test]
    fn response_locations_display() {
        let resp = WorkspaceQueryResponse::Locations {
            locations: vec![LocationInfo {
                uri: "file:///src/lib.rs".to_string(),
                range: Range {
                    start: Position {
                        line: 42,
                        character: 4,
                    },
                    end: Position {
                        line: 42,
                        character: 20,
                    },
                },
            }],
        };
        let output = resp.to_string();
        assert!(output.contains("1 location(s)"));
        assert!(output.contains("lib.rs:43:5"));
    }

    #[test]
    fn response_hover_display() {
        let resp = WorkspaceQueryResponse::Hover {
            contents: "```rust\nfn main()\n```\nThe entry point.".to_string(),
        };
        let output = resp.to_string();
        assert!(output.contains("fn main()"));
    }
}
