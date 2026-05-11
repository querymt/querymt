use tree_sitter::{Node, Parser};

use super::super::types::SymbolDigest;
use super::safe_slice;
use crate::index::symbol_index::{SymbolEntry, SymbolError, SymbolKind};

pub fn extract(source: &str) -> Result<Vec<SymbolEntry>, SymbolError> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_java::LANGUAGE.into())
        .map_err(|e| SymbolError::ParseError(format!("Failed to set Java parser language: {e}")))?;

    let tree = parser
        .parse(source, None)
        .ok_or_else(|| SymbolError::ParseError("Failed to parse Java source".to_string()))?;

    let mut symbols = Vec::new();
    let mut cursor = tree.root_node().walk();
    for node in tree.root_node().named_children(&mut cursor) {
        collect_top_level_symbol(&node, source, None, &mut symbols);
    }

    Ok(symbols)
}

fn collect_top_level_symbol(
    node: &Node,
    source: &str,
    parent: Option<&str>,
    symbols: &mut Vec<SymbolEntry>,
) {
    match node.kind() {
        "package_declaration" | "import_declaration" => symbols.push(import_symbol(node, source)),
        "class_declaration" | "record_declaration" => {
            symbols.push(class_symbol(node, source, parent))
        }
        "interface_declaration" => symbols.push(interface_symbol(node, source, parent)),
        "enum_declaration" => symbols.push(enum_symbol(node, source, parent)),
        _ => {}
    }
}

fn import_symbol(node: &Node, source: &str) -> SymbolEntry {
    let text = node_text(node, source)
        .trim()
        .trim_end_matches(';')
        .to_string();
    symbol_entry(
        node,
        source,
        SymbolKind::Import,
        text.clone(),
        text,
        None,
        None,
        None,
        Vec::new(),
    )
}

fn class_symbol(node: &Node, source: &str, parent: Option<&str>) -> SymbolEntry {
    let name = identifier_name(node, source);
    let qualified_name = qualify(parent, &name);
    let members = extract_class_members(node, source, &qualified_name);

    container_symbol(
        node,
        source,
        parent,
        SymbolKind::Class,
        name,
        class_header(node, source),
        members,
    )
}

fn interface_symbol(node: &Node, source: &str, parent: Option<&str>) -> SymbolEntry {
    let name = identifier_name(node, source);
    let qualified_name = qualify(parent, &name);
    let members = extract_interface_members(node, source, &qualified_name);

    container_symbol(
        node,
        source,
        parent,
        SymbolKind::Interface,
        name,
        class_header(node, source),
        members,
    )
}

fn enum_symbol(node: &Node, source: &str, parent: Option<&str>) -> SymbolEntry {
    let name = identifier_name(node, source);
    let members = extract_enum_members(node, source, &qualify(parent, &name));

    container_symbol(
        node,
        source,
        parent,
        SymbolKind::Enum,
        name,
        class_header(node, source),
        members,
    )
}

fn container_symbol(
    node: &Node,
    source: &str,
    parent: Option<&str>,
    kind: SymbolKind,
    name: String,
    signature: String,
    children: Vec<SymbolEntry>,
) -> SymbolEntry {
    let body = find_child_by_kinds(node, &["class_body", "interface_body", "enum_body"]);
    let (body_start_line, body_end_line) = body
        .map(|body| {
            (
                Some(body.start_position().row + 1),
                Some(body.end_position().row + 1),
            )
        })
        .unwrap_or((None, None));

    symbol_entry(
        node,
        source,
        kind,
        name.clone(),
        signature,
        parent.map(|value| value.to_string()),
        body_start_line,
        body_end_line,
        children,
    )
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
    let start_byte = node.start_byte();
    let end_byte = node.end_byte();
    let bytes = safe_slice(source, start_byte, end_byte).as_bytes();
    let line_count = node.end_position().row - node.start_position().row + 1;
    let digest = SymbolDigest::new(bytes, line_count);
    let qualified_name = qualify(parent.as_deref(), &name);

    SymbolEntry {
        kind,
        name,
        qualified_name,
        signature,
        start_line: node.start_position().row + 1,
        end_line: node.end_position().row + 1,
        start_byte,
        end_byte,
        body_start_line,
        body_end_line,
        parent,
        children,
        digest,
    }
}

