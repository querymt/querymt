//! Go outline extractor.

use tree_sitter::Parser;

use super::helpers::*;
use crate::index::outline_index::common::{IndexOptions, OutlineError, Section, SkeletonEntry};

pub fn extract(source: &str, options: &IndexOptions) -> Result<Vec<Section>, OutlineError> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_go::LANGUAGE.into())
        .map_err(|e| OutlineError::ParseError(format!("Failed to set Go language: {}", e)))?;

    let tree = parse_source(&mut parser, source)?;
    let root = tree.root_node();

    let mut imports = Vec::new();
    let mut types = Vec::new();
    let mut functions = Vec::new();
    let mut tests = Vec::new();
    let mut constants = Vec::new();

    let mut cursor = root.walk();
    for node in root.named_children(&mut cursor) {
        match node.kind() {
            "import_declaration" => {
                let text = node_text(&node, source)
                    .lines()
                    .map(|l| l.trim())
                    .collect::<Vec<_>>()
                    .join(" ");
                imports.push(entry_from_node(text, &node));
            }

            "type_declaration" => {
                // type_declaration contains type_spec children
                let mut inner_cursor = node.walk();
                for child in node.named_children(&mut inner_cursor) {
                    if child.kind() == "type_spec" {
                        extract_type_spec(&child, source, options, &mut types);
                    }
                }
            }

            "function_declaration" => {
                let sig = extract_go_fn_signature(&node, source);
                let name = find_child_by_kind(&node, "identifier")
                    .map(|n| node_text(&n, source).to_string())
                    .unwrap_or_default();

                if is_test_function(&name) {
                    if options.include_tests {
                        tests.push(entry_from_node(sig, &node));
                    }
                } else {
                    functions.push(entry_from_node(sig, &node));
                }
            }

            "method_declaration" => {
                let sig = extract_go_method_signature(&node, source);
                let name = find_child_by_kind(&node, "field_identifier")
                    .map(|n| node_text(&n, source).to_string())
                    .unwrap_or_default();

                if is_test_function(&name) {
                    if options.include_tests {
                        tests.push(entry_from_node(sig, &node));
                    }
                } else {
                    functions.push(entry_from_node(sig, &node));
                }
            }

            "const_declaration" | "var_declaration" => {
                let text = node_text(&node, source)
                    .lines()
                    .map(|l| l.trim())
                    .collect::<Vec<_>>()
                    .join(" ");
                // Truncate long const blocks
                let label = if text.len() > 120 {
                    format!("{}...", &text[..117])
                } else {
                    text
                };
                constants.push(entry_from_node(label, &node));
            }

            _ => {}
        }
    }

    let mut sections = Vec::new();
    if !imports.is_empty() {
        sections.push(Section::with_entries("imports", imports));
    }
    if !types.is_empty() {
        sections.push(Section::with_entries("types", types));
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
// Go-specific helpers
// ---------------------------------------------------------------------------

fn extract_type_spec(
    node: &tree_sitter::Node,
    source: &str,
    options: &IndexOptions,
    types: &mut Vec<SkeletonEntry>,
) {
    let name = find_child_by_kind(node, "type_identifier")
        .map(|n| node_text(&n, source).to_string())
        .unwrap_or_default();

    // Check what kind of type it is
    if let Some(struct_type) = find_child_by_kind(node, "struct_type") {
        let fields = extract_struct_fields(&struct_type, source, options);
        types.push(SkeletonEntry::with_children(
            format!("type {} struct", name),
            start_line(node),
            end_line(node),
            fields,
        ));
    } else if let Some(iface_type) = find_child_by_kind(node, "interface_type") {
        let methods = extract_interface_methods(&iface_type, source, options);
        types.push(SkeletonEntry::with_children(
            format!("type {} interface", name),
            start_line(node),
            end_line(node),
            methods,
        ));
    } else {
        // type alias or other
        let label = first_line_of(node, source).trim().to_string();
        types.push(entry_from_node(format!("type {}", label), node));
    }
}

fn extract_struct_fields(
    node: &tree_sitter::Node,
    source: &str,
    options: &IndexOptions,
) -> Vec<SkeletonEntry> {
    let body = match find_child_by_kind(node, "field_declaration_list") {
        Some(b) => b,
        None => return Vec::new(),
    };

    let mut fields = Vec::new();
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        if child.kind() == "field_declaration" {
            let label = node_text(&child, source).trim().to_string();
            fields.push(entry_from_node(label, &child));
        }
    }
    truncate_children(fields, options.max_children_per_item)
}

fn extract_interface_methods(
    node: &tree_sitter::Node,
    source: &str,
    options: &IndexOptions,
) -> Vec<SkeletonEntry> {
    let mut methods = Vec::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "method_spec" => {
                let label = node_text(&child, source).trim().to_string();
                methods.push(entry_from_node(label, &child));
            }
            "type_identifier" => {
                // Embedded interface
                let label = node_text(&child, source).trim().to_string();
                methods.push(entry_from_node(label, &child));
            }
            _ => {}
        }
    }
    truncate_children(methods, options.max_children_per_item)
}

fn extract_go_fn_signature(node: &tree_sitter::Node, source: &str) -> String {
    let name = find_child_by_kind(node, "identifier")
        .map(|n| node_text(&n, source).to_string())
        .unwrap_or_else(|| "unknown".to_string());

    let params = find_child_by_kind(node, "parameter_list")
        .map(|n| {
            node_text(&n, source)
                .lines()
                .map(|l| l.trim())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_else(|| "()".to_string());

    let result = extract_go_result_type(node, source);

    if result.is_empty() {
        format!("func {}{}", name, params)
    } else {
        format!("func {}{} {}", name, params, result)
    }
}

fn extract_go_method_signature(node: &tree_sitter::Node, source: &str) -> String {
    let receiver = find_child_by_kind(node, "parameter_list")
        .map(|n| node_text(&n, source).to_string())
        .unwrap_or_default();

    let name = find_child_by_kind(node, "field_identifier")
        .map(|n| node_text(&n, source).to_string())
        .unwrap_or_else(|| "unknown".to_string());

    // The second parameter_list is the actual params
    let mut param_lists = Vec::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "parameter_list" {
            param_lists.push(child);
        }
    }

    let params = if param_lists.len() > 1 {
        node_text(&param_lists[1], source)
            .lines()
            .map(|l| l.trim())
            .collect::<Vec<_>>()
            .join(" ")
    } else {
        "()".to_string()
    };

    let result = extract_go_result_type(node, source);

    if result.is_empty() {
        format!("func {} {}{}", receiver, name, params)
    } else {
        format!("func {} {}{} {}", receiver, name, params, result)
    }
}

fn extract_go_result_type(node: &tree_sitter::Node, source: &str) -> String {
    // Look for result type nodes after parameter lists
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "parameter_list" => continue,
            "block" => break,
            "type_identifier" | "pointer_type" | "slice_type" | "map_type" | "qualified_type"
            | "generic_type" => {
                return node_text(&child, source).trim().to_string();
            }
            _ => {}
        }
    }
    String::new()
}

fn is_test_function(name: &str) -> bool {
    name.starts_with("Test")
        || name.starts_with("Benchmark")
        || name.starts_with("Example")
        || name.starts_with("Fuzz")
}
