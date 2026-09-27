use tree_sitter::{Node, Parser};

use super::super::types::SymbolDigest;
use super::safe_slice;
use crate::index::symbol_index::{SymbolEntry, SymbolError, SymbolKind};

pub fn extract(source: &str) -> Result<Vec<SymbolEntry>, SymbolError> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_yaml::LANGUAGE.into())
        .map_err(|e| SymbolError::ParseError(format!("Failed to load YAML grammar: {e}")))?;

    let tree = parser
        .parse(source, None)
        .ok_or_else(|| SymbolError::ParseError("Failed to parse YAML source".to_string()))?;

    // stream > document > block_node > block_mapping
    let mut symbols = Vec::new();
    if let Some(mapping) = find_root_mapping(tree.root_node()) {
        collect_pairs(&mapping, source, &mut symbols);
    }
    Ok(symbols)
}

fn find_root_mapping<'a>(root: Node<'a>) -> Option<Node<'a>> {
    let mut cursor = root.walk();
    let document = root
        .named_children(&mut cursor)
        .find(|child| child.kind() == "document")?;
    let mut cursor = document.walk();
    let block_node = document
        .named_children(&mut cursor)
        .find(|child| child.kind() == "block_node")?;
    find_mapping_in(block_node)
}

fn find_mapping_in<'a>(node: Node<'a>) -> Option<Node<'a>> {
    if node.kind() == "block_mapping" {
        return Some(node);
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if let Some(found) = find_mapping_in(child) {
            return Some(found);
        }
    }
    None
}

fn collect_pairs(mapping: &Node, source: &str, symbols: &mut Vec<SymbolEntry>) {
    let mut cursor = mapping.walk();
    for pair in mapping.named_children(&mut cursor) {
        if pair.kind() != "block_mapping_pair" {
            continue;
        }
        let Some(key_node) = pair.child_by_field_name("key") else {
            continue;
        };
        let key = scalar_text(&key_node, source);
        let value = pair.child_by_field_name("value");

        let (children, value_label) = match &value.map(unwrap_node) {
            Some(v) if v.kind() == "block_mapping" => {
                let mut nested = Vec::new();
                collect_pairs(v, source, &mut nested);
                (nested, " {...}".to_string())
            }
            Some(v) if v.kind() == "block_sequence" => {
                let items = v.named_child_count();
                (Vec::new(), format!(" [{items} items]"))
            }
            Some(v) => (Vec::new(), format!(" {}", scalar_text(v, source))),
            None => (Vec::new(), String::new()),
        };

        symbols.push(symbol_entry(
            &pair,
            source,
            SymbolKind::Const,
            key.clone(),
            format!("{key}:{value_label}"),
            children,
        ));
    }
}

/// Peek through `block_node`/`flow_node` wrappers to the concrete value node.
fn unwrap_node<'a>(node: Node<'a>) -> Node<'a> {
    match node.kind() {
        "block_node" | "flow_node" => match node.named_child(0) {
            Some(child) => unwrap_node(child),
            None => node,
        },
        _ => node,
    }
}

/// Extract display text from flow/block scalar wrappers.
fn scalar_text(node: &Node, source: &str) -> String {
    let raw = node_text(node, source).trim();
    if raw.lines().count() > 1 {
        raw.lines().next().unwrap_or("").trim().to_string()
    } else {
        raw.trim_matches(['"', '\'']).to_string()
    }
}

fn symbol_entry(
    node: &Node,
    source: &str,
    kind: SymbolKind,
    name: String,
    signature: String,
    children: Vec<SymbolEntry>,
) -> SymbolEntry {
    let text = node_text(node, source);
    SymbolEntry {
        kind,
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
        children,
        digest: SymbolDigest::new(text.as_bytes(), text.lines().count()),
    }
}

fn node_text<'a>(node: &Node, source: &'a str) -> &'a str {
    safe_slice(source, node.start_byte(), node.end_byte())
}