fn extract_class_members(node: &Node, source: &str, parent: &str) -> Vec<SymbolEntry> {
    let Some(body) = find_child_by_kind(node, "class_body") else {
        return Vec::new();
    };

    let mut members = Vec::new();
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        match child.kind() {
            "method_declaration" | "constructor_declaration" => {
                members.push(function_symbol(&child, source, parent, SymbolKind::Method));
            }
            "field_declaration" => {
                members.extend(field_symbols(&child, source, parent));
            }
            "class_declaration" | "record_declaration" => {
                members.push(class_symbol(&child, source, Some(parent)));
            }
            "interface_declaration" => {
                members.push(interface_symbol(&child, source, Some(parent)));
            }
            "enum_declaration" => {
                members.push(enum_symbol(&child, source, Some(parent)));
            }
            _ => {}
        }
    }
    members
}

fn extract_interface_members(node: &Node, source: &str, parent: &str) -> Vec<SymbolEntry> {
    let Some(body) = find_child_by_kind(node, "interface_body") else {
        return Vec::new();
    };

    let mut members = Vec::new();
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        match child.kind() {
            "method_declaration" => {
                members.push(function_symbol(&child, source, parent, SymbolKind::Method));
            }
            "constant_declaration" => {
                members.extend(field_symbols(&child, source, parent));
            }
            _ => {}
        }
    }
    members
}

fn extract_enum_members(node: &Node, source: &str, parent: &str) -> Vec<SymbolEntry> {
    let Some(body) = find_child_by_kind(node, "enum_body") else {
        return Vec::new();
    };

    let mut members = Vec::new();
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        if child.kind() == "enum_constant" {
            let name = find_child_by_kind(&child, "identifier")
                .map(|name| node_text(&name, source).to_string())
                .unwrap_or_else(|| node_text(&child, source).trim().to_string());
            members.push(symbol_entry(
                &child,
                source,
                SymbolKind::EnumVariant,
                name.clone(),
                name,
                Some(parent.to_string()),
                None,
                None,
                Vec::new(),
            ));
        }
    }
    members
}

fn function_symbol(node: &Node, source: &str, parent: &str, kind: SymbolKind) -> SymbolEntry {
    let name = identifier_name(node, source);
    let signature = method_signature(node, source);
    let body = find_child_by_kind(node, "block");
    let (body_start_line, body_end_line) = body
        .map(|body| {
            (
                Some(body.start_position().row + 1),
                Some(body.end_position().row + 1),
            )
        })
        .unwrap_or((None, None));

    symbol_entry(
        node,
        source,
        kind,
        name,
        signature,
        Some(parent.to_string()),
        body_start_line,
        body_end_line,
        Vec::new(),
    )
}

fn field_symbols(node: &Node, source: &str, parent: &str) -> Vec<SymbolEntry> {
    let mut symbols = Vec::new();
    let mut cursor = node.walk();
    for declarator in node.named_children(&mut cursor) {
        if declarator.kind() != "variable_declarator" {
            continue;
        }

        let name = find_child_by_kind(&declarator, "identifier")
            .map(|id| node_text(&id, source).to_string())
            .unwrap_or_else(|| node_text(&declarator, source).trim().to_string());
        symbols.push(symbol_entry(
            &declarator,
            source,
            SymbolKind::Field,
            name.clone(),
            name,
            Some(parent.to_string()),
            None,
            None,
            Vec::new(),
        ));
    }

    if symbols.is_empty() {
        let label = first_line_signature(node, source)
            .trim_end_matches(';')
            .to_string();
        symbols.push(symbol_entry(
            node,
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

    symbols
}

fn class_header(node: &Node, source: &str) -> String {
    let text = node_text(node, source);
    if let Some(brace_pos) = text.find('{') {
        text[..brace_pos].trim().to_string()
    } else {
        first_line_signature(node, source)
    }
}

fn method_signature(node: &Node, source: &str) -> String {
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
        text.trim().trim_end_matches(';').to_string()
    }
}

fn identifier_name(node: &Node, source: &str) -> String {
    find_child_by_kind(node, "identifier")
        .map(|id| node_text(&id, source).to_string())
        .unwrap_or_else(|| "<anonymous>".to_string())
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

fn find_child_by_kind<'a>(node: &'a Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find(|child| child.kind() == kind)
}

fn find_child_by_kinds<'a>(node: &'a Node<'a>, kinds: &[&str]) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find(|child| kinds.iter().any(|kind| child.kind() == *kind))
}
