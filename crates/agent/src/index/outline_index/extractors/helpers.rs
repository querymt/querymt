//! Shared helpers for tree-sitter-based outline extraction.

use tree_sitter::{Node, Parser, Tree};

use crate::index::outline_index::common::{OutlineError, SkeletonEntry};

/// Parse source text with a tree-sitter parser and return the tree.
pub fn parse_source(parser: &mut Parser, source: &str) -> Result<Tree, OutlineError> {
    parser
        .parse(source, None)
        .ok_or_else(|| OutlineError::ParseError("tree-sitter parse returned None".into()))
}

/// Safely slice `source` between byte offsets, snapping both boundaries to
/// valid UTF-8 char boundaries.  This prevents panics when a byte offset
/// lands inside a multi-byte character (e.g. from arithmetic like
/// `start.saturating_sub(N)`).
pub fn safe_slice(source: &str, from: usize, to: usize) -> &str {
    let from = source.floor_char_boundary(from.min(source.len()));
    let to = source.ceil_char_boundary(to.min(source.len()));
    &source[from..to]
}

/// Extract the text of a node from the source.
pub fn node_text<'a>(node: &Node, source: &'a str) -> &'a str {
    let start = node.start_byte();
    let end = node.end_byte();
    debug_assert!(
        source.is_char_boundary(start) && source.is_char_boundary(end),
        "tree-sitter returned byte offsets that are not on char boundaries: \
         start={start}, end={end}, len={}",
        source.len(),
    );
    safe_slice(source, start, end)
}

/// 1-based start line for a node.
pub fn start_line(node: &Node) -> usize {
    node.start_position().row + 1
}

/// 1-based end line for a node.
pub fn end_line(node: &Node) -> usize {
    node.end_position().row + 1
}

/// Create a [`SkeletonEntry`] from a node with the given label.
pub fn entry_from_node(label: impl Into<String>, node: &Node) -> SkeletonEntry {
    SkeletonEntry::new(label, start_line(node), end_line(node))
}

/// Extract the first line of text for a node (useful for signature extraction).
pub fn first_line_of(node: &Node, source: &str) -> String {
    let text = node_text(node, source);
    text.lines().next().unwrap_or("").to_string()
}

/// Find the first named child with a given kind.
pub fn find_child_by_kind<'a>(node: &'a Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find(|&child| child.kind() == kind)
}

/// Find the first named child matching any of the given kinds.
pub fn find_child_by_kinds<'a>(node: &'a Node<'a>, kinds: &[&str]) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find(|&child| kinds.contains(&child.kind()))
}

/// Truncate a list of children to `max` entries, appending a summary entry
/// if truncation occurred.
pub fn truncate_children(
    mut children: Vec<SkeletonEntry>,
    max: Option<usize>,
) -> Vec<SkeletonEntry> {
    if let Some(max) = max
        && children.len() > max
    {
        let total = children.len();
        children.truncate(max);
        children.push(SkeletonEntry::new(
            format!("... ({} more)", total - max),
            0,
            0,
        ));
    }
    children
}
