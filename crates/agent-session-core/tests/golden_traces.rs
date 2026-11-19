//! Native golden test: replay each committed trace and compare the output.
//! Regenerate with `cargo run -p querymt-agent-session-core --example
//! regen-traces --features test-support`.

use querymt_agent_session_core::SessionCore;
use querymt_agent_session_core::input::SessionInput;
use serde_json::{Value, json};
use std::path::Path;

fn trace_files() -> Vec<std::path::PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/traces");
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .expect("traces dir")
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    files.sort();
    files
}

fn replay(batches: &Value) -> Value {
    let batches: Vec<Vec<SessionInput>> =
        serde_json::from_value(batches.clone()).expect("batches decode as SessionInput");
    let mut core = SessionCore::new("s1");
    let reports: Vec<Value> = batches
        .iter()
        .map(|batch| serde_json::to_value(core.apply(batch)).expect("report"))
        .collect();
    let ids = core.turn_ids();
    json!({
        "reports": reports,
        "header": core.header(),
        "turnIds": ids,
        "turns": core.turns(&ids),
    })
}

#[test]
fn golden_traces_match_the_committed_output() {
    let files = trace_files();
    assert!(files.len() >= 8, "expected the committed golden traces");
    for path in files {
        let text = std::fs::read_to_string(&path).expect("read trace");
        let file: Value = serde_json::from_str(&text).expect("trace json");
        assert_eq!(
            replay(&file["batches"]),
            file["expected"],
            "{} differs; run regen-traces if the change is intended",
            path.display()
        );
    }
}
