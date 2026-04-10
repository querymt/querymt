//! Java outline extractor.

use tree_sitter::Parser;

use super::helpers::*;
use crate::index::outline_index::common::{IndexOptions, OutlineError, Section, SkeletonEntry};

pub fn extract(source: &str, options: &IndexOptions) -> Result<Vec<Section>, OutlineError> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_java::LANGUAGE.into())
        .map_err(|e| OutlineError::ParseError(format!("Failed to set Java language: {}", e)))?;

    let tree = parse_source(&mut parser, source)?;
    let root = tree.root_node();

    let mut imports = Vec::new();
    let mut classes = Vec::new();
    let mut interfaces = Vec::new();
    let mut enums = Vec::new();
    let mut package = Vec::new();

    let mut cursor = root.walk();
    for node in root.named_children(&mut cursor) {
        match node.kind() {
            "package_declaration" => {
                let text = node_text(&node, source)
                    .trim()
                    .trim_end_matches(';')
                    .to_string();
                package.push(entry_from_node(text, &node));
            }

            "import_declaration" => {
                let text = node_text(&node, source)
                    .trim()
                    .trim_end_matches(';')
                    .to_string();
                imports.push(entry_from_node(text, &node));
            }

            "class_declaration" => {
                let entry = extract_class(&node, source, options);
                classes.push(entry);
            }

            "interface_declaration" => {
                let entry = extract_interface(&node, source, options);
                interfaces.push(entry);
            }

            "enum_declaration" => {
                let entry = extract_enum(&node, source, options);
                enums.push(entry);
            }

            "record_declaration" => {
                let entry = extract_class(&node, source, options);
                classes.push(entry);
            }

            _ => {}
        }
    }

    let mut sections = Vec::new();
    if !package.is_empty() {
        sections.push(Section::with_entries("package", package));
    }
    if !imports.is_empty() {
        sections.push(Section::with_entries("imports", imports));
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
// Java-specific helpers
// ---------------------------------------------------------------------------

fn extract_class(node: &tree_sitter::Node, source: &str, options: &IndexOptions) -> SkeletonEntry {
    let label = extract_class_header(node, source);
    let members = extract_class_members(node, source, options);
    SkeletonEntry::with_children(label, start_line(node), end_line(node), members)
}

fn extract_class_header(node: &tree_sitter::Node, source: &str) -> String {
    let text = node_text(node, source);
    if let Some(brace_pos) = text.find('{') {
        text[..brace_pos].trim().to_string()
    } else {
        first_line_of(node, source).trim().to_string()
    }
}

fn extract_class_members(
    node: &tree_sitter::Node,
    source: &str,
    options: &IndexOptions,
) -> Vec<SkeletonEntry> {
    let body = match find_child_by_kind(node, "class_body") {
        Some(b) => b,
        None => return Vec::new(),
    };

    let mut members = Vec::new();
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        match child.kind() {
            "method_declaration" | "constructor_declaration" => {
                let sig = extract_method_signature(&child, source);
                members.push(entry_from_node(sig, &child));
            }
            "field_declaration" => {
                let label = node_text(&child, source)
                    .lines()
                    .next()
                    .unwrap_or("")
                    .trim()
                    .trim_end_matches(';')
                    .to_string();
                members.push(entry_from_node(label, &child));
            }
            "class_declaration" => {
                // Inner class
                let inner = extract_class(&child, source, options);
                members.push(inner);
            }
            "interface_declaration" => {
                let inner = extract_interface(&child, source, options);
                members.push(inner);
            }
            "enum_declaration" => {
                let inner = extract_enum(&child, source, options);
                members.push(inner);
            }
            _ => {}
        }
    }
    truncate_children(members, options.max_children_per_item)
}

fn extract_interface(
    node: &tree_sitter::Node,
    source: &str,
    options: &IndexOptions,
) -> SkeletonEntry {
    let label = extract_class_header(node, source);

    let body = find_child_by_kind(node, "interface_body");
    let members = if let Some(body) = body {
        let mut members = Vec::new();
        let mut cursor = body.walk();
        for child in body.named_children(&mut cursor) {
            match child.kind() {
                "method_declaration" | "constant_declaration" => {
                    let sig = extract_method_signature(&child, source);
                    members.push(entry_from_node(sig, &child));
                }
                _ => {}
            }
        }
        truncate_children(members, options.max_children_per_item)
    } else {
        Vec::new()
    };

    SkeletonEntry::with_children(label, start_line(node), end_line(node), members)
}

fn extract_enum(node: &tree_sitter::Node, source: &str, options: &IndexOptions) -> SkeletonEntry {
    let label = extract_class_header(node, source);

    let body = find_child_by_kind(node, "enum_body");
    let members = if let Some(body) = body {
        let mut members = Vec::new();
        let mut cursor = body.walk();
        for child in body.named_children(&mut cursor) {
            if child.kind() == "enum_constant" {
                let name = find_child_by_kind(&child, "identifier")
                    .map(|n| node_text(&n, source).to_string())
                    .unwrap_or_else(|| node_text(&child, source).trim().to_string());
                members.push(entry_from_node(name, &child));
            }
        }
        truncate_children(members, options.max_children_per_item)
    } else {
        Vec::new()
    };

    SkeletonEntry::with_children(label, start_line(node), end_line(node), members)
}

fn extract_method_signature(node: &tree_sitter::Node, source: &str) -> String {
    // Take everything up to the body block `{`
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
        // Abstract method or interface method (ends with `;`)
        text.trim().trim_end_matches(';').to_string()
    }
}
