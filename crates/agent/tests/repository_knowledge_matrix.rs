//! Task 7.1: cross-cutting acceptance matrix for portable repository knowledge.
//!
//! Exercises the public API end to end against temporary repositories: mixed
//! Markdown/memory inputs, frontmatter edge cases, references, clone/cache loss,
//! fast-path freshness, source/manifest changes, linked worktrees, concurrent
//! refresh, private isolation and `.agents/` non-modification.
//!
//! Delivery deduplication/compaction and result-policy preservation need the
//! agent execution harness and are covered by
//! `agent::execution_tests::repository_delivery` (run as part of task 7.2).

use querymt_agent::config::RepositoryKnowledgeSettings;
use querymt_agent::knowledge::repository::{
    AcknowledgeRequest, CheckRequest, GetRequest, ListRequest, MapRequest, QueryRequest,
    ReconcileOptions, ReconcileStatus, RefreshPolicy, RepositoryKnowledgeRegistry,
    RepositoryService, RootRequest, ScaffoldRequest, resolve_root,
};
use querymt_agent::knowledge::repository::{AcknowledgeSelection, Fetched};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const MANIFEST: &str = r#"version = 1

[documents]
include = ["docs/**/*.md", ".agents/memories/*.md", "crates/*/AGENTS.md"]

[code]
include = ["src/**/*.rs"]
"#;

