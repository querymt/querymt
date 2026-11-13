use tree_sitter::{Node, Parser};

use super::super::types::SymbolDigest;
use super::safe_slice;
use crate::index::symbol_index::{SymbolEntry, SymbolError, SymbolKind};

pub fn extract(source: &str) -> Result<Vec<SymbolEntry>, SymbolError> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_json::LANGUAGE.into())
        .map_err(|e| SymbolError::ParseError(format!("Failed to load JSON grammar: {e}")))?;

    let tree = parser
        .parse(source, None)
        .ok_or_else(|| SymbolError::ParseError("Failed to parse JSON source".to_string()))?;

    let mut symbols = Vec::new();
    let root = tree.root_node();
    // `document` has no field names; its first named child is the root value.
    let mut cursor = root.walk();
    if let Some(container) = root.named_children(&mut cursor).next()
        && container.kind() == "object"
    {
        collect_pairs(&container, source, &mut symbols);
    }
    Ok(symbols)
}

fn collect_pairs(object: &Node, source: &str, symbols: &mut Vec<SymbolEntry>) {
    let mut cursor = object.walk();
    for pair in object.named_children(&mut cursor) {
        if pair.kind() != "pair" {
            continue;
        }
        let Some(key_node) = pair.child_by_field_name("key") else {
            continue;
        };
        let key = strip_quotes(node_text(&key_node, source));
        let value = pair.child_by_field_name("value");

        let children = match &value {
            Some(v) if v.kind() == "object" => {
                let mut nested = Vec::new();
                collect_pairs(v, source, &mut nested);
                nested
            }
            _ => Vec::new(),
        };

        let signature = match &value {
            Some(v) if v.kind() == "object" && !children.is_empty() => {
                format!("{key}: {{...}}")
            }
            Some(v) if v.kind() == "array" => {
                let items = v.named_child_count();
                format!("{key}: [{items} items]")
            }
            Some(v) => format!("{key}: {}", first_line(v, source)),
            None => key.clone(),
        };

        symbols.push(symbol_entry(
            &pair,
            source,
            SymbolKind::Const,
            key,
            signature,
            children,
        ));
    }
}

fn strip_quotes(text: &str) -> String {
    text.trim()
        .trim_start_matches('"')
        .trim_end_matches('"')
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
    let text = node_text(node, source);
    let line = text.lines().next().unwrap_or("").trim().to_string();
    if text.lines().count() > 1 {
        format!("{line}...")
    } else {
        line
    }
}

fn node_text<'a>(node: &Node, source: &'a str) -> &'a str {
    safe_slice(source, node.start_byte(), node.end_byte())
}
