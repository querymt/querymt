//! TypeScript and JavaScript outline extractor.
//!
//! Uses `tree-sitter-typescript` for `.ts`/`.tsx` and the JavaScript grammar
//! embedded in the same crate for `.js`/`.jsx`/`.mjs`/`.cjs`.

use tree_sitter::Parser;

use super::helpers::*;
use crate::index::outline_index::common::{IndexOptions, OutlineError, Section, SkeletonEntry};

pub fn extract(
    source: &str,
    language: &str,
    options: &IndexOptions,
) -> Result<Vec<Section>, OutlineError> {
    let mut parser = Parser::new();
    let ts_lang = match language {
        "typescript" => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        "javascript" => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(), // TS superset parses JS fine
        _ => {
            return Err(OutlineError::UnsupportedLanguage(language.to_string()));
        }
    };
    parser
        .set_language(&ts_lang)
        .map_err(|e| OutlineError::ParseError(format!("Failed to set TS/JS language: {}", e)))?;

    let tree = parse_source(&mut parser, source)?;
    let root = tree.root_node();

    let mut imports = Vec::new();
    let mut types = Vec::new();
    let mut classes = Vec::new();
    let mut functions = Vec::new();
    let mut tests = Vec::new();
    let mut exports = Vec::new();
    let mut constants = Vec::new();

    let mut cursor = root.walk();
    for node in root.named_children(&mut cursor) {
        match node.kind() {
            "import_statement" => {
                let text = node_text(&node, source)
                    .lines()
                    .map(|l| l.trim())
                    .collect::<Vec<_>>()
                    .join(" ");
                imports.push(entry_from_node(text, &node));
            }

            "interface_declaration" => {
                let label = extract_interface_label(&node, source);
                let members = extract_interface_members(&node, source, options);
                types.push(SkeletonEntry::with_children(
                    label,
                    start_line(&node),
                    end_line(&node),
                    members,
                ));
            }

            "type_alias_declaration" => {
                let label = first_line_of(&node, source).trim().to_string();
                types.push(entry_from_node(label, &node));
            }

            "enum_declaration" => {
                let label = extract_enum_label(&node, source);
                let members = extract_enum_members(&node, source, options);
                types.push(SkeletonEntry::with_children(
                    label,
                    start_line(&node),
                    end_line(&node),
                    members,
                ));
            }

            "class_declaration" => {
                let label = extract_class_label(&node, source);
                let members = extract_class_members(&node, source, options);
                classes.push(SkeletonEntry::with_children(
                    label,
                    start_line(&node),
                    end_line(&node),
                    members,
                ));
            }

            "abstract_class_declaration" => {
                let label = format!("abstract {}", extract_class_label(&node, source));
                let members = extract_class_members(&node, source, options);
                classes.push(SkeletonEntry::with_children(
                    label,
                    start_line(&node),
                    end_line(&node),
                    members,
                ));
            }

            "function_declaration" => {
                let sig = extract_ts_fn_signature(&node, source);
                if is_test_call_name(&node, source) {
                    if options.include_tests {
                        tests.push(entry_from_node(sig, &node));
                    }
                } else {
                    functions.push(entry_from_node(sig, &node));
                }
            }

            "export_statement" => {
                // Unwrap the exported declaration
                let mut inner_cursor = node.walk();
                for child in node.named_children(&mut inner_cursor) {
                    match child.kind() {
                        "function_declaration" => {
                            let sig = format!("export {}", extract_ts_fn_signature(&child, source));
                            functions.push(entry_from_node(sig, &node));
                        }
                        "class_declaration" => {
                            let label = format!("export {}", extract_class_label(&child, source));
                            let members = extract_class_members(&child, source, options);
                            classes.push(SkeletonEntry::with_children(
                                label,
                                start_line(&node),
                                end_line(&node),
                                members,
                            ));
                        }
                        "interface_declaration" => {
                            let label =
                                format!("export {}", extract_interface_label(&child, source));
                            let members = extract_interface_members(&child, source, options);
                            types.push(SkeletonEntry::with_children(
                                label,
                                start_line(&node),
                                end_line(&node),
                                members,
                            ));
                        }
                        "type_alias_declaration" => {
                            let label = format!("export {}", first_line_of(&child, source).trim());
                            types.push(entry_from_node(label, &node));
                        }
                        "enum_declaration" => {
                            let label = format!("export {}", extract_enum_label(&child, source));
                            let members = extract_enum_members(&child, source, options);
                            types.push(SkeletonEntry::with_children(
                                label,
                                start_line(&node),
                                end_line(&node),
                                members,
                            ));
                        }
                        "lexical_declaration" => {
                            // export const / export let
                            let label = format!("export {}", extract_lexical_label(&child, source));
                            if looks_like_function_assignment(&child, source) {
                                functions.push(entry_from_node(label, &node));
                            } else {
                                constants.push(entry_from_node(label, &node));
                            }
                        }
                        _ => {
                            let label = first_line_of(&node, source).trim().to_string();
                            exports.push(entry_from_node(label, &node));
                        }
                    }
                }
            }

            "lexical_declaration" | "variable_declaration" => {
                let label = extract_lexical_label(&node, source);
                if looks_like_function_assignment(&node, source) {
                    functions.push(entry_from_node(label, &node));
                } else {
                    constants.push(entry_from_node(label, &node));
                }
            }

            // Test frameworks: describe/it/test blocks
            "expression_statement" => {
                if let Some(call) = find_child_by_kind(&node, "call_expression")
                    && is_describe_or_test(&call, source)
                    && options.include_tests
                {
                    let label = extract_test_call_label(&call, source);
                    tests.push(entry_from_node(label, &node));
                }
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
    if !classes.is_empty() {
        sections.push(Section::with_entries("classes", classes));
    }
    if !functions.is_empty() {
        sections.push(Section::with_entries("functions", functions));
    }
    if !constants.is_empty() {
        sections.push(Section::with_entries("constants", constants));
    }
    if !exports.is_empty() {
        sections.push(Section::with_entries("exports", exports));
    }
    if !tests.is_empty() {
        sections.push(Section::with_entries("tests", tests));
    }

    Ok(sections)
}

// ---------------------------------------------------------------------------
// TS/JS-specific helpers
// ---------------------------------------------------------------------------

fn extract_ts_fn_signature(node: &tree_sitter::Node, source: &str) -> String {
    let name = find_child_by_kind(node, "identifier")
        .map(|n| node_text(&n, source).to_string())
        .unwrap_or_else(|| "anonymous".to_string());

    let params = find_child_by_kind(node, "formal_parameters")
        .map(|n| {
            node_text(&n, source)
                .lines()
                .map(|l| l.trim())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_else(|| "()".to_string());

    // Return type annotation
    let return_type = extract_ts_return_type(node, source);

    // async keyword
    let text = node_text(node, source);
    let is_async = text.trim_start().starts_with("async ");

    let prefix = if is_async { "async " } else { "" };

    if return_type.is_empty() {
        format!("{}function {}{}", prefix, name, params)
    } else {
        format!("{}function {}{}: {}", prefix, name, params, return_type)
    }
}

fn extract_ts_return_type(node: &tree_sitter::Node, source: &str) -> String {
    // Look for type_annotation child after formal_parameters
    let mut cursor = node.walk();
    let mut found_params = false;
    for child in node.named_children(&mut cursor) {
        if child.kind() == "formal_parameters" {
            found_params = true;
            continue;
        }
        if found_params && child.kind() == "type_annotation" {
            let text = node_text(&child, source).trim().to_string();
            // Strip leading `: `
            return text.strip_prefix(':').unwrap_or(&text).trim().to_string();
        }
        if child.kind() == "statement_block" {
            break;
        }
    }
    String::new()
}

fn extract_interface_label(node: &tree_sitter::Node, source: &str) -> String {
    let name = find_child_by_kind(node, "type_identifier")
        .map(|n| node_text(&n, source).to_string())
        .unwrap_or_default();

    // Check for extends
    let extends = find_child_by_kind(node, "extends_type_clause")
        .map(|n| format!(" {}", node_text(&n, source).trim()))
        .unwrap_or_default();

    format!("interface {}{}", name, extends)
}

fn extract_interface_members(
    node: &tree_sitter::Node,
    source: &str,
    options: &IndexOptions,
) -> Vec<SkeletonEntry> {
    let body = match find_child_by_kinds(node, &["object_type", "interface_body"]) {
        Some(b) => b,
        None => return Vec::new(),
    };

    let mut members = Vec::new();
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        match child.kind() {
            "property_signature"
            | "method_signature"
            | "call_signature"
            | "construct_signature"
            | "index_signature" => {
                let label = node_text(&child, source)
                    .lines()
                    .next()
                    .unwrap_or("")
                    .trim()
                    .trim_end_matches([',', ';'])
                    .to_string();
                members.push(entry_from_node(label, &child));
            }
            _ => {}
        }
    }
    truncate_children(members, options.max_children_per_item)
}

fn extract_class_label(node: &tree_sitter::Node, source: &str) -> String {
    let name = find_child_by_kind(node, "type_identifier")
        .map(|n| node_text(&n, source).to_string())
        .unwrap_or_default();

    let heritage = find_child_by_kind(node, "class_heritage")
        .map(|n| format!(" {}", node_text(&n, source).trim()))
        .unwrap_or_default();

    format!("class {}{}", name, heritage)
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
            "method_definition" | "public_field_definition" | "property_definition" => {
                let label = first_line_of(&child, source).trim().to_string();
                // Trim trailing `{` for methods
                let label = label.trim_end_matches('{').trim().to_string();
                members.push(entry_from_node(label, &child));
            }
            _ => {}
        }
    }
    truncate_children(members, options.max_children_per_item)
}

fn extract_enum_label(node: &tree_sitter::Node, source: &str) -> String {
    let name = find_child_by_kind(node, "identifier")
        .map(|n| node_text(&n, source).to_string())
        .unwrap_or_default();
    format!("enum {}", name)
}

fn extract_enum_members(
    node: &tree_sitter::Node,
    source: &str,
    options: &IndexOptions,
) -> Vec<SkeletonEntry> {
    let body = match find_child_by_kind(node, "enum_body") {
        Some(b) => b,
        None => return Vec::new(),
    };

    let mut members = Vec::new();
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        if child.kind() == "enum_assignment" || child.kind() == "property_identifier" {
            let label = node_text(&child, source)
                .trim()
                .trim_end_matches(',')
                .to_string();
            members.push(entry_from_node(label, &child));
        }
    }
    truncate_children(members, options.max_children_per_item)
}

fn extract_lexical_label(node: &tree_sitter::Node, source: &str) -> String {
    let text = first_line_of(node, source);
    let trimmed = text.trim();
    // Truncate at `=` + a reasonable amount for the value
    if let Some(eq_pos) = trimmed.find('=') {
        let lhs = &trimmed[..eq_pos + 1];
        let rhs = trimmed[eq_pos + 1..].trim();
        // If RHS is short, include it
        if rhs.len() < 60 {
            trimmed.trim_end_matches(['{', ';']).trim().to_string()
        } else {
            format!("{} ...", lhs)
        }
    } else {
        trimmed.to_string()
    }
}

fn looks_like_function_assignment(node: &tree_sitter::Node, source: &str) -> bool {
    let text = node_text(node, source);
    text.contains("=>") || text.contains("function(") || text.contains("function (")
}

fn is_describe_or_test(call_node: &tree_sitter::Node, source: &str) -> bool {
    let func_name = find_child_by_kind(call_node, "identifier")
        .or_else(|| find_child_by_kind(call_node, "member_expression"))
        .map(|n| node_text(&n, source).to_string())
        .unwrap_or_default();
    matches!(
        func_name.as_str(),
        "describe" | "it" | "test" | "beforeEach" | "afterEach" | "beforeAll" | "afterAll"
    )
}

fn extract_test_call_label(call_node: &tree_sitter::Node, source: &str) -> String {
    let func_name = find_child_by_kind(call_node, "identifier")
        .map(|n| node_text(&n, source).to_string())
        .unwrap_or_else(|| "test".to_string());

    // Try to get the test description (first string argument)
    if let Some(args) = find_child_by_kind(call_node, "arguments") {
        let mut cursor = args.walk();
        for child in args.named_children(&mut cursor) {
            if child.kind() == "string" || child.kind() == "template_string" {
                let desc = node_text(&child, source);
                return format!("{}({})", func_name, desc);
            }
        }
    }
    func_name
}

fn is_test_call_name(_node: &tree_sitter::Node, _source: &str) -> bool {
    // Regular function declarations are never test calls
    // (tests are expression_statements with call_expressions)
    false
}
