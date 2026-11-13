use tree_sitter::{Node, Parser};

use super::super::types::SymbolDigest;
use super::safe_slice;
use crate::index::symbol_index::{SymbolEntry, SymbolError, SymbolKind};

pub fn extract(source: &str, language: &str) -> Result<Vec<SymbolEntry>, SymbolError> {
    let mut parser = Parser::new();
    let ts_lang = match language {
        "typescript" | "javascript" => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        other => return Err(SymbolError::UnsupportedLanguage(other.to_string())),
    };
    parser
        .set_language(&ts_lang)
        .map_err(|e| SymbolError::ParseError(format!("Failed to set TS/JS language: {e}")))?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| SymbolError::ParseError("tree-sitter parse returned None".into()))?;

    let root = tree.root_node();
    let mut symbols = Vec::new();
    let mut cursor = root.walk();
    for node in root.named_children(&mut cursor) {
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
        "import_statement" => symbols.push(container_symbol(
            node,
            source,
            parent,
            SymbolKind::Import,
            compact_node_text(node, source),
            compact_node_text(node, source),
            Vec::new(),
        )),
        "interface_declaration" => symbols.push(interface_symbol(node, source, parent, false)),
        "type_alias_declaration" => symbols.push(container_symbol(
            node,
            source,
            parent,
            SymbolKind::TypeAlias,
            type_identifier_name(node, source),
            first_line_signature(node, source),
            Vec::new(),
        )),
        "enum_declaration" => symbols.push(enum_symbol(node, source, parent, false)),
        "class_declaration" => symbols.push(class_symbol(node, source, parent, false, false)),
        "abstract_class_declaration" => {
            symbols.push(class_symbol(node, source, parent, true, false))
        }
        "function_declaration" => symbols.push(function_symbol(
            node,
            source,
            parent,
            SymbolKind::Function,
            false,
        )),
        "export_statement" => collect_export_symbol(node, source, parent, symbols),
        "lexical_declaration" | "variable_declaration" => {
            let kind = if looks_like_function_assignment(node, source) {
                SymbolKind::Function
            } else {
                SymbolKind::Const
            };
            symbols.push(container_symbol(
                node,
                source,
                parent,
                kind,
                variable_name(node, source),
                lexical_label(node, source),
                Vec::new(),
            ));
        }
        "expression_statement" => {
            if let Some(call) = find_child_by_kind(node, "call_expression")
                && is_describe_or_test(&call, source)
            {
                symbols.push(container_symbol(
                    node,
                    source,
                    parent,
                    SymbolKind::Test,
                    test_call_name(&call, source),
                    test_call_label(&call, source),
                    Vec::new(),
                ));
            }
        }
        _ => {}
    }
}

fn collect_export_symbol(
    node: &Node,
    source: &str,
    parent: Option<&str>,
    symbols: &mut Vec<SymbolEntry>,
) {
    let mut cursor = node.walk();
    let mut handled = false;
    for child in node.named_children(&mut cursor) {
        handled = true;
        match child.kind() {
            "function_declaration" => symbols.push(function_symbol(
                &child,
                source,
                parent,
                SymbolKind::Function,
                true,
            )),
            "class_declaration" => symbols.push(class_symbol(&child, source, parent, false, true)),
            "abstract_class_declaration" => {
                symbols.push(class_symbol(&child, source, parent, true, true))
            }
            "interface_declaration" => symbols.push(interface_symbol(&child, source, parent, true)),
            "type_alias_declaration" => symbols.push(container_symbol(
                &child,
                source,
                parent,
                SymbolKind::TypeAlias,
                type_identifier_name(&child, source),
                format!("export {}", first_line_signature(&child, source)),
                Vec::new(),
            )),
            "enum_declaration" => symbols.push(enum_symbol(&child, source, parent, true)),
            "lexical_declaration" => {
                let kind = if looks_like_function_assignment(&child, source) {
                    SymbolKind::Function
                } else {
                    SymbolKind::Const
                };
                symbols.push(container_symbol(
                    &child,
                    source,
                    parent,
                    kind,
                    variable_name(&child, source),
                    format!("export {}", lexical_label(&child, source)),
                    Vec::new(),
                ));
            }
            _ => symbols.push(container_symbol(
                node,
                source,
                parent,
                SymbolKind::Unknown,
                first_line_signature(node, source),
                first_line_signature(node, source),
                Vec::new(),
            )),
        }
    }

    if !handled {
        symbols.push(container_symbol(
            node,
            source,
            parent,
            SymbolKind::Unknown,
            first_line_signature(node, source),
            first_line_signature(node, source),
            Vec::new(),
        ));
    }
}

