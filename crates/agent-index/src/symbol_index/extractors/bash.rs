use tree_sitter::{Node, Parser};

use super::super::types::SymbolDigest;
use super::safe_slice;
use crate::index::symbol_index::{SymbolEntry, SymbolError, SymbolKind};

pub fn extract(source: &str) -> Result<Vec<SymbolEntry>, SymbolError> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_bash::LANGUAGE.into())
        .map_err(|e| SymbolError::ParseError(format!("Failed to load Bash grammar: {e}")))?;

    let tree = parser
        .parse(source, None)
        .ok_or_else(|| SymbolError::ParseError("Failed to parse Bash source".to_string()))?;

    let mut symbols = Vec::new();
    let mut cursor = tree.root_node().walk();
    for child in tree.root_node().named_children(&mut cursor) {
        match child.kind() {
            "function_definition" => {
                symbols.push(function_symbol(&child, source));
            }
            "variable_assignment" => {
                if let Some(entry) = assignment_symbol(&child, source) {
                    symbols.push(entry);
                }
            }
            "command" => {
                if let Some(entry) = source_command_symbol(&child, source) {
                    symbols.push(entry);
                }
            }
            _ => {}
        }
    }
    Ok(symbols)
}

fn function_symbol(node: &Node, source: &str) -> SymbolEntry {
    let name = find_child_by_kind(node, "word")
        .map(|n| node_text(&n, source).to_string())
        .unwrap_or_else(|| "<anonymous>".to_string());
    let kind = if is_test_name(&name) {
        SymbolKind::Test
    } else {
        SymbolKind::Function
    };
    // Signature: everything on the first line, e.g. `run_build() {` or `function deploy {`.
    let signature = first_line_signature(node, source);
    symbol_entry(node, source, kind, name, signature)
}

fn assignment_symbol(node: &Node, source: &str) -> Option<SymbolEntry> {
    let name =
        find_child_by_kind(node, "variable_name").map(|n| node_text(&n, source).to_string())?;
    Some(symbol_entry(
        node,
        source,
        SymbolKind::Const,
        name,
        first_line_signature(node, source),
    ))
}

fn source_command_symbol(node: &Node, source: &str) -> Option<SymbolEntry> {
    let command_name = find_child_by_kind(node, "command_name")?;
    let verb =
        find_child_by_kind(&command_name, "word").map(|n| node_text(&n, source).to_string())?;
    if verb != "source" && verb != "." {
        return None;
    }
    let mut cursor = node.walk();
    let target = node
        .named_children(&mut cursor)
        .find(|child| child.kind() == "word")
        .map(|n| node_text(&n, source).to_string())
        .unwrap_or_else(|| verb.clone());
    Some(symbol_entry(
        node,
        source,
        SymbolKind::Import,
        target,
        first_line_signature(node, source),
    ))
}

fn is_test_name(name: &str) -> bool {
    name.starts_with("test_") || name.ends_with("_test")
}

fn symbol_entry(
    node: &Node,
    source: &str,
    kind: SymbolKind,
    name: String,
    signature: String,
) -> SymbolEntry {
    let text = node_text(node, source);
    let line_count = text.lines().count();
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
        children: Vec::new(),
        digest: SymbolDigest::new(text.as_bytes(), line_count),
    }
}

fn first_line_signature(node: &Node, source: &str) -> String {
    node_text(node, source)
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .trim_end_matches('{')
        .trim()
        .to_string()
}

fn node_text<'a>(node: &Node, source: &'a str) -> &'a str {
    safe_slice(source, node.start_byte(), node.end_byte())
}

fn find_child_by_kind<'a>(node: &'a Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find(|&child| child.kind() == kind)
}
