use tree_sitter::{Node, Parser};

use super::super::types::SymbolDigest;
use crate::index::symbol_index::{SymbolEntry, SymbolError, SymbolKind};

pub fn extract(source: &str) -> Result<Vec<SymbolEntry>, SymbolError> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_ruby::LANGUAGE.into())
        .map_err(|e| SymbolError::ParseError(format!("Failed to set Ruby parser language: {e}")))?;

    let tree = parser
        .parse(source, None)
        .ok_or_else(|| SymbolError::ParseError("Failed to parse Ruby source".to_string()))?;

    let mut symbols = Vec::new();
    let mut cursor = tree.root_node().walk();
    for node in tree.root_node().named_children(&mut cursor) {
        collect_top_level_symbol(&node, source, &mut symbols);
    }

    Ok(symbols)
}

fn collect_top_level_symbol(node: &Node, source: &str, symbols: &mut Vec<SymbolEntry>) {
    match node.kind() {
        "call" => {
            let text = node_text(node, source);
            if text.starts_with("require") || text.starts_with("require_relative") {
                let label = first_line_signature(node, source);
                symbols.push(symbol_entry(
                    node,
                    source,
                    SymbolKind::Import,
                    label.clone(),
                    label,
                    None,
                    None,
                    None,
                    Vec::new(),
                ));
            } else if is_rspec_block(node, source) {
                let label = first_line_signature(node, source);
                symbols.push(symbol_entry(
                    node,
                    source,
                    SymbolKind::Test,
                    label.clone(),
                    label,
                    None,
                    None,
                    None,
                    Vec::new(),
                ));
            }
        }
        "class" => symbols.push(class_symbol(node, source, None)),
        "module" => symbols.push(module_symbol(node, source, None)),
        "method" => {
            let signature = method_signature(node, source);
            let name = find_child_by_kind(node, "identifier")
                .map(|n| node_text(&n, source).to_string())
                .unwrap_or_else(|| "unknown".to_string());
            let kind = if name.starts_with("test_") {
                SymbolKind::Test
            } else {
                SymbolKind::Function
            };
            symbols.push(symbol_entry(
                node,
                source,
                kind,
                name,
                signature,
                None,
                None,
                None,
                Vec::new(),
            ));
        }
        "singleton_method" => {
            let signature = first_line_signature(node, source);
            symbols.push(symbol_entry(
                node,
                source,
                SymbolKind::Function,
                signature.clone(),
                signature,
                None,
                None,
                None,
                Vec::new(),
            ));
        }
        "assignment" => {
            let text = node_text(node, source);
            let lhs = text.split('=').next().unwrap_or("").trim();
            if lhs.chars().next().is_some_and(|c| c.is_uppercase()) {
                let label = first_line_signature(node, source);
                symbols.push(symbol_entry(
                    node,
                    source,
                    SymbolKind::Const,
                    lhs.to_string(),
                    label,
                    None,
                    None,
                    None,
                    Vec::new(),
                ));
            }
        }
        _ => {}
    }
}

fn class_symbol(node: &Node, source: &str, parent: Option<&str>) -> SymbolEntry {
    let name = find_child_by_kind(node, "constant")
        .or_else(|| find_child_by_kind(node, "scope_resolution"))
        .map(|n| node_text(&n, source).to_string())
        .unwrap_or_else(|| "anonymous".to_string());
    let qualified_name = qualify(parent, &name);
    let children = extract_body_methods(node, source, &qualified_name);
    symbol_entry(
        node,
        source,
        SymbolKind::Class,
        name,
        class_signature(node, source),
        parent.map(str::to_string),
        None,
        None,
        children,
    )
}

