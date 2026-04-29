use tree_sitter::{Node, Parser};

use crate::anchors::symbol_cache::SymbolDigest;
use crate::index::symbol_index::{SymbolEntry, SymbolError, SymbolKind};

pub fn extract(source: &str) -> Result<Vec<SymbolEntry>, SymbolError> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_rust::LANGUAGE.into())
        .map_err(|e| SymbolError::ParseError(format!("Failed to set Rust language: {e}")))?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| SymbolError::ParseError("tree-sitter parse returned None".into()))?;

    let root = tree.root_node();
    let mut symbols = Vec::new();
    let mut cursor = root.walk();
    for node in root.named_children(&mut cursor) {
        if let Some(symbol) = extract_top_level_symbol(&node, source, None) {
            symbols.push(symbol);
        }
    }

    Ok(symbols)
}

fn extract_top_level_symbol(
    node: &Node,
    source: &str,
    parent: Option<&str>,
) -> Option<SymbolEntry> {
    match node.kind() {
        "use_declaration" => Some(container_symbol(
            node,
            source,
            parent,
            SymbolKind::Import,
            first_line_signature(node, source)
                .trim_end_matches(';')
                .to_string(),
            first_line_signature(node, source)
                .trim_end_matches(';')
                .to_string(),
            Vec::new(),
        )),
        "function_item" => Some(function_symbol(
            node,
            source,
            parent,
            if is_test_function(node, source) {
                SymbolKind::Test
            } else {
                SymbolKind::Function
            },
        )),
        "struct_item" => Some(container_symbol(
            node,
            source,
            parent,
            SymbolKind::Struct,
            type_identifier_name(node, source),
            first_line_signature(node, source),
            extract_struct_fields(node, source),
        )),
        "enum_item" => Some(container_symbol(
            node,
            source,
            parent,
            SymbolKind::Enum,
            type_identifier_name(node, source),
            first_line_signature(node, source),
            extract_enum_variants(node, source),
        )),
        "trait_item" => {
            let name = type_identifier_name(node, source);
            let qualified_name = qualify(parent, &name);
            let children = extract_trait_children(node, source, &qualified_name);
            Some(container_symbol(
                node,
                source,
                parent,
                SymbolKind::Trait,
                name,
                first_line_signature(node, source),
                children,
            ))
        }
        "impl_item" => {
            let name = extract_impl_name(node, source);
            let qualified_name = qualify(parent, &name);
            let children = extract_impl_children(node, source, &qualified_name);
            Some(container_symbol(
                node,
                source,
                parent,
                SymbolKind::Impl,
                name,
                impl_signature(node, source),
                children,
            ))
        }
        "type_item" => Some(container_symbol(
            node,
            source,
            parent,
            SymbolKind::TypeAlias,
            type_identifier_name(node, source),
            first_line_signature(node, source),
            Vec::new(),
        )),
        "const_item" => Some(container_symbol(
            node,
            source,
            parent,
            SymbolKind::Const,
            identifier_name(node, source),
            first_line_signature(node, source),
            Vec::new(),
        )),
        "static_item" => Some(container_symbol(
            node,
            source,
            parent,
            SymbolKind::Static,
            identifier_name(node, source),
            first_line_signature(node, source),
            Vec::new(),
        )),
        "mod_item" => {
            let name = identifier_name(node, source);
            let qualified_name = qualify(parent, &name);
            let is_test = is_test_module(node, source);
            let children = extract_module_children(node, source, &qualified_name, is_test);
            Some(container_symbol(
                node,
                source,
                parent,
                if is_test {
                    SymbolKind::Test
                } else {
                    SymbolKind::Module
                },
                name,
                first_line_signature(node, source),
                children,
            ))
        }
        "macro_definition" => Some(container_symbol(
            node,
            source,
            parent,
            SymbolKind::Macro,
            identifier_name(node, source),
            macro_signature(node, source),
            Vec::new(),
        )),
        _ => None,
    }
}

