use tree_sitter::{Node, Parser};

use super::super::types::SymbolDigest;
use super::safe_slice;
use crate::index::symbol_index::{SymbolEntry, SymbolError, SymbolKind};

pub fn extract(source: &str) -> Result<Vec<SymbolEntry>, SymbolError> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_elixir::LANGUAGE.into())
        .map_err(|e| {
            SymbolError::ParseError(format!("Failed to set Elixir parser language: {e}"))
        })?;

    let tree = parser
        .parse(source, None)
        .ok_or_else(|| SymbolError::ParseError("Failed to parse Elixir source".to_string()))?;

    let mut symbols = Vec::new();
    let root = tree.root_node();
    collect_container_children(&root, source, None, false, &mut symbols);

    Ok(symbols)
}

fn collect_container_children(
    node: &Node,
    source: &str,
    parent: Option<&str>,
    in_exunit: bool,
    symbols: &mut Vec<SymbolEntry>,
) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_symbol(&child, source, parent, in_exunit, symbols);
    }
}

fn collect_symbol(
    node: &Node,
    source: &str,
    parent: Option<&str>,
    in_exunit: bool,
    symbols: &mut Vec<SymbolEntry>,
) {
    if node.kind() != "call" {
        collect_container_children(node, source, parent, in_exunit, symbols);
        return;
    }

    let text = node_text(node, source).trim_start();
    let Some(keyword) = call_keyword(text) else {
        collect_container_children(node, source, parent, in_exunit, symbols);
        return;
    };

    match keyword {
        "defmodule" | "defprotocol" | "defimpl" => {
            let name = module_name(text, keyword).unwrap_or_else(|| "anonymous".to_string());
            let qualified_name = qualify(parent, &name);
            let mut children = Vec::new();
            let module_is_exunit = keyword == "defmodule" && module_uses_exunit_case(node, source);
            collect_body_calls(
                node,
                source,
                Some(&qualified_name),
                module_is_exunit,
                &mut children,
            );
            let kind = if keyword == "defprotocol" {
                SymbolKind::Trait
            } else if keyword == "defimpl" {
                SymbolKind::Impl
            } else {
                SymbolKind::Module
            };
            symbols.push(symbol_entry(
                node,
                source,
                kind,
                name,
                first_line_signature(node, source),
                parent.map(str::to_string),
                children,
            ));
        }
        "def" | "defp" => {
            let name = function_name(text, keyword).unwrap_or_else(|| "unknown".to_string());
            let kind = if is_test_function(parent, &name) {
                SymbolKind::Test
            } else if parent.is_some() {
                SymbolKind::Method
            } else {
                SymbolKind::Function
            };
            symbols.push(symbol_entry(
                node,
                source,
                kind,
                name,
                first_line_signature(node, source),
                parent.map(str::to_string),
                Vec::new(),
            ));
        }
        "defmacro" | "defmacrop" => {
            let name = function_name(text, keyword).unwrap_or_else(|| "unknown".to_string());
            symbols.push(symbol_entry(
                node,
                source,
                SymbolKind::Macro,
                name,
                first_line_signature(node, source),
                parent.map(str::to_string),
                Vec::new(),
            ));
        }
        "alias" | "import" | "require" | "use" => {
            let signature = first_line_signature(node, source);
            symbols.push(symbol_entry(
                node,
                source,
                SymbolKind::Import,
                import_name(text, keyword).unwrap_or_else(|| signature.clone()),
                signature,
                parent.map(str::to_string),
                Vec::new(),
            ));
        }
        "describe" | "test" => {
            let signature = first_line_signature(node, source);
            let name = test_name(text, keyword).unwrap_or_else(|| signature.clone());
            let qualified_name = qualify(parent, &name);
            let mut children = Vec::new();
            if keyword == "describe" {
                collect_body_calls(
                    node,
                    source,
                    Some(&qualified_name),
                    in_exunit,
                    &mut children,
                );
            }
            let kind = if in_exunit {
                SymbolKind::Test
            } else if parent.is_some() {
                SymbolKind::Method
            } else {
                SymbolKind::Function
            };
            symbols.push(symbol_entry(
                node,
                source,
                kind,
                name,
                signature,
                parent.map(str::to_string),
                children,
            ));
        }
        _ => collect_container_children(node, source, parent, in_exunit, symbols),
    }
}

