//! Code surgeon tool for language-aware source code search and transformation

use async_trait::async_trait;
use querymt::chat::{FunctionTool, Tool};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use srgn::RegexPattern;
use srgn::scoping::Scoper;
use srgn::scoping::langs::{TreeSitterRegex, c, csharp, go, hcl, python, rust, typescript};
use srgn::scoping::regex::Regex;
use srgn::scoping::view::ScopedViewBuilder;

use crate::tools::{CapabilityRequirement, Tool as ToolTrait, ToolContext, ToolError};

/// A single match result in search mode
#[derive(Debug, Serialize, Deserialize)]
struct Match {
    line: usize,
    column: usize,
    text: String,
}

/// Search mode results
#[derive(Debug, Serialize, Deserialize)]
struct SearchResults {
    mode: String,
    matches: Vec<Match>,
    total_matches: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    file: Option<String>,
}

/// Transform mode results
#[derive(Debug, Serialize, Deserialize)]
struct TransformResults {
    mode: String,
    original_length: usize,
    transformed_length: usize,
    content: String,
    changes_made: bool,
}

pub struct CodeSurgeonTool;

impl Default for CodeSurgeonTool {
    fn default() -> Self {
        Self::new()
    }
}

impl CodeSurgeonTool {
    pub fn new() -> Self {
        Self
    }