fn function_symbol(
    node: &Node,
    source: &str,
    parent: Option<&str>,
    kind: SymbolKind,
) -> SymbolEntry {
    let name = identifier_name(node, source);
    let signature = function_signature(node, source);
    let body = find_child_by_kind(node, "block");
    symbol_entry(
        node,
        source,
        kind,
        name,
        signature,
        parent,
        body.map(|n| n.start_position().row + 1),
        body.map(|n| n.end_position().row + 1),
        Vec::new(),
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
    symbol_entry(
        node, source, kind, name, signature, parent, None, None, children,
    )
}

#[allow(clippy::too_many_arguments)]
fn symbol_entry(
    node: &Node,
    source: &str,
    kind: SymbolKind,
    name: String,
    signature: String,
    parent: Option<&str>,
    body_start_line: Option<usize>,
    body_end_line: Option<usize>,
    children: Vec<SymbolEntry>,
) -> SymbolEntry {
    let text = node_text(node, source);
    let line_count = text.lines().count();
    let qualified_name = qualify(parent, &name);
    SymbolEntry {
        kind,
        name,
        qualified_name,
        signature,
        start_line: node.start_position().row + 1,
        end_line: node.end_position().row + 1,
        start_byte: node.start_byte(),
        end_byte: node.end_byte(),
        body_start_line,
        body_end_line,
        parent: parent.map(str::to_string),
        children,
        digest: SymbolDigest::new(text.as_bytes(), line_count),
    }
}

fn extract_struct_fields(node: &Node, source: &str) -> Vec<SymbolEntry> {
    let Some(body) = find_child_by_kind(node, "field_declaration_list") else {
        return Vec::new();
    };
    let struct_name = type_identifier_name(node, source);
    let parent = struct_name.as_str();
    let mut fields = Vec::new();
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        if child.kind() == "field_declaration" {
            let name = find_child_by_kind(&child, "field_identifier")
                .map(|n| node_text(&n, source).to_string())
                .unwrap_or_else(|| first_line_signature(&child, source));
            fields.push(symbol_entry(
                &child,
                source,
                SymbolKind::Field,
                name,
                first_line_signature(&child, source)
                    .trim_end_matches(',')
                    .to_string(),
                Some(parent),
                None,
                None,
                Vec::new(),
            ));
        }
    }
    fields
}

fn extract_enum_variants(node: &Node, source: &str) -> Vec<SymbolEntry> {
    let Some(body) = find_child_by_kind(node, "enum_variant_list") else {
        return Vec::new();
    };
    let enum_name = type_identifier_name(node, source);
    let parent = enum_name.as_str();
    let mut variants = Vec::new();
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        if child.kind() == "enum_variant" {
            let name = find_child_by_kind(&child, "identifier")
                .or_else(|| find_child_by_kind(&child, "type_identifier"))
                .map(|n| node_text(&n, source).to_string())
                .unwrap_or_else(|| first_line_signature(&child, source));
            variants.push(symbol_entry(
                &child,
                source,
                SymbolKind::EnumVariant,
                name,
                first_line_signature(&child, source)
                    .trim_end_matches(',')
                    .to_string(),
                Some(parent),
                None,
                None,
                Vec::new(),
            ));
        }
    }
    variants
}

fn extract_trait_children(node: &Node, source: &str, parent: &str) -> Vec<SymbolEntry> {
    let Some(body) = find_child_by_kind(node, "declaration_list") else {
        return Vec::new();
    };
    let mut children = Vec::new();
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        match child.kind() {
            "function_signature_item" | "function_item" => {
                children.push(function_symbol(
                    &child,
                    source,
                    Some(parent),
                    SymbolKind::Method,
                ));
            }
            "type_item" => children.push(container_symbol(
                &child,
                source,
                Some(parent),
                SymbolKind::TypeAlias,
                type_identifier_name(&child, source),
                first_line_signature(&child, source),
                Vec::new(),
            )),
            "const_item" => children.push(container_symbol(
                &child,
                source,
                Some(parent),
                SymbolKind::Const,
                identifier_name(&child, source),
                first_line_signature(&child, source),
                Vec::new(),
            )),
            _ => {}
        }
    }
    children
}

