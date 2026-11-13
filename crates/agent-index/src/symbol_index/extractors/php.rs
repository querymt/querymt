use tree_sitter::{Node, Parser};

use super::super::types::SymbolDigest;
use super::safe_slice;
use crate::index::symbol_index::{SymbolEntry, SymbolError, SymbolKind};

pub fn extract(source: &str) -> Result<Vec<SymbolEntry>, SymbolError> {
    let mut parser = Parser::new();
    // PHP-only grammar: typical `.php` code files, no HTML interleaving.
    parser
        .set_language(&tree_sitter_php::LANGUAGE_PHP_ONLY.into())
        .map_err(|e| SymbolError::ParseError(format!("Failed to load PHP grammar: {e}")))?;

    let tree = parser
        .parse(source, None)
        .ok_or_else(|| SymbolError::ParseError("Failed to parse PHP source".to_string()))?;

    let mut symbols = Vec::new();
    let mut cursor = tree.root_node().walk();
    for child in tree.root_node().named_children(&mut cursor) {
        collect_top_level(&child, source, &mut symbols);
    }
    Ok(symbols)
}

fn collect_top_level(node: &Node, source: &str, symbols: &mut Vec<SymbolEntry>) {
    match node.kind() {
        "namespace_definition" => symbols.push(namespace_symbol(node, source)),
        "use_declaration" | "namespace_use_declaration" => {
            let text = first_line(node, source);
            symbols.push(container_symbol(
                node,
                source,
                SymbolKind::Import,
                text.clone(),
                text,
            ));
        }
        "const_declaration" => collect_const_declaration(node, source, None, symbols),
        "class_declaration" => {
            symbols.push(class_like_symbol(node, source, None, SymbolKind::Class))
        }
        "interface_declaration" => {
            symbols.push(class_like_symbol(node, source, None, SymbolKind::Interface))
        }
        "trait_declaration" => {
            symbols.push(class_like_symbol(node, source, None, SymbolKind::Trait))
        }
        "enum_declaration" => symbols.push(enum_symbol(node, source, None)),
        "function_definition" => symbols.push(function_symbol(node, source, None)),
        _ => {}
    }
}

fn namespace_symbol(node: &Node, source: &str) -> SymbolEntry {
    let name = find_child_by_kind(node, "namespace_name")
        .map(|n| node_text(&n, source).to_string())
        .unwrap_or_else(|| "anonymous".to_string());
    let signature = format!("namespace {name}");

    let mut children = Vec::new();
    if let Some(body) = find_child_by_kind(node, "declaration_list") {
        let mut cursor = body.walk();
        for child in body.named_children(&mut cursor) {
            collect_namespace_member(&child, source, &name, &mut children);
        }
    }

    container_symbol_with_children(node, source, SymbolKind::Module, name, signature, children)
}

fn collect_namespace_member(
    node: &Node,
    source: &str,
    parent: &str,
    children: &mut Vec<SymbolEntry>,
) {
    match node.kind() {
        "use_declaration" | "namespace_use_declaration" => {
            let text = node_text(node, source)
                .trim()
                .trim_end_matches(';')
                .to_string();
            children.push(container_symbol(
                node,
                source,
                SymbolKind::Import,
                text.clone(),
                text,
            ));
        }
        "const_declaration" => collect_const_declaration(node, source, Some(parent), children),
        "class_declaration" => children.push(class_like_symbol(
            node,
            source,
            Some(parent),
            SymbolKind::Class,
        )),
        "interface_declaration" => children.push(class_like_symbol(
            node,
            source,
            Some(parent),
            SymbolKind::Interface,
        )),
        "trait_declaration" => children.push(class_like_symbol(
            node,
            source,
            Some(parent),
            SymbolKind::Trait,
        )),
        "enum_declaration" => children.push(enum_symbol(node, source, Some(parent))),
        "function_definition" => children.push(function_symbol(node, source, Some(parent))),
        _ => {}
    }
}

