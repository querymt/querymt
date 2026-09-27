use tree_sitter::{Node, Parser};

use super::super::types::SymbolDigest;
use super::safe_slice;
use crate::index::symbol_index::{SymbolEntry, SymbolError, SymbolKind};

pub fn extract(source: &str) -> Result<Vec<SymbolEntry>, SymbolError> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_kotlin::LANGUAGE.into())
        .map_err(|e| SymbolError::ParseError(format!("Failed to load Kotlin grammar: {e}")))?;

    let tree = parser
        .parse(source, None)
        .ok_or_else(|| SymbolError::ParseError("Failed to parse Kotlin source".to_string()))?;

    let mut symbols = Vec::new();
    let mut cursor = tree.root_node().walk();
    for child in tree.root_node().named_children(&mut cursor) {
        collect_top_level(&child, source, &mut symbols);
    }
    Ok(symbols)
}

fn collect_top_level(node: &Node, source: &str, symbols: &mut Vec<SymbolEntry>) {
    match node.kind() {
        "import_list" => {
            let mut cursor = node.walk();
            for import in node.named_children(&mut cursor) {
                if import.kind() == "import_header" {
                    let text = node_text(&import, source).trim().to_string();
                    symbols.push(container_symbol(
                        &import,
                        source,
                        SymbolKind::Import,
                        text.clone(),
                        text,
                    ));
                }
            }
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
        "type_alias" => {
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
        "class_declaration" => symbols.push(class_like_symbol(node, source, None)),
        "object_declaration" => symbols.push(object_symbol(node, source)),
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

fn class_like_symbol(node: &Node, source: &str, parent: Option<&str>) -> SymbolEntry {
    // `class_declaration` also parses `interface` and `enum class`; disambiguate
    // by the leading keyword.
    let text = node_text(node, source);
    let kind = if text.trim_start().starts_with("interface") {
        SymbolKind::Interface
    } else if text.trim_start().starts_with("enum") {
        SymbolKind::Enum
    } else {
        SymbolKind::Class
    };

    let name = find_child_by_kind(node, "type_identifier")
        .map(|n| node_text(&n, source).to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let signature = first_line(node, source);

    let mut children = Vec::new();
    let body = find_child_by_kind(node, "class_body")
        .or_else(|| find_child_by_kind(node, "enum_class_body"));
    if let Some(body) = body {
        let mut cursor = body.walk();
        for child in body.named_children(&mut cursor) {
            collect_class_member(&child, source, &name, kind, &mut children);
        }
    }

    let qualified = qualify(parent, &name);
    let mut entry = container_symbol_with_children(node, source, kind, name, signature, children);
    entry.qualified_name = qualified;
    entry.parent = parent.map(str::to_string);
    entry
}

fn object_symbol(node: &Node, source: &str) -> SymbolEntry {
    let name = find_child_by_kind(node, "type_identifier")
        .map(|n| node_text(&n, source).to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let signature = first_line(node, source);

    let mut children = Vec::new();
    if let Some(body) = find_child_by_kind(node, "class_body") {
        let mut cursor = body.walk();
        for child in body.named_children(&mut cursor) {
            collect_class_member(&child, source, &name, SymbolKind::Class, &mut children);
        }
    }

    container_symbol_with_children(node, source, SymbolKind::Class, name, signature, children)
}

fn collect_class_member(
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
            if let Some(name) = simple_name(node, source) {
                children.push(container_symbol(
                    node,
                    source,
                    SymbolKind::EnumVariant,
                    name,
                    node_text(node, source).trim().to_string(),
                ));
            }
        }
        "class_declaration" | "object_declaration" => {
            children.push(class_like_symbol(node, source, Some(parent)))
        }
        _ => {}
    }
}

fn property_name(node: &Node, source: &str) -> Option<String> {
    let declaration = find_child_by_kind(node, "variable_declaration")?;
    find_child_by_kind(&declaration, "simple_identifier").map(|n| node_text(&n, source).to_string())
}

fn simple_name(node: &Node, source: &str) -> Option<String> {
    find_child_by_kind(node, "simple_identifier").map(|n| node_text(&n, source).to_string())
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