    /// Apply language scope to the builder
    fn apply_language_scope<'a>(
        language: &str,
        scope: Option<&str>,
        scope_pattern: Option<&str>,
        builder: &mut ScopedViewBuilder<'a>,
    ) -> Result<(), ToolError> {
        match language {
            "python" => Self::apply_python_scope(scope, scope_pattern, builder),
            "rust" => Self::apply_rust_scope(scope, scope_pattern, builder),
            "go" => Self::apply_go_scope(scope, scope_pattern, builder),
            "typescript" => Self::apply_typescript_scope(scope, scope_pattern, builder),
            "c" => Self::apply_c_scope(scope, scope_pattern, builder),
            "csharp" => Self::apply_csharp_scope(scope, scope_pattern, builder),
            "hcl" => Self::apply_hcl_scope(scope, scope_pattern, builder),
            _ => Err(ToolError::InvalidRequest(format!(
                "Unsupported language: {}. Must be one of: python, rust, go, typescript, c, csharp, hcl",
                language
            ))),
        }
    }

    fn apply_python_scope<'a>(
        scope: Option<&str>,
        _scope_pattern: Option<&str>, // Python doesn't support named patterns in srgn
        builder: &mut ScopedViewBuilder<'a>,
    ) -> Result<(), ToolError> {
        let query: Box<dyn Scoper> = match scope {
            None | Some("") => {
                return Err(ToolError::InvalidRequest(
                    "scope is required when language is specified".to_string(),
                ));
            }
            Some("comments") => {
                Box::new(python::CompiledQuery::from(python::PreparedQuery::Comments))
            }
            Some("strings") => {
                Box::new(python::CompiledQuery::from(python::PreparedQuery::Strings))
            }
            Some("doc-strings") | Some("docstrings") => Box::new(python::CompiledQuery::from(
                python::PreparedQuery::DocStrings,
            )),
            Some("imports") => {
                Box::new(python::CompiledQuery::from(python::PreparedQuery::Imports))
            }
            Some("class") => Box::new(python::CompiledQuery::from(python::PreparedQuery::Class)),
            Some("function") | Some("def") => {
                Box::new(python::CompiledQuery::from(python::PreparedQuery::Def))
            }
            Some("function-calls") => Box::new(python::CompiledQuery::from(
                python::PreparedQuery::FunctionCalls,
            )),
            Some("function-names") => Box::new(python::CompiledQuery::from(
                python::PreparedQuery::FunctionNames,
            )),
            Some(s) => {
                return Err(ToolError::InvalidRequest(format!(
                    "Unsupported Python scope: {}. Must be one of: comments, strings, doc-strings, imports, class, function, function-calls, function-names",
                    s
                )));
            }
        };
        builder.explode(query.as_ref());
        Ok(())
    }

    fn apply_rust_scope<'a>(
        scope: Option<&str>,
        scope_pattern: Option<&str>,
        builder: &mut ScopedViewBuilder<'a>,
    ) -> Result<(), ToolError> {
        let query: Box<dyn Scoper> = match (scope, scope_pattern) {
            (None, _) | (Some(""), _) => {
                return Err(ToolError::InvalidRequest(
                    "scope is required when language is specified".to_string(),
                ));
            }
            (Some("comments"), _) => {
                Box::new(rust::CompiledQuery::from(rust::PreparedQuery::Comments))
            }
            (Some("strings"), _) => {
                Box::new(rust::CompiledQuery::from(rust::PreparedQuery::Strings))
            }
            (Some("doc-comments") | Some("doccomments"), _) => {
                Box::new(rust::CompiledQuery::from(rust::PreparedQuery::DocComments))
            }
            (Some("uses"), _) => Box::new(rust::CompiledQuery::from(rust::PreparedQuery::Uses)),
            (Some("struct"), None) => {
                Box::new(rust::CompiledQuery::from(rust::PreparedQuery::Struct))
            }
            (Some("struct"), Some(pattern)) => {
                let regex = regex::bytes::Regex::new(pattern).map_err(|e| {
                    ToolError::InvalidRequest(format!("Invalid scope_pattern regex: {}", e))
                })?;
                Box::new(rust::CompiledQuery::from(rust::PreparedQuery::StructNamed(
                    TreeSitterRegex(regex),
                )))
            }
            (Some("enum"), None) => Box::new(rust::CompiledQuery::from(rust::PreparedQuery::Enum)),
            (Some("enum"), Some(pattern)) => {
                let regex = regex::bytes::Regex::new(pattern).map_err(|e| {
                    ToolError::InvalidRequest(format!("Invalid scope_pattern regex: {}", e))
                })?;
                Box::new(rust::CompiledQuery::from(rust::PreparedQuery::EnumNamed(
                    TreeSitterRegex(regex),
                )))
            }
            (Some("fn") | Some("function"), _) => {
                Box::new(rust::CompiledQuery::from(rust::PreparedQuery::Fn))
            }
            (Some("impl"), _) => Box::new(rust::CompiledQuery::from(rust::PreparedQuery::Impl)),
            (Some("trait"), None) => {
                Box::new(rust::CompiledQuery::from(rust::PreparedQuery::Trait))
            }
            (Some("trait"), Some(pattern)) => {
                let regex = regex::bytes::Regex::new(pattern).map_err(|e| {
                    ToolError::InvalidRequest(format!("Invalid scope_pattern regex: {}", e))
                })?;
                Box::new(rust::CompiledQuery::from(rust::PreparedQuery::TraitNamed(
                    TreeSitterRegex(regex),
                )))
            }
            (Some("attribute"), _) => {
                Box::new(rust::CompiledQuery::from(rust::PreparedQuery::Attribute))
            }
            (Some("unsafe"), _) => Box::new(rust::CompiledQuery::from(rust::PreparedQuery::Unsafe)),
            (Some("pub-enum"), _) => {
                Box::new(rust::CompiledQuery::from(rust::PreparedQuery::PubEnum))
            }
            (Some("type-identifier"), _) => Box::new(rust::CompiledQuery::from(
                rust::PreparedQuery::TypeIdentifier,
            )),
            (Some(s), _) => {
                return Err(ToolError::InvalidRequest(format!(
                    "Unsupported Rust scope: {}. Must be one of: comments, strings, doc-comments, uses, struct, enum, function, impl, trait, attribute, unsafe, pub-enum, type-identifier",
                    s
                )));
            }
        };
        builder.explode(query.as_ref());
        Ok(())
    }

    fn apply_go_scope<'a>(
        scope: Option<&str>,
        scope_pattern: Option<&str>,
        builder: &mut ScopedViewBuilder<'a>,
    ) -> Result<(), ToolError> {
        let query: Box<dyn Scoper> = match (scope, scope_pattern) {
            (None, _) | (Some(""), _) => {
                return Err(ToolError::InvalidRequest(
                    "scope is required when language is specified".to_string(),
                ));
            }
            (Some("comments"), _) => Box::new(go::CompiledQuery::from(go::PreparedQuery::Comments)),
            (Some("strings"), _) => Box::new(go::CompiledQuery::from(go::PreparedQuery::Strings)),
            (Some("imports"), _) => Box::new(go::CompiledQuery::from(go::PreparedQuery::Imports)),
            (Some("struct"), None) => Box::new(go::CompiledQuery::from(go::PreparedQuery::Struct)),
            (Some("struct"), Some(pattern)) => {
                let regex = regex::bytes::Regex::new(pattern).map_err(|e| {
                    ToolError::InvalidRequest(format!("Invalid scope_pattern regex: {}", e))
                })?;
                Box::new(go::CompiledQuery::from(go::PreparedQuery::StructNamed(
                    TreeSitterRegex(regex),
                )))
            }
            (Some("function") | Some("func"), _) => {
                Box::new(go::CompiledQuery::from(go::PreparedQuery::Func))
            }
            (Some("interface"), None) => {
                Box::new(go::CompiledQuery::from(go::PreparedQuery::Interface))
            }
            (Some("interface"), Some(pattern)) => {
                let regex = regex::bytes::Regex::new(pattern).map_err(|e| {
                    ToolError::InvalidRequest(format!("Invalid scope_pattern regex: {}", e))
                })?;
                Box::new(go::CompiledQuery::from(go::PreparedQuery::InterfaceNamed(
                    TreeSitterRegex(regex),
                )))
            }
            (Some(s), _) => {
                return Err(ToolError::InvalidRequest(format!(
                    "Unsupported Go scope: {}. Must be one of: comments, strings, imports, struct, function, interface",
                    s
                )));
            }
        };
        builder.explode(query.as_ref());
        Ok(())
    }

    fn apply_typescript_scope<'a>(
        scope: Option<&str>,
        _scope_pattern: Option<&str>,
        builder: &mut ScopedViewBuilder<'a>,
    ) -> Result<(), ToolError> {
        let query: Box<dyn Scoper> = match scope {
            None | Some("") => {
                return Err(ToolError::InvalidRequest(
                    "scope is required when language is specified".to_string(),
                ));
            }
            Some("comments") => Box::new(typescript::CompiledQuery::from(
                typescript::PreparedQuery::Comments,
            )),
            Some("strings") => Box::new(typescript::CompiledQuery::from(
                typescript::PreparedQuery::Strings,
            )),
            Some("imports") => Box::new(typescript::CompiledQuery::from(
                typescript::PreparedQuery::Imports,
            )),
            Some("class") => Box::new(typescript::CompiledQuery::from(
                typescript::PreparedQuery::Class,
            )),
            Some("function") => Box::new(typescript::CompiledQuery::from(
                typescript::PreparedQuery::Function,
            )),
            Some("interface") => Box::new(typescript::CompiledQuery::from(
                typescript::PreparedQuery::Interface,
            )),
            Some(s) => {
                return Err(ToolError::InvalidRequest(format!(
                    "Unsupported TypeScript scope: {}. Must be one of: comments, strings, imports, class, function, interface",
                    s
                )));
            }
        };
        builder.explode(query.as_ref());
        Ok(())
    }

    fn apply_c_scope<'a>(
        scope: Option<&str>,
        _scope_pattern: Option<&str>,
        builder: &mut ScopedViewBuilder<'a>,
    ) -> Result<(), ToolError> {
        let query: Box<dyn Scoper> = match scope {
            None | Some("") => {
                return Err(ToolError::InvalidRequest(
                    "scope is required when language is specified".to_string(),
                ));
            }
            Some("comments") => Box::new(c::CompiledQuery::from(c::PreparedQuery::Comments)),
            Some("strings") => Box::new(c::CompiledQuery::from(c::PreparedQuery::Strings)),
            Some("function") => Box::new(c::CompiledQuery::from(c::PreparedQuery::Function)),
            Some("struct") => Box::new(c::CompiledQuery::from(c::PreparedQuery::Struct)),
            Some(s) => {
                return Err(ToolError::InvalidRequest(format!(
                    "Unsupported C scope: {}. Must be one of: comments, strings, function, struct",
                    s
                )));
            }
        };
        builder.explode(query.as_ref());
        Ok(())
    }

    fn apply_csharp_scope<'a>(
        scope: Option<&str>,
        _scope_pattern: Option<&str>,
        builder: &mut ScopedViewBuilder<'a>,
    ) -> Result<(), ToolError> {
        let query: Box<dyn Scoper> = match scope {
            None | Some("") => {
                return Err(ToolError::InvalidRequest(
                    "scope is required when language is specified".to_string(),
                ));
            }
            Some("comments") => {
                Box::new(csharp::CompiledQuery::from(csharp::PreparedQuery::Comments))
            }
            Some("strings") => {
                Box::new(csharp::CompiledQuery::from(csharp::PreparedQuery::Strings))
            }
            Some("class") => Box::new(csharp::CompiledQuery::from(csharp::PreparedQuery::Class)),
            Some("function") | Some("method") => {
                Box::new(csharp::CompiledQuery::from(csharp::PreparedQuery::Method))
            }
            Some(s) => {
                return Err(ToolError::InvalidRequest(format!(
                    "Unsupported C# scope: {}. Must be one of: comments, strings, class, function",
                    s
                )));
            }
        };
        builder.explode(query.as_ref());
        Ok(())
    }

    fn apply_hcl_scope<'a>(
        scope: Option<&str>,
        _scope_pattern: Option<&str>,
        builder: &mut ScopedViewBuilder<'a>,
    ) -> Result<(), ToolError> {
        let query: Box<dyn Scoper> = match scope {
            None | Some("") => {
                return Err(ToolError::InvalidRequest(
                    "scope is required when language is specified".to_string(),
                ));
            }
            Some("comments") => Box::new(hcl::CompiledQuery::from(hcl::PreparedQuery::Comments)),
            Some("strings") => Box::new(hcl::CompiledQuery::from(hcl::PreparedQuery::Strings)),
            Some("resource") => Box::new(hcl::CompiledQuery::from(hcl::PreparedQuery::Resource)),
            Some("variable") => Box::new(hcl::CompiledQuery::from(hcl::PreparedQuery::Variable)),
            Some(s) => {
                return Err(ToolError::InvalidRequest(format!(
                    "Unsupported HCL scope: {}. Must be one of: comments, strings, resource, variable",
                    s
                )));
            }
        };
        builder.explode(query.as_ref());
        Ok(())
    }

    /// Extract matches from a scoped view (for search mode)
    fn extract_matches(content: &str, view: &srgn::scoping::view::ScopedView) -> Vec<Match> {
        let mut matches = Vec::new();

        // Get all in-scope ranges
        for scope in view.scopes().0.iter() {
            if let srgn::scoping::scope::Scope::In(cow_str, _) = &scope.0 {
                let match_text: &str = cow_str.as_ref();

                // Find this text in the content to get line/column info
                if let Some(offset) = content.find(match_text) {
                    // Count lines up to this offset
                    let prefix = &content[..offset];
                    let line = prefix.lines().count();
                    let line_start = prefix.rfind('\n').map(|i| i + 1).unwrap_or(0);
                    let column = offset - line_start;

                    matches.push(Match {
                        line: line + 1, // 1-indexed
                        column,
                        text: match_text.to_string(),
                    });
                }
            }
        }

        matches
    }
}