fn collect_body_calls(
    node: &Node,
    source: &str,
    parent: Option<&str>,
    in_exunit: bool,
    symbols: &mut Vec<SymbolEntry>,
) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if matches!(child.kind(), "do_block" | "stab_clause") {
            collect_descendant_calls(&child, source, parent, in_exunit, symbols);
        }
    }
}

fn collect_descendant_calls(
    node: &Node,
    source: &str,
    parent: Option<&str>,
    in_exunit: bool,
    symbols: &mut Vec<SymbolEntry>,
) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "call" {
            collect_symbol(&child, source, parent, in_exunit, symbols);
        } else {
            collect_descendant_calls(&child, source, parent, in_exunit, symbols);
        }
    }
}

fn module_uses_exunit_case(node: &Node, source: &str) -> bool {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if matches!(child.kind(), "do_block" | "stab_clause")
            && body_uses_exunit_case(&child, source)
        {
            return true;
        }
    }
    false
}

fn body_uses_exunit_case(node: &Node, source: &str) -> bool {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "call" {
            let text = node_text(&child, source).trim_start();
            if call_keyword(text) == Some("use")
                && import_name(text, "use").is_some_and(is_exunit_case_use)
            {
                return true;
            }
            continue;
        }

        if body_uses_exunit_case(&child, source) {
            return true;
        }
    }
    false
}

fn is_exunit_case_use(name: String) -> bool {
    let name = name.trim_start_matches('(').trim_start();
    name == "ExUnit.Case" || name.starts_with("ExUnit.Case,")
}

fn call_keyword(text: &str) -> Option<&str> {
    const KEYWORDS: &[&str] = &[
        "defmacrop",
        "defmacro",
        "defmodule",
        "defprotocol",
        "defimpl",
        "defp",
        "def",
        "alias",
        "import",
        "require",
        "use",
        "describe",
        "test",
    ];
    KEYWORDS.iter().copied().find(|keyword| {
        text == *keyword
            || text
                .strip_prefix(*keyword)
                .is_some_and(|rest| rest.starts_with(char::is_whitespace) || rest.starts_with('('))
    })
}

fn module_name(text: &str, keyword: &str) -> Option<String> {
    let rest = strip_keyword(text, keyword)?;
    if keyword == "defimpl" {
        return Some(
            rest.split(" do")
                .next()
                .unwrap_or(rest)
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .collect::<Vec<_>>()
                .join(" "),
        )
        .filter(|name| !name.is_empty());
    }

    Some(
        rest.split([',', '\n'])
            .next()
            .unwrap_or(rest)
            .trim()
            .trim_end_matches(" do")
            .to_string(),
    )
    .filter(|name| !name.is_empty())
}

fn function_name(text: &str, keyword: &str) -> Option<String> {
    let rest = strip_keyword(text, keyword)?;
    let rest = rest.trim_start_matches('(').trim_start();
    let name: String = rest
        .chars()
        .take_while(|c| c.is_alphanumeric() || matches!(c, '_' | '!' | '?' | '@'))
        .collect();
    if name.is_empty() { None } else { Some(name) }
}

fn import_name(text: &str, keyword: &str) -> Option<String> {
    strip_keyword(text, keyword).map(|rest| {
        rest.split('\n')
            .next()
            .unwrap_or(rest)
            .trim()
            .trim_end_matches(',')
            .to_string()
    })
}

fn test_name(text: &str, keyword: &str) -> Option<String> {
    let rest = strip_keyword(text, keyword)?;
    let trimmed = rest.trim_start_matches('(').trim_start();
    if let Some(stripped) = trimmed.strip_prefix('"') {
        return stripped.split('"').next().map(str::to_string);
    }
    Some(
        trimmed
            .split(['\n', ','])
            .next()
            .unwrap_or(trimmed)
            .trim()
            .to_string(),
    )
    .filter(|name| !name.is_empty())
}

fn strip_keyword<'a>(text: &'a str, keyword: &str) -> Option<&'a str> {
    text.trim_start()
        .strip_prefix(keyword)
        .map(|rest| rest.trim_start())
}

fn is_test_function(parent: Option<&str>, name: &str) -> bool {
    parent
        .and_then(|parent| parent.rsplit("::").next())
        .is_some_and(|module| module.ends_with("Test"))
        && (name.starts_with("test_") || name.ends_with("_test"))
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
