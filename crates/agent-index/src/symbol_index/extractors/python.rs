use tree_sitter::{Node, Parser};

use super::super::types::SymbolDigest;
use super::safe_slice;
use crate::index::symbol_index::{SymbolEntry, SymbolError, SymbolKind};

pub fn extract(source: &str) -> Result<Vec<SymbolEntry>, SymbolError> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_python::LANGUAGE.into())
        .map_err(|e| SymbolError::ParseError(format!("Failed to set Python language: {e}")))?;
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
        "import_statement" | "import_from_statement" => symbols.push(container_symbol(
            node,
            source,
            parent,
            SymbolKind::Import,
            node_text(node, source).trim().to_string(),
            node_text(node, source).trim().to_string(),
            Vec::new(),
        )),
        "class_definition" => symbols.push(class_symbol(node, source, parent, false)),
        "function_definition" => symbols.push(function_symbol(
            node,
            source,
            parent,
            if is_test_function_name(&identifier_name(node, source)) {
                SymbolKind::Test
            } else {
                SymbolKind::Function
            },
            String::new(),
        )),
        "decorated_definition" => collect_decorated_symbol(node, source, parent, symbols),
        "expression_statement" => {
            if let Some(assign) = find_child_by_kind(node, "assignment") {
                let text = node_text(&assign, source);
                let lhs = text.split('=').next().unwrap_or("").trim();
                if lhs.chars().next().is_some_and(|c| c.is_uppercase()) {
                    symbols.push(container_symbol(
                        node,
                        source,
                        parent,
                        SymbolKind::Const,
                        lhs.to_string(),
                        first_line_signature(node, source),
                        Vec::new(),
                    ));
                }
            }
        }
        _ => {}
    }
}

fn collect_decorated_symbol(
    node: &Node,
    source: &str,
    parent: Option<&str>,
    symbols: &mut Vec<SymbolEntry>,
) {
    let decorator = decorators(node, source);
    let Some(inner) = find_child_by_kinds(node, &["function_definition", "class_definition"])
    else {
        return;
    };

    match inner.kind() {
        "function_definition" => {
            let name = identifier_name(&inner, source);
            symbols.push(function_symbol(
                node,
                source,
                parent,
                if is_test_function_name(&name) {
                    SymbolKind::Test
                } else {
                    SymbolKind::Function
                },
                format!("{}{}", decorator, function_signature(&inner, source)),
            ));
        }
        "class_definition" => symbols.push(class_symbol_with_outer(
            node, &inner, source, parent, decorator,
        )),
        _ => {}
    }
}

fn class_symbol(node: &Node, source: &str, parent: Option<&str>, decorated: bool) -> SymbolEntry {
    let name = identifier_name(node, source);
    let signature = class_signature(node, source, String::new());
    let kind = if is_test_class(&name) {
        SymbolKind::Test
    } else {
        SymbolKind::Class
    };
    let qualified_name = qualify(parent, &name);
    let children = extract_class_methods(node, source, &qualified_name);
    let _ = decorated;
    container_symbol(node, source, parent, kind, name, signature, children)
}

fn class_symbol_with_outer(
    outer: &Node,
    inner: &Node,
    source: &str,
    parent: Option<&str>,
    decorator: String,
) -> SymbolEntry {
    let name = identifier_name(inner, source);
    let kind = if is_test_class(&name) {
        SymbolKind::Test
    } else {
        SymbolKind::Class
    };
    let qualified_name = qualify(parent, &name);
    let children = extract_class_methods(inner, source, &qualified_name);
    symbol_entry(
        outer,
        source,
        kind,
        name,
        class_signature(inner, source, decorator),
        parent,
        None,
        None,
        children,
    )
}

fn function_symbol(
    node: &Node,
    source: &str,
    parent: Option<&str>,
    kind: SymbolKind,
    explicit_signature: String,
) -> SymbolEntry {
    let function_node = if node.kind() == "decorated_definition" {
        find_child_by_kind(node, "function_definition").unwrap_or(*node)
    } else {
        *node
    };
    let name = identifier_name(&function_node, source);
    let signature = if explicit_signature.is_empty() {
        function_signature(&function_node, source)
    } else {
        explicit_signature
    };
    let body = find_child_by_kind(&function_node, "block");
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

fn extract_class_methods(node: &Node, source: &str, parent: &str) -> Vec<SymbolEntry> {
    let Some(body) = find_child_by_kind(node, "block") else {
        return Vec::new();
    };
    let mut methods = Vec::new();
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        match child.kind() {
            "function_definition" => methods.push(function_symbol(
                &child,
                source,
                Some(parent),
                SymbolKind::Method,
                String::new(),
            )),
            "decorated_definition" => {
                if let Some(inner) = find_child_by_kind(&child, "function_definition") {
                    let sig = format!(
                        "{}{}",
                        decorators(&child, source),
                        function_signature(&inner, source)
                    );
                    methods.push(function_symbol(
                        &child,
                        source,
                        Some(parent),
                        SymbolKind::Method,
                        sig,
                    ));
                }
            }
            _ => {}
        }
    }
    methods
}

fn function_signature(node: &Node, source: &str) -> String {
    let name = identifier_name(node, source);
    let params = find_child_by_kind(node, "parameters")
        .map(|n| compact_node_text(&n, source))
        .unwrap_or_else(|| "()".to_string());
    let return_type = find_child_by_kind(node, "type")
        .map(|n| format!(" -> {}", node_text(&n, source)))
        .unwrap_or_default();
    let prefix = if node_text(node, source).trim_start().starts_with("async ") {
        "async "
    } else {
        ""
    };
    format!("{prefix}def {name}{params}{return_type}")
}

fn class_signature(node: &Node, source: &str, decorator: String) -> String {
    let name = identifier_name(node, source);
    let superclasses = find_child_by_kind(node, "argument_list")
        .map(|n| node_text(&n, source).to_string())
        .unwrap_or_default();
    if superclasses.is_empty() {
        format!("{decorator}class {name}")
    } else {
        format!("{decorator}class {name}{superclasses}")
    }
}

fn decorators(node: &Node, source: &str) -> String {
    let mut decorators = Vec::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "decorator" {
            decorators.push(node_text(&child, source).trim().to_string());
        }
    }
    if decorators.is_empty() {
        String::new()
    } else {
        format!("{} ", decorators.join(" "))
    }
}

fn is_test_function_name(name: &str) -> bool {
    name.starts_with("test_") || name == "setUp" || name == "tearDown"
}

fn is_test_class(name: &str) -> bool {
    name.starts_with("Test") || name.ends_with("Test") || name.ends_with("Tests")
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
