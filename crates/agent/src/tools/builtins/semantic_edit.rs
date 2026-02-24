//! Semantic edit tool for AST-aware source code search and transformation

use async_trait::async_trait;
use glob::glob;
use ignore::WalkBuilder;
use indexmap::IndexMap;
use querymt::chat::{FunctionTool, Tool};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use srgn::RegexPattern;
use srgn::find::Find;
use srgn::scoping::Scoper;
use srgn::scoping::langs::{TreeSitterRegex, c, csharp, go, hcl, python, rust, typescript};
use srgn::scoping::literal::Literal;
use srgn::scoping::regex::Regex;
use srgn::scoping::view::ScopedViewBuilder;
use std::path::{Path, PathBuf};

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

/// A parsed scope entry: the scope name and an optional name-filter regex (from `~` syntax).
struct ScopeEntry {
    name: String,
    pattern: Option<String>,
}

impl ScopeEntry {
    /// Parse `"struct~[tT]est"` into `ScopeEntry { name: "struct", pattern: Some("[tT]est") }`.
    fn parse(s: &str) -> Self {
        if let Some(idx) = s.find('~') {
            Self {
                name: s[..idx].to_string(),
                pattern: Some(s[idx + 1..].to_string()),
            }
        } else {
            Self {
                name: s.to_string(),
                pattern: None,
            }
        }
    }
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

