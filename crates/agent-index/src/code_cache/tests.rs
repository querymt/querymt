//! Behavioural tests for the code cache (spec `code-index-cache`, group 3 scenarios).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use super::{CacheStorageKind, CodeCacheClient, CodeIndexCacheOptions};
use crate::function_index::FunctionIndexConfig;

fn function_source(name: &str) -> String {
    format!(
        "pub fn {name}(values: &[i32]) -> i32 {{\n    let mut total = 0;\n    for value in values {{\n        total += value;\n    }}\n    total\n}}\n"
    )
}

struct Workspace {
    _dir: tempfile::TempDir,
    root: PathBuf,
    base: PathBuf,
}

impl Workspace {
    fn new(files: &[(&str, &str)]) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("ws");
        fs::create_dir_all(&root).expect("create root");
        let workspace = Self {
            base: dir.path().join("cache"),
            root,
            _dir: dir,
        };
        for (name, content) in files {
            workspace.write(name, content);
        }
        workspace
    }

    fn write(&self, name: &str, content: &str) {
        let path = self.root.join(name);
        fs::create_dir_all(path.parent().expect("parent")).expect("create parent");
        fs::write(path, content).expect("write file");
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    fn options(&self) -> CodeIndexCacheOptions {
        CodeIndexCacheOptions::in_directory(&self.base)
    }

    fn start(&self, options: CodeIndexCacheOptions) -> CodeCacheClient {
        CodeCacheClient::spawn(&self.root, options, FunctionIndexConfig::default())
            .expect("spawn cache owner")
    }
}

fn entry_count(entries: &[(PathBuf, Vec<crate::IndexedFunctionEntry>)]) -> usize {
    entries.iter().map(|(_, e)| e.len()).sum()
}

fn names_of(entries: &[(PathBuf, Vec<crate::IndexedFunctionEntry>)], path: &Path) -> Vec<String> {
    entries
        .iter()
        .filter(|(p, _)| p == path)
        .flat_map(|(_, e)| e.iter().map(|f| f.name.clone()))
        .collect()
}

#[tokio::test]
async fn statistics_after_a_cold_build() {
    let ws = Workspace::new(&[
        ("a.rs", &function_source("alpha")),
        ("b.rs", &function_source("beta")),
        ("c.rs", &function_source("gamma")),
    ]);
    let client = ws.start(ws.options());
    let paths = ["a.rs", "b.rs", "c.rs"].map(|n| ws.path(n));

    let entries = client.index_all(&paths).await.expect("index");
    let stats = client.stats().await.expect("stats");

    assert_eq!(entry_count(&entries), 3);
    assert_eq!(stats.parsed_files, 3);
    assert_eq!(stats.reused_files, 0);
    assert_eq!(stats.storage, CacheStorageKind::Disk);
}

#[tokio::test]
async fn one_broken_file_among_valid_files() {
    let ws = Workspace::new(&[
        ("a.rs", &function_source("alpha")),
        ("b.rs", &function_source("beta")),
        ("garbage.rs", "fn {{{{ ))))) let = ;;; pub pub"),
    ]);
    // A file that is not UTF-8 text cannot be indexed as text.
    fs::write(ws.path("binary.rs"), [0xff, 0xfe, 0x00, 0xc3, 0x28]).expect("write binary");
    let paths = ["a.rs", "b.rs", "garbage.rs", "binary.rs"].map(|n| ws.path(n));

    let first = ws.start(ws.options());
    let entries = first.index_all(&paths).await.expect("first index");
    assert_eq!(names_of(&entries, &ws.path("a.rs")), ["alpha"]);
    assert_eq!(names_of(&entries, &ws.path("b.rs")), ["beta"]);
    assert!(names_of(&entries, &ws.path("binary.rs")).is_empty());
    first.shutdown().await;

    let second = ws.start(ws.options());
    let entries = second.index_all(&paths).await.expect("second index");
    assert_eq!(names_of(&entries, &ws.path("a.rs")), ["alpha"]);
    assert_eq!(names_of(&entries, &ws.path("b.rs")), ["beta"]);
    let stats = second.stats().await.expect("stats");
    assert_eq!(stats.parsed_files, 0, "broken files are stored results too");
    assert_eq!(stats.reused_files, 4);
}

