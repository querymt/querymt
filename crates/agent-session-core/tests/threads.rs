//! Native consumers (spec: Native Rust library, design D5a).

use querymt_agent_session_core::SessionCore;
use querymt_agent_session_core::input::SessionInput;
use serde_json::Value;
use std::path::Path;

/// The 4x golden snapshot batch.
fn snapshot_4x() -> Vec<SessionInput> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/traces/snapshot_4x.json");
    let text = std::fs::read_to_string(path).expect("read 4x trace");
    let file: Value = serde_json::from_str(&text).expect("trace json");
    let batches: Vec<Vec<SessionInput>> =
        serde_json::from_value(file["batches"].clone()).expect("batches decode");
    batches.into_iter().flatten().collect()
}

fn dump(core: &SessionCore) -> String {
    let ids = core.turn_ids();
    serde_json::to_string(&(core.header(), core.turns(&ids))).expect("serialize")
}

#[test]
fn snapshot_hydrated_on_a_background_thread() {
    let batch = snapshot_4x();

    let mut local = SessionCore::new("s1");
    local.apply(&batch);

    let background_batch = batch.clone();
    let moved = std::thread::spawn(move || {
        let mut core = SessionCore::new("s1");
        core.apply(&background_batch);
        core
    })
    .join()
    .expect("background thread");

    assert_eq!(dump(&moved), dump(&local));
    assert!(moved.turn_ids().len() > 50);
}
