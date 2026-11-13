use tree_sitter::{Node, Parser};

use super::super::types::SymbolDigest;
use super::safe_slice;
use crate::index::symbol_index::{SymbolEntry, SymbolError, SymbolKind};

pub fn extract(source: &str) -> Result<Vec<SymbolEntry>, SymbolError> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_c_sharp::LANGUAGE.into())
        .map_err(|e| SymbolError::ParseError(format!("Failed to set C# parser language: {e}")))?;

    let tree = parser
        .parse(source, None)
        .ok_or_else(|| SymbolError::ParseError("Failed to parse C# source".to_string()))?;

    let mut symbols = Vec::new();
    let mut cursor = tree.root_node().walk();
    for node in tree.root_node().named_children(&mut cursor) {
        collect_top_level_symbol(&node, source, &mut symbols);
    }

    Ok(symbols)
}

fn collect_top_level_symbol(node: &Node, source: &str, symbols: &mut Vec<SymbolEntry>) {
    match node.kind() {
        "using_directive" => {
            let text = node_text(node, source)
                .trim()
                .trim_end_matches(';')
                .to_string();
            symbols.push(symbol_entry(
                node,
                source,
                SymbolKind::Import,
                text.clone(),
                text,
                None,
                None,
                None,
                Vec::new(),
            ));
        }
        "namespace_declaration" | "file_scoped_namespace_declaration" => {
            symbols.push(namespace_symbol(node, source));
        }
        "class_declaration" | "record_declaration" | "struct_declaration" => {
            symbols.push(class_symbol(node, source, None));
        }
        "interface_declaration" => symbols.push(interface_symbol(node, source, None)),
        "enum_declaration" => symbols.push(enum_symbol(node, source, None)),
        _ => {}
    }
}

fn namespace_symbol(node: &Node, source: &str) -> SymbolEntry {
    let name = find_child_by_kind(node, "identifier")
        .or_else(|| find_child_by_kind(node, "qualified_name"))
        .map(|n| node_text(&n, source).to_string())
        .unwrap_or_else(|| "anonymous".to_string());

    let mut children = Vec::new();
    if let Some(body) = find_child_by_kind(node, "declaration_list") {
        let mut cursor = body.walk();
        for child in body.named_children(&mut cursor) {
            collect_namespace_member(&child, source, &name, &mut children);
        }
    } else {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            collect_namespace_member(&child, source, &name, &mut children);
        }
    }

    symbol_entry(
        node,
        source,
        SymbolKind::Module,
        name.clone(),
        format!("namespace {name}"),
        None,
        None,
        None,
        children,
    )
}

fn collect_namespace_member(
    node: &Node,
    source: &str,
    namespace_name: &str,
    out: &mut Vec<SymbolEntry>,
) {
    match node.kind() {
        "class_declaration" | "record_declaration" | "struct_declaration" => {
            out.push(class_symbol(node, source, Some(namespace_name)));
        }
        "interface_declaration" => out.push(interface_symbol(node, source, Some(namespace_name))),
        "enum_declaration" => out.push(enum_symbol(node, source, Some(namespace_name))),
        _ => {}
    }
}

fn class_symbol(node: &Node, source: &str, parent: Option<&str>) -> SymbolEntry {
    let name = identifier_name(node, source);
    let qualified_name = qualify(parent, &name);
    let children = extract_class_members(node, source, &qualified_name);
    symbol_entry(
        node,
        source,
        SymbolKind::Class,
        name,
        header_before_brace(node, source),
        parent.map(str::to_string),
        None,
        None,
        children,
    )
}

fn interface_symbol(node: &Node, source: &str, parent: Option<&str>) -> SymbolEntry {
    let name = identifier_name(node, source);
    let qualified_name = qualify(parent, &name);
    let children = extract_interface_members(node, source, &qualified_name);
    symbol_entry(
        node,
        source,
        SymbolKind::Interface,
        name,
        header_before_brace(node, source),
        parent.map(str::to_string),
        None,
        None,
        children,
    )
}

