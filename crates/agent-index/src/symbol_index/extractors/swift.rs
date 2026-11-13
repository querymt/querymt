use tree_sitter::{Node, Parser};

use super::super::types::SymbolDigest;
use super::safe_slice;
use crate::index::symbol_index::{SymbolEntry, SymbolError, SymbolKind};

pub fn extract(source: &str) -> Result<Vec<SymbolEntry>, SymbolError> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_swift::LANGUAGE.into())
        .map_err(|e| SymbolError::ParseError(format!("Failed to load Swift grammar: {e}")))?;

    let tree = parser
        .parse(source, None)
        .ok_or_else(|| SymbolError::ParseError("Failed to parse Swift source".to_string()))?;

    let mut symbols = Vec::new();
    let mut cursor = tree.root_node().walk();
    for child in tree.root_node().named_children(&mut cursor) {
        collect_top_level(&child, source, &mut symbols);
    }
    Ok(symbols)
}

fn collect_top_level(node: &Node, source: &str, symbols: &mut Vec<SymbolEntry>) {
    match node.kind() {
        "import_declaration" => {
            let text = node_text(node, source).trim().to_string();
            symbols.push(container_symbol(
                node,
                source,
                SymbolKind::Import,
                text.clone(),
                text,
            ));
        }
        "property_declaration" => {
            if let Some(name) = property_name(node, source) {
                symbols.push(container_symbol(
                    node,
                    source,
                    SymbolKind::Const,
                    name,
                    first_line(node, source),
                ));
            }
        }
        "class_declaration" => symbols.push(class_like_symbol(node, source, None)),
        "protocol_declaration" => symbols.push(protocol_symbol(node, source)),
        "typealias_declaration" => {
            let name = find_child_by_kind(node, "type_identifier")
                .map(|n| node_text(&n, source).to_string())
                .unwrap_or_else(|| "unknown".to_string());
            symbols.push(container_symbol(
                node,
                source,
                SymbolKind::TypeAlias,
                name,
                first_line(node, source),
            ));
        }
        "function_declaration" => {
            if let Some(name) = simple_name(node, source) {
                symbols.push(container_symbol(
                    node,
                    source,
                    SymbolKind::Function,
                    name,
                    first_line(node, source),
                ));
            }
        }
        _ => {}
    }
}

/// The Swift grammar parses `class`, `struct`, `enum`, `extension`, and `actor`
/// declarations all as `class_declaration`; disambiguate by leading keyword.
fn class_like_symbol(node: &Node, source: &str, parent: Option<&str>) -> SymbolEntry {
    let text = node_text(node, source);
    let kind = if text.trim_start().starts_with("struct") {
        SymbolKind::Struct
    } else if text.trim_start().starts_with("enum") {
        SymbolKind::Enum
    } else if text.trim_start().starts_with("extension") {
        SymbolKind::Impl
    } else {
        SymbolKind::Class
    };

    let name = type_name(node, source);
    let signature = first_line(node, source);

    let mut children = Vec::new();
    let body = find_child_by_kind(node, "class_body")
        .or_else(|| find_child_by_kind(node, "enum_class_body"));
    if let Some(body) = body {
        let mut cursor = body.walk();
        for child in body.named_children(&mut cursor) {
            collect_member(&child, source, &name, kind, &mut children);
        }
    }

    let qualified = qualify(parent, &name);
    let mut entry = container_symbol_with_children(node, source, kind, name, signature, children);
    entry.qualified_name = qualified;
    entry.parent = parent.map(str::to_string);
    entry
}