fn module_symbol(node: &Node, source: &str, parent: Option<&str>) -> SymbolEntry {
    let name = find_child_by_kind(node, "constant")
        .or_else(|| find_child_by_kind(node, "scope_resolution"))
        .map(|n| node_text(&n, source).to_string())
        .unwrap_or_else(|| "anonymous".to_string());
    let qualified_name = qualify(parent, &name);
    let children = extract_body_methods(node, source, &qualified_name);
    symbol_entry(
        node,
        source,
        SymbolKind::Module,
        name,
        first_line_signature(node, source),
        parent.map(str::to_string),
        None,
        None,
        children,
    )
}

fn extract_body_methods(node: &Node, source: &str, parent: &str) -> Vec<SymbolEntry> {
    let Some(body) = find_child_by_kind(node, "body_statement") else {
        return Vec::new();
    };

    let mut symbols = Vec::new();
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        match child.kind() {
            "method" => {
                let name = find_child_by_kind(&child, "identifier")
                    .map(|n| node_text(&n, source).to_string())
                    .unwrap_or_else(|| "unknown".to_string());
                let kind = if name.starts_with("test_") {
                    SymbolKind::Test
                } else {
                    SymbolKind::Method
                };
                symbols.push(symbol_entry(
                    &child,
                    source,
                    kind,
                    name,
                    method_signature(&child, source),
                    Some(parent.to_string()),
                    None,
                    None,
                    Vec::new(),
                ));
            }
            "singleton_method" => {
                let signature = first_line_signature(&child, source);
                symbols.push(symbol_entry(
                    &child,
                    source,
                    SymbolKind::Method,
                    signature.clone(),
                    signature,
                    Some(parent.to_string()),
                    None,
                    None,
                    Vec::new(),
                ));
            }
            _ => {}
        }
    }

    symbols
}

fn method_signature(node: &Node, source: &str) -> String {
    let name = find_child_by_kind(node, "identifier")
        .map(|n| node_text(&n, source).to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let params = find_child_by_kind(node, "method_parameters")
        .map(|n| node_text(&n, source).to_string())
        .unwrap_or_default();
    if params.is_empty() {
        format!("def {name}")
    } else {
        format!("def {name}{params}")
    }
}

fn class_signature(node: &Node, source: &str) -> String {
    first_line_signature(node, source)
}

fn is_rspec_block(node: &Node, source: &str) -> bool {
    let text = node_text(node, source);
    text.starts_with("describe ")
        || text.starts_with("context ")
        || text.starts_with("it ")
        || text.starts_with("RSpec.describe")
}

#[allow(clippy::too_many_arguments)]
fn symbol_entry(
    node: &Node,
    source: &str,
    kind: SymbolKind,
    name: String,
    signature: String,
    parent: Option<String>,
    body_start_line: Option<usize>,
    body_end_line: Option<usize>,
    children: Vec<SymbolEntry>,
) -> SymbolEntry {
    let text = node_text(node, source);
    let line_count = text.lines().count();
    SymbolEntry {
        kind,
        qualified_name: qualify(parent.as_deref(), &name),
        name,
        signature,
        start_line: node.start_position().row + 1,
        end_line: node.end_position().row + 1,
        start_byte: node.start_byte(),
        end_byte: node.end_byte(),
        body_start_line,
        body_end_line,
        parent,
        children,
        digest: SymbolDigest::new(text.as_bytes(), line_count),
    }
}

fn first_line_signature(node: &Node, source: &str) -> String {
    node_text(node, source)
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .to_string()
}

fn qualify(parent: Option<&str>, name: &str) -> String {
    match parent {
        Some(parent) if !parent.is_empty() => format!("{parent}::{name}"),
        _ => name.to_string(),
    }
}

fn node_text<'a>(node: &Node, source: &'a str) -> &'a str {
    safe_slice(source, node.start_byte(), node.end_byte())
}

fn safe_slice(source: &str, from: usize, to: usize) -> &str {
    let from = source.floor_char_boundary(from.min(source.len()));
    let to = source.ceil_char_boundary(to.min(source.len()));
    &source[from..to]
}

fn find_child_by_kind<'a>(node: &'a Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find(|child| child.kind() == kind)
}