fn enum_symbol(node: &Node, source: &str, parent: Option<&str>) -> SymbolEntry {
    let name = identifier_name(node, source);
    let qualified_name = qualify(parent, &name);
    let children = extract_enum_members(node, source, &qualified_name);
    symbol_entry(
        node,
        source,
        SymbolKind::Enum,
        name,
        header_before_brace(node, source),
        parent.map(str::to_string),
        None,
        None,
        children,
    )
}

fn extract_class_members(node: &Node, source: &str, parent: &str) -> Vec<SymbolEntry> {
    let Some(body) = find_child_by_kind(node, "declaration_list") else {
        return Vec::new();
    };

    let mut members = Vec::new();
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        match child.kind() {
            "method_declaration" | "constructor_declaration" => {
                let name = identifier_name(&child, source);
                members.push(symbol_entry(
                    &child,
                    source,
                    SymbolKind::Method,
                    name,
                    header_before_brace(&child, source),
                    Some(parent.to_string()),
                    None,
                    None,
                    Vec::new(),
                ));
            }
            "property_declaration" => {
                let label = first_line_signature(&child, source);
                members.push(symbol_entry(
                    &child,
                    source,
                    SymbolKind::Field,
                    label.clone(),
                    label,
                    Some(parent.to_string()),
                    None,
                    None,
                    Vec::new(),
                ));
            }
            "field_declaration" | "event_field_declaration" => {
                let label = node_text(&child, source)
                    .trim()
                    .trim_end_matches(';')
                    .to_string();
                members.push(symbol_entry(
                    &child,
                    source,
                    SymbolKind::Field,
                    label.clone(),
                    label,
                    Some(parent.to_string()),
                    None,
                    None,
                    Vec::new(),
                ));
            }
            "class_declaration" | "record_declaration" | "struct_declaration" => {
                members.push(class_symbol(&child, source, Some(parent)));
            }
            "interface_declaration" => members.push(interface_symbol(&child, source, Some(parent))),
            "enum_declaration" => members.push(enum_symbol(&child, source, Some(parent))),
            _ => {}
        }
    }
    members
}

fn extract_interface_members(node: &Node, source: &str, parent: &str) -> Vec<SymbolEntry> {
    let Some(body) = find_child_by_kind(node, "declaration_list") else {
        return Vec::new();
    };

    let mut members = Vec::new();
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        match child.kind() {
            "method_declaration" => {
                let name = identifier_name(&child, source);
                members.push(symbol_entry(
                    &child,
                    source,
                    SymbolKind::Method,
                    name,
                    header_before_brace(&child, source),
                    Some(parent.to_string()),
                    None,
                    None,
                    Vec::new(),
                ));
            }
            "property_declaration" => {
                let label = first_line_signature(&child, source);
                members.push(symbol_entry(
                    &child,
                    source,
                    SymbolKind::Field,
                    label.clone(),
                    label,
                    Some(parent.to_string()),
                    None,
                    None,
                    Vec::new(),
                ));
            }
            _ => {}
        }
    }
    members
}

fn extract_enum_members(node: &Node, source: &str, parent: &str) -> Vec<SymbolEntry> {
    let Some(body) = find_child_by_kind(node, "enum_member_declaration_list") else {
        return Vec::new();
    };

    let mut members = Vec::new();
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        if child.kind() == "enum_member_declaration" {
            let label = node_text(&child, source)
                .trim()
                .trim_end_matches(',')
                .to_string();
            members.push(symbol_entry(
                &child,
                source,
                SymbolKind::EnumVariant,
                label.clone(),
                label,
                Some(parent.to_string()),
                None,
                None,
                Vec::new(),
            ));
        }
    }
    members
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

fn identifier_name(node: &Node, source: &str) -> String {
    find_child_by_kind(node, "identifier")
        .map(|n| node_text(&n, source).to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

fn first_line_signature(node: &Node, source: &str) -> String {
    node_text(node, source)
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .to_string()
}

fn header_before_brace(node: &Node, source: &str) -> String {
    let text = node_text(node, source);
    if let Some(brace_pos) = text.find('{') {
        text[..brace_pos]
            .lines()
            .map(str::trim)
            .collect::<Vec<_>>()
            .join(" ")
            .trim()
            .to_string()
    } else {
        first_line_signature(node, source)
    }
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
        .find(|child| child.kind() == kind)
}
