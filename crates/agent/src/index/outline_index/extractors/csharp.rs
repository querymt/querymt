//! C# outline extractor.

use tree_sitter::Parser;

use super::helpers::*;
use crate::index::outline_index::common::{IndexOptions, OutlineError, Section, SkeletonEntry};

pub fn extract(source: &str, options: &IndexOptions) -> Result<Vec<Section>, OutlineError> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_c_sharp::LANGUAGE.into())
        .map_err(|e| OutlineError::ParseError(format!("Failed to set C# language: {}", e)))?;

    let tree = parse_source(&mut parser, source)?;
    let root = tree.root_node();

    let mut usings = Vec::new();
    let mut namespaces = Vec::new();
    let mut classes = Vec::new();
    let mut interfaces = Vec::new();
    let mut enums = Vec::new();

    let mut cursor = root.walk();
    for node in root.named_children(&mut cursor) {
        match node.kind() {
            "using_directive" => {
                let text = node_text(&node, source)
                    .trim()
                    .trim_end_matches(';')
                    .to_string();
                usings.push(entry_from_node(text, &node));
            }

            "namespace_declaration" | "file_scoped_namespace_declaration" => {
                let name = find_child_by_kind(&node, "identifier")
                    .or_else(|| find_child_by_kind(&node, "qualified_name"))
                    .map(|n| node_text(&n, source).to_string())
                    .unwrap_or_default();
                namespaces.push(entry_from_node(format!("namespace {}", name), &node));

                // Extract declarations inside the namespace
                extract_namespace_members(
                    &node,
                    source,
                    options,
                    &mut classes,
                    &mut interfaces,
                    &mut enums,
                );
            }

            "class_declaration" | "record_declaration" => {
                classes.push(extract_csharp_class(&node, source, options));
            }

            "struct_declaration" => {
                classes.push(extract_csharp_class(&node, source, options));
            }

            "interface_declaration" => {
                interfaces.push(extract_csharp_interface(&node, source, options));
            }

            "enum_declaration" => {
                enums.push(extract_csharp_enum(&node, source, options));
            }

            _ => {}
        }
    }

    let mut sections = Vec::new();
    if !usings.is_empty() {
        sections.push(Section::with_entries("usings", usings));
    }
    if !namespaces.is_empty() {
        sections.push(Section::with_entries("namespaces", namespaces));
    }
    if !classes.is_empty() {
        sections.push(Section::with_entries("classes", classes));
    }
    if !interfaces.is_empty() {
        sections.push(Section::with_entries("interfaces", interfaces));
    }
    if !enums.is_empty() {
        sections.push(Section::with_entries("enums", enums));
    }

    Ok(sections)
}

// ---------------------------------------------------------------------------
// C#-specific helpers
// ---------------------------------------------------------------------------

fn extract_namespace_members(
    node: &tree_sitter::Node,
    source: &str,
    options: &IndexOptions,
    classes: &mut Vec<SkeletonEntry>,
    interfaces: &mut Vec<SkeletonEntry>,
    enums: &mut Vec<SkeletonEntry>,
) {
    let body = find_child_by_kind(node, "declaration_list");
    let body = match body {
        Some(b) => b,
        None => {
            // File-scoped namespace: children are directly under the node
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                match child.kind() {
                    "class_declaration" | "record_declaration" | "struct_declaration" => {
                        classes.push(extract_csharp_class(&child, source, options));
                    }
                    "interface_declaration" => {
                        interfaces.push(extract_csharp_interface(&child, source, options));
                    }
                    "enum_declaration" => {
                        enums.push(extract_csharp_enum(&child, source, options));
                    }
                    _ => {}
                }
            }
            return;
        }
    };

    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        match child.kind() {
            "class_declaration" | "record_declaration" | "struct_declaration" => {
                classes.push(extract_csharp_class(&child, source, options));
            }
            "interface_declaration" => {
                interfaces.push(extract_csharp_interface(&child, source, options));
            }
            "enum_declaration" => {
                enums.push(extract_csharp_enum(&child, source, options));
            }
            _ => {}
        }
    }
}

fn extract_csharp_class(
    node: &tree_sitter::Node,
    source: &str,
    options: &IndexOptions,
) -> SkeletonEntry {
    let header = extract_header_before_brace(node, source);

    let body = find_child_by_kind(node, "declaration_list");
    let members = if let Some(body) = body {
        let mut members = Vec::new();
        let mut cursor = body.walk();
        for child in body.named_children(&mut cursor) {
            match child.kind() {
                "method_declaration" | "constructor_declaration" => {
                    let sig = extract_header_before_brace(&child, source);
                    members.push(entry_from_node(sig, &child));
                }
                "property_declaration" => {
                    let label = first_line_of(&child, source).trim().to_string();
                    members.push(entry_from_node(label, &child));
                }
                "field_declaration" | "event_field_declaration" => {
                    let label = node_text(&child, source)
                        .trim()
                        .trim_end_matches(';')
                        .to_string();
                    members.push(entry_from_node(label, &child));
                }
                "class_declaration" | "struct_declaration" | "record_declaration" => {
                    members.push(extract_csharp_class(&child, source, options));
                }
                _ => {}
            }
        }
        truncate_children(members, options.max_children_per_item)
    } else {
        Vec::new()
    };

    SkeletonEntry::with_children(header, start_line(node), end_line(node), members)
}

fn extract_csharp_interface(
    node: &tree_sitter::Node,
    source: &str,
    options: &IndexOptions,
) -> SkeletonEntry {
    let header = extract_header_before_brace(node, source);

    let body = find_child_by_kind(node, "declaration_list");
    let members = if let Some(body) = body {
        let mut members = Vec::new();
        let mut cursor = body.walk();
        for child in body.named_children(&mut cursor) {
            match child.kind() {
                "method_declaration" => {
                    let sig = extract_header_before_brace(&child, source);
                    members.push(entry_from_node(sig, &child));
                }
                "property_declaration" => {
                    let label = first_line_of(&child, source).trim().to_string();
                    members.push(entry_from_node(label, &child));
                }
                _ => {}
            }
        }
        truncate_children(members, options.max_children_per_item)
    } else {
        Vec::new()
    };

    SkeletonEntry::with_children(header, start_line(node), end_line(node), members)
}

fn extract_csharp_enum(
    node: &tree_sitter::Node,
    source: &str,
    options: &IndexOptions,
) -> SkeletonEntry {
    let header = extract_header_before_brace(node, source);

    let body = find_child_by_kind(node, "enum_member_declaration_list");
    let members = if let Some(body) = body {
        let mut members = Vec::new();
        let mut cursor = body.walk();
        for child in body.named_children(&mut cursor) {
            if child.kind() == "enum_member_declaration" {
                let label = node_text(&child, source)
                    .trim()
                    .trim_end_matches(',')
                    .to_string();
                members.push(entry_from_node(label, &child));
            }
        }
        truncate_children(members, options.max_children_per_item)
    } else {
        Vec::new()
    };

    SkeletonEntry::with_children(header, start_line(node), end_line(node), members)
}

fn extract_header_before_brace(node: &tree_sitter::Node, source: &str) -> String {
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
