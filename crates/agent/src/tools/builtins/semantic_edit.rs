//! Semantic edit tool for AST-aware source code search and transformation

use async_trait::async_trait;
use ignore::WalkBuilder;
use indexmap::IndexMap;
use querymt::chat::{FunctionTool, Tool};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use srgn::RegexPattern;
use srgn::find::Find;
use srgn::scoping::Scoper;
use srgn::scoping::langs::{TreeSitterRegex, c, csharp, go, hcl, python, rust, typescript};
use srgn::scoping::regex::Regex;
use srgn::scoping::view::ScopedViewBuilder;
use std::path::Path;

use crate::tools::{CapabilityRequirement, Tool as ToolTrait, ToolContext, ToolError};

/// A single match result
#[derive(Debug, Serialize, Deserialize)]
struct Match {
    file: String,
    line: usize,
    column: usize,
    text: String,
}

/// Search mode results (compact format)
#[derive(Debug, Serialize, Deserialize)]
struct SearchResults {
    mode: String,
    #[serde(serialize_with = "serialize_matches_compact")]
    results: Vec<Match>,
    total_matches: usize,
    files_searched: usize,
}

/// Serialize matches into compact IndexMap<String, String> format
fn serialize_matches_compact<S>(matches: &[Match], serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    use serde::ser::SerializeMap;

    let mut file_lines: IndexMap<String, Vec<String>> = IndexMap::new();

    for m in matches {
        let formatted = format!("{}:{}:{}", m.line, m.column, m.text);
        file_lines
            .entry(m.file.clone())
            .or_default()
            .push(formatted);
    }

    let mut map = serializer.serialize_map(Some(file_lines.len()))?;
    for (file, lines) in file_lines {
        map.serialize_entry(&file, &lines.join("\n"))?;
    }
    map.end()
}

/// Transform mode results
#[derive(Debug, Serialize, Deserialize)]
struct TransformResults {
    mode: String,
    files_modified: Vec<String>,
    total_files_modified: usize,
    files_searched: usize,
}

pub struct SemanticEditTool;

impl Default for SemanticEditTool {
    fn default() -> Self {
        Self::new()
    }
}

impl SemanticEditTool {
    pub fn new() -> Self {
        Self
    }

    /// Check if a path is valid for the given language
    fn is_valid_for_language(path: &Path, language: &str) -> bool {
        match language {
            "python" => {
                python::CompiledQuery::from(python::PreparedQuery::Comments).is_valid_path(path)
            }
            "rust" => rust::CompiledQuery::from(rust::PreparedQuery::Comments).is_valid_path(path),
            "go" => go::CompiledQuery::from(go::PreparedQuery::Comments).is_valid_path(path),
            "typescript" => typescript::CompiledQuery::from(typescript::PreparedQuery::Comments)
                .is_valid_path(path),
            "c" => c::CompiledQuery::from(c::PreparedQuery::Comments).is_valid_path(path),
            "csharp" => {
                csharp::CompiledQuery::from(csharp::PreparedQuery::Comments).is_valid_path(path)
            }
            "hcl" => hcl::CompiledQuery::from(hcl::PreparedQuery::Comments).is_valid_path(path),
            _ => false,
        }
    }

