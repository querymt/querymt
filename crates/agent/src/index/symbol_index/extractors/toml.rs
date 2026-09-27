use tree_sitter::{Node, Parser};

use super::super::types::SymbolDigest;
use super::safe_slice;
use crate::index::symbol_index::{SymbolEntry, SymbolError, SymbolKind};

pub fn extract(source: &str) -> Result<Vec<SymbolEntry>, SymbolError> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_toml_ng::LANGUAGE.into())
        .map_err(|e| SymbolError::ParseError(format!("Failed to load TOML grammar: {e}")))?;

    let tree = parser
        .parse(source, None)
        .ok_or_else(|| SymbolError::ParseError("Failed to parse TOML source".to_string()))?;

    let mut symbols = Vec::new();
    let root = tree.root_node();
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        match child.kind() {
            "pair" => {
                if let Some(entry) = pair_symbol(&child, source) {
                    symbols.push(entry);
                }
            }
            "table" | "table_array_element" => {
                symbols.push(table_symbol(&child, source));
            }
            _ => {}
        }
    }
    Ok(symbols)
}

/// `[table]` / `[[array]]` headers → Module (routed to `sections` in the
/// outline), with contained pairs as Const children.
fn table_symbol(node: &Node, source: &str) -> SymbolEntry {
    let signature = first_line(node, source);
    let name = signature
        .trim_start_matches(['[', '['])
        .trim_end_matches([']', ']'])
        .trim()
        .to_string();

    let mut children = Vec::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "pair"
            && let Some(entry) = pair_symbol(&child, source)
        {
            children.push(entry);
        }
    }

    symbol_entry(node, source, SymbolKind::Module, name, signature, children)
}

fn pair_symbol(node: &Node, source: &str) -> Option<SymbolEntry> {
    // toml-ng `pair` has no field names: first named child is the key, the
    // rest form the value.
    let mut cursor = node.walk();
    let mut children = node.named_children(&mut cursor);
    let key_node = children.next()?;
    let key = key_text(&key_node, source);
    let value_text = children
        .next()
        .map(|v| first_line(&v, source))
        .unwrap_or_default();

    let signature = if value_text.is_empty() {
        key.clone()
    } else {
        format!("{key} = {value_text}")
    };
    Some(symbol_entry(
        node,
        source,
        SymbolKind::Const,
        key,
        signature,
        Vec::new(),
    ))
}

/// `bare_key` text, or dotted/quoted keys joined with `.`.
fn key_text(node: &Node, source: &str) -> String {
    if node.kind() == "dotted_key" {
        let mut cursor = node.walk();
        let parts: Vec<String> = node
            .named_children(&mut cursor)
            .map(|part| node_text(&part, source).to_string())
            .collect();
        return parts.join(".");
    }
    node_text(node, source)
        .trim()
        .trim_matches(['"', '\''])
        .to_string()
}

fn symbol_entry(
    node: &Node,
    source: &str,
    kind: SymbolKind,
    name: String,
    signature: String,
    children: Vec<SymbolEntry>,
) -> SymbolEntry {
    let text = node_text(node, source);
    SymbolEntry {
        kind,
        qualified_name: name.clone(),
        name,
        signature,
        start_line: node.start_position().row + 1,
        end_line: node.end_position().row + 1,
        start_byte: node.start_byte(),
        end_byte: node.end_byte(),
        body_start_line: None,
        body_end_line: None,
        parent: None,
        children,
        digest: SymbolDigest::new(text.as_bytes(), text.lines().count()),
    }
}

fn first_line(node: &Node, source: &str) -> String {
    node_text(node, source)
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .to_string()
}

fn node_text<'a>(node: &Node, source: &'a str) -> &'a str {
    safe_slice(source, node.start_byte(), node.end_byte())
}