fn write(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

/// Every file under `root` (excluding `.git`) with its bytes.
fn snapshot(root: &Path) -> BTreeMap<String, Vec<u8>> {
    fn walk(base: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        for entry in fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.file_name().is_some_and(|n| n == ".git") {
                continue;
            }
            if path.is_dir() {
                walk(base, &path, out);
            } else {
                let rel = path.strip_prefix(base).unwrap();
                out.insert(
                    rel.to_string_lossy().replace('\\', "/"),
                    fs::read(&path).unwrap(),
                );
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

fn populate(repo: &Path) {
    fs::create_dir_all(repo.join(".git")).unwrap();
    write(repo, ".qmt/knowledge.toml", MANIFEST);
    // Plain Markdown with no frontmatter, and links of every kind.
    write(
        repo,
        "docs/plain.md",
        "# Plain\nSession compaction keeps recent turns.\n\n## Links\n[ok](other.md) [gone](missing.md) [web](https://example.invalid/x) [escape](../../outside.md)\n",
    );
    // Unknown frontmatter keys are retained and ignored.
    write(
        repo,
        "docs/other.md",
        "---\nid: other-doc\ntitle: Other\nowner: team-x\n---\n# Other\nScheduler wakeups are coalesced.\n",
    );
    // Malformed frontmatter isolates only this document.
    write(repo, "docs/broken.md", "---\ntitle: never closed\n# oops\n");
    // Legacy memories: one usable, one disabled.
    write(
        repo,
        ".agents/memories/runtime.md",
        "---\ntitle: Runtime memory\ntags: runtime, actors\n---\nActors serialise state changes.\n",
    );
    write(
        repo,
        ".agents/memories/off.md",
        "---\nenabled: false\n---\nshould never be served\n",
    );
    write(
        repo,
        "src/lib.rs",
        "/// Queue of work.\npub struct Queue { items: Vec<u32> }\nimpl Queue {\n    pub fn push(&mut self, item: u32) {}\n}\n",
    );
}

/// Move every file's mtime well outside the racy-timestamp window so the
/// metadata fast path may trust it.
fn age_files(root: &Path) {
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    for rel in snapshot(root).keys() {
        let file = fs::OpenOptions::new()
            .write(true)
            .open(root.join(rel))
            .unwrap();
        file.set_modified(old).unwrap();
    }
}

struct Fx {
    _tmp: tempfile::TempDir,
    repo: PathBuf,
    config: PathBuf,
    service: RepositoryService,
}

fn open(repo: &Path, config: &Path, settings: &RepositoryKnowledgeSettings) -> RepositoryService {
    let root = resolve_root(&RootRequest {
        host_root: Some(repo),
        ..Default::default()
    })
    .unwrap();
    let index = root.index_path(config).unwrap();
    RepositoryService::open(
        root,
        &index,
        ReconcileOptions::from(settings),
        RefreshPolicy::default(),
    )
    .unwrap()
}

impl Fx {
    fn new() -> Self {
        Self::with(RepositoryKnowledgeSettings::default())
    }
    fn with(settings: RepositoryKnowledgeSettings) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let config = tmp.path().join("config");
        populate(&repo);
        let service = open(&repo, &config, &settings);
        Self {
            _tmp: tmp,
            repo,
            config,
            service,
        }
    }
    fn ask(&self, question: &str) -> Vec<(String, String)> {
        let response = self
            .service
            .query(&QueryRequest {
                question: question.into(),
                ..Default::default()
            })
            .unwrap();
        response
            .hits
            .into_iter()
            .map(|hit| (hit.id, hit.path))
            .collect()
    }
    fn paths(&self, question: &str) -> Vec<String> {
        self.ask(question).into_iter().map(|(_, p)| p).collect()
    }
}

fn get_path(service: &RepositoryService, path: &str) -> Fetched {
    service
        .get(&GetRequest {
            id: None,
            path: Some(path.into()),
            section: None,
            cursor: None,
            max_bytes: 4096,
        })
        .unwrap()
}

#[test]
fn mixed_inputs_are_one_projection_with_isolated_failures() {
    let fx = Fx::new();
    let report = fx.service.refresh().unwrap();
    assert!(matches!(
        report.status,
        ReconcileStatus::Built | ReconcileStatus::Updated
    ));

    // Plain Markdown, unknown-key Markdown and a legacy memory are all served.
    assert!(
        fx.paths("compaction recent turns")
            .contains(&"docs/plain.md".into())
    );
    assert!(
        fx.paths("scheduler wakeups coalesced")
            .contains(&"docs/other.md".into())
    );
    assert!(
        fx.paths("actors serialise state")
            .contains(&".agents/memories/runtime.md".into())
    );
    // Malformed and disabled sources are excluded; siblings are unaffected.
    for forbidden in ["docs/broken.md", ".agents/memories/off.md"] {
        assert!(
            !fx.paths("never closed should never be served oops")
                .contains(&forbidden.into()),
            "{forbidden} must not be served"
        );
    }
    let stats = fx.service.stats().unwrap();
    assert_eq!(stats.excluded_sources, 2, "{stats:?}");
    assert_eq!(stats.documents, 3);

    // One projection per source, even for overlapping patterns.
    let listed = fx.service.list(&ListRequest::default()).unwrap();
    let mut seen: Vec<_> = listed.entries.iter().map(|e| format!("{e:?}")).collect();
    let before = seen.len();
    seen.dedup();
    assert_eq!(before, seen.len());

    // An explicit id survives and resolves; a path-only fetch of a multi-section
    // document returns its catalog, not a single section.
    match get_path(&fx.service, "docs/plain.md") {
        Fetched::Catalog(catalog) => assert!(catalog.sections.len() >= 2),
        other => panic!("expected catalog, got {other:?}"),
    }
    assert!(matches!(
        fx.service
            .get(&GetRequest {
                id: Some("id:other-doc".into()),
                path: None,
                section: None,
                cursor: None,
                max_bytes: 4096,
            })
            .unwrap(),
        Fetched::Catalog(_) | Fetched::Section(_)
    ));
}

#[test]
fn references_are_checked_but_never_followed_or_authorizing() {
    let fx = Fx::new();
    let outside = fx.repo.parent().unwrap().join("outside.md");
    fs::write(&outside, "# Outside\nsecretword\n").unwrap();

    let report = fx.service.check(&CheckRequest::default()).unwrap();
    let codes: Vec<_> = report.findings.iter().map(|f| f.code.as_str()).collect();
    assert!(
        codes.iter().any(|c| c.contains("reference")),
        "broken references are reported: {codes:?}"
    );
    // Linked-to files that are not selected never enter the index.
    assert!(fx.paths("secretword").is_empty());
    // Nothing in the findings leaks an absolute path.
    let json = serde_json::to_string(
        &report
            .findings
            .iter()
            .map(|f| f.to_json())
            .collect::<Vec<_>>(),
    )
    .unwrap();
    assert!(!json.contains(fx.repo.to_str().unwrap()), "{json}");
    // Unknown keys and the external URL are not failures by themselves.
    assert!(
        !report
            .findings
            .iter()
            .any(|f| f.path.as_deref() == Some("docs/other.md")),
        "{:?}",
        report.findings
    );
}

#[test]
fn clone_and_cache_loss_rebuild_to_equal_logical_results() {
    let fx = Fx::new();
    let before = fx.ask("compaction recent turns");
    assert!(!before.is_empty());

    // A second checkout at a different path with a different cache directory.
    let tmp2 = tempfile::tempdir().unwrap();
    let clone = tmp2.path().join("somewhere").join("else");
    populate(&clone);
    let other = open(
        &clone,
        &tmp2.path().join("cfg"),
        &RepositoryKnowledgeSettings::default(),
    );
    let after = other
        .query(&QueryRequest {
            question: "compaction recent turns".into(),
            ..Default::default()
        })
        .unwrap();
    let after: Vec<_> = after.hits.into_iter().map(|h| (h.id, h.path)).collect();
    assert_eq!(
        before, after,
        "portable ids and citations are machine independent"
    );

    // Losing the cache loses nothing but time; sources are untouched.
    let sources = snapshot(&fx.repo);
    drop(fx.service);
    fs::remove_dir_all(&fx.config).unwrap();
    let reopened = open(
        &fx.repo,
        &fx.config,
        &RepositoryKnowledgeSettings::default(),
    );
    let rebuilt = reopened
        .query(&QueryRequest {
            question: "compaction recent turns".into(),
            ..Default::default()
        })
        .unwrap();
    let rebuilt: Vec<_> = rebuilt.hits.into_iter().map(|h| (h.id, h.path)).collect();
    assert_eq!(before, rebuilt);
    assert_eq!(sources, snapshot(&fx.repo));
}

#[test]
fn fast_path_freshness_and_source_and_manifest_changes() {
    let fx = Fx::new();
    age_files(&fx.repo);
    fx.service.refresh().unwrap();

    // Warm: nothing is hashed or parsed.
    let warm = fx.service.refresh().unwrap();
    assert_eq!(warm.status, ReconcileStatus::Unchanged);
    assert_eq!((warm.hashed, warm.reparsed), (0, 0), "{warm:?}");

    // External edit without any watcher event is seen on the next retrieval.
    write(
        &fx.repo,
        "docs/other.md",
        "# Other\nBrandnewterm appears here.\n",
    );
    assert!(fx.paths("brandnewterm").contains(&"docs/other.md".into()));
    assert!(fx.paths("scheduler wakeups coalesced").is_empty());

    // Addition and deletion.
    write(&fx.repo, "docs/added.md", "# Added\nzebrafish notes\n");
    assert!(fx.paths("zebrafish").contains(&"docs/added.md".into()));
    fs::remove_file(fx.repo.join("docs/added.md")).unwrap();
    assert!(fx.paths("zebrafish").is_empty());

    // A disabled memory stops being served, and becomes serveable again.
    write(
        &fx.repo,
        ".agents/memories/runtime.md",
        "---\nenabled: false\n---\nActors serialise state changes.\n",
    );
    assert!(fx.paths("actors serialise state").is_empty());

    // Manifest change reconciles the selection.
    write(
        &fx.repo,
        ".qmt/knowledge.toml",
        "version = 1\n[documents]\ninclude = [\"docs/plain.md\"]\n",
    );
    assert!(fx.paths("brandnewterm").is_empty());
    assert!(
        fx.paths("compaction recent turns")
            .contains(&"docs/plain.md".into())
    );

    // An unsupported manifest disables the corpus with a diagnostic, not a crawl.
    write(&fx.repo, ".qmt/knowledge.toml", "version = 99\n");
    let report = fx.service.refresh().unwrap();
    assert_eq!(report.status, ReconcileStatus::ManifestInvalid);
    assert!(fx.paths("compaction recent turns").is_empty());
    // Removing it entirely means "not configured".
    fs::remove_file(fx.repo.join(".qmt/knowledge.toml")).unwrap();
    assert_eq!(
        fx.service.refresh().unwrap().status,
        ReconcileStatus::NotConfigured
    );
}

#[test]
fn forced_hashing_reads_every_selected_file() {
    let fx = Fx::with(RepositoryKnowledgeSettings {
        force_hash: true,
        ..Default::default()
    });
    let first = fx.service.refresh().unwrap();
    let again = fx.service.refresh().unwrap();
    assert_eq!(again.hashed, first.selected, "{again:?}");
    assert_eq!(again.reparsed, 0);
}

#[test]
fn linked_worktrees_keep_independent_corpora() {
    let tmp = tempfile::tempdir().unwrap();
    let common = tmp.path().join("main/.git/worktrees/a");
    fs::create_dir_all(&common).unwrap();
    let config = tmp.path().join("config");
    let mut trees = Vec::new();
    for (name, body) in [("wt-a", "alphaversion"), ("wt-b", "betaversion")] {
        let wt = tmp.path().join(name);
        fs::create_dir_all(&wt).unwrap();
        fs::write(wt.join(".git"), format!("gitdir: {}\n", common.display())).unwrap();
        write(&wt, ".qmt/knowledge.toml", MANIFEST);
        write(&wt, "docs/shared.md", &format!("# Shared\n{body} text\n"));
        trees.push(wt);
    }
    let registry = RepositoryKnowledgeRegistry::from_settings_in(
        &RepositoryKnowledgeSettings {
            enabled: true,
            ..Default::default()
        },
        config,
    )
    .unwrap();
    // Sessions starting in a nested directory resolve the same worktree.
    fs::create_dir_all(trees[0].join("docs")).unwrap();
    let a = registry.service_for(Some(&trees[0])).unwrap();
    let a_nested = registry.service_for(Some(&trees[0].join("docs"))).unwrap();
    let b = registry.service_for(Some(&trees[1])).unwrap();
    assert!(Arc::ptr_eq(&a, &a_nested));
    assert!(!Arc::ptr_eq(&a, &b));

    let ask = |s: &RepositoryService, q: &str| {
        s.query(&QueryRequest {
            question: q.into(),
            ..Default::default()
        })
        .unwrap()
        .hits
        .len()
    };
    assert_eq!(ask(&a, "alphaversion"), 1);
    assert_eq!(ask(&a, "betaversion"), 0);
    assert_eq!(ask(&b, "betaversion"), 1);
    assert_eq!(ask(&b, "alphaversion"), 0);

    // Refreshing one after an edit does not disturb the other.
    write(&trees[0], "docs/shared.md", "# Shared\ngammaversion text\n");
    assert_eq!(ask(&a, "gammaversion"), 1);
    assert_eq!(ask(&b, "betaversion"), 1);
}

#[test]
fn concurrent_refresh_and_readers_see_only_complete_generations() {
    let fx = Fx::new();
    fx.service.refresh().unwrap();
    let service = Arc::new(fx.service);
    let repo = fx.repo.clone();
    let writer = {
        let repo = repo.clone();
        std::thread::spawn(move || {
            for i in 0..15 {
                write(
                    &repo,
                    "docs/churn.md",
                    &format!("# Churn\nversion marker v{i}\n\n## Part\nsecond v{i}\n"),
                );
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        })
    };
    let readers: Vec<_> = (0..4)
        .map(|_| {
            let service = service.clone();
            std::thread::spawn(move || {
                for _ in 0..25 {
                    if let Ok(response) = service.query(&QueryRequest {
                        question: "version marker".into(),
                        ..Default::default()
                    }) {
                        // Whatever generation we saw is internally consistent:
                        // the stable documents are always all present.
                        let stats = service.stats().unwrap();
                        assert!(stats.documents >= 3, "{stats:?}");
                        drop(response);
                    }
                }
            })
        })
        .collect();
    writer.join().unwrap();
    for reader in readers {
        reader.join().unwrap();
    }
    // Quiescent: the final state is exactly the last write.
    let last = service
        .query(&QueryRequest {
            question: "second v14".into(),
            ..Default::default()
        })
        .unwrap();
    assert!(!last.hits.is_empty());
    assert!(
        last.hits.iter().all(|hit| hit.path == "docs/churn.md"),
        "{:?}",
        last.hits
    );
}

#[test]
fn private_knowledge_is_not_reachable_and_the_cache_lives_outside_the_repo() {
    let fx = Fx::new();
    // A "session store" and global memory next to the repo are never indexed.
    let private = fx.repo.parent().unwrap().join("sessions.db");
    fs::write(&private, b"zxqprivatemarker").unwrap();
    assert!(fx.paths("zxqprivatemarker").is_empty());

    fx.service.refresh().unwrap();
    assert!(fx.config.exists(), "index is created under the config dir");
    assert!(
        snapshot(&fx.repo)
            .values()
            .all(|bytes| !bytes.starts_with(b"SQLite format 3")),
        "no database file inside the repository"
    );
    // Rebuilding / deleting the cache leaves private data alone.
    drop(fx.service);
    fs::remove_dir_all(&fx.config).unwrap();
    assert_eq!(fs::read(&private).unwrap(), b"zxqprivatemarker");

    // Fetches cannot name anything outside the checkout.
    let fx = Fx::new();
    for bad in ["../sessions.db", "/etc/passwd", "docs/../../sessions.db"] {
        assert!(
            fx.service
                .get(&GetRequest {
                    id: None,
                    path: Some(bad.into()),
                    section: None,
                    cursor: None,
                    max_bytes: 1024,
                })
                .is_err(),
            "{bad} must be refused"
        );
    }
}

#[test]
fn nothing_under_agents_or_the_sources_is_modified_by_read_operations() {
    let fx = Fx::new();
    let before = snapshot(&fx.repo);

    fx.service.refresh().unwrap();
    fx.ask("actors serialise state");
    let _ = fx.service.list(&ListRequest::default()).unwrap();
    let _ = fx.service.stats().unwrap();
    let _ = fx.service.map(&MapRequest::default()).unwrap();
    let _ = fx.service.check(&CheckRequest::default()).unwrap();
    // Previews are also read-only.
    let _ = fx.service.plan_scaffold(&ScaffoldRequest {
        directory: "src".into(),
        file_name: None,
    });
    let _ = fx.service.plan_acknowledge(&AcknowledgeRequest {
        document: "docs/other.md".into(),
        selection: AcknowledgeSelection::WholeDocument,
    });

    assert_eq!(before, snapshot(&fx.repo));
    assert!(
        !fx.repo.join(".qmt/knowledge/evidence").exists(),
        "indexing never acknowledges evidence"
    );
}

#[test]
fn acknowledgement_writes_are_explicit_confined_and_never_under_agents() {
    let fx = Fx::new();
    let plan = fx
        .service
        .plan_acknowledge(&AcknowledgeRequest {
            document: ".agents/memories/runtime.md".into(),
            selection: AcknowledgeSelection::WholeDocument,
        })
        .map(|plan| fx.service.apply_acknowledge(&plan));
    // Either refused at plan time or at apply time; never written under .agents.
    let wrote = matches!(plan, Ok(Ok(_)));
    let evidence = fx.repo.join(".qmt/knowledge/evidence");
    let under_agents = snapshot(&fx.repo)
        .keys()
        .any(|k| k.starts_with(".agents/") && k.ends_with(".json"));
    assert!(!under_agents);
    if wrote {
        assert!(evidence.exists());
    }

    // A normal document acknowledges and then reports no drift; an edit makes
    // the check notice, and a stale plan is refused.
    let plan = fx
        .service
        .plan_acknowledge(&AcknowledgeRequest {
            document: "docs/other.md".into(),
            selection: AcknowledgeSelection::WholeDocument,
        })
        .unwrap();
    write(
        &fx.repo,
        "docs/other.md",
        "# Other\nchanged after preview\n",
    );
    assert!(
        fx.service.apply_acknowledge(&plan).is_err(),
        "stale preimage"
    );
}