    /// Apply language scope to the builder
    fn apply_language_scope<'a>(
        language: &str,
        scope: &str,
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
        scope: &str,
        _scope_pattern: Option<&str>, // Python doesn't support named patterns in srgn
        builder: &mut ScopedViewBuilder<'a>,
    ) -> Result<(), ToolError> {
        let query: Box<dyn Scoper> = match scope {
            "comments" => Box::new(python::CompiledQuery::from(python::PreparedQuery::Comments)),
            "strings" => Box::new(python::CompiledQuery::from(python::PreparedQuery::Strings)),
            "doc-strings" | "docstrings" => Box::new(python::CompiledQuery::from(
                python::PreparedQuery::DocStrings,
            )),
            "imports" => Box::new(python::CompiledQuery::from(python::PreparedQuery::Imports)),
            "class" => Box::new(python::CompiledQuery::from(python::PreparedQuery::Class)),
            "function" | "def" => Box::new(python::CompiledQuery::from(python::PreparedQuery::Def)),
            "function-calls" => Box::new(python::CompiledQuery::from(
                python::PreparedQuery::FunctionCalls,
            )),
            "function-names" => Box::new(python::CompiledQuery::from(
                python::PreparedQuery::FunctionNames,
            )),
            s => {
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
        scope: &str,
        scope_pattern: Option<&str>,
        builder: &mut ScopedViewBuilder<'a>,
    ) -> Result<(), ToolError> {
        let query: Box<dyn Scoper> = match (scope, scope_pattern) {
            ("comments", _) => Box::new(rust::CompiledQuery::from(rust::PreparedQuery::Comments)),
            ("strings", _) => Box::new(rust::CompiledQuery::from(rust::PreparedQuery::Strings)),
            ("doc-comments" | "doccomments", _) => {
                Box::new(rust::CompiledQuery::from(rust::PreparedQuery::DocComments))
            }
            ("uses", _) => Box::new(rust::CompiledQuery::from(rust::PreparedQuery::Uses)),
            ("struct", None) => Box::new(rust::CompiledQuery::from(rust::PreparedQuery::Struct)),
            ("struct", Some(pattern)) => {
                let regex = regex::bytes::Regex::new(pattern).map_err(|e| {
                    ToolError::InvalidRequest(format!("Invalid scope_pattern regex: {}", e))
                })?;
                Box::new(rust::CompiledQuery::from(rust::PreparedQuery::StructNamed(
                    TreeSitterRegex(regex),
                )))
            }
            ("enum", None) => Box::new(rust::CompiledQuery::from(rust::PreparedQuery::Enum)),
            ("enum", Some(pattern)) => {
                let regex = regex::bytes::Regex::new(pattern).map_err(|e| {
                    ToolError::InvalidRequest(format!("Invalid scope_pattern regex: {}", e))
                })?;
                Box::new(rust::CompiledQuery::from(rust::PreparedQuery::EnumNamed(
                    TreeSitterRegex(regex),
                )))
            }
            ("fn" | "function", _) => Box::new(rust::CompiledQuery::from(rust::PreparedQuery::Fn)),
            ("impl", _) => Box::new(rust::CompiledQuery::from(rust::PreparedQuery::Impl)),
            ("trait", None) => Box::new(rust::CompiledQuery::from(rust::PreparedQuery::Trait)),
            ("trait", Some(pattern)) => {
                let regex = regex::bytes::Regex::new(pattern).map_err(|e| {
                    ToolError::InvalidRequest(format!("Invalid scope_pattern regex: {}", e))
                })?;
                Box::new(rust::CompiledQuery::from(rust::PreparedQuery::TraitNamed(
                    TreeSitterRegex(regex),
                )))
            }
            ("attribute", _) => Box::new(rust::CompiledQuery::from(rust::PreparedQuery::Attribute)),
            ("unsafe", _) => Box::new(rust::CompiledQuery::from(rust::PreparedQuery::Unsafe)),
            ("pub-enum", _) => Box::new(rust::CompiledQuery::from(rust::PreparedQuery::PubEnum)),
            ("type-identifier", _) => Box::new(rust::CompiledQuery::from(
                rust::PreparedQuery::TypeIdentifier,
            )),
            (s, _) => {
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
        scope: &str,
        scope_pattern: Option<&str>,
        builder: &mut ScopedViewBuilder<'a>,
    ) -> Result<(), ToolError> {
        let query: Box<dyn Scoper> = match (scope, scope_pattern) {
            ("comments", _) => Box::new(go::CompiledQuery::from(go::PreparedQuery::Comments)),
            ("strings", _) => Box::new(go::CompiledQuery::from(go::PreparedQuery::Strings)),
            ("imports", _) => Box::new(go::CompiledQuery::from(go::PreparedQuery::Imports)),
            ("struct", None) => Box::new(go::CompiledQuery::from(go::PreparedQuery::Struct)),
            ("struct", Some(pattern)) => {
                let regex = regex::bytes::Regex::new(pattern).map_err(|e| {
                    ToolError::InvalidRequest(format!("Invalid scope_pattern regex: {}", e))
                })?;
                Box::new(go::CompiledQuery::from(go::PreparedQuery::StructNamed(
                    TreeSitterRegex(regex),
                )))
            }
            ("function" | "func", _) => Box::new(go::CompiledQuery::from(go::PreparedQuery::Func)),
            ("interface", None) => Box::new(go::CompiledQuery::from(go::PreparedQuery::Interface)),
            ("interface", Some(pattern)) => {
                let regex = regex::bytes::Regex::new(pattern).map_err(|e| {
                    ToolError::InvalidRequest(format!("Invalid scope_pattern regex: {}", e))
                })?;
                Box::new(go::CompiledQuery::from(go::PreparedQuery::InterfaceNamed(
                    TreeSitterRegex(regex),
                )))
            }
            (s, _) => {
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
        scope: &str,
        _scope_pattern: Option<&str>,
        builder: &mut ScopedViewBuilder<'a>,
    ) -> Result<(), ToolError> {
        let query: Box<dyn Scoper> = match scope {
            "comments" => Box::new(typescript::CompiledQuery::from(
                typescript::PreparedQuery::Comments,
            )),
            "strings" => Box::new(typescript::CompiledQuery::from(
                typescript::PreparedQuery::Strings,
            )),
            "imports" => Box::new(typescript::CompiledQuery::from(
                typescript::PreparedQuery::Imports,
            )),
            "class" => Box::new(typescript::CompiledQuery::from(
                typescript::PreparedQuery::Class,
            )),
            "function" => Box::new(typescript::CompiledQuery::from(
                typescript::PreparedQuery::Function,
            )),
            "interface" => Box::new(typescript::CompiledQuery::from(
                typescript::PreparedQuery::Interface,
            )),
            s => {
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
        scope: &str,
        _scope_pattern: Option<&str>,
        builder: &mut ScopedViewBuilder<'a>,
    ) -> Result<(), ToolError> {
        let query: Box<dyn Scoper> = match scope {
            "comments" => Box::new(c::CompiledQuery::from(c::PreparedQuery::Comments)),
            "strings" => Box::new(c::CompiledQuery::from(c::PreparedQuery::Strings)),
            "function" => Box::new(c::CompiledQuery::from(c::PreparedQuery::Function)),
            "struct" => Box::new(c::CompiledQuery::from(c::PreparedQuery::Struct)),
            s => {
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
        scope: &str,
        _scope_pattern: Option<&str>,
        builder: &mut ScopedViewBuilder<'a>,
    ) -> Result<(), ToolError> {
        let query: Box<dyn Scoper> = match scope {
            "comments" => Box::new(csharp::CompiledQuery::from(csharp::PreparedQuery::Comments)),
            "strings" => Box::new(csharp::CompiledQuery::from(csharp::PreparedQuery::Strings)),
            "class" => Box::new(csharp::CompiledQuery::from(csharp::PreparedQuery::Class)),
            "function" | "method" => {
                Box::new(csharp::CompiledQuery::from(csharp::PreparedQuery::Method))
            }
            s => {
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
        scope: &str,
        _scope_pattern: Option<&str>,
        builder: &mut ScopedViewBuilder<'a>,
    ) -> Result<(), ToolError> {
        let query: Box<dyn Scoper> = match scope {
            "comments" => Box::new(hcl::CompiledQuery::from(hcl::PreparedQuery::Comments)),
            "strings" => Box::new(hcl::CompiledQuery::from(hcl::PreparedQuery::Strings)),
            "resource" => Box::new(hcl::CompiledQuery::from(hcl::PreparedQuery::Resource)),
            "variable" => Box::new(hcl::CompiledQuery::from(hcl::PreparedQuery::Variable)),
            s => {
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
    fn extract_matches(
        content: &str,
        view: &srgn::scoping::view::ScopedView,
        file_path: &str,
    ) -> Vec<Match> {
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
                        file: file_path.to_string(),
                        line: line + 1, // 1-indexed
                        column,
                        text: match_text.to_string(),
                    });
                }
            }
        }

        matches
    }

    /// Process a single file
    async fn process_file(
        file_path: &Path,
        language: &str,
        scope: &str,
        scope_pattern: Option<&str>,
        pattern: Option<&str>,
        replacement: Option<&str>,
        action: Option<&str>,
    ) -> Result<(Vec<Match>, bool), ToolError> {
        // Read file
        let content = tokio::fs::read_to_string(file_path)
            .await
            .map_err(|e| ToolError::ProviderError(format!("Failed to read file: {}", e)))?;

        let original_length = content.len();

        // Build scoped view
        let mut builder = ScopedViewBuilder::new(&content);

        // Apply language scope
        Self::apply_language_scope(language, scope, scope_pattern, &mut builder)?;

        // Apply regex pattern scope if specified
        if let Some(pattern_str) = pattern {
            let regex_pattern = RegexPattern::new(pattern_str)
                .map_err(|e| ToolError::InvalidRequest(format!("Invalid regex pattern: {}", e)))?;
            let scoper = Regex::new(regex_pattern);
            builder.explode(&scoper);
        }

        // Build the view
        let mut view = builder.build();

        // Determine mode: search or transform
        let is_search_mode = replacement.is_none() && action.is_none();

        if is_search_mode {
            // SEARCH MODE: Extract and return matches
            let matches = Self::extract_matches(&content, &view, file_path.to_str().unwrap_or(""));
            Ok((matches, false))
        } else {
            // TRANSFORM MODE: Apply actions and write file

            // Apply replacement first if specified
            if let Some(repl) = replacement {
                let _ = view.replace(repl.to_string());
            }

            // Apply action if specified
            if let Some(act) = action {
                match act {
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
                    _ => {
                        return Err(ToolError::InvalidRequest(format!(
                            "Unknown action: {}. Must be one of: delete, squeeze, upper, lower, titlecase, normalize",
                            act
                        )));
                    }
                }
            }

            let transformed_content = view.to_string();
            let changes_made = transformed_content != content;

            if changes_made {
                // Failsafe: don't wipe non-empty files
                if original_length > 0 && transformed_content.is_empty() {
                    return Err(ToolError::ProviderError(format!(
                        "Refusing to wipe non-empty file: {}",
                        file_path.display()
                    )));
                }

                // Write file in-place
                tokio::fs::write(file_path, transformed_content.as_bytes())
                    .await
                    .map_err(|e| {
                        ToolError::ProviderError(format!("Failed to write file: {}", e))
                    })?;
            }

            Ok((Vec::new(), changes_made))
        }
    }
}

#[async_trait]
impl ToolTrait for SemanticEditTool {
    fn name(&self) -> &str {
        "semantic_edit"
    }

    fn definition(&self) -> Tool {
        Tool {
            tool_type: "function".to_string(),
            function: FunctionTool {
                name: self.name().to_string(),
                description: r#"AST-aware search and replace. Understands code structure - won't match "TODO" in a string when searching comments.

REQUIRES: language + scope (this is for semantic operations, not plain regex)

EXAMPLES:
- Find TODOs in comments: {language: "rust", scope: "comments", pattern: "TODO"}
- Delete all docstrings: {language: "python", scope: "doc-strings", action: "delete"}  
- Rename in functions only: {language: "go", scope: "function", pattern: "oldName", replacement: "newName"}
- Find unsafe blocks: {language: "rust", scope: "unsafe"}

SCOPES BY LANGUAGE:
Python: comments, strings, doc-strings, imports, class, function, function-calls, function-names
Rust: comments, strings, doc-comments, uses, struct, enum, function, impl, trait, attribute, unsafe, pub-enum, type-identifier
Go: comments, strings, imports, struct, function, interface
TypeScript: comments, strings, imports, class, function, interface
C: comments, strings, function, struct
C#: comments, strings, class, function
HCL: comments, strings, resource, variable

Walks directories recursively, respects .gitignore. Set path to limit scope.
Transform mode modifies files in-place."#.to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "language": {
                            "type": "string",
                            "description": "Language for grammar-aware scoping. One of: python, rust, go, typescript, c, csharp, hcl",
                            "enum": ["python", "rust", "go", "typescript", "c", "csharp", "hcl"]
                        },
                        "scope": {
                            "type": "string",
                            "description": "Tree-sitter node type to scope to. Available scopes depend on language."
                        },
                        "path": {
                            "type": "string",
                            "description": "File or directory to process. If omitted, processes entire workspace."
                        },
                        "scope_pattern": {
                            "type": "string",
                            "description": "Optional regex pattern to filter scoped items by name. Only supported for some scopes in Rust/Go (struct, enum, trait, interface)."
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
                            "enum": ["delete", "squeeze", "upper", "lower", "titlecase", "normalize"]
                        },
                        "hidden": {
                            "type": "boolean",
                            "description": "Include hidden files and directories. Default: false"
                        },
                        "gitignored": {
                            "type": "boolean",
                            "description": "Include .gitignore'd files and directories. Default: false"
                        }
                    },
                    "required": ["language", "scope"]
                }),
            },
        }
    }

    fn required_capabilities(&self) -> &'static [CapabilityRequirement] {
        &[CapabilityRequirement::Filesystem]
    }

    async fn call(&self, args: Value, context: &dyn ToolContext) -> Result<String, ToolError> {
        // Extract required parameters
        let language = args
            .get("language")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidRequest("language is required".to_string()))?;

        let scope = args
            .get("scope")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidRequest("scope is required".to_string()))?;

        // Extract optional parameters
        let scope_pattern = args.get("scope_pattern").and_then(Value::as_str);
        let pattern = args.get("pattern").and_then(Value::as_str);
        let replacement = args.get("replacement").and_then(Value::as_str);
        let action = args.get("action").and_then(Value::as_str);
        let hidden = args.get("hidden").and_then(Value::as_bool).unwrap_or(false);
        let gitignored = args
            .get("gitignored")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        // Determine mode
        let is_search_mode = replacement.is_none() && action.is_none();

        // Resolve path (default to current working directory)
        let path = if let Some(path_str) = args.get("path").and_then(Value::as_str) {
            context.resolve_path(path_str)?
        } else {
            context
                .cwd()
                .ok_or_else(|| {
                    ToolError::InvalidRequest(
                        "No working directory set and no path specified".to_string(),
                    )
                })?
                .to_path_buf()
        };

        // Check if path is a file or directory
        let metadata: std::fs::Metadata = tokio::fs::metadata(&path)
            .await
            .map_err(|e| ToolError::InvalidRequest(format!("Invalid path: {}", e)))?;

        let mut all_matches = Vec::new();
        let mut modified_files = Vec::new();
        let mut files_searched = 0;

        if metadata.is_file() {
            // Process single file
            if Self::is_valid_for_language(&path, language) {
                files_searched += 1;
                let (matches, modified) = Self::process_file(
                    &path,
                    language,
                    scope,
                    scope_pattern,
                    pattern,
                    replacement,
                    action,
                )
                .await?;

                all_matches.extend(matches);
                if modified {
                    modified_files.push(path.to_string_lossy().to_string());
                }
            }
        } else {
            // Walk directory
            let walker = WalkBuilder::new(&path)
                .hidden(!hidden)
                .git_ignore(!gitignored)
                .build();

            for entry in walker {
                let entry = entry.map_err(|e| {
                    ToolError::ProviderError(format!("Error walking directory: {}", e))
                })?;

                let entry_path = entry.path();

                // Skip if not a file
                if !entry_path.is_file() {
                    continue;
                }

                // Skip if not valid for this language
                if !Self::is_valid_for_language(entry_path, language) {
                    continue;
                }

                files_searched += 1;

                // Process file
                match Self::process_file(
                    entry_path,
                    language,
                    scope,
                    scope_pattern,
                    pattern,
                    replacement,
                    action,
                )
                .await
                {
                    Ok((matches, modified)) => {
                        all_matches.extend(matches);
                        if modified {
                            modified_files.push(entry_path.to_string_lossy().to_string());
                        }
                    }
                    Err(e) => {
                        // Log error but continue processing other files
                        eprintln!("Error processing {}: {}", entry_path.display(), e);
                    }
                }
            }
        }

        // Return results based on mode
        if is_search_mode {
            // Sort matches by file modification time (most recent first)
            use std::collections::HashMap;
            let mut file_times: HashMap<String, std::time::SystemTime> = HashMap::new();
            for m in &all_matches {
                if !file_times.contains_key(&m.file)
                    && let Ok(metadata) = std::fs::metadata(&m.file)
                    && let Ok(modified) = metadata.modified()
                {
                    file_times.insert(m.file.clone(), modified);
                }
            }

            all_matches.sort_by(|a, b| {
                let a_time = file_times.get(&a.file);
                let b_time = file_times.get(&b.file);
                b_time.cmp(&a_time) // Reverse for most recent first
            });

            // Convert to relative paths
            for m in &mut all_matches {
                let rel_path = Path::new(&m.file)
                    .strip_prefix(&path)
                    .unwrap_or(Path::new(&m.file))
                    .display()
                    .to_string();
                m.file = rel_path;
            }

            let results = SearchResults {
                mode: "search".to_string(),
                total_matches: all_matches.len(),
                results: all_matches,
                files_searched,
            };

            serde_json::to_string_pretty(&results)
                .map_err(|e| ToolError::ProviderError(format!("Failed to serialize: {}", e)))
        } else {
            // Convert to relative paths
            let relative_modified_files: Vec<String> = modified_files
                .into_iter()
                .map(|f| {
                    Path::new(&f)
                        .strip_prefix(&path)
                        .unwrap_or(Path::new(&f))
                        .display()
                        .to_string()
                })
                .collect();

            let results = TransformResults {
                mode: "transform".to_string(),
                total_files_modified: relative_modified_files.len(),
                files_modified: relative_modified_files,
                files_searched,
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
    async fn test_search_mode_single_file() {
        let temp_dir = TempDir::new().unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));

        let code = r#"# TODO: implement this
def foo():
    # This is a comment
    todo = "not a comment"
    return todo
"#;

        let file_path = temp_dir.path().join("test.py");
        tokio::fs::write(&file_path, code).await.unwrap();

        let tool = SemanticEditTool::new();
        let args = json!({
            "path": file_path.to_str().unwrap(),
            "language": "python",
            "scope": "comments",
            "pattern": "TODO"
        });

        let result = tool.call(args, &context).await.unwrap();

        // Parse as generic JSON to check the serialized format
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

        assert_eq!(parsed["mode"], "search");
        assert_eq!(parsed["total_matches"], 1);
        assert_eq!(parsed["files_searched"], 1);

        // Check that results is an object with file paths as keys
        let results = parsed["results"].as_object().unwrap();
        assert_eq!(results.len(), 1);

        // Get the first (and only) file's matches
        let file_matches = results.values().next().unwrap().as_str().unwrap();
        assert!(file_matches.contains("TODO"));
    }

    #[tokio::test]
    async fn test_search_mode_directory() {
        let temp_dir = TempDir::new().unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));

        // Create multiple Python files
        let code1 = "# TODO: first\ndef foo():\n    pass\n";
        let code2 = "# TODO: second\ndef bar():\n    pass\n";

        tokio::fs::write(temp_dir.path().join("file1.py"), code1)
            .await
            .unwrap();
        tokio::fs::write(temp_dir.path().join("file2.py"), code2)
            .await
            .unwrap();

        let tool = SemanticEditTool::new();
        let args = json!({
            "path": temp_dir.path().to_str().unwrap(),
            "language": "python",
            "scope": "comments",
            "pattern": "TODO"
        });

        let result = tool.call(args, &context).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

        assert_eq!(parsed["mode"], "search");
        assert_eq!(parsed["total_matches"], 2);
        assert_eq!(parsed["files_searched"], 2);
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

        let file_path = temp_dir.path().join("test.py");
        tokio::fs::write(&file_path, code).await.unwrap();

        let tool = SemanticEditTool::new();
        let args = json!({
            "path": file_path.to_str().unwrap(),
            "language": "python",
            "scope": "comments",
            "action": "delete"
        });

        let result = tool.call(args, &context).await.unwrap();
        let parsed: TransformResults = serde_json::from_str(&result).unwrap();

        assert_eq!(parsed.mode, "transform");
        assert_eq!(parsed.total_files_modified, 1);

        // Verify file was actually modified
        let modified_content = tokio::fs::read_to_string(&file_path).await.unwrap();
        assert!(!modified_content.contains("This is a comment"));
        assert!(!modified_content.contains("Another comment"));
        assert!(modified_content.contains("def foo():"));
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

        let file_path = temp_dir.path().join("test.py");
        tokio::fs::write(&file_path, code).await.unwrap();

        let tool = SemanticEditTool::new();
        let args = json!({
            "path": file_path.to_str().unwrap(),
            "language": "python",
            "scope": "strings",
            "pattern": "World",
            "replacement": "Universe"
        });

        let result = tool.call(args, &context).await.unwrap();
        let parsed: TransformResults = serde_json::from_str(&result).unwrap();

        assert_eq!(parsed.mode, "transform");
        assert_eq!(parsed.total_files_modified, 1);

        // Verify file was actually modified
        let modified_content = tokio::fs::read_to_string(&file_path).await.unwrap();
        assert!(modified_content.contains("Hello Universe"));
    }

    #[tokio::test]
    async fn test_language_filtering() {
        let temp_dir = TempDir::new().unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));

        // Create Python and non-Python files
        tokio::fs::write(temp_dir.path().join("test.py"), "# TODO\n")
            .await
            .unwrap();
        tokio::fs::write(temp_dir.path().join("test.txt"), "TODO\n")
            .await
            .unwrap();

        let tool = SemanticEditTool::new();
        let args = json!({
            "path": temp_dir.path().to_str().unwrap(),
            "language": "python",
            "scope": "comments",
            "pattern": "TODO"
        });

        let result = tool.call(args, &context).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

        // Should only process the .py file
        assert_eq!(parsed["files_searched"], 1);
    }

    #[tokio::test]
    async fn test_default_workspace_path() {
        let temp_dir = TempDir::new().unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));

        let code = "# TODO\n";
        tokio::fs::write(temp_dir.path().join("test.py"), code)
            .await
            .unwrap();

        let tool = SemanticEditTool::new();
        let args = json!({
            // No path specified - should use workspace root
            "language": "python",
            "scope": "comments",
            "pattern": "TODO"
        });

        let result = tool.call(args, &context).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

        assert_eq!(parsed["files_searched"], 1);
        assert_eq!(parsed["total_matches"], 1);
    }

    #[tokio::test]
    async fn test_no_matches_doesnt_modify() {
        let temp_dir = TempDir::new().unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));

        let code = "def foo():\n    pass\n";
        let file_path = temp_dir.path().join("test.py");
        tokio::fs::write(&file_path, code).await.unwrap();

        let tool = SemanticEditTool::new();
        let args = json!({
            "path": file_path.to_str().unwrap(),
            "language": "python",
            "scope": "comments",  // No comments in this file
            "action": "delete"
        });

        // Should succeed but not modify anything
        let result = tool.call(args, &context).await.unwrap();
        let parsed: TransformResults = serde_json::from_str(&result).unwrap();

        assert_eq!(parsed.total_files_modified, 0);

        // Verify file wasn't modified
        let content = tokio::fs::read_to_string(&file_path).await.unwrap();
        assert_eq!(content, code);
    }
}