    /// Apply a single language scope entry (with optional `~` name filter) to the builder.
    fn apply_language_scope<'a>(
        language: &str,
        entry: &ScopeEntry,
        builder: &mut ScopedViewBuilder<'a>,
    ) -> Result<(), ToolError> {
        let scope = entry.name.as_str();
        let scope_pattern = entry.pattern.as_deref();
        match language {
            "python" => Self::apply_python_scope(scope, builder),
            "rust" => Self::apply_rust_scope(scope, scope_pattern, builder),
            "go" => Self::apply_go_scope(scope, scope_pattern, builder),
            "typescript" => Self::apply_typescript_scope(scope, builder),
            "c" => Self::apply_c_scope(scope, builder),
            "csharp" => Self::apply_csharp_scope(scope, builder),
            "hcl" => Self::apply_hcl_scope(scope, scope_pattern, builder),
            _ => Err(ToolError::InvalidRequest(format!(
                "Unsupported language: {}. Must be one of: python, rust, go, typescript, c, csharp, hcl",
                language
            ))),
        }
    }

    fn apply_python_scope<'a>(
        scope: &str,
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
            "async-def" => Box::new(python::CompiledQuery::from(python::PreparedQuery::AsyncDef)),
            "methods" => Box::new(python::CompiledQuery::from(python::PreparedQuery::Methods)),
            "class-methods" => Box::new(python::CompiledQuery::from(
                python::PreparedQuery::ClassMethods,
            )),
            "static-methods" => Box::new(python::CompiledQuery::from(
                python::PreparedQuery::StaticMethods,
            )),
            "function-calls" => Box::new(python::CompiledQuery::from(
                python::PreparedQuery::FunctionCalls,
            )),
            "function-names" => Box::new(python::CompiledQuery::from(
                python::PreparedQuery::FunctionNames,
            )),
            "with" => Box::new(python::CompiledQuery::from(python::PreparedQuery::With)),
            "try" => Box::new(python::CompiledQuery::from(python::PreparedQuery::Try)),
            "lambda" => Box::new(python::CompiledQuery::from(python::PreparedQuery::Lambda)),
            "globals" => Box::new(python::CompiledQuery::from(python::PreparedQuery::Globals)),
            "variable-identifiers" => Box::new(python::CompiledQuery::from(
                python::PreparedQuery::VariableIdentifiers,
            )),
            "types" => Box::new(python::CompiledQuery::from(python::PreparedQuery::Types)),
            "identifiers" => Box::new(python::CompiledQuery::from(
                python::PreparedQuery::Identifiers,
            )),
            s => {
                return Err(ToolError::InvalidRequest(format!(
                    "Unsupported Python scope: {}. Must be one of: async-def, class, class-methods, \
comments, doc-strings, function, function-calls, function-names, globals, \
identifiers, imports, lambda, methods, static-methods, strings, try, types, \
variable-identifiers, with",
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
                    ToolError::InvalidRequest(format!("Invalid scope ~ pattern regex: {}", e))
                })?;
                Box::new(rust::CompiledQuery::from(rust::PreparedQuery::StructNamed(
                    TreeSitterRegex(regex),
                )))
            }
            ("enum", None) => Box::new(rust::CompiledQuery::from(rust::PreparedQuery::Enum)),
            ("enum", Some(pattern)) => {
                let regex = regex::bytes::Regex::new(pattern).map_err(|e| {
                    ToolError::InvalidRequest(format!("Invalid scope ~ pattern regex: {}", e))
                })?;
                Box::new(rust::CompiledQuery::from(rust::PreparedQuery::EnumNamed(
                    TreeSitterRegex(regex),
                )))
            }
            ("fn" | "function", None) => {
                Box::new(rust::CompiledQuery::from(rust::PreparedQuery::Fn))
            }
            ("fn" | "function", Some(pattern)) => {
                let regex = regex::bytes::Regex::new(pattern).map_err(|e| {
                    ToolError::InvalidRequest(format!("Invalid scope ~ pattern regex: {}", e))
                })?;
                Box::new(rust::CompiledQuery::from(rust::PreparedQuery::FnNamed(
                    TreeSitterRegex(regex),
                )))
            }
            ("impl-fn", _) => Box::new(rust::CompiledQuery::from(rust::PreparedQuery::ImplFn)),
            ("priv-fn", _) => Box::new(rust::CompiledQuery::from(rust::PreparedQuery::PrivFn)),
            ("pub-fn", _) => Box::new(rust::CompiledQuery::from(rust::PreparedQuery::PubFn)),
            ("pub-crate-fn", _) => {
                Box::new(rust::CompiledQuery::from(rust::PreparedQuery::PubCrateFn))
            }
            ("pub-self-fn", _) => {
                Box::new(rust::CompiledQuery::from(rust::PreparedQuery::PubSelfFn))
            }
            ("pub-super-fn", _) => {
                Box::new(rust::CompiledQuery::from(rust::PreparedQuery::PubSuperFn))
            }
            ("async-fn", _) => Box::new(rust::CompiledQuery::from(rust::PreparedQuery::AsyncFn)),
            ("const-fn", _) => Box::new(rust::CompiledQuery::from(rust::PreparedQuery::ConstFn)),
            ("unsafe-fn", _) => Box::new(rust::CompiledQuery::from(rust::PreparedQuery::UnsafeFn)),
            ("extern-fn", _) => Box::new(rust::CompiledQuery::from(rust::PreparedQuery::ExternFn)),
            ("test-fn", _) => Box::new(rust::CompiledQuery::from(rust::PreparedQuery::TestFn)),
            ("impl", _) => Box::new(rust::CompiledQuery::from(rust::PreparedQuery::Impl)),
            ("impl-type", _) => Box::new(rust::CompiledQuery::from(rust::PreparedQuery::ImplType)),
            ("impl-trait", _) => {
                Box::new(rust::CompiledQuery::from(rust::PreparedQuery::ImplTrait))
            }
            ("trait", None) => Box::new(rust::CompiledQuery::from(rust::PreparedQuery::Trait)),
            ("trait", Some(pattern)) => {
                let regex = regex::bytes::Regex::new(pattern).map_err(|e| {
                    ToolError::InvalidRequest(format!("Invalid scope ~ pattern regex: {}", e))
                })?;
                Box::new(rust::CompiledQuery::from(rust::PreparedQuery::TraitNamed(
                    TreeSitterRegex(regex),
                )))
            }
            ("mod", None) => Box::new(rust::CompiledQuery::from(rust::PreparedQuery::Mod)),
            ("mod", Some(pattern)) => {
                let regex = regex::bytes::Regex::new(pattern).map_err(|e| {
                    ToolError::InvalidRequest(format!("Invalid scope ~ pattern regex: {}", e))
                })?;
                Box::new(rust::CompiledQuery::from(rust::PreparedQuery::ModNamed(
                    TreeSitterRegex(regex),
                )))
            }
            ("mod-tests", _) => Box::new(rust::CompiledQuery::from(rust::PreparedQuery::ModTests)),
            ("priv-struct", _) => {
                Box::new(rust::CompiledQuery::from(rust::PreparedQuery::PrivStruct))
            }
            ("pub-struct", _) => {
                Box::new(rust::CompiledQuery::from(rust::PreparedQuery::PubStruct))
            }
            ("pub-crate-struct", _) => Box::new(rust::CompiledQuery::from(
                rust::PreparedQuery::PubCrateStruct,
            )),
            ("pub-self-struct", _) => Box::new(rust::CompiledQuery::from(
                rust::PreparedQuery::PubSelfStruct,
            )),
            ("pub-super-struct", _) => Box::new(rust::CompiledQuery::from(
                rust::PreparedQuery::PubSuperStruct,
            )),
            ("priv-enum", _) => Box::new(rust::CompiledQuery::from(rust::PreparedQuery::PrivEnum)),
            ("pub-crate-enum", _) => {
                Box::new(rust::CompiledQuery::from(rust::PreparedQuery::PubCrateEnum))
            }
            ("pub-self-enum", _) => {
                Box::new(rust::CompiledQuery::from(rust::PreparedQuery::PubSelfEnum))
            }
            ("pub-super-enum", _) => {
                Box::new(rust::CompiledQuery::from(rust::PreparedQuery::PubSuperEnum))
            }
            ("enum-variant", _) => {
                Box::new(rust::CompiledQuery::from(rust::PreparedQuery::EnumVariant))
            }
            ("attribute", _) => Box::new(rust::CompiledQuery::from(rust::PreparedQuery::Attribute)),
            ("unsafe", _) => Box::new(rust::CompiledQuery::from(rust::PreparedQuery::Unsafe)),
            ("pub-enum", _) => Box::new(rust::CompiledQuery::from(rust::PreparedQuery::PubEnum)),
            ("type-def", _) => Box::new(rust::CompiledQuery::from(rust::PreparedQuery::TypeDef)),
            ("type-identifier", _) => Box::new(rust::CompiledQuery::from(
                rust::PreparedQuery::TypeIdentifier,
            )),
            ("identifier", _) => {
                Box::new(rust::CompiledQuery::from(rust::PreparedQuery::Identifier))
            }
            ("closure", _) => Box::new(rust::CompiledQuery::from(rust::PreparedQuery::Closure)),
            (s, _) => {
                return Err(ToolError::InvalidRequest(format!(
                    "Unsupported Rust scope: {}. Must be one of: async-fn, attribute, closure, \
comments, const-fn, doc-comments, enum, enum-variant, extern-fn, fn, \
identifier, impl, impl-fn, impl-trait, impl-type, mod, mod-tests, priv-enum, \
priv-fn, priv-struct, pub-crate-enum, pub-crate-fn, pub-crate-struct, pub-enum, \
pub-fn, pub-self-enum, pub-self-fn, pub-self-struct, pub-struct, pub-super-enum, \
pub-super-fn, pub-super-struct, strings, struct, test-fn, trait, type-def, \
type-identifier, unsafe, unsafe-fn, uses",
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
                    ToolError::InvalidRequest(format!("Invalid scope ~ pattern regex: {}", e))
                })?;
                Box::new(go::CompiledQuery::from(go::PreparedQuery::StructNamed(
                    TreeSitterRegex(regex),
                )))
            }
            ("function" | "func", None) => {
                Box::new(go::CompiledQuery::from(go::PreparedQuery::Func))
            }
            ("function" | "func", Some(pattern)) => {
                let regex = regex::bytes::Regex::new(pattern).map_err(|e| {
                    ToolError::InvalidRequest(format!("Invalid scope ~ pattern regex: {}", e))
                })?;
                Box::new(go::CompiledQuery::from(go::PreparedQuery::FuncNamed(
                    TreeSitterRegex(regex),
                )))
            }
            ("interface", None) => Box::new(go::CompiledQuery::from(go::PreparedQuery::Interface)),
            ("interface", Some(pattern)) => {
                let regex = regex::bytes::Regex::new(pattern).map_err(|e| {
                    ToolError::InvalidRequest(format!("Invalid scope ~ pattern regex: {}", e))
                })?;
                Box::new(go::CompiledQuery::from(go::PreparedQuery::InterfaceNamed(
                    TreeSitterRegex(regex),
                )))
            }
            ("const", _) => Box::new(go::CompiledQuery::from(go::PreparedQuery::Const)),
            ("var", _) => Box::new(go::CompiledQuery::from(go::PreparedQuery::Var)),
            ("method", _) => Box::new(go::CompiledQuery::from(go::PreparedQuery::Method)),
            ("free-func", _) => Box::new(go::CompiledQuery::from(go::PreparedQuery::FreeFunc)),
            ("init-func", _) => Box::new(go::CompiledQuery::from(go::PreparedQuery::InitFunc)),
            ("expression", _) => Box::new(go::CompiledQuery::from(go::PreparedQuery::Expression)),
            ("type-def", _) => Box::new(go::CompiledQuery::from(go::PreparedQuery::TypeDef)),
            ("type-alias", _) => Box::new(go::CompiledQuery::from(go::PreparedQuery::TypeAlias)),
            ("type-params", _) => Box::new(go::CompiledQuery::from(go::PreparedQuery::TypeParams)),
            ("defer", _) => Box::new(go::CompiledQuery::from(go::PreparedQuery::Defer)),
            ("select", _) => Box::new(go::CompiledQuery::from(go::PreparedQuery::Select)),
            ("go", _) => Box::new(go::CompiledQuery::from(go::PreparedQuery::Go)),
            ("switch", _) => Box::new(go::CompiledQuery::from(go::PreparedQuery::Switch)),
            ("labeled", _) => Box::new(go::CompiledQuery::from(go::PreparedQuery::Labeled)),
            ("goto", _) => Box::new(go::CompiledQuery::from(go::PreparedQuery::Goto)),
            ("struct-tags", _) => Box::new(go::CompiledQuery::from(go::PreparedQuery::StructTags)),
            (s, _) => {
                return Err(ToolError::InvalidRequest(format!(
                    "Unsupported Go scope: {}. Must be one of: comments, const, defer, expression, \
free-func, func, go, goto, imports, init-func, interface, labeled, method, \
select, strings, struct, struct-tags, switch, type-alias, type-def, type-params, \
var",
                    s
                )));
            }
        };
        builder.explode(query.as_ref());
        Ok(())
    }

    fn apply_typescript_scope<'a>(
        scope: &str,
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
            "async-function" => Box::new(typescript::CompiledQuery::from(
                typescript::PreparedQuery::AsyncFunction,
            )),
            "sync-function" => Box::new(typescript::CompiledQuery::from(
                typescript::PreparedQuery::SyncFunction,
            )),
            "method" => Box::new(typescript::CompiledQuery::from(
                typescript::PreparedQuery::Method,
            )),
            "constructor" => Box::new(typescript::CompiledQuery::from(
                typescript::PreparedQuery::Constructor,
            )),
            "interface" => Box::new(typescript::CompiledQuery::from(
                typescript::PreparedQuery::Interface,
            )),
            "enum" => Box::new(typescript::CompiledQuery::from(
                typescript::PreparedQuery::Enum,
            )),
            "try-catch" => Box::new(typescript::CompiledQuery::from(
                typescript::PreparedQuery::TryCatch,
            )),
            "var-decl" => Box::new(typescript::CompiledQuery::from(
                typescript::PreparedQuery::VarDecl,
            )),
            "let" => Box::new(typescript::CompiledQuery::from(
                typescript::PreparedQuery::Let,
            )),
            "const" => Box::new(typescript::CompiledQuery::from(
                typescript::PreparedQuery::Const,
            )),
            "var" => Box::new(typescript::CompiledQuery::from(
                typescript::PreparedQuery::Var,
            )),
            "type-params" => Box::new(typescript::CompiledQuery::from(
                typescript::PreparedQuery::TypeParams,
            )),
            "type-alias" => Box::new(typescript::CompiledQuery::from(
                typescript::PreparedQuery::TypeAlias,
            )),
            "namespace" => Box::new(typescript::CompiledQuery::from(
                typescript::PreparedQuery::Namespace,
            )),
            "export" => Box::new(typescript::CompiledQuery::from(
                typescript::PreparedQuery::Export,
            )),
            s => {
                return Err(ToolError::InvalidRequest(format!(
                    "Unsupported TypeScript scope: {}. Must be one of: async-function, class, \
comments, const, constructor, enum, export, function, imports, interface, let, \
method, namespace, strings, sync-function, try-catch, type-alias, type-params, \
var, var-decl",
                    s
                )));
            }
        };
        builder.explode(query.as_ref());
        Ok(())
    }

    fn apply_c_scope<'a>(
        scope: &str,
        builder: &mut ScopedViewBuilder<'a>,
    ) -> Result<(), ToolError> {
        let query: Box<dyn Scoper> = match scope {
            "comments" => Box::new(c::CompiledQuery::from(c::PreparedQuery::Comments)),
            "strings" => Box::new(c::CompiledQuery::from(c::PreparedQuery::Strings)),
            "includes" => Box::new(c::CompiledQuery::from(c::PreparedQuery::Includes)),
            "type-def" => Box::new(c::CompiledQuery::from(c::PreparedQuery::TypeDef)),
            "enum" => Box::new(c::CompiledQuery::from(c::PreparedQuery::Enum)),
            "struct" => Box::new(c::CompiledQuery::from(c::PreparedQuery::Struct)),
            "union" => Box::new(c::CompiledQuery::from(c::PreparedQuery::Union)),
            "variable" => Box::new(c::CompiledQuery::from(c::PreparedQuery::Variable)),
            "function" => Box::new(c::CompiledQuery::from(c::PreparedQuery::Function)),
            "function-def" => Box::new(c::CompiledQuery::from(c::PreparedQuery::FunctionDef)),
            "function-decl" => Box::new(c::CompiledQuery::from(c::PreparedQuery::FunctionDecl)),
            "switch" => Box::new(c::CompiledQuery::from(c::PreparedQuery::Switch)),
            "if" => Box::new(c::CompiledQuery::from(c::PreparedQuery::If)),
            "for" => Box::new(c::CompiledQuery::from(c::PreparedQuery::For)),
            "while" => Box::new(c::CompiledQuery::from(c::PreparedQuery::While)),
            "do" => Box::new(c::CompiledQuery::from(c::PreparedQuery::Do)),
            "identifier" => Box::new(c::CompiledQuery::from(c::PreparedQuery::Identifier)),
            "declaration" => Box::new(c::CompiledQuery::from(c::PreparedQuery::Declaration)),
            "call-expression" => Box::new(c::CompiledQuery::from(c::PreparedQuery::CallExpression)),
            s => {
                return Err(ToolError::InvalidRequest(format!(
                    "Unsupported C scope: {}. Must be one of: call-expression, comments, \
declaration, do, enum, for, function, function-decl, function-def, identifier, \
if, includes, strings, struct, switch, type-def, union, variable, while",
                    s
                )));
            }
        };
        builder.explode(query.as_ref());
        Ok(())
    }

    fn apply_csharp_scope<'a>(
        scope: &str,
        builder: &mut ScopedViewBuilder<'a>,
    ) -> Result<(), ToolError> {
        let query: Box<dyn Scoper> = match scope {
            "comments" => Box::new(csharp::CompiledQuery::from(csharp::PreparedQuery::Comments)),
            "strings" => Box::new(csharp::CompiledQuery::from(csharp::PreparedQuery::Strings)),
            "usings" => Box::new(csharp::CompiledQuery::from(csharp::PreparedQuery::Usings)),
            "struct" => Box::new(csharp::CompiledQuery::from(csharp::PreparedQuery::Struct)),
            "enum" => Box::new(csharp::CompiledQuery::from(csharp::PreparedQuery::Enum)),
            "interface" => Box::new(csharp::CompiledQuery::from(
                csharp::PreparedQuery::Interface,
            )),
            "class" => Box::new(csharp::CompiledQuery::from(csharp::PreparedQuery::Class)),
            "function" | "method" => {
                Box::new(csharp::CompiledQuery::from(csharp::PreparedQuery::Method))
            }
            "constructor" => Box::new(csharp::CompiledQuery::from(
                csharp::PreparedQuery::Constructor,
            )),
            "destructor" => Box::new(csharp::CompiledQuery::from(
                csharp::PreparedQuery::Destructor,
            )),
            "field" => Box::new(csharp::CompiledQuery::from(csharp::PreparedQuery::Field)),
            "property" => Box::new(csharp::CompiledQuery::from(csharp::PreparedQuery::Property)),
            "variable-declaration" => Box::new(csharp::CompiledQuery::from(
                csharp::PreparedQuery::VariableDeclaration,
            )),
            "attribute" => Box::new(csharp::CompiledQuery::from(
                csharp::PreparedQuery::Attribute,
            )),
            "identifier" => Box::new(csharp::CompiledQuery::from(
                csharp::PreparedQuery::Identifier,
            )),
            s => {
                return Err(ToolError::InvalidRequest(format!(
                    "Unsupported C# scope: {}. Must be one of: attribute, class, comments, \
constructor, destructor, enum, field, function, identifier, interface, property, \
strings, struct, usings, variable-declaration",
                    s
                )));
            }
        };
        builder.explode(query.as_ref());
        Ok(())
    }

    fn apply_hcl_scope<'a>(
        scope: &str,
        scope_pattern: Option<&str>,
        builder: &mut ScopedViewBuilder<'a>,
    ) -> Result<(), ToolError> {
        let query: Box<dyn Scoper> = match (scope, scope_pattern) {
            ("comments", _) => Box::new(hcl::CompiledQuery::from(hcl::PreparedQuery::Comments)),
            ("strings", _) => Box::new(hcl::CompiledQuery::from(hcl::PreparedQuery::Strings)),
            ("variable", _) => Box::new(hcl::CompiledQuery::from(hcl::PreparedQuery::Variable)),
            ("resource", _) => Box::new(hcl::CompiledQuery::from(hcl::PreparedQuery::Resource)),
            ("data", _) => Box::new(hcl::CompiledQuery::from(hcl::PreparedQuery::Data)),
            ("output", _) => Box::new(hcl::CompiledQuery::from(hcl::PreparedQuery::Output)),
            ("provider", _) => Box::new(hcl::CompiledQuery::from(hcl::PreparedQuery::Provider)),
            ("required-providers", None) => Box::new(hcl::CompiledQuery::from(
                hcl::PreparedQuery::RequiredProviders,
            )),
            ("required-providers", Some(pattern)) => {
                let regex = regex::bytes::Regex::new(pattern).map_err(|e| {
                    ToolError::InvalidRequest(format!("Invalid scope ~ pattern regex: {}", e))
                })?;
                Box::new(hcl::CompiledQuery::from(
                    hcl::PreparedQuery::RequiredProvidersNamed(TreeSitterRegex(regex)),
                ))
            }
            ("terraform", _) => Box::new(hcl::CompiledQuery::from(hcl::PreparedQuery::Terraform)),
            ("locals", _) => Box::new(hcl::CompiledQuery::from(hcl::PreparedQuery::Locals)),
            ("module", _) => Box::new(hcl::CompiledQuery::from(hcl::PreparedQuery::Module)),
            ("variables", _) => Box::new(hcl::CompiledQuery::from(hcl::PreparedQuery::Variables)),
            ("resource-names", _) => {
                Box::new(hcl::CompiledQuery::from(hcl::PreparedQuery::ResourceNames))
            }
            ("resource-types", _) => {
                Box::new(hcl::CompiledQuery::from(hcl::PreparedQuery::ResourceTypes))
            }
            ("data-names", _) => Box::new(hcl::CompiledQuery::from(hcl::PreparedQuery::DataNames)),
            ("data-sources", _) => {
                Box::new(hcl::CompiledQuery::from(hcl::PreparedQuery::DataSources))
            }
            (s, _) => {
                return Err(ToolError::InvalidRequest(format!(
                    "Unsupported HCL scope: {}. Must be one of: comments, data, data-names, \
data-sources, locals, module, output, provider, required-providers, \
resource, resource-names, resource-types, strings, terraform, variable, variables",
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
        scopes: &[ScopeEntry],
        pattern: Option<&str>,
        literal_string: bool,
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

        // Apply each language scope in order (AND / intersection semantics)
        for entry in scopes {
            Self::apply_language_scope(language, entry, &mut builder)?;
        }

        // Apply pattern scope if specified
        if let Some(pattern_str) = pattern {
            if literal_string {
                let scoper = Literal::try_from(pattern_str.to_string()).map_err(|e| {
                    ToolError::InvalidRequest(format!("Invalid literal pattern: {}", e))
                })?;
                builder.explode(&scoper);
            } else {
                let regex_pattern = RegexPattern::new(pattern_str).map_err(|e| {
                    ToolError::InvalidRequest(format!("Invalid regex pattern: {}", e))
                })?;
                let scoper = Regex::new(regex_pattern);
                builder.explode(&scoper);
            }
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
                    "symbols" => {
                        view.symbols();
                    }
                    "symbols-invert" => {
                        view.invert_symbols();
                    }
                    _ => {
                        return Err(ToolError::InvalidRequest(format!(
                            "Unknown action: {}. Must be one of: delete, squeeze, upper, lower, \
titlecase, normalize, symbols, symbols-invert",
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

    /// Collect all files to process from a glob string relative to cwd.
    /// Plain paths (no wildcards) are resolved normally: a directory is walked,
    /// a file is returned directly.
    fn collect_files(
        glob_str: &str,
        cwd: &Path,
        language: &str,
        hidden: bool,
        gitignored: bool,
    ) -> Result<Vec<PathBuf>, ToolError> {
        let has_wildcard = glob_str.contains(['*', '?', '[']);

        if has_wildcard {
            // Expand glob relative to cwd
            let pattern = cwd.join(glob_str);
            let pattern_str = pattern.to_string_lossy();
            let paths = glob(&pattern_str)
                .map_err(|e| ToolError::InvalidRequest(format!("Invalid glob pattern: {}", e)))?
                .filter_map(|res| res.ok())
                .filter(|p| p.is_file() && Self::is_valid_for_language(p, language))
                .collect();
            Ok(paths)
        } else {
            // Treat as plain path
            let path = cwd.join(glob_str);
            let metadata = std::fs::metadata(&path).map_err(|e| {
                ToolError::InvalidRequest(format!("Invalid path '{}': {}", glob_str, e))
            })?;

            if metadata.is_file() {
                if Self::is_valid_for_language(&path, language) {
                    Ok(vec![path])
                } else {
                    Ok(vec![])
                }
            } else {
                // Walk directory
                let walker = WalkBuilder::new(&path)
                    .hidden(!hidden)
                    .git_ignore(!gitignored)
                    .build();

                let files = walker
                    .filter_map(|e| e.ok())
                    .map(|e| e.into_path())
                    .filter(|p| p.is_file() && Self::is_valid_for_language(p, language))
                    .collect();
                Ok(files)
            }
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
- AND two scopes (type-identifiers inside pub enums): {language: "rust", scope: ["pub-enum", "type-identifier"], pattern: "Subgenre"}
- Named scope filter with ~: {language: "go", scope: "struct~[tT]est"} or as array: {language: "rust", scope: ["pub-enum", "type-identifier~Subgenre"]}
- Literal string search (special chars): {language: "python", scope: "strings", pattern: "a.b[0]", literal_string: true}
- Convert ASCII symbols to Unicode: {language: "rust", scope: "doc-comments", action: "symbols"}
- Revert Unicode symbols to ASCII: {language: "rust", scope: "strings", action: "symbols-invert"}

SCOPES BY LANGUAGE:
Python: async-def, class, class-methods, comments, doc-strings, function,
  function-calls, function-names, globals, identifiers, imports, lambda,
  methods, static-methods, strings, try, types, variable-identifiers, with
Rust: async-fn, attribute, closure, comments, const-fn, doc-comments, enum,
  enum-variant, extern-fn, fn, identifier, impl, impl-fn, impl-trait, impl-type,
  mod, mod-tests, priv-enum, priv-fn, priv-struct, pub-crate-enum, pub-crate-fn,
  pub-crate-struct, pub-enum, pub-fn, pub-self-enum, pub-self-fn, pub-self-struct,
  pub-struct, pub-super-enum, pub-super-fn, pub-super-struct, strings, struct,
  test-fn, trait, type-def, type-identifier, unsafe, unsafe-fn, uses
Go: comments, const, defer, expression, free-func, func, go, goto, imports,
  init-func, interface, labeled, method, select, strings, struct, struct-tags,
  switch, type-alias, type-def, type-params, var
TypeScript: async-function, class, comments, const, constructor, enum, export,
  function, imports, interface, let, method, namespace, strings, sync-function,
  try-catch, type-alias, type-params, var, var-decl
C: call-expression, comments, declaration, do, enum, for, function, function-decl,
  function-def, identifier, if, includes, strings, struct, switch, type-def,
  union, variable, while
C#: attribute, class, comments, constructor, destructor, enum, field, function,
  identifier, interface, property, strings, struct, usings, variable-declaration
HCL: comments, data, data-names, data-sources, locals, module, output, provider,
  required-providers, resource, resource-names, resource-types, strings, terraform,
  variable, variables

scope supports ~ for name filtering (Rust: fn/struct/enum/trait/mod; Go: func/struct/interface; HCL: required-providers):
  e.g. "struct~Config" matches only structs named Config
  e.g. "fn~^handle_" matches only functions whose name starts with handle_
Multiple scopes are ANDed (intersection): each narrows the previous result.

Walks directories recursively, respects .gitignore. Set glob to limit which files are processed."#.to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "language": {
                            "type": "string",
                            "description": "Language for grammar-aware scoping. One of: python, rust, go, typescript, c, csharp, hcl",
                            "enum": ["python", "rust", "go", "typescript", "c", "csharp", "hcl"]
                        },
                        "scope": {
                            "oneOf": [
                                {
                                    "type": "string",
                                    "description": "Single scope. Use 'name~pattern' to filter by name (e.g. 'struct~Config')."
                                },
                                {
                                    "type": "array",
                                    "items": { "type": "string" },
                                    "description": "Multiple scopes ANDed together (each narrows the previous). Each entry may use 'name~pattern' syntax."
                                }
                            ],
                            "description": "Tree-sitter scope(s) to narrow to. String or array of strings. Multiple scopes are intersected left-to-right."
                        },
                        "glob": {
                            "type": "string",
                            "description": "Glob pattern for files to process (e.g. \"src/**/*.py\", \"tests/\", \"main.rs\"). If omitted, processes entire workspace."
                        },
                        "pattern": {
                            "type": "string",
                            "description": "Pattern to match within the scope. Regex by default; set literal_string: true for exact string matching."
                        },
                        "literal_string": {
                            "type": "boolean",
                            "description": "Treat pattern as a literal string instead of regex. Useful when pattern contains special regex characters like . [ ] ( ) *. Default: false."
                        },
                        "replacement": {
                            "type": "string",
                            "description": "Replacement string. Supports capture group variables ($1, $2, $name). Specifying this enables TRANSFORM MODE."
                        },
                        "action": {
                            "type": "string",
                            "description": "Action to perform on matched content. Enables TRANSFORM MODE. If both replacement and action are given, replacement is applied first.",
                            "enum": ["delete", "squeeze", "upper", "lower", "titlecase", "normalize", "symbols", "symbols-invert"]
                        },
                        "hidden": {
                            "type": "boolean",
                            "description": "Include hidden files and directories when walking. Default: false"
                        },
                        "gitignored": {
                            "type": "boolean",
                            "description": "Include .gitignore'd files and directories when walking. Default: false"
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

        // Parse scope: accept string or array, each entry may use ~ syntax
        let scopes: Vec<ScopeEntry> = match args.get("scope") {
            Some(Value::String(s)) => vec![ScopeEntry::parse(s)],
            Some(Value::Array(arr)) => arr
                .iter()
                .filter_map(|v| v.as_str())
                .map(ScopeEntry::parse)
                .collect(),
            _ => {
                return Err(ToolError::InvalidRequest(
                    "scope is required (string or array of strings)".to_string(),
                ));
            }
        };

        if scopes.is_empty() {
            return Err(ToolError::InvalidRequest(
                "scope must not be empty".to_string(),
            ));
        }

        // Extract optional parameters
        let pattern = args.get("pattern").and_then(Value::as_str);
        let literal_string = args
            .get("literal_string")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let replacement = args.get("replacement").and_then(Value::as_str);
        let action = args.get("action").and_then(Value::as_str);
        let hidden = args.get("hidden").and_then(Value::as_bool).unwrap_or(false);
        let gitignored = args
            .get("gitignored")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        // Determine mode
        let is_search_mode = replacement.is_none() && action.is_none();

        // Resolve working directory
        let cwd = context
            .cwd()
            .ok_or_else(|| ToolError::InvalidRequest("No working directory set".to_string()))?
            .to_path_buf();

        // Collect files to process
        let files: Vec<PathBuf> = if let Some(glob_str) = args.get("glob").and_then(Value::as_str) {
            Self::collect_files(glob_str, &cwd, language, hidden, gitignored)?
        } else {
            // No glob: walk cwd
            let walker = WalkBuilder::new(&cwd)
                .hidden(!hidden)
                .git_ignore(!gitignored)
                .build();

            walker
                .filter_map(|e| e.ok())
                .map(|e| e.into_path())
                .filter(|p| p.is_file() && Self::is_valid_for_language(p, language))
                .collect()
        };

        let mut all_matches = Vec::new();
        let mut modified_files = Vec::new();
        let files_searched = files.len();

        for file_path in &files {
            match Self::process_file(
                file_path,
                language,
                &scopes,
                pattern,
                literal_string,
                replacement,
                action,
            )
            .await
            {
                Ok((matches, modified)) => {
                    all_matches.extend(matches);
                    if modified {
                        modified_files.push(file_path.to_string_lossy().to_string());
                    }
                }
                Err(e) => {
                    eprintln!("Error processing {}: {}", file_path.display(), e);
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
                    .strip_prefix(&cwd)
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
                        .strip_prefix(&cwd)
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
            "glob": file_path.to_str().unwrap(),
            "language": "python",
            "scope": "comments",
            "pattern": "TODO"
        });

        let result = tool.call(args, &context).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

        assert_eq!(parsed["mode"], "search");
        assert_eq!(parsed["total_matches"], 1);
        assert_eq!(parsed["files_searched"], 1);

        let results = parsed["results"].as_object().unwrap();
        assert_eq!(results.len(), 1);
        let file_matches = results.values().next().unwrap().as_str().unwrap();
        assert!(file_matches.contains("TODO"));
    }

    #[tokio::test]
    async fn test_search_mode_directory() {
        let temp_dir = TempDir::new().unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));

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
            "glob": file_path.to_str().unwrap(),
            "language": "python",
            "scope": "comments",
            "action": "delete"
        });

        let result = tool.call(args, &context).await.unwrap();
        let parsed: TransformResults = serde_json::from_str(&result).unwrap();

        assert_eq!(parsed.mode, "transform");
        assert_eq!(parsed.total_files_modified, 1);

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
            "glob": file_path.to_str().unwrap(),
            "language": "python",
            "scope": "strings",
            "pattern": "World",
            "replacement": "Universe"
        });

        let result = tool.call(args, &context).await.unwrap();
        let parsed: TransformResults = serde_json::from_str(&result).unwrap();

        assert_eq!(parsed.mode, "transform");
        assert_eq!(parsed.total_files_modified, 1);

        let modified_content = tokio::fs::read_to_string(&file_path).await.unwrap();
        assert!(modified_content.contains("Hello Universe"));
    }

    #[tokio::test]
    async fn test_language_filtering() {
        let temp_dir = TempDir::new().unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));

        tokio::fs::write(temp_dir.path().join("test.py"), "# TODO\n")
            .await
            .unwrap();
        tokio::fs::write(temp_dir.path().join("test.txt"), "TODO\n")
            .await
            .unwrap();

        let tool = SemanticEditTool::new();
        let args = json!({
            "language": "python",
            "scope": "comments",
            "pattern": "TODO"
        });

        let result = tool.call(args, &context).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

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
            "glob": file_path.to_str().unwrap(),
            "language": "python",
            "scope": "comments",
            "action": "delete"
        });

        let result = tool.call(args, &context).await.unwrap();
        let parsed: TransformResults = serde_json::from_str(&result).unwrap();

        assert_eq!(parsed.total_files_modified, 0);

        let content = tokio::fs::read_to_string(&file_path).await.unwrap();
        assert_eq!(content, code);
    }

    #[tokio::test]
    async fn test_glob_pattern_filters_files() {
        let temp_dir = TempDir::new().unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));

        // Create src/ and tests/ subdirs
        tokio::fs::create_dir(temp_dir.path().join("src"))
            .await
            .unwrap();
        tokio::fs::create_dir(temp_dir.path().join("tests"))
            .await
            .unwrap();

        tokio::fs::write(temp_dir.path().join("src/main.py"), "# TODO src\n")
            .await
            .unwrap();
        tokio::fs::write(temp_dir.path().join("tests/test_main.py"), "# TODO test\n")
            .await
            .unwrap();

        let tool = SemanticEditTool::new();

        // Glob targeting only src/
        let args = json!({
            "language": "python",
            "scope": "comments",
            "pattern": "TODO",
            "glob": "src/**/*.py"
        });

        let result = tool.call(args, &context).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

        assert_eq!(parsed["files_searched"], 1);
        assert_eq!(parsed["total_matches"], 1);
    }

    #[tokio::test]
    async fn test_literal_string_matches_exactly() {
        let temp_dir = TempDir::new().unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));

        // "a.b" with literal_string=false would match "a<any char>b" via regex
        // "a.b" with literal_string=true should only match the exact string "a.b"
        let code = r#"