#[tokio::test]
async fn two_workspaces_have_separate_caches() {
    let dir = tempfile::tempdir().expect("tempdir");
    let base = dir.path().join("cache");
    let one = dir.path().join("one");
    let two = dir.path().join("two");
    for (root, name) in [(&one, "first_fn"), (&two, "second_fn")] {
        fs::create_dir_all(root).expect("create root");
        fs::write(root.join("a.rs"), function_source(name)).expect("write");
    }
    let options = CodeIndexCacheOptions::in_directory(&base);

    let dir_one = options.cache_directory(&one).expect("dir one");
    let dir_two = options.cache_directory(&two).expect("dir two");
    assert_ne!(dir_one, dir_two);
    for directory in [&dir_one, &dir_two] {
        assert_eq!(directory.parent(), Some(base.as_path()));
        let name = directory
            .file_name()
            .and_then(|n| n.to_str())
            .expect("name");
        assert_eq!(name.len(), 16, "16 hex digit hash");
        assert!(name.chars().all(|c| c.is_ascii_hexdigit()));
    }

    let spawn = |root: &Path| {
        CodeCacheClient::spawn(root, options.clone(), FunctionIndexConfig::default())
            .expect("spawn")
    };
    let (client_one, client_two) = (spawn(&one), spawn(&two));
    let entries_one = client_one
        .index_all(&[one.join("a.rs")])
        .await
        .expect("one");
    let entries_two = client_two
        .index_all(&[two.join("a.rs")])
        .await
        .expect("two");
    assert_eq!(names_of(&entries_one, &one.join("a.rs")), ["first_fn"]);
    assert_eq!(names_of(&entries_two, &two.join("a.rs")), ["second_fn"]);
    client_one.shutdown().await;
    client_two.shutdown().await;
    assert!(
        dir_one.is_dir() && dir_two.is_dir(),
        "each root has its directory"
    );

    // Restart: the same relative path still resolves to each root's own result.
    let (client_one, client_two) = (spawn(&one), spawn(&two));
    let entries_one = client_one
        .index_all(&[one.join("a.rs")])
        .await
        .expect("one");
    let entries_two = client_two
        .index_all(&[two.join("a.rs")])
        .await
        .expect("two");
    assert_eq!(names_of(&entries_one, &one.join("a.rs")), ["first_fn"]);
    assert_eq!(names_of(&entries_two, &two.join("a.rs")), ["second_fn"]);
    assert_eq!(client_one.stats().await.expect("stats").parsed_files, 0);
    assert_eq!(client_two.stats().await.expect("stats").parsed_files, 0);
}

/// Captures log records for the one test that asserts on warnings.
struct CaptureLogger(Mutex<Vec<String>>);

impl log::Log for CaptureLogger {
    fn enabled(&self, _: &log::Metadata<'_>) -> bool {
        true
    }

    fn log(&self, record: &log::Record<'_>) {
        if record.level() == log::Level::Warn {
            self.0
                .lock()
                .expect("log lock")
                .push(record.args().to_string());
        }
    }

    fn flush(&self) {}
}

fn capture_logger() -> &'static CaptureLogger {
    static LOGGER: OnceLock<&'static CaptureLogger> = OnceLock::new();
    LOGGER.get_or_init(|| {
        let logger: &'static CaptureLogger =
            Box::leak(Box::new(CaptureLogger(Mutex::new(Vec::new()))));
        log::set_logger(logger).expect("only this test installs a logger");
        log::set_max_level(log::LevelFilter::Warn);
        logger
    })
}

#[tokio::test]
async fn cache_directory_held_by_another_owner() {
    let logger = capture_logger();
    let ws = Workspace::new(&[
        ("a.rs", &function_source("alpha")),
        ("b.rs", &function_source("beta")),
    ]);
    let paths = ["a.rs", "b.rs"].map(|n| ws.path(n));

    let holder = ws.start(ws.options());
    holder.index_all(&paths).await.expect("holder index");
    assert_eq!(
        holder.stats().await.expect("stats").storage,
        CacheStorageKind::Disk
    );

    let second = ws.start(ws.options());
    let entries = second.index_all(&paths).await.expect("second index");
    assert_eq!(names_of(&entries, &ws.path("a.rs")), ["alpha"]);
    assert_eq!(names_of(&entries, &ws.path("b.rs")), ["beta"]);
    let symbols = second
        .symbols(vec![(ws.path("a.rs"), Arc::from(function_source("alpha")))])
        .await
        .expect("lookup");
    assert!(symbols[0].is_ok());
    assert_eq!(
        second.stats().await.expect("stats").storage,
        CacheStorageKind::Memory
    );

    let warnings = logger.0.lock().expect("log lock").clone();
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("held by another owner") && w.contains("memory")),
        "expected a memory-fallback warning, got {warnings:?}"
    );
}

