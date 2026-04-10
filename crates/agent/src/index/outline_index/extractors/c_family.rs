//! C and C++ outline extractors.

use tree_sitter::Parser;

use super::helpers::*;
use crate::index::outline_index::common::{IndexOptions, OutlineError, Section, SkeletonEntry};

// ---------------------------------------------------------------------------
// C extractor
// ---------------------------------------------------------------------------

pub fn extract_c(source: &str, options: &IndexOptions) -> Result<Vec<Section>, OutlineError> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_c::LANGUAGE.into())
        .map_err(|e| OutlineError::ParseError(format!("Failed to set C language: {}", e)))?;

    extract_common(&mut parser, source, options, "c")
}

// ---------------------------------------------------------------------------
// C++ extractor
// ---------------------------------------------------------------------------

pub fn extract_cpp(source: &str, options: &IndexOptions) -> Result<Vec<Section>, OutlineError> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_cpp::LANGUAGE.into())
        .map_err(|e| OutlineError::ParseError(format!("Failed to set C++ language: {}", e)))?;

    extract_common(&mut parser, source, options, "cpp")
}

// ---------------------------------------------------------------------------
// Shared C/C++ extraction
// ---------------------------------------------------------------------------

fn extract_common(
    parser: &mut Parser,
    source: &str,
    options: &IndexOptions,
    _lang: &str,
) -> Result<Vec<Section>, OutlineError> {
    let tree = parse_source(parser, source)?;
    let root = tree.root_node();

    let mut includes = Vec::new();
    let mut types = Vec::new();
    let mut functions = Vec::new();
    let mut constants = Vec::new();

    let mut cursor = root.walk();
    for node in root.named_children(&mut cursor) {
        match node.kind() {
            "preproc_include" => {
                let text = node_text(&node, source).trim().to_string();
                includes.push(entry_from_node(text, &node));
            }

            "preproc_def" | "preproc_function_def" => {
                let text = first_line_of(&node, source).trim().to_string();
                constants.push(entry_from_node(text, &node));
            }

            "struct_specifier" | "union_specifier" => {
                let entry = extract_struct_or_union(&node, source, options);
                if let Some(e) = entry {
                    types.push(e);
                }
            }

            "enum_specifier" => {
                let entry = extract_c_enum(&node, source, options);
                if let Some(e) = entry {
                    types.push(e);
                }
            }

            "type_definition" => {
                let text = first_line_of(&node, source).trim().to_string();
                types.push(entry_from_node(text, &node));
            }

            "function_definition" => {
                let sig = extract_c_fn_signature(&node, source);
                functions.push(entry_from_node(sig, &node));
            }

            "declaration" => {
                // Could be a function prototype or a global variable
                let text = node_text(&node, source);
                if text.contains('(') {
                    // Function prototype
                    let label = text.trim().trim_end_matches(';').to_string();
                    let label = label
                        .lines()
                        .map(|l| l.trim())
                        .collect::<Vec<_>>()
                        .join(" ");
                    functions.push(entry_from_node(label, &node));
                } else {
                    let label = first_line_of(&node, source)
                        .trim()
                        .trim_end_matches(';')
                        .to_string();
                    constants.push(entry_from_node(label, &node));
                }
            }

            // C++ specific
            "class_specifier" => {
                let entry = extract_cpp_class(&node, source, options);
                if let Some(e) = entry {
                    types.push(e);
                }
            }

            "namespace_definition" => {
                let name = find_child_by_kind(&node, "identifier")
                    .or_else(|| find_child_by_kind(&node, "namespace_identifier"))
                    .map(|n| node_text(&n, source).to_string())
                    .unwrap_or_else(|| "anonymous".to_string());
                types.push(entry_from_node(format!("namespace {}", name), &node));
            }

            "template_declaration" => {
                // Extract the inner declaration
                let label = first_line_of(&node, source).trim().to_string();
                types.push(entry_from_node(label, &node));
            }

            _ => {}
        }
    }

    let mut sections = Vec::new();
    if !includes.is_empty() {
        sections.push(Section::with_entries("includes", includes));
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

    Ok(sections)
}

// ---------------------------------------------------------------------------
// C/C++ helpers
// ---------------------------------------------------------------------------

fn extract_struct_or_union(
    node: &tree_sitter::Node,
    source: &str,
    options: &IndexOptions,
) -> Option<SkeletonEntry> {
    let kind = node.kind(); // "struct_specifier" or "union_specifier"
    let keyword = if kind == "union_specifier" {
        "union"
    } else {
        "struct"
    };

    let name = find_child_by_kind(node, "type_identifier")
        .map(|n| node_text(&n, source).to_string())
        .unwrap_or_else(|| "anonymous".to_string());

    let fields = extract_c_struct_fields(node, source, options);
    Some(SkeletonEntry::with_children(
        format!("{} {}", keyword, name),
        start_line(node),
        end_line(node),
        fields,
    ))
}

fn extract_c_struct_fields(
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
            let label = node_text(&child, source)
                .trim()
                .trim_end_matches(';')
                .to_string();
            fields.push(entry_from_node(label, &child));
        }
    }
    truncate_children(fields, options.max_children_per_item)
}