fn protocol_symbol(node: &Node, source: &str) -> SymbolEntry {
    let name = type_name(node, source);
    let signature = first_line(node, source);

    let mut children = Vec::new();
    if let Some(body) = find_child_by_kind(node, "protocol_body") {
        let mut cursor = body.walk();
        for child in body.named_children(&mut cursor) {
            match child.kind() {
                "protocol_function_declaration" => {
                    if let Some(member_name) = simple_name(&child, source) {
                        let mut entry = container_symbol(
                            &child,
                            source,
                            SymbolKind::Method,
                            member_name,
                            first_line(&child, source),
                        );
                        entry.qualified_name = format!("{name}::{}", entry.name);
                        entry.parent = Some(name.clone());
                        children.push(entry);
                    }
                }
                "protocol_property_declaration" => {
                    if let Some(member_name) = property_name(&child, source) {
                        children.push(container_symbol(
                            &child,
                            source,
                            SymbolKind::Field,
                            member_name,
                            first_line(&child, source),
                        ));
                    }
                }
                _ => {}
            }
        }
    }

    container_symbol_with_children(node, source, SymbolKind::Trait, name, signature, children)
}

fn collect_member(
    node: &Node,
    source: &str,
    parent: &str,
    container_kind: SymbolKind,
    children: &mut Vec<SymbolEntry>,
) {
    match node.kind() {
        "function_declaration" => {
            if let Some(name) = simple_name(node, source) {
                let mut entry = container_symbol(
                    node,
                    source,
                    SymbolKind::Method,
                    name,
                    first_line(node, source),
                );
                entry.qualified_name = format!("{parent}::{}", entry.name);
                entry.parent = Some(parent.to_string());
                children.push(entry);
            }
        }
        "property_declaration" => {
            let kind = if container_kind == SymbolKind::Enum {
                SymbolKind::EnumVariant
            } else {
                SymbolKind::Field
            };
            if let Some(name) = property_name(node, source) {
                children.push(container_symbol(
                    node,
                    source,
                    kind,
                    name,
                    first_line(node, source),
                ));
            }
        }
        "enum_entry" => {
            let signature = node_text(node, source)
                .trim()
                .trim_end_matches(',')
                .to_string();
            children.push(container_symbol(
                node,
                source,
                SymbolKind::EnumVariant,
                signature.clone(),
                signature,
            ));
        }
        "class_declaration" => children.push(class_like_symbol(node, source, Some(parent))),
        "typealias_declaration" => {
            let name = find_child_by_kind(node, "type_identifier")
                .map(|n| node_text(&n, source).to_string())
                .unwrap_or_else(|| "unknown".to_string());
            children.push(container_symbol(
                node,
                source,
                SymbolKind::TypeAlias,
                name,
                first_line(node, source),
            ));
        }
        _ => {}
    }
}

fn property_name(node: &Node, source: &str) -> Option<String> {
    let pattern = find_child_by_kind(node, "pattern")?;
    find_child_by_kind(&pattern, "simple_identifier").map(|n| node_text(&n, source).to_string())
}

fn simple_name(node: &Node, source: &str) -> Option<String> {
    find_child_by_kind(node, "simple_identifier").map(|n| node_text(&n, source).to_string())
}

fn type_name(node: &Node, source: &str) -> String {
    find_child_by_kind(node, "type_identifier")
        .map(|n| node_text(&n, source).to_string())
        .or_else(|| {
            find_child_by_kind(node, "user_type").map(|n| node_text(&n, source).to_string())
        })
        .unwrap_or_else(|| "unknown".to_string())
}

fn container_symbol(
    node: &Node,
    source: &str,
    kind: SymbolKind,
    name: String,
    signature: String,
) -> SymbolEntry {
    container_symbol_with_children(node, source, kind, name, signature, Vec::new())
}

fn container_symbol_with_children(
    node: &Node,
    source: &str,
    kind: SymbolKind,
    name: String,
    signature: String,
    children: Vec<SymbolEntry>,
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
        children,
        digest: SymbolDigest::new(text.as_bytes(), line_count),
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

fn qualify(parent: Option<&str>, name: &str) -> String {
    match parent {
        Some(parent) if !parent.is_empty() => format!("{parent}::{name}"),
        _ => name.to_string(),
    }
}

fn node_text<'a>(node: &Node, source: &'a str) -> &'a str {
    safe_slice(source, node.start_byte(), node.end_byte())
}

fn find_child_by_kind<'a>(node: &'a Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find(|&child| child.kind() == kind)
}
