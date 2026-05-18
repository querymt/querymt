use std::collections::HashSet;

use tree_sitter::{Node, Parser};

use super::super::types::SymbolDigest;
use super::safe_slice;
use crate::index::symbol_index::{SymbolEntry, SymbolError, SymbolKind};

const MAX_CHILD_SYMBOLS: usize = 24;

pub fn extract(source: &str) -> Result<Vec<SymbolEntry>, SymbolError> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_nix::LANGUAGE.into())
        .map_err(|e| SymbolError::ParseError(format!("Failed to set Nix parser language: {e}")))?;

    let tree = parser
        .parse(source, None)
        .ok_or_else(|| SymbolError::ParseError("Failed to parse Nix source".to_string()))?;

    let mut symbols = Vec::new();
    collect_import_expressions(&tree.root_node(), source, None, &mut symbols);
    collect_bindings(&tree.root_node(), source, None, false, &mut symbols);
    Ok(symbols)
}

fn collect_bindings(
    node: &Node,
    source: &str,
    parent: Option<&str>,
    nested: bool,
    symbols: &mut Vec<SymbolEntry>,
) {
    if node.kind() == "binding" {
        symbols.extend(binding_symbols(node, source, parent, nested));
        return;
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_bindings(&child, source, parent, nested, symbols);
    }
}

fn binding_symbols(
    node: &Node,
    source: &str,
    parent: Option<&str>,
    nested: bool,
) -> Vec<SymbolEntry> {
    let Some(attrpath) = child_by_field(node, "attrpath") else {
        return Vec::new();
    };
    let Some(expression) = child_by_field(node, "expression") else {
        return Vec::new();
    };
    let name = attrpath_name(&attrpath, source);
    if name.is_empty() {
        return Vec::new();
    }

    let qualified_name = qualify(parent, &name);
    let mut children = Vec::new();
    let kind = classify_binding(&name, &expression, nested);

    collect_imports_binding(&name, &expression, source, parent, &mut children);
    if is_module_like(kind) && should_collect_children(&expression) {
        collect_direct_child_bindings(
            &expression,
            source,
            Some(&qualified_name),
            true,
            &mut children,
        );
    }

    let mut entries = children
        .iter()
        .filter(|child| child.kind == SymbolKind::Import)
        .cloned()
        .collect::<Vec<_>>();
    let symbol_children = if name == "imports" {
        Vec::new()
    } else {
        children
    };
    entries.push(symbol_entry(
        node,
        source,
        kind,
        name,
        binding_signature(&attrpath, &expression, source),
        parent.map(str::to_string),
        symbol_children,
    ));
    entries
}

fn collect_direct_child_bindings(
    node: &Node,
    source: &str,
    parent: Option<&str>,
    nested: bool,
    symbols: &mut Vec<SymbolEntry>,
) {
    let mut seen = HashSet::new();
    collect_child_binding_sets(node, source, parent, nested, symbols, &mut seen);
}

fn collect_child_binding_sets(
    node: &Node,
    source: &str,
    parent: Option<&str>,
    nested: bool,
    symbols: &mut Vec<SymbolEntry>,
    seen: &mut HashSet<usize>,
) {
    if symbols.len() >= MAX_CHILD_SYMBOLS {
        return;
    }

    if node.kind() == "binding_set" {
        collect_binding_set_children(node, source, parent, nested, symbols, seen);
        return;
    }

    if !is_child_binding_container(node) {
        return;
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if symbols.len() >= MAX_CHILD_SYMBOLS {
            break;
        }
        collect_child_binding_sets(&child, source, parent, nested, symbols, seen);
    }
}

fn collect_binding_set_children(
    binding_set: &Node,
    source: &str,
    parent: Option<&str>,
    nested: bool,
    symbols: &mut Vec<SymbolEntry>,
    seen: &mut HashSet<usize>,
) {
    if !seen.insert(binding_set.start_byte()) {
        return;
    }

    let mut cursor = binding_set.walk();
    for child in binding_set.named_children(&mut cursor) {
        if symbols.len() >= MAX_CHILD_SYMBOLS {
            break;
        }
        if child.kind() == "binding" {
            symbols.extend(binding_symbols(&child, source, parent, nested));
        }
    }
}

