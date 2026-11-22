//! Spec `code-index-cache`: the symbol tools read through the workspace index and fall back
//! to direct parsing, with identical output either way.

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use querymt::chat::ToolResultPart;
use querymt_agent_index::{CodeCacheClient, CodeIndexCacheOptions, FunctionIndexConfig};
use serde_json::{Value, json};
use tempfile::TempDir;

use super::find_references::FindSymbolReferencesTool;
use super::get_function::GetFunctionTool;
use super::get_symbol::GetSymbolTool;
use super::helpers::CODE_INDEX_LOOKUP_DEADLINE;
use super::index::IndexTool;
use super::replace_symbol::ReplaceSymbolTool;
use crate::tools::{AgentToolContext, Tool};

const RUST_SOURCE: &str = "pub struct Config {\n    pub name: String,\n}\n\nimpl Config {\n    pub fn new(name: String) -> Self {\n        Self { name }\n    }\n}\n\npub fn greet(name: &str) -> String {\n    format!(\"hello {name}\")\n}\n";
const PYTHON_SOURCE: &str = "import os\n\nclass Config:\n    def validate(self) -> bool:\n        return True\n\ndef greet(name):\n    return 'hello ' + name\n";
const TS_SOURCE: &str = "export class Config {\n    validate(): boolean {\n        return true;\n    }\n}\n\nexport function greet(name: string): string {\n    return `hello ${name}`;\n}\n";

/// `(file name, source)` pairs in several languages; each defines `greet`.
const FILES: [(&str, &str); 3] = [
    ("lib.rs", RUST_SOURCE),
    ("util.py", PYTHON_SOURCE),
    ("app.ts", TS_SOURCE),
];

struct Fixture {
    dir: TempDir,
}

