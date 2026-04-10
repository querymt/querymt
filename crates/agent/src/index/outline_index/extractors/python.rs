//! Python outline extractor.

use tree_sitter::Parser;

use super::helpers::*;
use crate::index::outline_index::common::{IndexOptions, OutlineError, Section, SkeletonEntry};

pub fn extract(source: &str, options: &IndexOptions) -> Result<Vec<Section>, OutlineError> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_python::LANGUAGE.into())
        .map_err(|e| OutlineError::ParseError(format!("Failed to set Python language: {}", e)))?;

    let tree = parse_source(&mut parser, source)?;
    let root = tree.root_node();

    let mut imports = Vec::new();
    let mut classes = Vec::new();
    let mut functions = Vec::new();
    let mut tests = Vec::new();
    let mut constants = Vec::new();

    let mut cursor = root.walk();
    for node in root.named_children(&mut cursor) {
        match node.kind() {
            "import_statement" | "import_from_statement" => {
                let text = node_text(&node, source).trim().to_string();
                imports.push(entry_from_node(text, &node));
            }

            "class_definition" => {
                let name = find_child_by_kind(&node, "identifier")
                    .map(|n| node_text(&n, source).to_string())
                    .unwrap_or_default();

                let superclasses = find_child_by_kind(&node, "argument_list")
                    .map(|n| node_text(&n, source).to_string())
                    .unwrap_or_default();

                let methods = extract_class_methods(&node, source, options);

                let label = if superclasses.is_empty() {
                    format!("class {}", name)
                } else {
                    format!("class {}{}", name, superclasses)
                };

                if is_test_class(&name) && !options.include_tests {
                    continue;
                }

                if is_test_class(&name) {
                    tests.push(SkeletonEntry::with_children(
                        label,
                        start_line(&node),
                        end_line(&node),
                        methods,
                    ));
                } else {
                    classes.push(SkeletonEntry::with_children(
                        label,
                        start_line(&node),
                        end_line(&node),
                        methods,
                    ));
                }
            }

            "function_definition" => {
                let name = find_child_by_kind(&node, "identifier")
                    .map(|n| node_text(&n, source).to_string())
                    .unwrap_or_default();

                let sig = extract_python_fn_signature(&node, source);

                if is_test_function_name(&name) {
                    if options.include_tests {
                        tests.push(entry_from_node(sig, &node));
                    }
                } else {
                    functions.push(entry_from_node(sig, &node));
                }
            }

            "decorated_definition" => {
                // Decorated functions/classes: extract the inner definition
                if let Some(inner) =
                    find_child_by_kinds(&node, &["function_definition", "class_definition"])
                {
                    let decorator = extract_decorators(&node, source);
                    match inner.kind() {
                        "function_definition" => {
                            let name = find_child_by_kind(&inner, "identifier")
                                .map(|n| node_text(&n, source).to_string())
                                .unwrap_or_default();
                            let sig = format!(
                                "{}{}",
                                decorator,
                                extract_python_fn_signature(&inner, source)
                            );

                            if is_test_function_name(&name) {
                                if options.include_tests {
                                    tests.push(entry_from_node(sig, &node));
                                }
                            } else {
                                functions.push(entry_from_node(sig, &node));
                            }
                        }
                        "class_definition" => {
                            let name = find_child_by_kind(&inner, "identifier")
                                .map(|n| node_text(&n, source).to_string())
                                .unwrap_or_default();
                            let methods = extract_class_methods(&inner, source, options);
                            let label = format!("{}class {}", decorator, name);

                            if is_test_class(&name) {
                                if options.include_tests {
                                    tests.push(SkeletonEntry::with_children(
                                        label,
                                        start_line(&node),
                                        end_line(&node),
                                        methods,
                                    ));
                                }
                            } else {
                                classes.push(SkeletonEntry::with_children(
                                    label,
                                    start_line(&node),
                                    end_line(&node),
                                    methods,
                                ));
                            }
                        }
                        _ => {}
                    }
                }
            }

            "expression_statement" => {
                // Top-level assignments (constants)
                if let Some(assign) = find_child_by_kind(&node, "assignment") {
                    let text = node_text(&assign, source);
                    // Only include simple uppercase constants or type aliases
                    let lhs = text.split('=').next().unwrap_or("").trim();
                    if lhs.chars().next().is_some_and(|c| c.is_uppercase()) {
                        let label = first_line_of(&node, source).trim().to_string();
                        constants.push(entry_from_node(label, &node));
                    }
                }
            }

            _ => {}
        }
    }

    let mut sections = Vec::new();
    if !imports.is_empty() {
        sections.push(Section::with_entries("imports", imports));
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
// Python-specific helpers
// ---------------------------------------------------------------------------

fn extract_python_fn_signature(node: &tree_sitter::Node, source: &str) -> String {
    let name = find_child_by_kind(node, "identifier")
        .map(|n| node_text(&n, source).to_string())
        .unwrap_or_else(|| "unknown".to_string());

    let params = find_child_by_kind(node, "parameters")
        .map(|n| {
            let text = node_text(&n, source);
            text.lines().map(|l| l.trim()).collect::<Vec<_>>().join(" ")
        })
        .unwrap_or_else(|| "()".to_string());

    // Return type annotation
    let return_type = find_child_by_kind(node, "type")
        .map(|n| format!(" -> {}", node_text(&n, source)))
        .unwrap_or_default();

    // Check for async
    let text = node_text(node, source);
    let is_async = text.trim_start().starts_with("async ");

    let prefix = if is_async { "async " } else { "" };

    format!("{}def {}{}{}", prefix, name, params, return_type)
}

fn extract_class_methods(
    node: &tree_sitter::Node,
    source: &str,
    options: &IndexOptions,
) -> Vec<SkeletonEntry> {
    let body = match find_child_by_kind(node, "block") {
        Some(b) => b,
        None => return Vec::new(),
    };

    let mut methods = Vec::new();
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        match child.kind() {
            "function_definition" => {
                let sig = extract_python_fn_signature(&child, source);
                methods.push(entry_from_node(sig, &child));
            }
            "decorated_definition" => {
                if let Some(inner) = find_child_by_kind(&child, "function_definition") {
                    let decorator = extract_decorators(&child, source);
                    let sig = format!(
                        "{}{}",
                        decorator,
                        extract_python_fn_signature(&inner, source)
                    );
                    methods.push(entry_from_node(sig, &child));
                }
            }
            _ => {}
        }
    }
    truncate_children(methods, options.max_children_per_item)
}

fn extract_decorators(node: &tree_sitter::Node, source: &str) -> String {
    let mut decorators = Vec::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "decorator" {
            decorators.push(node_text(&child, source).trim().to_string());
        }
    }
    if decorators.is_empty() {
        String::new()
    } else {
        format!("{} ", decorators.join(" "))
    }
}

fn is_test_function_name(name: &str) -> bool {
    name.starts_with("test_") || name == "setUp" || name == "tearDown"
}

fn is_test_class(name: &str) -> bool {
    name.starts_with("Test") || name.ends_with("Test") || name.ends_with("Tests")
}