fn interface_symbol(
    node: &Node,
    source: &str,
    parent: Option<&str>,
    exported: bool,
) -> SymbolEntry {
    let name = type_identifier_name(node, source);
    let qualified_name = qualify(parent, &name);
    let mut signature = interface_label(node, source);
    if exported {
        signature = format!("export {signature}");
    }
    container_symbol(
        node,
        source,
        parent,
        SymbolKind::Interface,
        name,
        signature,
        extract_interface_members(node, source, &qualified_name),
    )
}

fn enum_symbol(node: &Node, source: &str, parent: Option<&str>, exported: bool) -> SymbolEntry {
    let name = identifier_name(node, source);
    let mut signature = format!("enum {name}");
    if exported {
        signature = format!("export {signature}");
    }
    container_symbol(
        node,
        source,
        parent,
        SymbolKind::Enum,
        name,
        signature,
        extract_enum_members(node, source),
    )
}

fn class_symbol(
    node: &Node,
    source: &str,
    parent: Option<&str>,
    abstract_class: bool,
    exported: bool,
) -> SymbolEntry {
    let name = type_identifier_name(node, source);
    let qualified_name = qualify(parent, &name);
    let mut signature = class_label(node, source);
    if abstract_class {
        signature = format!("abstract {signature}");
    }
    if exported {
        signature = format!("export {signature}");
    }
    container_symbol(
        node,
        source,
        parent,
        SymbolKind::Class,
        name,
        signature,
        extract_class_members(node, source, &qualified_name),
    )
}

fn function_symbol(
    node: &Node,
    source: &str,
    parent: Option<&str>,
    kind: SymbolKind,
    exported: bool,
) -> SymbolEntry {
    let name = identifier_name(node, source);
    let mut signature = function_signature(node, source);
    if exported {
        signature = format!("export {signature}");
    }
    let body = find_child_by_kind(node, "statement_block");
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
    SymbolEntry {
        kind,
        qualified_name: qualify(parent, &name),
        name,
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

fn extract_interface_members(node: &Node, source: &str, parent: &str) -> Vec<SymbolEntry> {
    let Some(body) = find_child_by_kinds(node, &["object_type", "interface_body"]) else {
        return Vec::new();
    };
    let mut members = Vec::new();
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        match child.kind() {
            "property_signature"
            | "method_signature"
            | "call_signature"
            | "construct_signature"
            | "index_signature" => {
                let signature = first_line_signature(&child, source)
                    .trim_end_matches([',', ';'])
                    .to_string();
                members.push(container_symbol(
                    &child,
                    source,
                    Some(parent),
                    if child.kind() == "method_signature" {
                        SymbolKind::Method
                    } else {
                        SymbolKind::Field
                    },
                    member_name(&child, source),
                    signature,
                    Vec::new(),
                ));
            }
            _ => {}
        }
    }
    members
}

fn extract_class_members(node: &Node, source: &str, parent: &str) -> Vec<SymbolEntry> {
    let Some(body) = find_child_by_kind(node, "class_body") else {
        return Vec::new();
    };
    let mut members = Vec::new();
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        match child.kind() {
            "method_definition" => members.push(container_symbol(
                &child,
                source,
                Some(parent),
                SymbolKind::Method,
                member_name(&child, source),
                first_line_signature(&child, source)
                    .trim_end_matches('{')
                    .trim()
                    .to_string(),
                Vec::new(),
            )),
            "public_field_definition" | "property_definition" => members.push(container_symbol(
                &child,
                source,
                Some(parent),
                SymbolKind::Field,
                member_name(&child, source),
                first_line_signature(&child, source),
                Vec::new(),
            )),
            _ => {}
        }
    }
    members
}

fn extract_enum_members(node: &Node, source: &str) -> Vec<SymbolEntry> {
    let Some(body) = find_child_by_kind(node, "enum_body") else {
        return Vec::new();
    };
    let enum_name = identifier_name(node, source);
    let parent = enum_name.as_str();
    let mut members = Vec::new();
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        if child.kind() == "enum_assignment" || child.kind() == "property_identifier" {
            let signature = node_text(&child, source)
                .trim()
                .trim_end_matches(',')
                .to_string();
            members.push(container_symbol(
                &child,
                source,
                Some(parent),
                SymbolKind::EnumVariant,
                member_name(&child, source),
                signature,
                Vec::new(),
            ));
        }
    }
    members
}

fn function_signature(node: &Node, source: &str) -> String {
    let name = identifier_name(node, source);
    let params = find_child_by_kind(node, "formal_parameters")
        .map(|n| compact_node_text(&n, source))
        .unwrap_or_else(|| "()".to_string());
    let return_type = return_type(node, source);
    let prefix = if node_text(node, source).trim_start().starts_with("async ") {
        "async "
    } else {
        ""
    };
    if return_type.is_empty() {
        format!("{prefix}function {name}{params}")
    } else {
        format!("{prefix}function {name}{params}: {return_type}")
    }
}