fn extract_c_enum(
    node: &tree_sitter::Node,
    source: &str,
    options: &IndexOptions,
) -> Option<SkeletonEntry> {
    let name = find_child_by_kind(node, "type_identifier")
        .map(|n| node_text(&n, source).to_string())
        .unwrap_or_else(|| "anonymous".to_string());

    let body = find_child_by_kind(node, "enumerator_list");
    let variants = if let Some(body) = body {
        let mut variants = Vec::new();
        let mut cursor = body.walk();
        for child in body.named_children(&mut cursor) {
            if child.kind() == "enumerator" {
                let label = node_text(&child, source).trim().to_string();
                variants.push(entry_from_node(label, &child));
            }
        }
        truncate_children(variants, options.max_children_per_item)
    } else {
        Vec::new()
    };

    Some(SkeletonEntry::with_children(
        format!("enum {}", name),
        start_line(node),
        end_line(node),
        variants,
    ))
}

fn extract_c_fn_signature(node: &tree_sitter::Node, source: &str) -> String {
    let text = node_text(node, source);
    if let Some(brace_pos) = text.find('{') {
        text[..brace_pos]
            .lines()
            .map(|l| l.trim())
            .collect::<Vec<_>>()
            .join(" ")
            .trim()
            .to_string()
    } else {
        first_line_of(node, source).trim().to_string()
    }
}

fn extract_cpp_class(
    node: &tree_sitter::Node,
    source: &str,
    options: &IndexOptions,
) -> Option<SkeletonEntry> {
    let name = find_child_by_kind(node, "type_identifier")
        .map(|n| node_text(&n, source).to_string())
        .unwrap_or_else(|| "anonymous".to_string());

    let body = find_child_by_kind(node, "field_declaration_list");
    let members = if let Some(body) = body {
        let mut members = Vec::new();
        let mut cursor = body.walk();
        for child in body.named_children(&mut cursor) {
            match child.kind() {
                "function_definition" => {
                    let sig = extract_c_fn_signature(&child, source);
                    members.push(entry_from_node(sig, &child));
                }
                "declaration" | "field_declaration" => {
                    let label = node_text(&child, source)
                        .trim()
                        .trim_end_matches(';')
                        .to_string();
                    members.push(entry_from_node(label, &child));
                }
                "access_specifier" => {
                    let label = node_text(&child, source).trim().to_string();
                    members.push(entry_from_node(label, &child));
                }
                _ => {}
            }
        }
        truncate_children(members, options.max_children_per_item)
    } else {
        Vec::new()
    };

    Some(SkeletonEntry::with_children(
        format!("class {}", name),
        start_line(node),
        end_line(node),
        members,
    ))
}
