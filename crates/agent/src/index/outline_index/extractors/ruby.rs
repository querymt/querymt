//! Ruby outline extractor.

use tree_sitter::Parser;

use super::helpers::*;
use crate::index::outline_index::common::{IndexOptions, OutlineError, Section, SkeletonEntry};

pub fn extract(source: &str, options: &IndexOptions) -> Result<Vec<Section>, OutlineError> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_ruby::LANGUAGE.into())
        .map_err(|e| OutlineError::ParseError(format!("Failed to set Ruby language: {}", e)))?;

    let tree = parse_source(&mut parser, source)?;
    let root = tree.root_node();

    let mut requires = Vec::new();
    let mut classes = Vec::new();
    let mut modules = Vec::new();
    let mut functions = Vec::new();
    let mut tests = Vec::new();
    let mut constants = Vec::new();

    let mut cursor = root.walk();
    for node in root.named_children(&mut cursor) {
        match node.kind() {
            "call" => {
                let text = node_text(&node, source);
                if text.starts_with("require") || text.starts_with("require_relative") {
                    let label = first_line_of(&node, source).trim().to_string();
                    requires.push(entry_from_node(label, &node));
                } else if is_rspec_block(&node, source) && options.include_tests {
                    let label = extract_rspec_label(&node, source);
                    tests.push(entry_from_node(label, &node));
                }
            }

            "class" => {
                let entry = extract_ruby_class(&node, source, options);
                classes.push(entry);
            }

            "module" => {
                let name = find_child_by_kind(&node, "constant")
                    .or_else(|| find_child_by_kind(&node, "scope_resolution"))
                    .map(|n| node_text(&n, source).to_string())
                    .unwrap_or_default();
                let methods = extract_ruby_body_methods(&node, source, options);
                modules.push(SkeletonEntry::with_children(
                    format!("module {}", name),
                    start_line(&node),
                    end_line(&node),
                    methods,
                ));
            }

            "method" => {
                let sig = extract_ruby_method_sig(&node, source);
                let name = find_child_by_kind(&node, "identifier")
                    .map(|n| node_text(&n, source).to_string())
                    .unwrap_or_default();

                if is_test_method(&name) {
                    if options.include_tests {
                        tests.push(entry_from_node(sig, &node));
                    }
                } else {
                    functions.push(entry_from_node(sig, &node));
                }
            }

            "singleton_method" => {
                let sig = extract_ruby_singleton_method_sig(&node, source);
                functions.push(entry_from_node(sig, &node));
            }

            "assignment" => {
                let text = node_text(&node, source);
                // Ruby constants start with uppercase
                let lhs = text.split('=').next().unwrap_or("").trim();
                if lhs.chars().next().is_some_and(|c| c.is_uppercase()) {
                    let label = first_line_of(&node, source).trim().to_string();
                    constants.push(entry_from_node(label, &node));
                }
            }

            _ => {}
        }
    }

    let mut sections = Vec::new();
    if !requires.is_empty() {
        sections.push(Section::with_entries("requires", requires));
    }
    if !modules.is_empty() {
        sections.push(Section::with_entries("modules", modules));
    }
    if !classes.is_empty() {
        sections.push(Section::with_entries("classes", classes));
    }
    if !functions.is_empty() {
        sections.push(Section::with_entries("functions", functions));
    }
    if !constants.is_empty() {
        sections.push(Section::with_entries("constants", constants));
    }
    if !tests.is_empty() {
        sections.push(Section::with_entries("tests", tests));
    }

    Ok(sections)
}

// ---------------------------------------------------------------------------
// Ruby-specific helpers
// ---------------------------------------------------------------------------

fn extract_ruby_class(
    node: &tree_sitter::Node,
    source: &str,
    options: &IndexOptions,
) -> SkeletonEntry {
    let name = find_child_by_kind(node, "constant")
        .or_else(|| find_child_by_kind(node, "scope_resolution"))
        .map(|n| node_text(&n, source).to_string())
        .unwrap_or_default();

    // Check for superclass
    let superclass = find_child_by_kind(node, "superclass")
        .map(|n| {
            format!(
                " < {}",
                node_text(&n, source).trim().trim_start_matches('<').trim()
            )
        })
        .unwrap_or_default();

    let methods = extract_ruby_body_methods(node, source, options);

    SkeletonEntry::with_children(
        format!("class {}{}", name, superclass),
        start_line(node),
        end_line(node),
        methods,
    )
}

fn extract_ruby_body_methods(
    node: &tree_sitter::Node,
    source: &str,
    options: &IndexOptions,
) -> Vec<SkeletonEntry> {
    // Ruby class/module bodies: look for method nodes in the body
    let body = find_child_by_kind(node, "body_statement");
    let body = match body {
        Some(b) => b,
        None => return Vec::new(),
    };

    let mut methods = Vec::new();
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        match child.kind() {
            "method" => {
                let sig = extract_ruby_method_sig(&child, source);
                methods.push(entry_from_node(sig, &child));
            }
            "singleton_method" => {
                let sig = extract_ruby_singleton_method_sig(&child, source);
                methods.push(entry_from_node(sig, &child));
            }
            _ => {}
        }
    }
    truncate_children(methods, options.max_children_per_item)
}

fn extract_ruby_method_sig(node: &tree_sitter::Node, source: &str) -> String {
    let name = find_child_by_kind(node, "identifier")
        .map(|n| node_text(&n, source).to_string())
        .unwrap_or_else(|| "unknown".to_string());

    let params = find_child_by_kind(node, "method_parameters")
        .map(|n| node_text(&n, source).to_string())
        .unwrap_or_default();

    if params.is_empty() {
        format!("def {}", name)
    } else {
        format!("def {}{}", name, params)
    }
}

fn extract_ruby_singleton_method_sig(node: &tree_sitter::Node, source: &str) -> String {
    first_line_of(node, source).trim().to_string()
}

fn is_test_method(name: &str) -> bool {
    name.starts_with("test_")
}

fn is_rspec_block(node: &tree_sitter::Node, source: &str) -> bool {
    let text = node_text(node, source);
    text.starts_with("describe ")
        || text.starts_with("context ")
        || text.starts_with("it ")
        || text.starts_with("RSpec.describe")
}

fn extract_rspec_label(node: &tree_sitter::Node, source: &str) -> String {
    first_line_of(node, source).trim().to_string()
}
