use tree_sitter::{Node, Parser};

use super::super::types::SymbolDigest;
use super::safe_slice;
use super::typescript;
use crate::index::symbol_index::{SymbolEntry, SymbolError, SymbolKind};

pub fn extract(source: &str) -> Result<Vec<SymbolEntry>, SymbolError> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_svelte_ng::LANGUAGE.into())
        .map_err(|e| SymbolError::ParseError(format!("Failed to load Svelte grammar: {e}")))?;

    let tree = parser
        .parse(source, None)
        .ok_or_else(|| SymbolError::ParseError("Failed to parse Svelte source".to_string()))?;

    let mut symbols = Vec::new();
    let root = tree.root_node();
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        match child.kind() {
            "script_element" => {
                symbols.extend(script_symbols(&child, source)?);
            }
            "element" => {
                if let Some(entry) = element_symbol(&child, source) {
                    symbols.push(entry);
                }
            }
            "snippet_statement" => symbols.push(snippet_symbol(&child, source)),
            "if_statement" | "each_statement" | "await_statement" | "key_statement" => {
                symbols.push(markup_symbol(&child, source));
            }
            "style_element" => symbols.push(markup_symbol(&child, source)),
            _ => {}
        }
    }
    Ok(symbols)
}

/// Re-parse the `<script>` body with the TypeScript grammar and return its
/// symbols offset back to whole-file coordinates. Symbols are flattened to the
/// top level so imports/functions/constants land in their outline sections.
fn script_symbols(node: &Node, source: &str) -> Result<Vec<SymbolEntry>, SymbolError> {
    let Some(raw_text) = find_child_by_kind(node, "raw_text") else {
        return Ok(Vec::new());
    };
    let script_source = safe_slice(source, raw_text.start_byte(), raw_text.end_byte());
    let mut inner = typescript::extract(script_source, "typescript")?;
    offset_symbols(
        &mut inner,
        raw_text.start_position().row,
        raw_text.start_byte(),
    );
    Ok(inner)
}

/// Recursively shift symbol line/byte offsets from script-snippet coordinates
/// to whole-file coordinates. Digests are content-relative and stay untouched.
fn offset_symbols(symbols: &mut [SymbolEntry], line_offset: usize, byte_offset: usize) {
    for symbol in symbols {
        symbol.start_line += line_offset;
        symbol.end_line += line_offset;
        symbol.start_byte += byte_offset;
        symbol.end_byte += byte_offset;
        if let Some(line) = symbol.body_start_line.as_mut() {
            *line += line_offset;
        }
        if let Some(line) = symbol.body_end_line.as_mut() {
            *line += line_offset;
        }
        offset_symbols(&mut symbol.children, line_offset, byte_offset);
    }
}

/// Top-level markup element: `<Tag attr1 attr2 ...>` (names only, no values).
fn element_symbol(node: &Node, source: &str) -> Option<SymbolEntry> {
    let tag = find_child_by_kind(node, "start_tag")
        .or_else(|| find_child_by_kind(node, "self_closing_tag"))?;
    let tag_name =
        find_child_by_kind(&tag, "tag_name").map(|n| node_text(&n, source).to_string())?;

    let mut cursor = tag.walk();
    let attrs: Vec<String> = tag
        .named_children(&mut cursor)
        .filter(|child| child.kind() == "attribute")
        .map(|attribute| {
            find_child_by_kind(&attribute, "attribute_name")
                .map(|n| node_text(&n, source).to_string())
                .unwrap_or_default()
        })
        .collect();

    let mut signature = format!("<{tag_name}");
    for attr in attrs {
        signature.push(' ');
        signature.push_str(&attr);
    }
    signature.push('>');

    Some(markup_entry(node, source, signature))
}

/// `{#snippet name(args)}` → Function.
fn snippet_symbol(node: &Node, source: &str) -> SymbolEntry {
    // The name lives under the `snippet_start` opener node.
    let name = find_child_by_kind(node, "snippet_start")
        .and_then(|start| {
            let mut cursor = start.walk();
            start
                .named_children(&mut cursor)
                .find(|child| child.kind() == "snippet_name")
                .map(|n| node_text(&n, source).to_string())
        })
        .unwrap_or_else(|| "snippet".to_string());
    let signature = first_line(node, source);
    let text = node_text(node, source);
    SymbolEntry {
        kind: SymbolKind::Function,
        qualified_name: name.clone(),
        name,
        signature,
        start_line: node.start_position().row + 1,
        end_line: node.end_position().row + 1,
        start_byte: node.start_byte(),
        end_byte: node.end_byte(),
        body_start_line: None,
        body_end_line: None,
        parent: None,
        children: Vec::new(),
        digest: SymbolDigest::new(text.as_bytes(), text.lines().count()),
    }
}

/// Template blocks (`{#if ...}`, `{#each ...}`, `{#await ...}`, `{#key ...}`)
/// and `<style>` → compact markup entries (first line only).
fn markup_symbol(node: &Node, source: &str) -> SymbolEntry {
    markup_entry(node, source, first_line(node, source))
}

fn markup_entry(node: &Node, source: &str, signature: String) -> SymbolEntry {
    let text = node_text(node, source);
    let name = signature.clone();
    SymbolEntry {
        kind: SymbolKind::Module,
        qualified_name: name.clone(),
        name,
        signature,
        start_line: node.start_position().row + 1,
        end_line: node.end_position().row + 1,
        start_byte: node.start_byte(),
        end_byte: node.end_byte(),
        body_start_line: None,
        body_end_line: None,
        parent: None,
        children: Vec::new(),
        digest: SymbolDigest::new(text.as_bytes(), text.lines().count()),
    }
}

fn first_line(node: &Node, source: &str) -> String {
    node_text(node, source)
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .to_string()
}

fn node_text<'a>(node: &Node, source: &'a str) -> &'a str {
    safe_slice(source, node.start_byte(), node.end_byte())
}

fn find_child_by_kind<'a>(node: &'a Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find(|&child| child.kind() == kind)
}