#[tokio::test]
async fn damaged_cache_is_rebuilt() {
    let ws = Workspace::new(&[
        ("a.rs", &function_source("alpha")),
        ("b.rs", &function_source("beta")),
    ]);
    let paths = ["a.rs", "b.rs"].map(|n| ws.path(n));
    let options = ws.options();

    let first = ws.start(options.clone());
    first.index_all(&paths).await.expect("first index");
    first.shutdown().await;

    let directory = options.cache_directory(&ws.root).expect("directory");
    let mut damaged = 0;
    for entry in fs::read_dir(&directory).expect("cache dir") {
        let path = entry.expect("entry").path();
        if path.is_file() {
            fs::write(&path, b"not a valid cache file").expect("overwrite");
            damaged += 1;
        }
    }
    assert!(damaged > 0, "the cache directory holds files to damage");

    let second = ws.start(options.clone());
    let entries = second.index_all(&paths).await.expect("rebuild");
    assert_eq!(names_of(&entries, &ws.path("a.rs")), ["alpha"]);
    assert_eq!(names_of(&entries, &ws.path("b.rs")), ["beta"]);
    assert_eq!(second.stats().await.expect("stats").parsed_files, 2);
    second.shutdown().await;

    let third = ws.start(options);
    third.index_all(&paths).await.expect("warm");
    let stats = third.stats().await.expect("stats");
    assert_eq!(stats.parsed_files, 0, "the rebuilt cache persists");
    assert_eq!(stats.storage, CacheStorageKind::Disk);
}

#[tokio::test]
async fn schema_version_change_rebuilds_the_cache() {
    let ws = Workspace::new(&[
        ("a.rs", &function_source("alpha")),
        ("b.rs", &function_source("beta")),
    ]);
    let paths = ["a.rs", "b.rs"].map(|n| ws.path(n));
    let version = |schema_version| CodeIndexCacheOptions {
        schema_version,
        ..ws.options()
    };

    let first = ws.start(version(1));
    first.index_all(&paths).await.expect("index");
    first.shutdown().await;

    let same = ws.start(version(1));
    same.index_all(&paths).await.expect("same version");
    assert_eq!(same.stats().await.expect("stats").parsed_files, 0);
    same.shutdown().await;

    let changed = ws.start(version(2));
    changed.index_all(&paths).await.expect("new version");
    assert_eq!(changed.stats().await.expect("stats").parsed_files, 2);
}

#[tokio::test]
async fn idle_results_are_evicted_and_recomputed() {
    let ws = Workspace::new(&[("a.rs", &function_source("alpha"))]);
    let options = CodeIndexCacheOptions {
        max_idle: Duration::from_millis(1),
        ..ws.options()
    };
    let client = ws.start(options);
    let file = (ws.path("a.rs"), Arc::<str>::from(function_source("alpha")));

    let before = client.symbols(vec![file.clone()]).await.expect("first");
    assert_eq!(client.stats().await.expect("stats").parsed_files, 1);

    let unchanged = client.symbols(vec![file.clone()]).await.expect("unchanged");
    let stats = client.stats().await.expect("stats");
    assert_eq!(
        stats.parsed_files, 1,
        "unchanged text is reused before eviction"
    );
    assert_eq!(stats.reused_files, 1);
    assert_eq!(
        unchanged[0].as_ref().expect("symbols").symbols,
        before[0].as_ref().expect("symbols").symbols
    );

    tokio::time::sleep(Duration::from_millis(50)).await;
    let evicted = client.collect().await.expect("collect");
    assert!(evicted > 0, "the idle result is evicted");

    let after = client.symbols(vec![file]).await.expect("after");
    assert_eq!(client.stats().await.expect("stats").parsed_files, 2);
    assert_eq!(
        after[0].as_ref().expect("symbols").symbols,
        before[0].as_ref().expect("symbols").symbols
    );
}
