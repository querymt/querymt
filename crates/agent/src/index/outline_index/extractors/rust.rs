//! Rust outline extractor.

use tree_sitter::Parser;

use super::helpers::*;
use crate::index::outline_index::common::{IndexOptions, OutlineError, Section, SkeletonEntry};

pub fn extract(source: &str, options: &IndexOptions) -> Result<Vec<Section>, OutlineError> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_rust::LANGUAGE.into())
        .map_err(|e| OutlineError::ParseError(format!("Failed to set Rust language: {}", e)))?;

    let tree = parse_source(&mut parser, source)?;
    let root = tree.root_node();

    let mut imports = Vec::new();
    let mut types = Vec::new();
    let mut traits = Vec::new();
    let mut impls = Vec::new();
    let mut functions = Vec::new();
    let mut tests = Vec::new();
    let mut macros = Vec::new();
    let mut constants = Vec::new();

    let mut cursor = root.walk();
    for node in root.named_children(&mut cursor) {
        match node.kind() {
            "use_declaration" => {
                imports.push(entry_from_node(
                    node_text(&node, source).trim_end_matches(';').trim(),
                    &node,
                ));
            }

            "struct_item" => {
                let name = find_child_by_kind(&node, "type_identifier")
                    .map(|n| node_text(&n, source).to_string())
                    .unwrap_or_default();
                let vis = extract_visibility(&node, source);
                let fields = extract_struct_fields(&node, source, options);
                let label = format!("{}struct {}", vis, name);
                types.push(SkeletonEntry::with_children(
                    label,
                    start_line(&node),
                    end_line(&node),
                    fields,
                ));
            }

            "enum_item" => {
                let name = find_child_by_kind(&node, "type_identifier")
                    .map(|n| node_text(&n, source).to_string())
                    .unwrap_or_default();
                let vis = extract_visibility(&node, source);
                let variants = extract_enum_variants(&node, source, options);
                let label = format!("{}enum {}", vis, name);
                types.push(SkeletonEntry::with_children(
                    label,
                    start_line(&node),
                    end_line(&node),
                    variants,
                ));
            }

            "type_item" => {
                let label = first_line_of(&node, source);
                types.push(entry_from_node(label.trim_end_matches(';').trim(), &node));
            }

            "trait_item" => {
                let name = find_child_by_kind(&node, "type_identifier")
                    .map(|n| node_text(&n, source).to_string())
                    .unwrap_or_default();
                let vis = extract_visibility(&node, source);
                let methods = extract_trait_methods(&node, source, options);
                let label = format!("{}trait {}", vis, name);
                traits.push(SkeletonEntry::with_children(
                    label,
                    start_line(&node),
                    end_line(&node),
                    methods,
                ));
            }

            "impl_item" => {
                let label = extract_impl_label(&node, source);
                let methods = extract_impl_methods(&node, source, options);
                impls.push(SkeletonEntry::with_children(
                    label,
                    start_line(&node),
                    end_line(&node),
                    methods,
                ));
            }

            "function_item" => {
                let sig = extract_rust_fn_signature(&node, source);
                let entry = entry_from_node(sig, &node);
                if is_test_function(&node, source) {
                    if options.include_tests {
                        tests.push(entry);
                    }
                } else {
                    functions.push(entry);
                }
            }

            "mod_item" => {
                if is_test_module(&node, source) {
                    if options.include_tests {
                        tests.push(entry_from_node(extract_mod_label(&node, source), &node));
                    }
                } else {
                    // Non-test modules: treat as a type-level declaration
                    let label = extract_mod_label(&node, source);
                    types.push(entry_from_node(label, &node));
                }
            }

            "macro_definition" => {
                let name = find_child_by_kind(&node, "identifier")
                    .map(|n| node_text(&n, source).to_string())
                    .unwrap_or_else(|| "unknown".to_string());
                macros.push(entry_from_node(format!("macro_rules! {}", name), &node));
            }

            "const_item" | "static_item" => {
                let label = first_line_of(&node, source);
                constants.push(entry_from_node(label.trim(), &node));
            }

            // Attributed items: the attribute wraps the real item
            "attribute_item" => {}

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
    if !traits.is_empty() {
        sections.push(Section::with_entries("traits", traits));
    }
    if !impls.is_empty() {
        sections.push(Section::with_entries("impls", impls));
    }
    if !functions.is_empty() {
        sections.push(Section::with_entries("functions", functions));
    }
    if !macros.is_empty() {
        sections.push(Section::with_entries("macros", macros));
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
// Rust-specific helpers
// ---------------------------------------------------------------------------

fn extract_visibility(node: &tree_sitter::Node, source: &str) -> &'static str {
    if let Some(vis) = find_child_by_kind(node, "visibility_modifier") {
        let text = node_text(&vis, source);
        if text == "pub" {
            "pub "
        } else if text.starts_with("pub(") {
            // pub(crate), pub(super), etc. — return with trailing space
            "pub(...) "
        } else {
            ""
        }
    } else {
        ""
    }
}

fn extract_struct_fields(
    node: &tree_sitter::Node,
    source: &str,
    options: &IndexOptions,
) -> Vec<SkeletonEntry> {
    let body = match find_child_by_kind(node, "field_declaration_list") {
        Some(b) => b,
        None => return Vec::new(), // Tuple struct or unit struct
    };

    let mut fields = Vec::new();
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        if child.kind() == "field_declaration" {
            let label = node_text(&child, source)
                .lines()
                .next()
                .unwrap_or("")
                .trim()
                .trim_end_matches(',')
                .to_string();
            fields.push(entry_from_node(label, &child));
        }
    }
    truncate_children(fields, options.max_children_per_item)
}

fn extract_enum_variants(
    node: &tree_sitter::Node,
    source: &str,
    options: &IndexOptions,
) -> Vec<SkeletonEntry> {
    let body = match find_child_by_kind(node, "enum_variant_list") {
        Some(b) => b,
        None => return Vec::new(),
    };

    let mut variants = Vec::new();
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        if child.kind() == "enum_variant" {
            let label = first_line_of(&child, source)
                .trim()
                .trim_end_matches(',')
                .to_string();
            variants.push(entry_from_node(label, &child));
        }
    }
    truncate_children(variants, options.max_children_per_item)
}