#[async_trait]
impl ToolTrait for CodeSurgeonTool {
    fn name(&self) -> &str {
        "code_surgeon"
    }

    fn definition(&self) -> Tool {
        Tool {
            tool_type: "function".to_string(),
            function: FunctionTool {
                name: self.name().to_string(),
                description: r#"Language-aware source code search and transformation using tree-sitter grammars.

MODE SELECTION:
- SEARCH mode: No action/replacement specified → returns matches with line/column info
- TRANSFORM mode: action or replacement specified → returns modified content (caller must write back)

REQUIRED INPUTS:
- One of: content (string) OR file_path (path to read)
- If scope is set, then language is required

BEHAVIOR:
- scope: Selects AST nodes (e.g., comments, strings, function, class). Entire node is selected.
- pattern: Optional regex filter applied within scope. If omitted, entire scope is selected.
- replacement: Applied to pattern matches within scope (enables TRANSFORM mode)
- action: Applied to matched content after replacement (enables TRANSFORM mode)

RETURN SCHEMA:
Search mode: { "mode": "search", "matches": [{"line": int, "column": int, "text": str}], "total_matches": int, "file": str? }
Transform mode: { "mode": "transform", "original_length": int, "transformed_length": int, "content": str, "changes_made": bool }

SUPPORTED SCOPES BY LANGUAGE:
Python: comments, strings, doc-strings, imports, class, function, function-calls, function-names
Rust: comments, strings, doc-comments, uses, struct, enum, function, impl, trait, attribute, unsafe, pub-enum, type-identifier
  (struct, enum, trait support scope_pattern for name filtering)
Go: comments, strings, imports, struct, function, interface
  (struct, interface support scope_pattern for name filtering)
TypeScript: comments, strings, imports, class, function, interface
C: comments, strings, function, struct
C#: comments, strings, class, function (or method)
HCL: comments, strings, resource, variable

ACTIONS: delete, squeeze, upper, lower, titlecase, normalize, symbols, german

LIMITATION: custom_query is not supported.

Examples:
- Search TODOs in comments: {"language": "python", "scope": "comments", "pattern": "TODO", "content": "..."}
- Delete comments: {"language": "rust", "scope": "comments", "action": "delete", "file_path": "main.rs"}
- Rename in imports: {"language": "python", "scope": "imports", "pattern": "old_name", "replacement": "new_name", "content": "..."}
- Uppercase strings: {"language": "go", "scope": "strings", "action": "upper", "content": "..."}
"#.to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "content": {
                            "type": "string",
                            "description": "Source code content to process. Either 'content' or 'file_path' is required."
                        },
                        "file_path": {
                            "type": "string",
                            "description": "Path to file to read and process. Either 'content' or 'file_path' is required."
                        },
                        "language": {
                            "type": "string",
                            "description": "Language for grammar-aware scoping. One of: python, rust, go, typescript, c, csharp, hcl",
                            "enum": ["python", "rust", "go", "typescript", "c", "csharp", "hcl"]
                        },
                        "scope": {
                            "type": "string",
                            "description": "Tree-sitter node type to scope to. Available scopes depend on language. Common: comments, strings, class, function, imports"
                        },
                        "scope_pattern": {
                            "type": "string",
                            "description": "Optional regex pattern to filter scoped items by name. Only supported for some scopes in Rust/Go (struct, enum, trait, interface). Example: with scope='struct', use scope_pattern='Test.*' to match only test-related structs."
                        },
                        "pattern": {
                            "type": "string",
                            "description": "Regex pattern to match within the scope. In search mode, defines what to find. In transform mode, defines what to replace/transform."
                        },
                        "replacement": {
                            "type": "string",
                            "description": "Replacement string. Supports capture group variables ($1, $2, $name). Specifying this enables TRANSFORM MODE."
                        },
                        "action": {
                            "type": "string",
                            "description": "Action to perform on matched content. Specifying this enables TRANSFORM MODE. If both replacement and action are given, replacement is applied first.",
                            "enum": ["delete", "squeeze", "upper", "lower", "titlecase", "normalize", "symbols", "german"]
                        }
                    },
                    "anyOf": [
                        { "required": ["content"] },
                        { "required": ["file_path"] }
                    ]
                }),
            },
        }
    }

    fn required_capabilities(&self) -> &'static [CapabilityRequirement] {
        &[CapabilityRequirement::Filesystem]
    }

    async fn call(&self, args: Value, context: &dyn ToolContext) -> Result<String, ToolError> {
        // Get input content (either from content or file_path)
        let content = if let Some(content_str) = args.get("content").and_then(Value::as_str) {
            content_str.to_string()
        } else if let Some(file_path_str) = args.get("file_path").and_then(Value::as_str) {
            let file_path = context.resolve_path(file_path_str)?;
            tokio::fs::read_to_string(&file_path)
                .await
                .map_err(|e| ToolError::ProviderError(format!("Failed to read file: {}", e)))?
        } else {
            return Err(ToolError::InvalidRequest(
                "Either 'content' or 'file_path' is required".to_string(),
            ));
        };

        let original_length = content.len();

        // Build scoped view
        let mut builder = ScopedViewBuilder::new(&content);

        // Apply language scope if specified
        if let Some(language) = args.get("language").and_then(Value::as_str) {
            let scope = args.get("scope").and_then(Value::as_str);
            let scope_pattern = args.get("scope_pattern").and_then(Value::as_str);
            Self::apply_language_scope(language, scope, scope_pattern, &mut builder)?;
        } else if args.get("scope").is_some() {
            return Err(ToolError::InvalidRequest(
                "scope requires language to be specified".to_string(),
            ));
        }

        // Apply custom query if specified
        if args.get("custom_query").is_some() {
            return Err(ToolError::InvalidRequest(
                "custom_query is not yet implemented".to_string(),
            ));
        }

        // Apply regex pattern scope if specified
        if let Some(pattern_str) = args.get("pattern").and_then(Value::as_str) {
            let regex_pattern = RegexPattern::new(pattern_str)
                .map_err(|e| ToolError::InvalidRequest(format!("Invalid regex pattern: {}", e)))?;
            let scoper = Regex::new(regex_pattern);
            builder.explode(&scoper);
        }

        // Build the view
        let mut view = builder.build();

        // Determine mode: search or transform
        let has_replacement = args.get("replacement").is_some();
        let has_action = args.get("action").is_some();
        let is_search_mode = !has_replacement && !has_action;

        if is_search_mode {
            // SEARCH MODE: Extract and return matches
            let matches = Self::extract_matches(&content, &view);
            let file_path = args
                .get("file_path")
                .and_then(Value::as_str)
                .map(|s| s.to_string());

            let results = SearchResults {
                mode: "search".to_string(),
                total_matches: matches.len(),
                matches,
                file: file_path,
            };

            serde_json::to_string_pretty(&results)
                .map_err(|e| ToolError::ProviderError(format!("Failed to serialize: {}", e)))
        } else {
            // TRANSFORM MODE: Apply actions and return transformed content

            // Apply replacement first if specified
            if let Some(replacement) = args.get("replacement").and_then(Value::as_str) {
                let _ = view.replace(replacement.to_string());
            }

            // Apply action if specified
            if let Some(action) = args.get("action").and_then(Value::as_str) {
                match action {
                    "delete" => {
                        view.delete();
                    }
                    "squeeze" => {
                        view.squeeze();
                    }
                    "upper" => {
                        view.upper();
                    }
                    "lower" => {
                        view.lower();
                    }
                    "titlecase" => {
                        view.titlecase();
                    }
                    "normalize" => {
                        view.normalize();
                    }
                    "symbols" => {
                        view.symbols();
                    }
                    "german" => {
                        view.german();
                    }
                    _ => {
                        return Err(ToolError::InvalidRequest(format!(
                            "Unknown action: {}. Must be one of: delete, squeeze, upper, lower, titlecase, normalize, symbols, german",
                            action
                        )));
                    }
                }
            }

            let transformed_content = view.to_string();
            let transformed_length = transformed_content.len();
            let changes_made = transformed_content != content;

            let results = TransformResults {
                mode: "transform".to_string(),
                original_length,
                transformed_length,
                content: transformed_content,
                changes_made,
            };

            serde_json::to_string_pretty(&results)
                .map_err(|e| ToolError::ProviderError(format!("Failed to serialize: {}", e)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::AgentToolContext;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_search_mode_python_comments() {
        let temp_dir = TempDir::new().unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));

        let code = r#"# TODO: implement this
def foo():
    # This is a comment
    todo = "not a comment"
    return todo
"#;

        let tool = CodeSurgeonTool::new();
        let args = json!({
            "content": code,
            "language": "python",
            "scope": "comments",
            "pattern": "TODO"
        });

        let result = tool.call(args, &context).await.unwrap();
        let parsed: SearchResults = serde_json::from_str(&result).unwrap();

        assert_eq!(parsed.mode, "search");
        assert_eq!(parsed.total_matches, 1);
        assert!(parsed.matches[0].text.contains("TODO"));
    }

    #[tokio::test]
    async fn test_transform_mode_delete_comments() {
        let temp_dir = TempDir::new().unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));

        let code = r#"# This is a comment
def foo():
    # Another comment
    return 42
"#;

        let tool = CodeSurgeonTool::new();
        let args = json!({
            "content": code,
            "language": "python",
            "scope": "comments",
            "action": "delete"
        });

        let result = tool.call(args, &context).await.unwrap();
        let parsed: TransformResults = serde_json::from_str(&result).unwrap();

        assert_eq!(parsed.mode, "transform");
        assert!(parsed.changes_made);
        assert!(!parsed.content.contains("This is a comment"));
        assert!(!parsed.content.contains("Another comment"));
        assert!(parsed.content.contains("def foo():"));
    }

    #[tokio::test]
    async fn test_transform_mode_replace_in_strings() {
        let temp_dir = TempDir::new().unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));

        let code = r#"