fn class_like_symbol(
    node: &Node,
    source: &str,
    parent: Option<&str>,
    kind: SymbolKind,
) -> SymbolEntry {
    let name = type_name(node, source);
    let signature = first_line(node, source);

    let mut children = Vec::new();
    if let Some(body) = find_child_by_kind(node, "declaration_list") {
        let mut cursor = body.walk();
        for child in body.named_children(&mut cursor) {
            match child.kind() {
                "method_declaration" => children.push(method_symbol(&child, source, &name)),
                "const_declaration" => {
                    collect_const_declaration(&child, source, Some(&name), &mut children)
                }
                _ => {}
            }
        }
    }

    let qualified = qualify(parent, &name);
    let mut entry = container_symbol_with_children(node, source, kind, name, signature, children);
    entry.qualified_name = qualified;
    entry.parent = parent.map(str::to_string);
    entry
}

fn enum_symbol(node: &Node, source: &str, parent: Option<&str>) -> SymbolEntry {
    let name = type_name(node, source);
    let signature = first_line(node, source);

    let mut children = Vec::new();
    if let Some(body) = find_child_by_kind(node, "enum_declaration_list") {
        let mut cursor = body.walk();
        for child in body.named_children(&mut cursor) {
            if child.kind() == "enum_case" {
                children.push(container_symbol(
                    &child,
                    source,
                    SymbolKind::EnumVariant,
                    type_name(&child, source),
                    node_text(&child, source)
                        .trim()
                        .trim_end_matches([',', ';'])
                        .to_string(),
                ));
            }
        }
    }

    let qualified = qualify(parent, &name);
    let mut entry =
        container_symbol_with_children(node, source, SymbolKind::Enum, name, signature, children);
    entry.qualified_name = qualified;
    entry.parent = parent.map(str::to_string);
    entry
}

fn function_symbol(node: &Node, source: &str, parent: Option<&str>) -> SymbolEntry {
    let name = type_name(node, source);
    let signature = first_line(node, source);
    let qualified = qualify(parent, &name);
    let mut entry = container_symbol(node, source, SymbolKind::Function, name, signature);
    entry.qualified_name = qualified;
    entry.parent = parent.map(str::to_string);
    entry
}

fn method_symbol(node: &Node, source: &str, parent: &str) -> SymbolEntry {
    let name = type_name(node, source);
    let signature = first_line(node, source);
    let qualified = format!("{parent}::{name}");
    let mut entry = container_symbol(node, source, SymbolKind::Method, name, signature);
    entry.qualified_name = qualified;
    entry.parent = Some(parent.to_string());
    entry
}

fn collect_const_declaration(
    node: &Node,
    source: &str,
    parent: Option<&str>,
    symbols: &mut Vec<SymbolEntry>,
) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() != "const_element" {
            continue;
        }
        let name = type_name(&child, source);
        let qualified = qualify(parent, &name);
        let signature = node_text(&child, source)
            .trim()
            .trim_end_matches(';')
            .to_string();
        let mut entry = container_symbol(&child, source, SymbolKind::Const, name, signature);
        entry.qualified_name = qualified;
        entry.parent = parent.map(str::to_string);
        symbols.push(entry);
    }
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
        .trim_end_matches('{')
        .trim()
        .to_string()
}

fn qualify(parent: Option<&str>, name: &str) -> String {
    match parent {
        Some(parent) if !parent.is_empty() => format!("{parent}::{name}"),
        _ => name.to_string(),
    }
}

fn type_name(node: &Node, source: &str) -> String {
    find_child_by_kind(node, "name")
        .map(|n| node_text(&n, source).to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

fn node_text<'a>(node: &Node, source: &'a str) -> &'a str {
    safe_slice(source, node.start_byte(), node.end_byte())
}

fn find_child_by_kind<'a>(node: &'a Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find(|&child| child.kind() == kind)
}
