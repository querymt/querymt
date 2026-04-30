use tree_sitter::{Node, Parser};

use super::super::types::SymbolDigest;
use crate::index::symbol_index::{SymbolEntry, SymbolError, SymbolKind};

pub fn extract(source: &str) -> Result<Vec<SymbolEntry>, SymbolError> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_go::LANGUAGE.into())
        .map_err(|e| SymbolError::ParseError(format!("Failed to set Go parser language: {e}")))?;

    let tree = parser
        .parse(source, None)
        .ok_or_else(|| SymbolError::ParseError("Failed to parse Go source".to_string()))?;

    let mut symbols = Vec::new();
    let mut cursor = tree.root_node().walk();
    for node in tree.root_node().named_children(&mut cursor) {
        collect_top_level_symbol(&node, source, &mut symbols);
    }

    Ok(symbols)
}

fn collect_top_level_symbol(node: &Node, source: &str, symbols: &mut Vec<SymbolEntry>) {
    match node.kind() {
        "import_declaration" => {
            let text = node_text(node, source)
                .lines()
                .map(str::trim)
                .collect::<Vec<_>>()
                .join(" ");
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
        "type_declaration" => {
            let mut inner = node.walk();
            for child in node.named_children(&mut inner) {
                if child.kind() == "type_spec" {
                    symbols.push(type_symbol(&child, source));
                }
            }
        }
        "function_declaration" => {
            let name = find_child_by_kind(node, "identifier")
                .map(|n| node_text(&n, source).to_string())
                .unwrap_or_else(|| "unknown".to_string());
            let signature = function_signature(node, source);
            let kind = if is_test_name(&name) {
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
        "method_declaration" => {
            let name = find_child_by_kind(node, "field_identifier")
                .map(|n| node_text(&n, source).to_string())
                .unwrap_or_else(|| "unknown".to_string());
            let signature = method_signature(node, source);
            let kind = if is_test_name(&name) {
                SymbolKind::Test
            } else {
                SymbolKind::Method
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
        "const_declaration" | "var_declaration" => {
            let text = node_text(node, source)
                .lines()
                .map(str::trim)
                .collect::<Vec<_>>()
                .join(" ");
            symbols.push(symbol_entry(
                node,
                source,
                SymbolKind::Const,
                text.clone(),
                text,
                None,
                None,
                None,
                Vec::new(),
            ));
        }
        _ => {}
    }
}

fn type_symbol(node: &Node, source: &str) -> SymbolEntry {
    let name = find_child_by_kind(node, "type_identifier")
        .map(|n| node_text(&n, source).to_string())
        .unwrap_or_else(|| "unknown".to_string());

    if let Some(struct_type) = find_child_by_kind(node, "struct_type") {
        let children = extract_struct_fields(&struct_type, source, &name);
        return symbol_entry(
            node,
            source,
            SymbolKind::Struct,
            name.clone(),
            format!("type {name} struct"),
            None,
            None,
            None,
            children,
        );
    }

    if let Some(interface_type) = find_child_by_kind(node, "interface_type") {
        let children = extract_interface_methods(&interface_type, source, &name);
        return symbol_entry(
            node,
            source,
            SymbolKind::Interface,
            name.clone(),
            format!("type {name} interface"),
            None,
            None,
            None,
            children,
        );
    }

    symbol_entry(
        node,
        source,
        SymbolKind::TypeAlias,
        name,
        first_line_signature(node, source),
        None,
        None,
        None,
        Vec::new(),
    )
}

fn extract_struct_fields(node: &Node, source: &str, parent: &str) -> Vec<SymbolEntry> {
    let Some(body) = find_child_by_kind(node, "field_declaration_list") else {
        return Vec::new();
    };

    let mut fields = Vec::new();
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        if child.kind() == "field_declaration" {
            let label = node_text(&child, source).trim().to_string();
            fields.push(symbol_entry(
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
    }
    fields
}

fn extract_interface_methods(node: &Node, source: &str, parent: &str) -> Vec<SymbolEntry> {
    let mut methods = Vec::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "method_spec" => {
                let name = find_child_by_kind(&child, "field_identifier")
                    .or_else(|| find_child_by_kind(&child, "identifier"))
                    .map(|n| node_text(&n, source).to_string())
                    .unwrap_or_else(|| node_text(&child, source).trim().to_string());
                methods.push(symbol_entry(
                    &child,
                    source,
                    SymbolKind::Method,
                    name,
                    node_text(&child, source).trim().to_string(),
                    Some(parent.to_string()),
                    None,
                    None,
                    Vec::new(),
                ));
            }
            "type_identifier" => {
                let label = node_text(&child, source).trim().to_string();
                methods.push(symbol_entry(
                    &child,
                    source,
                    SymbolKind::TypeAlias,
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
    methods
}

fn function_signature(node: &Node, source: &str) -> String {
    let name = find_child_by_kind(node, "identifier")
        .map(|n| node_text(&n, source).to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let params = find_child_by_kind(node, "parameter_list")
        .map(|n| compact_node_text(&n, source))
        .unwrap_or_else(|| "()".to_string());
    let result = result_type(node, source);
    if result.is_empty() {
        format!("func {name}{params}")
    } else {
        format!("func {name}{params} {result}")
    }
}

fn method_signature(node: &Node, source: &str) -> String {
    first_line_signature(node, source)
}

fn result_type(node: &Node, source: &str) -> String {
    find_child_by_kind(node, "result")
        .map(|n| compact_node_text(&n, source))
        .unwrap_or_default()
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

fn compact_node_text(node: &Node, source: &str) -> String {
    node_text(node, source)
        .lines()
        .map(str::trim)
        .collect::<Vec<_>>()
        .join(" ")
}

fn first_line_signature(node: &Node, source: &str) -> String {
    node_text(node, source)
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .to_string()
}

fn is_test_name(name: &str) -> bool {
    name.starts_with("Test")
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