message = "Hello World"
other = 123
"#;

        let tool = CodeSurgeonTool::new();
        let args = json!({
            "content": code,
            "language": "python",
            "scope": "strings",
            "pattern": "World",
            "replacement": "Universe"
        });

        let result = tool.call(args, &context).await.unwrap();
        let parsed: TransformResults = serde_json::from_str(&result).unwrap();

        assert_eq!(parsed.mode, "transform");
        assert!(parsed.changes_made);
        assert!(parsed.content.contains("Hello Universe"));
    }

    #[tokio::test]
    async fn test_file_path_input() {
        let temp_dir = TempDir::new().unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));

        let code = "# TODO: test\ndef foo():\n    pass\n";
        let file_path = temp_dir.path().join("test.py");
        tokio::fs::write(&file_path, code).await.unwrap();

        let tool = CodeSurgeonTool::new();
        let args = json!({
            "file_path": file_path.to_str().unwrap(),
            "language": "python",
            "scope": "comments",
            "pattern": "TODO"
        });

        let result = tool.call(args, &context).await.unwrap();
        let parsed: SearchResults = serde_json::from_str(&result).unwrap();

        assert_eq!(parsed.mode, "search");
        assert_eq!(parsed.total_matches, 1);
        assert!(parsed.file.is_some());
    }

    #[tokio::test]
    async fn test_invalid_language_scope_combination() {
        let temp_dir = TempDir::new().unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));

        let tool = CodeSurgeonTool::new();
        let args = json!({
            "content": "# comment",
            "language": "python",
            "scope": "invalid_scope"
        });

        let result = tool.call(args, &context).await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Unsupported Python scope")
        );
    }

    #[tokio::test]
    async fn test_rust_unsafe_search() {
        let temp_dir = TempDir::new().unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));

        let code = r#"
fn safe_function() {
    let x = 5;
}

unsafe fn unsafe_function() {
    // unsafe code here
}

fn another_safe() {
    let unsafe_var = "not unsafe";
}
"#;

        let tool = CodeSurgeonTool::new();
        let args = json!({
            "content": code,
            "language": "rust",
            "scope": "unsafe"
        });

        let result = tool.call(args, &context).await.unwrap();
        let parsed: SearchResults = serde_json::from_str(&result).unwrap();

        assert_eq!(parsed.mode, "search");
        // Should find the unsafe function, not the variable named "unsafe_var"
        assert!(parsed.total_matches > 0);
    }

    #[tokio::test]
    async fn test_action_uppercase() {
        let temp_dir = TempDir::new().unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));

        let code = r#"msg = "hello world""#;

        let tool = CodeSurgeonTool::new();
        let args = json!({
            "content": code,
            "language": "python",
            "scope": "strings",
            "action": "upper"
        });

        let result = tool.call(args, &context).await.unwrap();
        let parsed: TransformResults = serde_json::from_str(&result).unwrap();

        assert_eq!(parsed.mode, "transform");
        assert!(parsed.content.contains("HELLO WORLD"));
    }
}