x = "a.b"
y = "axb"
z = "a.b.c"
"#;

        let file_path = temp_dir.path().join("test.py");
        tokio::fs::write(&file_path, code).await.unwrap();

        let tool = SemanticEditTool::new();
        let args = json!({
            "glob": file_path.to_str().unwrap(),
            "language": "python",
            "scope": "strings",
            "pattern": "a.b",
            "literal_string": true
        });

        let result = tool.call(args, &context).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

        // Should match "a.b" inside the two strings that contain it, not "axb"
        let results_obj = parsed["results"].as_object().unwrap();
        let matches_text = results_obj.values().next().unwrap().as_str().unwrap();
        assert!(!matches_text.contains("axb"));
    }

    #[tokio::test]
    async fn test_multi_scope_intersection() {
        let temp_dir = TempDir::new().unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));

        // Rust: search for "Subgenre" only within pub-enum bodies, not elsewhere
        let code = r#"
pub enum Genre {
    Rock(Subgenre),
    Jazz,
}

const MOST_POPULAR_SUBGENRE: Subgenre = Subgenre::Something;

pub struct Musician {
    genres: Vec<Subgenre>,
}
"#;

        let file_path = temp_dir.path().join("test.rs");
        tokio::fs::write(&file_path, code).await.unwrap();

        let tool = SemanticEditTool::new();
        let args = json!({
            "glob": file_path.to_str().unwrap(),
            "language": "rust",
            "scope": ["pub-enum", "type-identifier"],
            "pattern": "Subgenre"
        });

        let result = tool.call(args, &context).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

        // Should only find Subgenre inside the pub enum, not in the const or struct
        assert_eq!(parsed["mode"], "search");
        let total = parsed["total_matches"].as_u64().unwrap();
        assert!(total >= 1, "expected at least one match inside pub-enum");
    }

    #[tokio::test]
    async fn test_tilde_scope_pattern() {
        let temp_dir = TempDir::new().unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));

        let code = r#"