fn extract_trait_methods(
    node: &tree_sitter::Node,
    source: &str,
    options: &IndexOptions,
) -> Vec<SkeletonEntry> {
    let body = match find_child_by_kind(node, "declaration_list") {
        Some(b) => b,
        None => return Vec::new(),
    };

    let mut methods = Vec::new();
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        if child.kind() == "function_signature_item" || child.kind() == "function_item" {
            let sig = extract_rust_fn_signature(&child, source);
            methods.push(entry_from_node(sig, &child));
        } else if child.kind() == "type_item" {
            let label = first_line_of(&child, source)
                .trim()
                .trim_end_matches(';')
                .to_string();
            methods.push(entry_from_node(label, &child));
        }
    }
    truncate_children(methods, options.max_children_per_item)
}

fn extract_impl_methods(
    node: &tree_sitter::Node,
    source: &str,
    options: &IndexOptions,
) -> Vec<SkeletonEntry> {
    let body = match find_child_by_kind(node, "declaration_list") {
        Some(b) => b,
        None => return Vec::new(),
    };

    let mut methods = Vec::new();
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        if child.kind() == "function_item" {
            let sig = extract_rust_fn_signature(&child, source);
            methods.push(entry_from_node(sig, &child));
        } else if child.kind() == "type_item" || child.kind() == "const_item" {
            let label = first_line_of(&child, source)
                .trim()
                .trim_end_matches(';')
                .to_string();
            methods.push(entry_from_node(label, &child));
        }
    }
    truncate_children(methods, options.max_children_per_item)
}

