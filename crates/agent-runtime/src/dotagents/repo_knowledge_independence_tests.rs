//! Runtime-side check that the `.agents` protocol loader and repository
//! knowledge read one memory independently.
//!
//! It needs `DotagentsLoader` (runtime) next to the services-layer repository
//! tools, so it lives here instead of in `tools/builtins/repo_knowledge.rs`.

use crate::config::RepositoryKnowledgeSettings;
use crate::dotagents::{DotagentsLoadOptions, DotagentsLoader};
use crate::knowledge::repository::RepositoryKnowledgeRegistry;
use crate::tools::builtins::repo_knowledge::RepoKnowledgeQueryTool;
use crate::tools::{AgentToolContext, Tool as ToolTrait, ToolError};
use querymt::chat::ToolResultPart;
use serde_json::{Value, json};
use std::fs;
use std::path::PathBuf;

const MANIFEST: &str =
    "version = 1\n[documents]\ninclude = [\"docs/**/*.md\"]\n[code]\ninclude = [\"src/**/*.rs\"]\n";

struct Fx {
    _tmp: tempfile::TempDir,
    repo: PathBuf,
    ctx: AgentToolContext,
}

impl Fx {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir_all(repo.join(".git")).unwrap();
        let registry = RepositoryKnowledgeRegistry::from_settings_in(
            &RepositoryKnowledgeSettings::enabled(),
            tmp.path().join("cfg"),
        );
        let ctx = AgentToolContext::basic("s".into(), Some(repo.clone()))
            .with_repository_knowledge(registry);
        let fx = Self {
            _tmp: tmp,
            repo,
            ctx,
        };
        fx.put(".qmt/knowledge.toml", MANIFEST);
        fx
    }

    fn put(&self, rel: &str, content: &str) {
        let path = self.repo.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    async fn run(&self, tool: &dyn ToolTrait, args: Value) -> Result<String, ToolError> {
        let parts = tool.call(args, &self.ctx).await?;
        Ok(parts
            .into_iter()
            .map(|part| match part {
                ToolResultPart::Text { text } => text,
                _ => String::new(),
            })
            .collect())
    }
}

/// Scenarios "Protocol memories are selected as documents" and "Workspace
/// memory is also imported by the protocol loader": with the `.agents`
/// protocol loader enabled on the same checkout, both read the memory
/// independently, the loader's result is identical before and after
/// repository indexing, and nothing under `.agents/` changes.
#[tokio::test]
async fn the_protocol_loader_and_repository_indexing_read_one_memory_independently() {
    let fx = Fx::new();
    fx.put(
        ".qmt/knowledge.toml",
        "version = 1\n[documents]\ninclude = [\".agents/memories/*.md\"]\n",
    );
    fx.put(
        ".agents/memories/runtime.md",
        "---\nid: runtime\ntitle: Runtime\n---\n# Runtime\nquokka lifecycle details\n",
    );
    let load = || {
        DotagentsLoader::new(
            DotagentsLoadOptions::enabled()
                .with_workspace(&fx.repo)
                .with_global_enabled(false),
        )
        .load()
        .unwrap()
        .memories
    };
    fn walk(dir: &std::path::Path, out: &mut Vec<(std::path::PathBuf, Option<Vec<u8>>)>) {
        let mut entries: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        entries.sort();
        for path in entries {
            if path.is_dir() {
                out.push((path.clone(), None));
                walk(&path, out);
            } else {
                let bytes = fs::read(&path).unwrap();
                out.push((path, Some(bytes)));
            }
        }
    }
    let snapshot = |dir: &std::path::Path| {
        let mut out = Vec::new();
        walk(dir, &mut out);
        out
    };
    let agents_before = snapshot(&fx.repo.join(".agents"));
    let before = load();
    assert_eq!(before.len(), 1);
    let memory = before.values().next().unwrap();
    assert_eq!(memory.id, "runtime");
    assert!(memory.body.contains("quokka lifecycle details"));

    let found = fx
        .run(&RepoKnowledgeQueryTool, json!({"question": "quokka"}))
        .await
        .unwrap();
    assert_eq!(
        found.matches(".agents/memories/runtime.md:").count(),
        1,
        "{found}"
    );

    assert_eq!(load(), before, "protocol import is unchanged by indexing");
    assert_eq!(snapshot(&fx.repo.join(".agents")), agents_before);
}