fn is_child_binding_container(node: &Node) -> bool {
    matches!(
        node.kind(),
        "function_expression"
            | "let_expression"
            | "attrset_expression"
            | "rec_attrset_expression"
            | "let_attrset_expression"
    )
}

fn collect_imports_binding(
    name: &str,
    expression: &Node,
    source: &str,
    parent: Option<&str>,
    symbols: &mut Vec<SymbolEntry>,
) {
    if name != "imports" {
        return;
    }

    let mut cursor = expression.walk();
    for child in expression.named_children(&mut cursor) {
        if matches!(
            child.kind(),
            "path_expression" | "spath_expression" | "hpath_expression"
        ) {
            let label = node_text(&child, source).trim().to_string();
            symbols.push(symbol_entry(
                &child,
                source,
                SymbolKind::Import,
                label.clone(),
                label,
                parent.map(str::to_string),
                Vec::new(),
            ));
        }
    }
}

fn collect_import_expressions(
    node: &Node,
    source: &str,
    parent: Option<&str>,
    symbols: &mut Vec<SymbolEntry>,
) {
    if node.kind() == "apply_expression" && is_import_apply(node, source) {
        let signature = first_line_signature(node, source);
        symbols.push(symbol_entry(
            node,
            source,
            SymbolKind::Import,
            import_apply_name(node, source).unwrap_or_else(|| signature.clone()),
            signature,
            parent.map(str::to_string),
            Vec::new(),
        ));
        return;
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_import_expressions(&child, source, parent, symbols);
    }
}

fn classify_binding(name: &str, expression: &Node, nested: bool) -> SymbolKind {
    if expression.kind() == "function_expression" {
        if is_function_module(name, expression) {
            SymbolKind::Module
        } else if nested {
            SymbolKind::Method
        } else {
            SymbolKind::Function
        }
    } else if is_attrset(expression) || is_module_binding(name, expression) {
        SymbolKind::Module
    } else if nested {
        SymbolKind::Field
    } else {
        SymbolKind::Const
    }
}

fn is_module_like(kind: SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::Module | SymbolKind::Function | SymbolKind::Method
    )
}

fn should_collect_children(expression: &Node) -> bool {
    has_descendant_of_kind(expression, "binding_set")
}

fn is_function_module(name: &str, expression: &Node) -> bool {
    name.contains('.') && has_descendant_of_kind(expression, "attrset_expression")
}

fn is_module_binding(name: &str, expression: &Node) -> bool {
    module_name_hint(name)
        || (name.contains('.') && has_descendant_of_kind(expression, "binding_set"))
}

fn is_attrset(node: &Node) -> bool {
    matches!(
        node.kind(),
        "attrset_expression" | "rec_attrset_expression" | "let_attrset_expression"
    )
}

fn module_name_hint(name: &str) -> bool {
    matches!(
        name,
        "packages" | "devShells" | "nixosModules" | "overlays" | "apps" | "checks" | "formatter"
    ) || name.contains("Modules")
}

fn is_import_apply(node: &Node, source: &str) -> bool {
    let Some(function) = child_by_field(node, "function") else {
        return false;
    };
    let text = node_text(&function, source).trim();
    text == "import" || text == "builtins.import"
}

fn import_apply_name(node: &Node, source: &str) -> Option<String> {
    child_by_field(node, "argument")
        .map(|arg| node_text(&arg, source).trim().to_string())
        .filter(|text| !text.is_empty())
}

fn binding_signature(attrpath: &Node, expression: &Node, source: &str) -> String {
    let lhs = node_text(attrpath, source).trim();
    let rhs = first_line_signature(expression, source);
    if rhs.is_empty() {
        format!("{lhs} = ...")
    } else {
        format!("{lhs} = {rhs}")
    }
}

fn attrpath_name(node: &Node, source: &str) -> String {
    node_text(node, source)
        .trim()
        .replace("${", "$")
        .replace('}', "")
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
        .trim_end_matches(';')
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

fn has_descendant_of_kind(node: &Node, kind: &str) -> bool {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == kind || has_descendant_of_kind(&child, kind) {
            return true;
        }
    }
    false
}