fn extract_impl_label(node: &tree_sitter::Node, source: &str) -> String {
    // impl blocks: `impl Type` or `impl Trait for Type`
    let text = node_text(node, source);
    // Take everything up to `{`
    if let Some(brace_pos) = text.find('{') {
        text[..brace_pos].trim().to_string()
    } else {
        first_line_of(node, source).trim().to_string()
    }
}

fn extract_rust_fn_signature(node: &tree_sitter::Node, source: &str) -> String {
    let vis = extract_visibility(node, source);

    // Check for `async` keyword
    let is_async = node_text(node, source).trim_start().starts_with("async ")
        || node_text(node, source)
            .trim_start()
            .starts_with("pub async ")
        || node_text(node, source)
            .trim_start()
            .starts_with("pub(crate) async ");

    let name = find_child_by_kind(node, "identifier")
        .map(|n| node_text(&n, source).to_string())
        .unwrap_or_else(|| "unknown".to_string());

    let params = find_child_by_kind(node, "parameters")
        .map(|n| node_text(&n, source).to_string())
        .unwrap_or_else(|| "()".to_string());

    // Collapse multi-line params to single line
    let params = params
        .lines()
        .map(|l| l.trim())
        .collect::<Vec<_>>()
        .join(" ");

    // Return type
    let return_type = extract_rust_return_type(node, source);

    let async_prefix = if is_async && !vis.contains("async") {
        "async "
    } else {
        ""
    };

    if return_type.is_empty() {
        format!("{}{}fn {}{}", vis, async_prefix, name, params)
    } else {
        format!(
            "{}{}fn {}{} -> {}",
            vis, async_prefix, name, params, return_type
        )
    }
}

fn extract_rust_return_type(node: &tree_sitter::Node, source: &str) -> String {
    // The return type in tree-sitter-rust is represented as a child node
    // following the parameters. Look for it between params end and body start.
    let params_end = find_child_by_kind(node, "parameters")
        .map(|n| n.end_byte())
        .unwrap_or(0);
    let body_start = find_child_by_kind(node, "block")
        .map(|n| n.start_byte())
        .unwrap_or(node.end_byte());

    if params_end > 0 && body_start > params_end {
        let between = safe_slice(source, params_end, body_start);
        let trimmed = between.trim();
        if let Some(rest) = trimmed.strip_prefix("->") {
            return rest.trim().to_string();
        }
    }
    String::new()
}

fn extract_mod_label(node: &tree_sitter::Node, source: &str) -> String {
    let vis = extract_visibility(node, source);
    let name = find_child_by_kind(node, "identifier")
        .map(|n| node_text(&n, source).to_string())
        .unwrap_or_else(|| "unknown".to_string());
    format!("{}mod {}", vis, name)
}

fn is_test_function(node: &tree_sitter::Node, source: &str) -> bool {
    // Check for #[test] or #[tokio::test] attributes
    let start = node.start_byte();
    if start > 0 {
        // Look back a reasonable amount for attributes.
        // Use safe_slice to avoid panicking on multi-byte UTF-8 chars.
        let lookback = start.saturating_sub(200);
        let prefix = safe_slice(source, lookback, start);
        if prefix.contains("#[test]")
            || prefix.contains("#[tokio::test]")
            || prefix.contains("#[rstest]")
        {
            return true;
        }
    }
    false
}

fn is_test_module(node: &tree_sitter::Node, source: &str) -> bool {
    let name = find_child_by_kind(node, "identifier")
        .map(|n| node_text(&n, source).to_string())
        .unwrap_or_default();
    if name == "tests" || name == "test" {
        return true;
    }
    // Check for #[cfg(test)] attribute before the module.
    // Use safe_slice to avoid panicking on multi-byte UTF-8 chars.
    let start = node.start_byte();
    if start > 0 {
        let lookback = start.saturating_sub(200);
        let prefix = safe_slice(source, lookback, start);
        if prefix.contains("#[cfg(test)]") {
            return true;
        }
    }
    false
}