fn return_type(node: &Node, source: &str) -> String {
    let mut cursor = node.walk();
    let mut found_params = false;
    for child in node.named_children(&mut cursor) {
        if child.kind() == "formal_parameters" {
            found_params = true;
            continue;
        }
        if found_params && child.kind() == "type_annotation" {
            let text = node_text(&child, source).trim().to_string();
            return text.strip_prefix(':').unwrap_or(&text).trim().to_string();
        }
        if child.kind() == "statement_block" {
            break;
        }
    }
    String::new()
}

fn interface_label(node: &Node, source: &str) -> String {
    let name = type_identifier_name(node, source);
    let extends = find_child_by_kind(node, "extends_type_clause")
        .map(|n| format!(" {}", node_text(&n, source).trim()))
        .unwrap_or_default();
    format!("interface {name}{extends}")
}

fn class_label(node: &Node, source: &str) -> String {
    let name = type_identifier_name(node, source);
    let heritage = find_child_by_kind(node, "class_heritage")
        .map(|n| format!(" {}", node_text(&n, source).trim()))
        .unwrap_or_default();
    format!("class {name}{heritage}")
}

fn lexical_label(node: &Node, source: &str) -> String {
    let trimmed = first_line_signature(node, source);
    if let Some(eq_pos) = trimmed.find('=') {
        let lhs = &trimmed[..eq_pos + 1];
        let rhs = trimmed[eq_pos + 1..].trim();
        if rhs.len() < 60 {
            trimmed.trim_end_matches(['{', ';']).trim().to_string()
        } else {
            format!("{} ...", lhs)
        }
    } else {
        trimmed
    }
}

fn looks_like_function_assignment(node: &Node, source: &str) -> bool {
    let text = node_text(node, source);
    text.contains("=>") || text.contains("function(") || text.contains("function (")
}

fn is_describe_or_test(call_node: &Node, source: &str) -> bool {
    let func_name = find_child_by_kind(call_node, "identifier")
        .or_else(|| find_child_by_kind(call_node, "member_expression"))
        .map(|n| node_text(&n, source).to_string())
        .unwrap_or_default();
    matches!(
        func_name.as_str(),
        "describe" | "it" | "test" | "beforeEach" | "afterEach" | "beforeAll" | "afterAll"
    )
}

fn test_call_label(call_node: &Node, source: &str) -> String {
    let func_name = test_call_name(call_node, source);
    if let Some(args) = find_child_by_kind(call_node, "arguments") {
        let mut cursor = args.walk();
        for child in args.named_children(&mut cursor) {
            if child.kind() == "string" || child.kind() == "template_string" {
                return format!("{}({})", func_name, node_text(&child, source));
            }
        }
    }
    func_name
}

fn test_call_name(call_node: &Node, source: &str) -> String {
    find_child_by_kind(call_node, "identifier")
        .map(|n| node_text(&n, source).to_string())
        .unwrap_or_else(|| "test".to_string())
}

fn variable_name(node: &Node, source: &str) -> String {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "variable_declarator"
            && let Some(name) = find_child_by_kind(&child, "identifier")
        {
            return node_text(&name, source).to_string();
        }
    }
    identifier_name(node, source)
}

fn member_name(node: &Node, source: &str) -> String {
    find_child_by_kinds(
        node,
        &[
            "property_identifier",
            "private_property_identifier",
            "identifier",
            "type_identifier",
        ],
    )
    .map(|n| node_text(&n, source).to_string())
    .unwrap_or_else(|| first_line_signature(node, source))
}

fn identifier_name(node: &Node, source: &str) -> String {
    find_child_by_kind(node, "identifier")
        .map(|n| node_text(&n, source).to_string())
        .unwrap_or_else(|| type_identifier_name(node, source))
}

fn type_identifier_name(node: &Node, source: &str) -> String {
    find_child_by_kind(node, "type_identifier")
        .map(|n| node_text(&n, source).to_string())
        .unwrap_or_else(|| "unknown".to_string())
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

fn compact_node_text(node: &Node, source: &str) -> String {
    node_text(node, source)
        .lines()
        .map(str::trim)
        .collect::<Vec<_>>()
        .join(" ")
}

fn qualify(parent: Option<&str>, name: &str) -> String {
    parent
        .filter(|parent| !parent.is_empty())
        .map(|parent| format!("{parent}::{name}"))
        .unwrap_or_else(|| name.to_string())
}

fn node_text<'a>(node: &Node, source: &'a str) -> &'a str {
    safe_slice(source, node.start_byte(), node.end_byte())
}

fn find_child_by_kind<'a>(node: &'a Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find(|&child| child.kind() == kind)
}

fn find_child_by_kinds<'a>(node: &'a Node<'a>, kinds: &[&str]) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find(|&child| kinds.contains(&child.kind()))
}
