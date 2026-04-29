use tree_sitter::{Node, Parser};

use crate::anchors::symbol_cache::SymbolDigest;
use crate::index::symbol_index::{SymbolEntry, SymbolError, SymbolKind};

pub fn extract_c(source: &str) -> Result<Vec<SymbolEntry>, SymbolError> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_c::LANGUAGE.into())
        .map_err(|e| SymbolError::ParseError(format!("Failed to set C parser language: {e}")))?;
    extract_common(&mut parser, source)
}

pub fn extract_cpp(source: &str) -> Result<Vec<SymbolEntry>, SymbolError> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_cpp::LANGUAGE.into())
        .map_err(|e| SymbolError::ParseError(format!("Failed to set C++ parser language: {e}")))?;
    extract_common(&mut parser, source)
}

fn extract_common(parser: &mut Parser, source: &str) -> Result<Vec<SymbolEntry>, SymbolError> {
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| SymbolError::ParseError("Failed to parse C/C++ source".to_string()))?;

    let mut symbols = Vec::new();
    let mut cursor = tree.root_node().walk();
    for node in tree.root_node().named_children(&mut cursor) {
        collect_top_level_symbol(&node, source, &mut symbols);
    }

    Ok(symbols)
}

fn collect_top_level_symbol(node: &Node, source: &str, out: &mut Vec<SymbolEntry>) {
    match node.kind() {
        "preproc_include" => {
            let text = node_text(node, source).trim().to_string();
            out.push(symbol_entry(
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
        "preproc_def" | "preproc_function_def" => {
            let text = first_line_signature(node, source);
            out.push(symbol_entry(
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
        "struct_specifier" | "union_specifier" => {
            if let Some(symbol) = struct_or_union_symbol(node, source) {
                out.push(symbol);
            }
        }
        "enum_specifier" => {
            if let Some(symbol) = enum_symbol(node, source) {
                out.push(symbol);
            }
        }
        "type_definition" => {
            let text = first_line_signature(node, source);
            out.push(symbol_entry(
                node,
                source,
                SymbolKind::TypeAlias,
                text.clone(),
                text,
                None,
                None,
                None,
                Vec::new(),
            ));
        }
        "function_definition" => {
            let sig = c_fn_signature(node, source);
            out.push(symbol_entry(
                node,
                source,
                SymbolKind::Function,
                sig.clone(),
                sig,
                None,
                None,
                None,
                Vec::new(),
            ));
        }
        "declaration" => {
            let text = node_text(node, source);
            if text.contains('(') {
                let sig = text
                    .trim()
                    .trim_end_matches(';')
                    .lines()
                    .map(str::trim)
                    .collect::<Vec<_>>()
                    .join(" ");
                out.push(symbol_entry(
                    node,
                    source,
                    SymbolKind::Function,
                    sig.clone(),
                    sig,
                    None,
                    None,
                    None,
                    Vec::new(),
                ));
            } else {
                let label = first_line_signature(node, source)
                    .trim_end_matches(';')
                    .to_string();
                out.push(symbol_entry(
                    node,
                    source,
                    SymbolKind::Const,
                    label.clone(),
                    label,
                    None,
                    None,
                    None,
                    Vec::new(),
                ));
            }
        }
        "class_specifier" => {
            if let Some(symbol) = cpp_class_symbol(node, source) {
                out.push(symbol);
            }
        }
        "namespace_definition" => {
            let name = find_child_by_kind(node, "identifier")
                .or_else(|| find_child_by_kind(node, "namespace_identifier"))
                .map(|n| node_text(&n, source).to_string())
                .unwrap_or_else(|| "anonymous".to_string());
            out.push(symbol_entry(
                node,
                source,
                SymbolKind::Module,
                name.clone(),
                format!("namespace {name}"),
                None,
                None,
                None,
                Vec::new(),
            ));
        }
        "template_declaration" => {
            let label = first_line_signature(node, source);
            out.push(symbol_entry(
                node,
                source,
                SymbolKind::TypeAlias,
                label.clone(),
                label,
                None,
                None,
                None,
                Vec::new(),
            ));
        }
        _ => {}
    }
}

fn struct_or_union_symbol(node: &Node, source: &str) -> Option<SymbolEntry> {
    let keyword = if node.kind() == "union_specifier" {
        "union"
    } else {
        "struct"
    };
    let name = find_child_by_kind(node, "type_identifier")
        .map(|n| node_text(&n, source).to_string())
        .unwrap_or_else(|| "anonymous".to_string());
    let kind = if keyword == "struct" {
        SymbolKind::Struct
    } else {
        SymbolKind::TypeAlias
    };
    let children = extract_struct_fields(node, source, &name);
    Some(symbol_entry(
        node,
        source,
        kind,
        name.clone(),
        format!("{keyword} {name}"),
        None,
        None,
        None,
        children,
    ))
}

fn extract_struct_fields(node: &Node, source: &str, parent: &str) -> Vec<SymbolEntry> {
    let Some(body) = find_child_by_kind(node, "field_declaration_list") else {
        return Vec::new();
    };
    let mut fields = Vec::new();
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        if child.kind() == "field_declaration" {
            let label = node_text(&child, source)
                .trim()
                .trim_end_matches(';')
                .to_string();
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

fn enum_symbol(node: &Node, source: &str) -> Option<SymbolEntry> {
    let name = find_child_by_kind(node, "type_identifier")
        .map(|n| node_text(&n, source).to_string())
        .unwrap_or_else(|| "anonymous".to_string());
    let mut children = Vec::new();
    if let Some(body) = find_child_by_kind(node, "enumerator_list") {
        let mut cursor = body.walk();
        for child in body.named_children(&mut cursor) {
            if child.kind() == "enumerator" {
                let label = node_text(&child, source).trim().to_string();
                children.push(symbol_entry(
                    &child,
                    source,
                    SymbolKind::EnumVariant,
                    label.clone(),
                    label,
                    Some(name.clone()),
                    None,
                    None,
                    Vec::new(),
                ));
            }
        }
    }
    Some(symbol_entry(
        node,
        source,
        SymbolKind::Enum,
        name.clone(),
        format!("enum {name}"),
        None,
        None,
        None,
        children,
    ))
}

fn cpp_class_symbol(node: &Node, source: &str) -> Option<SymbolEntry> {
    let name = find_child_by_kind(node, "type_identifier")
        .map(|n| node_text(&n, source).to_string())
        .unwrap_or_else(|| "anonymous".to_string());
    let mut children = Vec::new();
    if let Some(body) = find_child_by_kind(node, "field_declaration_list") {
        let mut cursor = body.walk();
        for child in body.named_children(&mut cursor) {
            match child.kind() {
                "function_definition" => {
                    let sig = c_fn_signature(&child, source);
                    children.push(symbol_entry(
                        &child,
                        source,
                        SymbolKind::Method,
                        sig.clone(),
                        sig,
                        Some(name.clone()),
                        None,
                        None,
                        Vec::new(),
                    ));
                }
                "declaration" | "field_declaration" => {
                    let label = node_text(&child, source)
                        .trim()
                        .trim_end_matches(';')
                        .to_string();
                    children.push(symbol_entry(
                        &child,
                        source,
                        SymbolKind::Field,
                        label.clone(),
                        label,
                        Some(name.clone()),
                        None,
                        None,
                        Vec::new(),
                    ));
                }
                "access_specifier" => {
                    let label = node_text(&child, source).trim().to_string();
                    children.push(symbol_entry(
                        &child,
                        source,
                        SymbolKind::Unknown,
                        label.clone(),
                        label,
                        Some(name.clone()),
                        None,
                        None,
                        Vec::new(),
                    ));
                }
                _ => {}
            }
        }
    }
    Some(symbol_entry(
        node,
        source,
        SymbolKind::Class,
        name.clone(),
        format!("class {name}"),
        None,
        None,
        None,
        children,
    ))
}

fn c_fn_signature(node: &Node, source: &str) -> String {
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
