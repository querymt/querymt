use tree_sitter::{Node, Parser};

use super::super::types::SymbolDigest;
use super::safe_slice;
use crate::index::symbol_index::{SymbolEntry, SymbolError, SymbolKind};

pub fn extract(source: &str) -> Result<Vec<SymbolEntry>, SymbolError> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_lua::LANGUAGE.into())
        .map_err(|e| SymbolError::ParseError(format!("Failed to load Lua grammar: {e}")))?;

    let tree = parser
        .parse(source, None)
        .ok_or_else(|| SymbolError::ParseError("Failed to parse Lua source".to_string()))?;

    let mut symbols = Vec::new();
    collect_symbols(&tree.root_node(), source, None, &mut symbols);
    Ok(symbols)
}

fn collect_symbols(
    node: &Node,
    source: &str,
    parent: Option<&str>,
    symbols: &mut Vec<SymbolEntry>,
) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "function_declaration" => {
                symbols.push(function_declaration_symbol(&child, source, parent))
            }
            "variable_declaration" => collect_variable_declaration(&child, source, parent, symbols),
            "assignment_statement" => collect_assignment_statement(&child, source, parent, symbols),
            "function_call" => {
                if let Some(symbol) = function_call_symbol(&child, source, parent) {
                    symbols.push(symbol);
                }
            }
            _ => collect_symbols(&child, source, parent, symbols),
        }
    }
}

fn collect_variable_declaration(
    node: &Node,
    source: &str,
    parent: Option<&str>,
    symbols: &mut Vec<SymbolEntry>,
) {
    if let Some(local_function) = direct_child_by_kind(node, "function_declaration") {
        symbols.push(function_declaration_symbol(&local_function, source, parent));
        return;
    }

    if let Some(assignment) = direct_child_by_kind(node, "assignment_statement") {
        collect_assignment_statement(&assignment, source, parent, symbols);
    }
}

fn collect_assignment_statement(
    node: &Node,
    source: &str,
    parent: Option<&str>,
    symbols: &mut Vec<SymbolEntry>,
) {
    let Some(name_node) = assignment_name_node(node) else {
        return;
    };
    let Some(value_node) = assignment_value_node(node) else {
        return;
    };

    let name = variable_name(&name_node, source);
    if name.is_empty() {
        return;
    }

    match value_node.kind() {
        "function_definition" => symbols.push(function_assignment_symbol(
            node,
            &value_node,
            source,
            parent,
            name,
        )),
        "function_call" if is_require_call(&value_node, source) => {
            symbols.push(import_symbol(node, &value_node, source, parent))
        }
        "table_constructor" if is_module_table_name(&name) => symbols.push(symbol_entry(
            node,
            source,
            SymbolKind::Module,
            name,
            first_line_signature(node, source),
            parent.map(str::to_string),
            Vec::new(),
        )),
        _ if is_uppercase_const(&name) && is_scalar_node(&value_node) => {
            symbols.push(symbol_entry(
                node,
                source,
                SymbolKind::Const,
                name,
                first_line_signature(node, source),
                parent.map(str::to_string),
                Vec::new(),
            ))
        }
        _ => {}
    }
}

fn function_declaration_symbol(node: &Node, source: &str, parent: Option<&str>) -> SymbolEntry {
    let name_node = child_by_field(node, "name").or_else(|| first_named_child(node));
    let raw_name = name_node
        .as_ref()
        .map(|n| variable_name(n, source))
        .unwrap_or_else(|| "<anonymous>".to_string());
    let kind = function_kind(&raw_name);
    let name = display_name_for_kind(&raw_name, kind);
    let kind = if is_test_name(&name) {
        SymbolKind::Test
    } else {
        kind
    };

    symbol_entry(
        node,
        source,
        kind,
        name,
        first_line_signature(node, source),
        parent.map(str::to_string),
        Vec::new(),
    )
}

fn function_assignment_symbol(
    assignment: &Node,
    function: &Node,
    source: &str,
    parent: Option<&str>,
    raw_name: String,
) -> SymbolEntry {
    let kind = function_kind(&raw_name);
    let name = display_name_for_kind(&raw_name, kind);
    let kind = if is_test_name(&name) {
        SymbolKind::Test
    } else {
        kind
    };

    let signature = assignment_signature(assignment, function, source);
    symbol_entry(
        assignment,
        source,
        kind,
        name,
        signature,
        parent.map(str::to_string),
        Vec::new(),
    )
}

fn function_call_symbol(node: &Node, source: &str, parent: Option<&str>) -> Option<SymbolEntry> {
    if is_require_call(node, source) {
        return Some(import_symbol(node, node, source, parent));
    }

    let call_name = call_name(node, source)?;
    if !matches!(call_name.as_str(), "describe" | "it" | "pending") {
        return None;
    }

    let test_name = first_string_argument(node, source).unwrap_or_else(|| call_name.clone());
    let children = if call_name == "describe" {
        collect_call_tests(node, source, Some(&test_name))
    } else {
        Vec::new()
    };

    Some(symbol_entry(
        node,
        source,
        SymbolKind::Test,
        test_name,
        first_line_signature(node, source),
        parent.map(str::to_string),
        children,
    ))
}