fn extract_impl_children(node: &Node, source: &str, parent: &str) -> Vec<SymbolEntry> {
    let Some(body) = find_child_by_kind(node, "declaration_list") else {
        return Vec::new();
    };
    let mut children = Vec::new();
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        match child.kind() {
            "function_item" => {
                children.push(function_symbol(
                    &child,
                    source,
                    Some(parent),
                    SymbolKind::Method,
                ));
            }
            "type_item" => children.push(container_symbol(
                &child,
                source,
                Some(parent),
                SymbolKind::TypeAlias,
                type_identifier_name(&child, source),
                first_line_signature(&child, source),
                Vec::new(),
            )),
            "const_item" => children.push(container_symbol(
                &child,
                source,
                Some(parent),
                SymbolKind::Const,
                identifier_name(&child, source),
                first_line_signature(&child, source),
                Vec::new(),
            )),
            _ => {}
        }
    }
    children
}

fn extract_module_children(
    node: &Node,
    source: &str,
    parent: &str,
    parent_is_test: bool,
) -> Vec<SymbolEntry> {
    let Some(body) = find_child_by_kind(node, "declaration_list") else {
        return Vec::new();
    };
    let mut children = Vec::new();
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        if let Some(mut symbol) = extract_top_level_symbol(&child, source, Some(parent)) {
            if parent_is_test && symbol.kind == SymbolKind::Function {
                symbol.kind = SymbolKind::Test;
            }
            children.push(symbol);
        }
    }
    children
}

fn function_signature(node: &Node, source: &str) -> String {
    let body_start = find_child_by_kind(node, "block")
        .map(|n| n.start_byte())
        .unwrap_or(node.end_byte());
    safe_slice(source, node.start_byte(), body_start)
        .trim()
        .trim_end_matches(';')
        .to_string()
}

fn first_line_signature(node: &Node, source: &str) -> String {
    node_text(node, source)
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .trim_end_matches(';')
        .to_string()
}

fn impl_signature(node: &Node, source: &str) -> String {
    let text = node_text(node, source);
    text.find('{')
        .map(|idx| text[..idx].trim().to_string())
        .unwrap_or_else(|| first_line_signature(node, source))
}

fn macro_signature(node: &Node, source: &str) -> String {
    let name = identifier_name(node, source);
    format!("macro_rules! {name}")
}

fn extract_impl_name(node: &Node, source: &str) -> String {
    impl_signature(node, source)
        .strip_prefix("impl")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("impl")
        .to_string()
}

fn identifier_name(node: &Node, source: &str) -> String {
    find_child_by_kind(node, "identifier")
        .map(|n| node_text(&n, source).to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

fn type_identifier_name(node: &Node, source: &str) -> String {
    find_child_by_kind(node, "type_identifier")
        .map(|n| node_text(&n, source).to_string())
        .unwrap_or_else(|| identifier_name(node, source))
}

fn qualify(parent: Option<&str>, name: &str) -> String {
    parent
        .filter(|parent| !parent.is_empty())
        .map(|parent| format!("{parent}::{name}"))
        .unwrap_or_else(|| name.to_string())
}

fn is_test_function(node: &Node, source: &str) -> bool {
    let start = node.start_byte();
    if start == 0 {
        return false;
    }
    let prefix = safe_slice(source, start.saturating_sub(200), start);
    prefix.contains("#[test]") || prefix.contains("#[tokio::test]") || prefix.contains("#[rstest]")
}

fn is_test_module(node: &Node, source: &str) -> bool {
    let name = identifier_name(node, source);
    if name == "tests" || name == "test" {
        return true;
    }
    let start = node.start_byte();
    if start == 0 {
        return false;
    }
    safe_slice(source, start.saturating_sub(200), start).contains("#[cfg(test)]")
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
        .find(|&child| child.kind() == kind)
}