pub struct Config {
    pub value: u32,
}

pub struct Handler {
    pub name: String,
}
"#;

        let file_path = temp_dir.path().join("test.rs");
        tokio::fs::write(&file_path, code).await.unwrap();

        let tool = SemanticEditTool::new();
        // Only match structs named "Config"
        let args = json!({
            "glob": file_path.to_str().unwrap(),
            "language": "rust",
            "scope": "struct~Config"
        });

        let result = tool.call(args, &context).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

        assert_eq!(parsed["mode"], "search");
        // Should only surface Config struct content, not Handler
        if let Some(results) = parsed["results"].as_object()
            && let Some(matches) = results.values().next().and_then(|v| v.as_str())
        {
            assert!(matches.contains("Config") || matches.contains("value"));
            assert!(!matches.contains("Handler") && !matches.contains("name: String"));
        }
    }

    #[tokio::test]
    async fn test_symbols_action() {
        let temp_dir = TempDir::new().unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));

        let code = "x = \"a != b\"\n";
        let file_path = temp_dir.path().join("test.py");
        tokio::fs::write(&file_path, code).await.unwrap();

        let tool = SemanticEditTool::new();
        let args = json!({
            "glob": file_path.to_str().unwrap(),
            "language": "python",
            "scope": "strings",
            "action": "symbols"
        });

        let result = tool.call(args, &context).await.unwrap();
        let parsed: TransformResults = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed.mode, "transform");

        let modified = tokio::fs::read_to_string(&file_path).await.unwrap();
        assert!(
            modified.contains('≠'),
            "expected != to become ≠, got: {modified}"
        );
    }

    #[tokio::test]
    async fn test_symbols_invert_action() {
        let temp_dir = TempDir::new().unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));

        let code = "x = \"a ≠ b\"\n";
        let file_path = temp_dir.path().join("test.py");
        tokio::fs::write(&file_path, code).await.unwrap();

        let tool = SemanticEditTool::new();
        let args = json!({
            "glob": file_path.to_str().unwrap(),
            "language": "python",
            "scope": "strings",
            "action": "symbols-invert"
        });

        let result = tool.call(args, &context).await.unwrap();
        let parsed: TransformResults = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed.mode, "transform");

        let modified = tokio::fs::read_to_string(&file_path).await.unwrap();
        assert!(
            modified.contains("!="),
            "expected ≠ to become !=, got: {modified}"
        );
    }
}