fn collect_call_tests(node: &Node, source: &str, parent: Option<&str>) -> Vec<SymbolEntry> {
    let mut children = Vec::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "function_call" {
            if let Some(symbol) = function_call_symbol(&child, source, parent) {
                children.push(symbol);
            } else {
                children.extend(collect_call_tests(&child, source, parent));
            }
        } else {
            children.extend(collect_call_tests(&child, source, parent));
        }
    }
    children
}

fn import_symbol(node: &Node, call_node: &Node, source: &str, parent: Option<&str>) -> SymbolEntry {
    let import_name =
        first_string_argument(call_node, source).unwrap_or_else(|| "require".to_string());
    symbol_entry(
        node,
        source,
        SymbolKind::Import,
        import_name,
        first_line_signature(node, source),
        parent.map(str::to_string),
        Vec::new(),
    )
}

fn assignment_name_node<'a>(node: &'a Node<'a>) -> Option<Node<'a>> {
    first_child_in_assignment_list(node, "variable_list")
}

fn assignment_value_node<'a>(node: &'a Node<'a>) -> Option<Node<'a>> {
    first_child_in_assignment_list(node, "expression_list")
}

fn first_child_in_assignment_list<'a>(node: &'a Node<'a>, list_kind: &str) -> Option<Node<'a>> {
    let list = direct_child_by_kind(node, list_kind)?;
    let mut cursor = list.walk();
    list.named_children(&mut cursor).next()
}

fn variable_name(node: &Node, source: &str) -> String {
    node_text(node, source)
        .trim()
        .replace('[', ".")
        .replace([']', '"', '\''], "")
}

fn display_name_for_kind(raw_name: &str, kind: SymbolKind) -> String {
    if kind == SymbolKind::Method {
        raw_name.to_string()
    } else {
        raw_name
            .rsplit(['.', ':'])
            .next()
            .unwrap_or(raw_name)
            .to_string()
    }
}

fn function_kind(name: &str) -> SymbolKind {
    if name.contains('.') || name.contains(':') {
        SymbolKind::Method
    } else {
        SymbolKind::Function
    }
}

fn is_test_name(name: &str) -> bool {
    name.starts_with("test_") || name.ends_with("_test")
}

fn is_module_table_name(name: &str) -> bool {
    matches!(name, "M" | "Module") || name.ends_with("Module")
}

fn is_uppercase_const(name: &str) -> bool {
    let leaf = name.rsplit(['.', ':']).next().unwrap_or(name);
    leaf.chars().any(|c| c.is_ascii_alphabetic())
        && leaf
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

fn is_scalar_node(node: &Node) -> bool {
    matches!(node.kind(), "number" | "string" | "true" | "false" | "nil")
}

fn is_require_call(node: &Node, source: &str) -> bool {
    node.kind() == "function_call" && call_name(node, source).as_deref() == Some("require")
}

fn call_name(node: &Node, source: &str) -> Option<String> {
    child_by_field(node, "name").map(|name| variable_name(&name, source))
}

fn first_string_argument(node: &Node, source: &str) -> Option<String> {
    let arguments = child_by_field(node, "arguments")?;
    let mut cursor = arguments.walk();
    for child in arguments.named_children(&mut cursor) {
        if child.kind() == "string" {
            return Some(string_value(&child, source));
        }
    }
    None
}

fn string_value(node: &Node, source: &str) -> String {
    child_by_field(node, "content")
        .map(|content| node_text(&content, source).to_string())
        .unwrap_or_else(|| {
            node_text(node, source)
                .trim()
                .trim_matches(['\"', '\''])
                .to_string()
        })
}

fn assignment_signature(assignment: &Node, function: &Node, source: &str) -> String {
    let assignment_text = node_text(assignment, source).trim();
    let function_text = node_text(function, source).trim();
    if let Some(index) = function_text.find(')') {
        let prefix_len = function
            .start_byte()
            .saturating_sub(assignment.start_byte())
            + index
            + 1;
        return assignment_text
            .get(..prefix_len.min(assignment_text.len()))
            .unwrap_or(assignment_text)
            .trim()
            .to_string();
    }
    first_line_signature(assignment, source)
}

fn symbol_entry(
    node: &Node,
    source: &str,
    kind: SymbolKind,
    name: String,
    signature: String,
    parent: Option<String>,
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
        body_start_line: None,
        body_end_line: None,
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

fn child_by_field<'a>(node: &'a Node<'a>, field: &str) -> Option<Node<'a>> {
    node.child_by_field_name(field)
}

fn direct_child_by_kind<'a>(node: &'a Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find(|child| child.kind() == kind)
}

fn first_named_child<'a>(node: &'a Node<'a>) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).next()
}