impl Fixture {
    fn new() -> Self {
        let dir = TempDir::new().expect("tempdir");
        for (name, source) in FILES {
            fs::write(dir.path().join(name), source).expect("write fixture");
        }
        Self { dir }
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn write(&self, name: &str, content: &str) {
        fs::write(self.root().join(name), content).expect("write file");
    }

    fn client(&self) -> CodeCacheClient {
        CodeCacheClient::spawn(
            self.root(),
            CodeIndexCacheOptions::memory(),
            FunctionIndexConfig::default(),
        )
        .expect("spawn cache owner")
    }

    fn plain(&self) -> AgentToolContext {
        AgentToolContext::basic("session".to_owned(), Some(self.root().to_path_buf()))
    }

    fn indexed(&self, client: &CodeCacheClient) -> AgentToolContext {
        self.plain().with_workspace_index(Some(client.clone()))
    }
}

fn text(parts: Vec<ToolResultPart>) -> String {
    parts
        .into_iter()
        .filter_map(|part| match part {
            ToolResultPart::Text { text } => Some(text),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn get_symbol_args(root: &Path, file: &str) -> Value {
    json!({"requests": [{"path": file, "symbol": "greet"}], "root": root})
}

async fn get_symbol(context: &AgentToolContext, root: &Path, file: &str) -> String {
    text(
        GetSymbolTool::new()
            .call(get_symbol_args(root, file), context)
            .await
            .expect("get_symbol"),
    )
}

async fn parsed_and_reused(client: &CodeCacheClient) -> (u64, u64) {
    let stats = client.stats().await.expect("stats");
    (stats.parsed_files, stats.reused_files)
}

/// Run all five tools on every fixture file and return their outputs in a fixed order.
async fn run_all_tools(context: &AgentToolContext, root: &Path) -> Vec<String> {
    let mut outputs = Vec::new();
    for (file, _) in FILES {
        outputs.push(text(
            IndexTool::new()
                .call(json!({"path": file, "root": root}), context)
                .await
                .expect("index"),
        ));
        outputs.push(get_symbol(context, root, file).await);
        outputs.push(text(
            GetFunctionTool::new()
                .call(
                    json!({"paths": [file], "names": ["greet"], "root": root}),
                    context,
                )
                .await
                .expect("get_function"),
        ));
    }
    outputs.push(text(
        FindSymbolReferencesTool::new()
            .call(
                json!({"paths": ["."], "symbols": ["greet", "Config"], "root": root}),
                context,
            )
            .await
            .expect("find_symbol_references"),
    ));
    outputs
}

async fn replace_greet(context: &AgentToolContext, root: &Path, file: &str) -> String {
    text(
        ReplaceSymbolTool::new()
            .call(
                json!({
                    "replacements": [{
                        "path": file,
                        "symbol": "greet",
                        "new_text": match file {
                            "lib.rs" => "pub fn greet(name: &str) -> String {\n    name.to_owned()\n}",
                            "util.py" => "def greet(name):\n    return name",
                            _ => "export function greet(name: string): string {\n    return name;\n}",
                        }
                    }],
                    "root": root
                }),
                context,
            )
            .await
            .expect("replace_symbol"),
    )
}

#[tokio::test]
async fn symbol_tool_output_matches_with_and_without_the_index() {
    let fx = Fixture::new();
    let client = fx.client();

    let direct = run_all_tools(&fx.plain(), fx.root()).await;
    let cached = run_all_tools(&fx.indexed(&client), fx.root()).await;
    assert_eq!(cached, direct);
    assert!(
        cached.iter().all(|output| !output.is_empty()),
        "every tool produced output"
    );
    let (parsed, _) = parsed_and_reused(&client).await;
    assert!(parsed > 0, "the index answered the calls");

    // `replace_symbol` writes identical content with and without the index.
    for (file, source) in FILES {
        let direct_output = replace_greet(&fx.plain(), fx.root(), file).await;
        let direct_written = fs::read_to_string(fx.root().join(file)).expect("read");
        fx.write(file, source);

        let cached_output = replace_greet(&fx.indexed(&client), fx.root(), file).await;
        let cached_written = fs::read_to_string(fx.root().join(file)).expect("read");
        fx.write(file, source);

        assert_eq!(cached_output, direct_output, "{file}: tool output");
        assert_eq!(cached_written, direct_written, "{file}: written file");
        assert_ne!(
            cached_written, source,
            "{file}: the replacement was applied"
        );
    }
}

#[tokio::test]
async fn edit_not_yet_reported_by_the_watcher() {
    let fx = Fixture::new();
    let client = fx.client();
    let context = fx.indexed(&client);

    let before = get_symbol(&context, fx.root(), "lib.rs").await;

    // The symbol moves down; no watcher change set is applied to the index.
    let edited = format!("// a\n// b\n// c\n\n{RUST_SOURCE}");
    fx.write("lib.rs", &edited);
    let after = get_symbol(&context, fx.root(), "lib.rs").await;

    let direct = get_symbol(&fx.plain(), fx.root(), "lib.rs").await;
    assert_ne!(after, before, "the second call sees the new lines");
    assert_eq!(after, direct, "and matches a direct parse of the new text");
}

#[tokio::test]
async fn repeated_lookup_on_an_unchanged_file_is_reused() {
    let fx = Fixture::new();
    let client = fx.client();
    let context = fx.indexed(&client);

    let first = get_symbol(&context, fx.root(), "lib.rs").await;
    let (parsed, reused) = parsed_and_reused(&client).await;
    assert_eq!(parsed, 1);

    let second = get_symbol(&context, fx.root(), "lib.rs").await;
    assert_eq!(second, first);
    assert_eq!(
        parsed_and_reused(&client).await,
        (parsed, reused + 1),
        "no new parse, one more reused result"
    );
}

#[tokio::test]
async fn directory_reference_search_reuses_unchanged_files() {
    let fx = Fixture::new();
    let client = fx.client();
    let context = fx.indexed(&client);
    let search = || async {
        text(
            FindSymbolReferencesTool::new()
                .call(
                    json!({"paths": ["."], "symbols": ["greet"], "root": fx.root()}),
                    &context,
                )
                .await
                .expect("find_symbol_references"),
        )
    };

    let first = search().await;
    let (parsed, reused) = parsed_and_reused(&client).await;
    assert_eq!(parsed, FILES.len() as u64);
    assert!(first.contains("greet"));

    let second = search().await;
    assert_eq!(second, first);
    assert_eq!(
        parsed_and_reused(&client).await,
        (parsed, reused + FILES.len() as u64)
    );
}

#[tokio::test]
async fn no_workspace_index_yet() {
    let fx = Fixture::new();
    let client = fx.client();

    let without = get_symbol(&fx.plain(), fx.root(), "lib.rs").await;
    let with = get_symbol(&fx.indexed(&client), fx.root(), "lib.rs").await;
    assert_eq!(without, with);
    assert!(without.contains("greet"));
}

#[tokio::test]
async fn file_outside_the_workspace_root() {
    let fx = Fixture::new();
    let outside = TempDir::new().expect("outside dir");
    fs::write(outside.path().join("other.rs"), RUST_SOURCE).expect("write");
    let client = fx.client();
    let context = fx.indexed(&client);
    let args = json!({
        "paths": [outside.path().join("other.rs")],
        "names": ["greet"],
        "root": outside.path()
    });

    let via_index_context = text(
        GetFunctionTool::new()
            .call(args.clone(), &context)
            .await
            .expect("get_function"),
    );
    let direct = text(
        GetFunctionTool::new()
            .call(args, &fx.plain())
            .await
            .expect("get_function"),
    );
    assert_eq!(via_index_context, direct);
    assert!(via_index_context.contains("greet"));
    assert_eq!(parsed_and_reused(&client).await, (0, 0), "stats unchanged");
}

#[tokio::test]
async fn index_does_not_answer_in_time() {
    let fx = Fixture::new();
    let context = fx
        .plain()
        .with_workspace_index(Some(CodeCacheClient::unresponsive(fx.root())));

    let started = Instant::now();
    let output = get_symbol(&context, fx.root(), "lib.rs").await;
    let elapsed = started.elapsed();

    assert_eq!(output, get_symbol(&fx.plain(), fx.root(), "lib.rs").await);
    assert!(
        elapsed >= CODE_INDEX_LOOKUP_DEADLINE,
        "waited for the deadline, took {elapsed:?}"
    );
    assert!(
        elapsed < CODE_INDEX_LOOKUP_DEADLINE + Duration::from_secs(3),
        "fell back promptly, took {elapsed:?}"
    );
}

#[tokio::test]
async fn index_fails_the_lookup() {
    let fx = Fixture::new();
    let context = fx
        .plain()
        .with_workspace_index(Some(CodeCacheClient::disconnected(fx.root())));

    let started = Instant::now();
    let output = text(
        FindSymbolReferencesTool::new()
            .call(
                json!({"paths": ["."], "symbols": ["greet"], "root": fx.root()}),
                &context,
            )
            .await
            .expect("the index must not fail the tool call"),
    );
    let direct = text(
        FindSymbolReferencesTool::new()
            .call(
                json!({"paths": ["."], "symbols": ["greet"], "root": fx.root()}),
                &fx.plain(),
            )
            .await
            .expect("find_symbol_references"),
    );
    assert_eq!(output, direct);
    assert!(output.contains("greet"));
    assert!(
        started.elapsed() < CODE_INDEX_LOOKUP_DEADLINE,
        "a stopped owner fails immediately, not at the deadline"
    );
}

#[tokio::test]
async fn busy_index_costs_one_deadline_per_call() {
    // More files than one directory-search batch, so a per-batch deadline would show.
    let fx = Fixture::new();
    for i in 0..300 {
        fx.write(&format!("gen_{i:03}.rs"), "pub fn unrelated() {}\n");
    }
    let context = fx
        .plain()
        .with_workspace_index(Some(CodeCacheClient::unresponsive(fx.root())));
    let args = json!({
        "paths": ["."],
        "symbols": ["greet"],
        "root": fx.root(),
        "max_results": 1000
    });

    let started = Instant::now();
    let output = text(
        FindSymbolReferencesTool::new()
            .call(args.clone(), &context)
            .await
            .expect("find_symbol_references"),
    );
    let elapsed = started.elapsed();
    let direct = text(
        FindSymbolReferencesTool::new()
            .call(args, &fx.plain())
            .await
            .expect("find_symbol_references"),
    );

    assert_eq!(output, direct);
    assert!(
        elapsed < CODE_INDEX_LOOKUP_DEADLINE * 2,
        "the index is skipped after the first timeout, took {elapsed:?}"
    );
}

#[tokio::test]
async fn reference_search_limit_applies_across_paths() {
    let fx = Fixture::new();
    fs::create_dir_all(fx.root().join("sub")).expect("sub");
    fx.write(
        "sub/more.rs",
        "pub fn greet_again() {}\npub fn call() { greet_again(); }\n",
    );
    let call = |max_results: u64| {
        let context = fx.plain();
        let root = fx.root().to_path_buf();
        async move {
            text(
                FindSymbolReferencesTool::new()
                    .call(
                        json!({
                            "paths": ["lib.rs", "sub"],
                            "symbols": ["greet"],
                            "root": root,
                            "max_results": max_results
                        }),
                        &context,
                    )
                    .await
                    .expect("find_symbol_references"),
            )
        }
    };

    let all = call(100).await;
    assert_eq!(
        all.lines().filter(|l| l.starts_with("  ")).count(),
        3,
        "{all}"
    );
    assert!(!all.contains("Results capped"));

    // `lib.rs` has one `greet` match, half of the limit; the directory after it must still
    // be searched, and the third match is reported as capped.
    let capped = call(2).await;
    let match_lines = capped.lines().filter(|l| l.starts_with("  ")).count();
    assert_eq!(match_lines, 2, "{capped}");
    assert!(
        capped.contains("sub/more.rs"),
        "directory searched: {capped}"
    );
    assert!(capped.contains("Results capped"), "{capped}");
}
