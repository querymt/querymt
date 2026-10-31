//! Every provider dispatch in this crate is either recorded or explicitly
//! classified (task 6.4). A new `.chat(..)`-style call in production code
//! fails this test until it is added here with a classification, so a paid
//! request cannot silently bypass accounting.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// How a dispatch site is accounted for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    /// Records start/settlement through an `AttemptOperation`.
    Recorded,
    /// Not a provider request: a call on the agent's own prompt API, which
    /// reaches a provider only through a recorded path.
    AgentApi,
}

/// (path relative to `src/`, expected call count, classification).
const SITES: &[(&str, usize, Class)] = &[
    // Main loop: non-streaming, streaming and tool-bearing requests.
    ("agent/execution/transitions.rs", 3, Class::Recorded),
    // Auxiliary generation.
    ("session/compaction.rs", 2, Class::Recorded),
    ("delegation/summarizer.rs", 2, Class::Recorded),
    // `SessionHandle::submit_request`, which requires a `DispatchLedger`.
    ("session/provider.rs", 1, Class::Recorded),
    // `Agent::chat` / `AgentRunner::chat` run a normal prompt cycle.
    ("runner.rs", 1, Class::AgentApi),
];

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        if path.is_dir() {
            if name != "tests" && name != "test_utils" {
                rust_files(&path, out);
            }
        } else if name.ends_with(".rs") && !name.ends_with("_tests.rs") && name != "tests.rs" {
            out.push(path);
        }
    }
}

/// Production source only: stop at the `#[cfg(test)]` module.
fn production_part(source: &str) -> String {
    let mut out = String::new();
    for line in source.lines() {
        if line.trim() == "#[cfg(test)]" {
            break;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

fn dispatch_calls(source: &str) -> usize {
    const CALLS: [&str; 5] = [
        ".chat(",
        ".chat_with_tools(",
        ".chat_stream(",
        ".chat_stream_with_tools(",
        ".embed(",
    ];
    production_part(source)
        .lines()
        .filter(|line| {
            let t = line.trim_start();
            !(t.starts_with("//") || t.starts_with("///") || t.starts_with("//!"))
        })
        .map(|line| CALLS.iter().filter(|c| line.contains(*c)).count())
        .sum()
}

#[test]
fn every_dispatch_site_is_recorded_or_classified() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rust_files(&root, &mut files);

    let mut found: BTreeMap<String, usize> = BTreeMap::new();
    for file in files {
        let rel = file
            .strip_prefix(&root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        let n = dispatch_calls(&std::fs::read_to_string(&file).unwrap());
        if n > 0 {
            found.insert(rel, n);
        }
    }

    let expected: BTreeMap<String, usize> = SITES
        .iter()
        .map(|(path, count, _)| (path.to_string(), *count))
        .collect();
    assert_eq!(
        found, expected,
        "provider dispatch sites changed: record the new call with an \
         AttemptOperation (or classify it) and update SITES"
    );
}

#[test]
fn recorded_sites_require_a_ledger_by_signature() {
    // Compile-time evidence that there is no unrecorded variant: these paths
    // only accept a ledger / operation.
    let compaction = include_str!("../session/compaction.rs");
    assert!(compaction.contains("ledger: &DispatchLedger"));
    let provider = include_str!("../session/provider.rs");
    assert!(provider.contains("ledger: &crate::accounting::DispatchLedger"));
    let summarizer = include_str!("../delegation/summarizer.rs");
    assert!(summarizer.contains("operation: &AttemptOperation"));
}
