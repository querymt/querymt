use tree_sitter::{Node, Parser};

use super::super::types::SymbolDigest;
use super::safe_slice;
use crate::index::symbol_index::{SymbolEntry, SymbolError, SymbolKind};

pub fn extract(source: &str) -> Result<Vec<SymbolEntry>, SymbolError> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_md::LANGUAGE.into())
        .map_err(|e| SymbolError::ParseError(format!("Failed to load Markdown grammar: {e}")))?;

    let tree = parser
        .parse(source, None)
        .ok_or_else(|| SymbolError::ParseError("Failed to parse Markdown source".to_string()))?;

    // tree-sitter-md nests `section` nodes by heading level already; map each
    // section to a Module entry (routed to `sections` in the outline) with
    // nested sections as children.
    let mut symbols = Vec::new();
    let root = tree.root_node();
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        match child.kind() {
            "section" => collect_section(&child, source, &mut symbols),
            "atx_heading" | "setext_heading" => {
                symbols.push(heading_symbol(&child, source, Vec::new()))
            }
            _ => {}
        }
    }
    Ok(symbols)
}

/// Collect the section (heading entry with nested sections as children) into
/// `out`. Sections without a heading (e.g. content before the first heading)
/// promote their nested sections to `out`.
fn collect_section(node: &Node, source: &str, out: &mut Vec<SymbolEntry>) {
    let mut children = Vec::new();
    let mut heading = None;
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "section" => collect_section(&child, source, &mut children),
            // The grammar does not wrap setext headings in their own section;
            // they appear as additional headings inside the current one.
            "setext_heading" => children.push(heading_symbol(&child, source, Vec::new())),
            "atx_heading" if heading.is_none() => {
                heading = Some(child);
            }
            _ => {}
        }
    }

    match heading {
        Some(heading) => out.push(heading_symbol(&heading, source, children)),
        None => out.extend(children),
    }
}

fn heading_symbol(node: &Node, source: &str, children: Vec<SymbolEntry>) -> SymbolEntry {
    let level = heading_level(node, source);
    let text = heading_text(node, source);
    let signature = format!("{} {}", "#".repeat(level), text);

    let node_text = node_text(node, source);
    SymbolEntry {
        kind: SymbolKind::Module,
        qualified_name: signature.clone(),
        name: signature.clone(),
        signature,
        start_line: node.start_position().row + 1,
        end_line: node.end_position().row + 1,
        start_byte: node.start_byte(),
        end_byte: node.end_byte(),
        body_start_line: None,
        body_end_line: None,
        parent: None,
        children,
        digest: SymbolDigest::new(node_text.as_bytes(), node_text.lines().count()),
    }
}

fn heading_level(node: &Node, source: &str) -> usize {
    match node.kind() {
        "setext_heading" => {
            if find_child_by_kind(node, "setext_h1_underline").is_some() {
                1
            } else {
                2
            }
        }
        _ => node_text(node, source)
            .trim_start()
            .chars()
            .take_while(|&c| c == '#')
            .count()
            .max(1),
    }
}

fn heading_text(node: &Node, source: &str) -> String {
    node.child_by_field_name("heading_content")
        .map(|n| node_text(&n, source).trim().to_string())
        .unwrap_or_default()
}

fn node_text<'a>(node: &Node, source: &'a str) -> &'a str {
    safe_slice(source, node.start_byte(), node.end_byte())
}

fn find_child_by_kind<'a>(node: &'a Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find(|&child| child.kind() == kind)
}
